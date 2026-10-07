"""Cross-platform tests for the diagnostic bundle's isolation/provenance contract."""
import importlib.util
import os
import subprocess
import pathlib
import plistlib
import tempfile
import unittest
import sys
import shlex
import tarfile

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("bundle", pathlib.Path(__file__).with_name("mac-experiment-bundle.py"))
bundle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundle)
REV = "0123456789abcdef0123456789abcdef01234567"


class BundleTest(unittest.TestCase):
    def test_shipping_archive_omits_staging_ownership_markers(self):
        # Falsifier: remove the marker exclusion from the production tar command.
        shell = pathlib.Path(__file__).with_name('mac-bundle.sh').read_text(encoding='utf-8')
        command = shlex.split(next(line for line in shell.splitlines() if line.startswith('tar ')))
        with tempfile.TemporaryDirectory() as directory:
            stage = pathlib.Path(directory)
            paths = ['Falcon.app/Contents/Resources/docs/licenses', 'docs/licenses']
            for name in paths:
                folder = stage/name
                folder.mkdir(parents=True)
                (folder/'.falcon-generated-materials').write_text('internal marker')
                (folder/'keep.txt').write_text('licence')
            output = stage/'fixture.tgz'
            expanded = []
            for arg in command:
                if arg == '${PACKAGE_FILES[@]}':
                    expanded.extend(['Falcon.app', 'docs/licenses'])
                else:
                    expanded.append(arg.replace('$OUT/$ARCHIVE', str(output)).replace('$OUT', str(stage)))
            subprocess.run(expanded, check=True, capture_output=True)
            with tarfile.open(output) as archive:
                names = archive.getnames()
                self.assertFalse(any(n.endswith('/.falcon-generated-materials') for n in names))
                for name in paths:
                    self.assertIn(name+'/keep.txt', names)

    def test_source_revision_matches_checkout_and_archives_do_not_inherit_parent_head(self):
        with tempfile.TemporaryDirectory(prefix="falcon-revision-") as tmp:
            repo = pathlib.Path(tmp) / "checkout"; repo.mkdir()
            env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
            def git(*args):
                return subprocess.check_output(["git", "-C", str(repo), *args], env=env, stderr=subprocess.DEVNULL).decode().strip()
            git("init", "--quiet")
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "--allow-empty", "--no-gpg-sign", "-m", "fixture")
            head = git("rev-parse", "HEAD")
            self.assertEqual(bundle.source_revision(repo), head)
            self.assertEqual(bundle.source_revision(repo, head), head)
            self.assertEqual(bundle.source_revision(repo, head, require_clean=True), head)
            dirty=repo/'local-edit.txt';dirty.write_text('uncommitted')
            with self.assertRaisesRegex(ValueError,'clean checkout'):
                bundle.source_revision(repo, head, require_clean=True)
            dirty.unlink()
            with self.assertRaises(ValueError): bundle.source_revision(repo, "f" * 40)
            archive = repo / "nested-archive"; archive.mkdir()
            self.assertEqual(bundle.source_revision(archive, REV), REV)
            with self.assertRaises(ValueError): bundle.source_revision(archive)
            archive = pathlib.Path(tmp) / "standalone-archive"; archive.mkdir()
            self.assertEqual(bundle.source_revision(archive, REV), REV)
            for invalid in [None, "bad", "A" * 40]:
                with self.assertRaises(ValueError): bundle.source_revision(archive, invalid)

    def test_smoke_rejects_failed_host_and_blocking_dialogs(self):
        smoke_spec = importlib.util.spec_from_file_location("smoke", pathlib.Path(__file__).with_name("test-mac-chrome-smoke.py"))
        smoke = importlib.util.module_from_spec(smoke_spec)
        smoke_spec.loader.exec_module(smoke)
        ready = {"build": "1.0.8-mac-full04", "ready": True, "welcome": False, "association": False, "photo_width": 960, "full_toolbar": True, "toolbar_roundtrip": True, "toolbar_clipped": False, "toolbar_mouse_down_can_move_window": False, "titlebar_height": 38.0}
        smoke.validate_report(ready)
        # toolbar_clipped True / None: the full04-3 build drew a 44 pt toolbar into a 38 pt title bar (run 36111887084).
        for key, value in (("full_toolbar", False), ("toolbar_roundtrip", False), ("ready", False), ("welcome", True), ("association", True), ("photo_width", 0), ("build", "1.0.8-mac-chrome02"),
                           ("toolbar_mouse_down_can_move_window", True), ("toolbar_mouse_down_can_move_window", None), ("toolbar_clipped", True), ("toolbar_clipped", None), ("titlebar_height", None), ("titlebar_height", 0)):
            with self.assertRaises(RuntimeError):
                smoke.validate_report(dict(ready, **{key: value}))

    def test_identity_does_not_register_document_or_url_handlers(self):
        original = {"CFBundleIdentifier": "com.hwu0101.falcon", "CFBundleVersion": "1.0.8",
                    "CFBundleDocumentTypes": ["image"], "UTImportedTypeDeclarations": ["raw"],
                    "UTExportedTypeDeclarations": ["exported"], "CFBundleURLTypes": ["falcon"]}
        results = [bundle.diagnostic_plist(original, m, REV) for m in bundle.MODES]
        self.assertEqual(len({i["CFBundleIdentifier"] for i in results}), 5)
        for info in results:
            for key in ("CFBundleDocumentTypes", "UTImportedTypeDeclarations", "UTExportedTypeDeclarations", "CFBundleURLTypes"):
                self.assertNotIn(key, info)
            self.assertEqual(info["FalconSourceRevision"], REV)
            self.assertEqual(info["CFBundleVersion"], "1.0.8")
            self.assertEqual(info["FalconBuildLabel"], "1.0.8-mac-full04")
        self.assertIn("CFBundleDocumentTypes", original)

    def test_invalid_mode_and_revision_fail(self):
        for mode, revision in (("shipping", REV), ("../other", REV), ("control", "unknown")):
            with self.assertRaises(ValueError):
                bundle.diagnostic_plist({}, mode, revision)

    def test_diagnostic_identity_keeps_the_complete_source_version(self):
        info = bundle.diagnostic_plist({"CFBundleVersion": "1.0.9", "FalconSourceVersion": "1.0.9-rc.1"}, "candidate", REV)
        self.assertEqual(info["FalconBuildLabel"], "1.0.9-rc.1-mac-full04")

    def test_resources_and_plist_agree_and_shipping_binary_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            app = pathlib.Path(d) / "Falcon Mac Public Host 04.app"
            executable = app / "Contents/MacOS/falcon"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"FALCON_MAC_NATIVE_TOOLBAR_01")
            bundle.check_binary(executable, "shipping")
            plist = app / "Contents/Info.plist"
            plist.write_bytes(plistlib.dumps({"CFBundleExecutable": "falcon"}))
            with self.assertRaises(ValueError):
                bundle.configure(app, "native-host", REV)
            executable.write_bytes(b"FALCON_MAC_CHROME_EXPERIMENT_01")
            with self.assertRaises(ValueError):
                bundle.check_binary(executable, "shipping")
            with self.assertRaises(ValueError):
                bundle.check_binary(executable, "native-host")
            executable.write_bytes(b"FALCON_MAC_CHROME_EXPERIMENT_04")
            with self.assertRaises(ValueError):
                bundle.check_binary(executable, "shipping")
            bundle.configure(app, "native-host", REV)
            resources = app / "Contents/Resources"
            self.assertEqual((resources / "experiment-mode.txt").read_text().strip(), "native-host")
            self.assertEqual((resources / "source-revision.txt").read_text().strip(), REV)
            info = plistlib.loads(plist.read_bytes())
            self.assertEqual(info["FalconExperimentMode"], "native-host")
            self.assertGreater((resources / "Tester instructions.txt").stat().st_size, 500)

            self.assertGreater((resources / "Tester instructions zh-CN.txt").stat().st_size, 500)
            self.assertIn("Redistribution and use", (resources / "Chromium notice.txt").read_text(encoding="utf-8"))
            self.assertIn("Apache License", (resources / "winit LICENSE.txt").read_text(encoding="utf-8"))
            self.assertIn("modified copy of winit 0.30.13", (resources / "winit modifications.txt").read_text(encoding="utf-8"))

    def test_shell_mode_names_match_and_do_not_change_shipping_name(self):
        shell = pathlib.Path(__file__).with_name("mac-bundle.sh").read_text(encoding="utf-8")
        for mode, name in bundle.MODES.items():
            self.assertIn(f'{mode}) APP_NAME="{name}";', shell)
        self.assertIn('shipping) APP_NAME="Falcon";', shell)
        self.assertIn('ARCHIVE="falcon-${VERSION}-macos-arm64.tgz"', shell)

    def test_empty_argument_array_is_safe_for_macos_bash_32(self):
        # Static compatibility gate: reverting to the unguarded expansion fails this check.
        # Native bash 3.2 execution is still a Mac check; modern bash -n cannot detect this bug.
        shell = pathlib.Path(__file__).with_name('mac-bundle.sh').read_text(encoding='utf-8')
        line = next(line for line in shell.splitlines() if line.startswith('SOURCE_REVISION='))
        self.assertIn('${SOURCE_ARGS[@]+"${SOURCE_ARGS[@]}"}', line)

    def test_shipping_keeps_normal_identity_handlers_and_bundles_notices(self):
        # Falsifier: route shipping through diagnostic_plist, or skip resource setup.
        with tempfile.TemporaryDirectory() as d:
            app = pathlib.Path(d) / "Falcon.app"
            executable = app / "Contents/MacOS/falcon"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"FALCON_MAC_NATIVE_TOOLBAR_01")
            original = {"CFBundleIdentifier": "com.hwu0101.falcon", "CFBundleVersion": "1.0.8",
                        "CFBundleDocumentTypes": ["images"], "UTImportedTypeDeclarations": ["raw"],
                        "FalconSourceRevision": REV, "FalconSourceVersion": "1.0.8"}
            plist = app / "Contents/Info.plist"
            plist.write_bytes(plistlib.dumps(original))
            bundle.configure(app, "shipping", REV)
            self.assertEqual(plistlib.loads(plist.read_bytes()), original)
            resources = app / "Contents/Resources"
            self.assertFalse((resources / "experiment-mode.txt").exists())
            # Falsifier: leave internal ownership markers inside the app before signing.
            self.assertFalse(list(resources.rglob('.falcon-generated-materials')))
            self.assertEqual((resources / "source-revision.txt").read_text().strip(), REV)
            self.assertTrue((resources / "winit LICENSE.txt").is_file())
            self.assertTrue((resources / "winit modifications.txt").is_file())
            self.assertGreater((resources / "Tester instructions.txt").stat().st_size, 500)

            # Falsifier: skip release_materials.copy in the shipping branch.
            import release_materials
            root = pathlib.Path(__file__).resolve().parents[1]
            for name in release_materials.FILES:
                self.assertEqual((resources/name).read_bytes(), (root/name).read_bytes())
                self.assertEqual((app.parent/name).read_bytes(), (root/name).read_bytes())
            self.assertEqual((resources/'BUILDING.md').read_bytes(), release_materials.build_guide(root).read_bytes())
            for path in (root/'docs/licenses').rglob('*'):
                if path.is_file():
                    self.assertEqual((resources/path.relative_to(root)).read_bytes(), path.read_bytes())

    def test_normal_launch_requires_normal_build_identity(self):
        smoke_spec = importlib.util.spec_from_file_location("smoke", pathlib.Path(__file__).with_name("test-mac-chrome-smoke.py"))
        smoke = importlib.util.module_from_spec(smoke_spec)
        smoke_spec.loader.exec_module(smoke)
        report = {"build": "1.0.8", "ready": True, "welcome": False, "association": False,
                  "photo_width": 960, "full_toolbar": True, "toolbar_roundtrip": True,
                  "toolbar_clipped": False, "toolbar_mouse_down_can_move_window": False, "titlebar_height": 38}
        smoke.validate_report(report, "1.0.8")
        with self.assertRaises(RuntimeError):
            smoke.validate_report(report, "1.0.8-mac-full04")
        with self.assertRaises(RuntimeError):
            smoke.validate_report(dict(report, toolbar_clipped=True), "1.0.8")


if __name__ == "__main__":
    unittest.main()
