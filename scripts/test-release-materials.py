"""Release packaging refuses stale/missing notice material; no app execution."""

from pathlib import Path
import json
import shutil
import tempfile
import os
import subprocess
import unittest
from unittest.mock import patch
import release_materials as materials

ROOT = Path(__file__).resolve().parents[1]


class ReleaseMaterials(unittest.TestCase):
    @unittest.skipUnless(os.name == 'nt', 'Windows junction semantics')
    def test_real_junctions_are_refused_before_any_overwrite(self):
        # Falsifier: remove reparse-point detection or skip preflight for a broken junction.
        for case in ('tree', 'child', 'broken-tree'):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                base = Path(directory).resolve()
                target = base / 'package'
                materials.copy(ROOT, target)
                tree = target / 'docs/licenses'
                linked_target = target / 'junction-target'
                linked_target.mkdir()
                sentinel = linked_target / 'keep.txt'
                sentinel.write_text('owner file')
                link = tree / 'child' if case == 'child' else tree
                if link == tree:
                    shutil.rmtree(tree)
                subprocess.run(
                    [
                        'powershell',
                        '-NoProfile',
                        '-Command',
                        'New-Item -ItemType Junction -Path $env:FALCON_TEST_LINK -Target $env:FALCON_TEST_TARGET | Out-Null',
                    ],
                    env={
                        **os.environ,
                        'FALCON_TEST_LINK': str(link),
                        'FALCON_TEST_TARGET': str(linked_target),
                    },
                    check=True,
                    capture_output=True,
                )
                try:
                    self.assertTrue(materials.linked(link))
                    if case == 'broken-tree':
                        sentinel.unlink()
                        linked_target.rmdir()
                    (target / 'LICENSE').write_text('owner licence')
                    with self.assertRaises(ValueError):
                        materials.copy(ROOT, target)
                    self.assertEqual((target / 'LICENSE').read_text(), 'owner licence')
                    if case != 'broken-tree':
                        self.assertEqual(sentinel.read_text(), 'owner file')
                finally:
                    # Only unlink the verified junction within this test's temporary root.
                    if os.path.lexists(link):
                        self.assertTrue(link.absolute().is_relative_to(base))
                        os.rmdir(link)

    def test_interrupted_owned_copy_can_be_retried(self):
        # Falsifier: write the ownership marker only after all resources finish copying.
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'package'
            original_copy = materials.shutil.copyfile

            def interrupt(source, destination):
                if Path(source).name == 'notice-sources.json':
                    raise OSError('interrupted')
                return original_copy(source, destination)

            with patch.object(materials.shutil, 'copyfile', side_effect=interrupt):
                with self.assertRaisesRegex(OSError, 'interrupted'):
                    materials.copy(ROOT, target)
            materials.copy(ROOT, target)
            self.assertEqual((target / 'LICENSE').read_bytes(), (ROOT / 'LICENSE').read_bytes())

    def fixture(self, root):
        names = list(materials.FILES) + [
            'falcon/Cargo.lock',
            'falcon/Cargo.toml',
            'falcon/about.toml',
        ]
        # Every vendored and workspace manifest is a release input; a fixed vendor list went
        # stale when the femtovg copy was added.
        names += [
            str(p.relative_to(ROOT)).replace('\\', '/')
            for folder in ('falcon/vendor', 'falcon/crates')
            for p in (ROOT / folder).glob('*/Cargo.toml')
        ]
        names += ['falcon/native/Cargo.toml']
        names += ['falcon/native/src/main.rs']
        names += list(materials.SUPPLEMENTS) + list(materials.FONTS)
        names += [str(materials.build_guide(ROOT).relative_to(ROOT))]
        for name in names:
            destination = root / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, destination)
        shutil.copytree(ROOT / 'docs/licenses', root / 'docs/licenses')

    def edit_inventory(self, root, edit):
        path = root / 'docs/licenses/dependency-inventory.json'
        data = json.loads(path.read_text())
        edit(data)
        path.write_text(json.dumps(data), encoding='utf-8')

    # Each case fails if its corresponding release_materials guard is removed.
    def test_lockfile_change_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            with (root / 'falcon/Cargo.lock').open('a') as f:
                f.write('\n[[package]]\nname = "unexpected-library"\nversion = "1.0.0"\n')
            with self.assertRaisesRegex(ValueError, 'Cargo.lock'):
                materials.validate(root)

    def test_notices_change_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            with (root / 'THIRD-PARTY-NOTICES.txt').open('a') as f:
                f.write('\nChanged notice\n')
            with self.assertRaisesRegex(ValueError, 'notices changed'):
                materials.validate(root)

    def test_font_change_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            path = root / materials.FONTS[0]
            data = bytearray(path.read_bytes())
            data[-1] ^= 1
            path.write_bytes(data)
            with self.assertRaisesRegex(ValueError, 'embedded font'):
                materials.validate(root)

    def test_changed_targets_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            self.edit_inventory(root, lambda data: data.update(targets=['x86_64-pc-windows-msvc']))
            with self.assertRaisesRegex(ValueError, 'targets changed'):
                materials.validate(root)

    def test_required_notices_are_present_even_with_recomputed_hash(self):
        for phrase in [
            'Independent JPEG Group',
            'Slint Royalty-Free',
            'GNU LESSER GENERAL PUBLIC LICENSE',
        ]:
            with self.subTest(phrase=phrase), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.fixture(root)
                path = root / 'THIRD-PARTY-NOTICES.txt'
                path.write_text(
                    path.read_text(encoding='utf-8').replace(phrase, ''), encoding='utf-8'
                )
                self.edit_inventory(
                    root, lambda data: data.update(notices_sha256=materials.text_hash(path))
                )
                with self.assertRaisesRegex(ValueError, 'Missing notice'):
                    materials.validate(root)

    def test_package_licence_coverage_is_required(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            self.edit_inventory(root, lambda data: data['packages'][0].update(selected=[]))
            with self.assertRaisesRegex(ValueError, 'package coverage'):
                materials.validate(root)

    # Falsifiers: omit upstream file hashes or the actual embedded-font declaration scan.
    def test_changed_upstream_notice_and_new_font_declaration_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            manifest = json.loads(
                (root / 'docs/licenses/notice-sources.json').read_text(encoding='utf-8')
            )
            item = next(e for p in manifest['packages'].values() for e in p['files'])
            path = root / item['file']
            original = path.read_bytes()
            path.write_bytes(original + b'changed')
            with self.assertRaisesRegex(ValueError, 'Upstream notice changed'):
                materials.validate(root)
            path.write_bytes(original)
            with (root / 'falcon/native/src/main.rs').open('a', encoding='utf-8') as f:
                f.write(
                    '\nconst EXTRA: &[u8] = include_bytes!("../assets/fonts/Unreviewed.ttf");\n'
                )
            with self.assertRaisesRegex(ValueError, 'font declarations changed'):
                materials.validate(root)

    def test_recopied_materials_do_not_keep_stale_licence_files(self):
        # Falsifier: merge licences with dirs_exist_ok=True without removing stale output.
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'package'
            materials.copy(ROOT, target)
            stale = target / 'docs/licenses/obsolete.txt'
            stale.write_text('old package licence')
            materials.copy(ROOT, target)
            self.assertFalse(stale.exists())

    # Falsifier: replace an unowned licence tree, or overwrite files before ownership preflight.
    def test_unowned_material_tree_is_preserved(self):
        for marker in [None, 'wrong marker']:
            with self.subTest(marker=marker), tempfile.TemporaryDirectory() as directory:
                target = Path(directory) / 'borrowed'
                licences = target / 'docs/licenses'
                licences.mkdir(parents=True)
                sentinel = licences / 'keep.txt'
                sentinel.write_text('owner file')
                (target / 'LICENSE').write_text('owner licence')
                if marker:
                    (licences / '.falcon-generated-materials').write_text(marker)
                with self.assertRaisesRegex(ValueError, 'not owned'):
                    materials.copy(ROOT, target)
                self.assertEqual(sentinel.read_text(), 'owner file')
                self.assertEqual((target / 'LICENSE').read_text(), 'owner licence')

    # Falsifier: remove any link refusal before recursive replacement.
    def test_recognised_links_are_refused_before_cleanup(self):
        for location in ['tree', 'marker', 'child']:
            with self.subTest(location=location), tempfile.TemporaryDirectory() as directory:
                target = Path(directory) / 'package'
                materials.copy(ROOT, target)
                tree = target / 'docs/licenses'
                marker = tree / materials.MATERIAL_MARKER
                child = tree / 'keep'
                child.write_text('preserved')
                link = {'tree': tree, 'marker': marker, 'child': child}[location]
                # The OS predicate is the boundary; exercise the real copy/preflight ordering.
                with patch.object(materials, 'linked', side_effect=lambda p: p == link):
                    with self.assertRaises(ValueError):
                        materials.copy(ROOT, target)
                self.assertEqual(child.read_text(), 'preserved')

    # Falsifier: check only lock/notices hashes and ignore changed feature/manifests.
    def test_manifest_change_without_lock_change_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            materials.validate(root)
            with (root / 'falcon/native/Cargo.toml').open('a', encoding='utf-8') as file:
                file.write('\n# changed project configuration\n')
            with self.assertRaisesRegex(ValueError, 'project configuration'):
                materials.validate(root)

    # Falsifier: test only is_file on the runtime licence document.
    def test_empty_runtime_notices_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            (root / 'docs/licenses/rust-standard-library.html').write_bytes(b'')
            with self.assertRaisesRegex(ValueError, 'runtime notices'):
                materials.validate(root)

    # Falsifier: omit the supplement hashes from the validator.
    def test_changed_supplement_requires_notice_regeneration(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fixture(root)
            with (root / materials.SUPPLEMENTS[0]).open('a', encoding='utf-8') as file:
                file.write('\nNew upstream attribution\n')
            with self.assertRaisesRegex(ValueError, 'supplementary notice'):
                materials.validate(root)

    def test_copied_bundle_contains_complete_materials(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / 'package'
            materials.copy(ROOT, destination)
            for name in materials.FILES:
                self.assertEqual((ROOT / name).read_bytes(), (destination / name).read_bytes())
            self.assertEqual(
                materials.build_guide(ROOT).read_bytes(), (destination / 'BUILDING.md').read_bytes()
            )
            self.assertGreater(
                (destination / 'docs/licenses/rust-standard-library.html').stat().st_size, 1000
            )

    def test_build_docs_name_every_vendored_patch(self):
        # Falsifier: a [patch.crates-io] entry the build requires but a shipped guide or the
        # README leaves out (femtovg was missing until the issue #11 batch, D2).
        import tomllib
        patches = tomllib.loads((ROOT / 'falcon/Cargo.toml').read_text(encoding='utf-8'))['patch']['crates-io']
        # The private checkout keeps the public README beside the export templates.
        readme = ROOT / 'docs/distribution/public-readme.md'
        readme = readme if readme.is_file() else ROOT / 'README.md'
        for guide in (materials.build_guide(ROOT), ROOT / 'REBUILDING.md', readme):
            text = guide.read_text(encoding='utf-8').lower()
            for name in patches:
                self.assertIn(name.lower(), text, f'{guide.name} names the patched {name}')

    def test_shipped_guides_have_no_prerelease_wording(self):
        # Falsifier: a shipped build guide still calls the release a candidate or unfinished.
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / 'package'
            materials.copy(ROOT, destination)
            for name in ('BUILDING.md', 'REBUILDING.md'):
                text = (destination / name).read_text(encoding='utf-8').lower()
                for phrase in ('source candidate', 'staging candidate', 'still in progress', 'pre-release'):
                    self.assertNotIn(phrase, text, f'{name}: {phrase}')


if __name__ == '__main__':
    unittest.main()
