"""Verify release contents and fail-closed packaging without compiling or running Falcon."""

import hashlib
from contextlib import nullcontext
import importlib.util
import json
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location(
    'windows_package', Path(__file__).with_name('package-windows.py')
)
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)


pe_spec = importlib.util.spec_from_file_location(
    'pe_fixtures', Path(__file__).with_name('test-windows-release.py')
)
pe_fixtures = importlib.util.module_from_spec(pe_spec)
pe_spec.loader.exec_module(pe_fixtures)


class WindowsPackage(unittest.TestCase):
    def test_package_contains_built_bytes_revision_and_readable_guides(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'synthetic.exe'
            binary.write_bytes(b'synthetic package fixture, not a real app')
            receipt = {
                'source_revision': 'a' * 40,
                'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
            }
            # The public entry point must obtain its input from build(), never a CLI binary.
            with patch.object(
                package, 'isolated_source', return_value=nullcontext((package.ROOT, 'a' * 40, {}))
            ), patch.object(package, 'clean_revision', return_value='a' * 40), patch.object(
                package, 'build', return_value=(binary, receipt)
            ) as build, patch.object(
                package, 'commit_stamp', return_value=(2026, 10, 29, 3, 4, 6)
            ) as stamp:
                output = package.package(root / 'out')
                build.assert_called_once_with(package.ROOT, 'a' * 40, {})
                stamp.assert_called_once_with(package.ROOT, 'a' * 40)
            prefix = 'Falcon Photo Viewer/'
            with zipfile.ZipFile(output) as archive:
                # Falsifier: stamp 1 January 1980 again. Every file carries the source commit's time
                # (that fixed date showed as 1899 in time zones ahead of UTC).
                self.assertEqual({i.date_time for i in archive.infolist()}, {(2026, 10, 29, 3, 4, 6)})
                # Falsifier: include the staging-only ownership marker in the download.
                self.assertFalse(
                    any(n.endswith('/.falcon-generated-materials') for n in archive.namelist())
                )
                self.assertEqual(archive.read(prefix + 'Falcon.exe'), binary.read_bytes())
                self.assertEqual(archive.read(prefix + 'source-revision.txt'), b'a' * 40 + b'\n')
                self.assertEqual(
                    json.loads(archive.read(prefix + 'build-receipt.json'))['binary_sha256'],
                    receipt['binary_sha256'],
                )
                for name in (
                    'LICENSE',
                    'NOTICE',
                    'BUILDING.md',
                    'REBUILDING.md',
                    'THIRD-PARTY-NOTICES.txt',
                    'docs/licenses/rust-standard-library.html',
                ):
                    self.assertGreater(len(archive.read(prefix + name)), 100)
                # Falsifier (issue #11 batch, D1): the Read me still sends a first setup to Update,
                # or doesn't say how a copy in a new folder takes over photo opening.
                readme = archive.read(prefix + 'Read me.txt').decode('utf-8')
                self.assertIn('Add to Open With', readme)
                self.assertIn('choose Update when it asks', readme)
            checksum = output.with_suffix(output.suffix + '.sha256').read_text().split()[0]
            self.assertEqual(checksum, hashlib.sha256(output.read_bytes()).hexdigest())
            with patch.object(package, 'build') as build:
                with self.assertRaisesRegex(ValueError, 'overwrite'):
                    package.package(root / 'out')
                build.assert_not_called()

    def test_source_change_during_build_refuses_package(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / 'fixture.exe'
            binary.write_bytes(b'changed source fixture')
            receipt = {
                'source_revision': 'a' * 40,
                'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
            }
            with patch.object(
                package, 'isolated_source', return_value=nullcontext((package.ROOT, 'a' * 40, {}))
            ), patch.object(package, 'clean_revision', return_value='b' * 40), patch.object(
                package, 'build', return_value=(binary, receipt)
            ):
                with self.assertRaisesRegex(ValueError, 'Source changed'):
                    package.package(Path(directory) / 'out')
            self.assertFalse((Path(directory) / 'out').exists())

    def test_build_pins_locked_target_and_checks_the_actual_output(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / 'falcon.exe'
            helper = Path(directory) / 'out/falcon-shell-icons.dll'
            helper.parent.mkdir()
            helper.write_bytes(pe_fixtures.WindowsRelease().pe())
            binary.write_bytes(b'synthetic build output' + helper.read_bytes())

            class Process:
                def __init__(self):
                    self.stdout = io.StringIO(
                        json.dumps(
                            {
                                'reason': 'build-script-executed',
                                'package_id': 'native',
                                'out_dir': str(helper.parent),
                            }
                        )
                        + '\n'
                        + json.dumps(
                            {
                                'reason': 'compiler-artifact',
                                'package_id': 'native',
                                'target': {'name': 'falcon'},
                                'executable': str(binary),
                            }
                        )
                        + '\n'
                    )

                def wait(self):
                    return 0

                def poll(self):
                    return 0

            env = {'CARGO_TARGET_DIR': directory}
            with patch.object(package, 'validate'), patch.object(
                package.sys, 'platform', 'win32'
            ), patch.object(
                package.subprocess,
                'check_output',
                side_effect=['release: 1.96.0\n', 'cargo 1.96.0 (fixture)'],
            ), patch.object(
                package.subprocess, 'Popen', return_value=Process()
            ) as launch, patch.object(
                package, 'verify_executable', return_value=['kernel32.dll']
            ) as verify:
                output, receipt = package.build(package.ROOT, 'a' * 40, env)
                self.assertIn('--locked', launch.call_args.args[0])
                self.assertIn('x86_64-pc-windows-msvc', launch.call_args.args[0])
                self.assertEqual(launch.call_args.kwargs['env'], env)
                verify.assert_called_once_with(binary, package.ROOT)
                self.assertEqual(output, binary)
                self.assertEqual(receipt['cargo'], 'cargo 1.96.0 (fixture)')
                self.assertEqual(receipt['icon_helper']['imports'], ['kernel32.dll'])
                self.assertEqual(receipt['rustc'], 'release: 1.96.0')
            with patch.object(package, 'validate'), patch.object(
                package.sys, 'platform', 'win32'
            ), patch.object(
                package.subprocess,
                'check_output',
                side_effect=['release: 1.96.0\n', 'cargo 1.96.0 (fixture)'],
            ), patch.object(
                package.subprocess, 'Popen', return_value=Process()
            ), patch.object(
                package, 'verify_executable', side_effect=ValueError('bad icon')
            ):
                with self.assertRaisesRegex(ValueError, 'bad icon'):
                    package.build(package.ROOT, 'a' * 40, env)

    def test_build_refuses_wrong_toolchain_and_output_outside_fresh_target(self):
        # Falsifiers: remove either explicit toolchain check or fresh-target containment check.
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / 'outside.exe'
            binary.write_bytes(b'synthetic')

            class Process:
                def __init__(self):
                    self.stdout = io.StringIO(
                        json.dumps(
                            {
                                'reason': 'compiler-artifact',
                                'target': {'name': 'falcon'},
                                'executable': str(binary),
                            }
                        )
                        + '\n'
                    )

                def wait(self):
                    return 0

                def poll(self):
                    return 0

            with patch.object(package, 'validate'), patch.object(
                package.sys, 'platform', 'win32'
            ), patch.object(
                package.subprocess,
                'check_output',
                side_effect=['release: 1.95.0\n', 'cargo 1.96.0 (fixture)'],
            ), patch.object(
                package.subprocess, 'Popen'
            ) as launch:
                with self.assertRaisesRegex(ValueError, 'Unexpected release toolchain'):
                    package.build(package.ROOT, 'a' * 40, {'CARGO_TARGET_DIR': directory})
                launch.assert_not_called()
            with patch.object(package, 'validate'), patch.object(
                package.sys, 'platform', 'win32'
            ), patch.object(
                package.subprocess,
                'check_output',
                side_effect=['release: 1.96.0\n', 'cargo 1.96.0 (fixture)'],
            ), patch.object(
                package.subprocess, 'Popen', return_value=Process()
            ), patch.object(
                package, 'verify_executable'
            ) as verify:
                with self.assertRaisesRegex(ValueError, 'escaped the fresh target'):
                    package.build(
                        package.ROOT,
                        'a' * 40,
                        {'CARGO_TARGET_DIR': str(Path(directory) / 'target')},
                    )
                verify.assert_not_called()

    # Falsifiers: ignore helper imports, choose the wrong build-script directory, or skip embedding.
    def test_embedded_helper_is_the_native_build_output_and_has_no_crt(self):
        for case in ['good', 'crt', 'delay-crt', 'not-embedded', 'wrong-package', 'outside']:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                target = Path(directory) / 'target'
                target.mkdir()
                out = target / 'native'
                out.mkdir()
                helper = out / 'falcon-shell-icons.dll'
                helper.write_bytes(
                    pe_fixtures.WindowsRelease().pe(
                        b'VCRUNTIME140.dll' if 'crt' in case else b'kernel32.dll',
                        case == 'delay-crt',
                    )
                )
                if case == 'outside':
                    (Path(directory) / helper.name).write_bytes(helper.read_bytes())
                binary = target / 'falcon.exe'
                binary.write_bytes(
                    b'app' + (b'' if case == 'not-embedded' else helper.read_bytes())
                )
                record = {
                    'reason': 'build-script-executed',
                    'package_id': 'other' if case == 'wrong-package' else 'native',
                    'out_dir': str(out if case != 'outside' else Path(directory)),
                }
                artifact = {
                    'reason': 'compiler-artifact',
                    'package_id': 'native',
                    'target': {'name': 'falcon'},
                    'executable': str(binary),
                }

                class Process:
                    def __init__(self):
                        self.stdout = io.StringIO(
                            json.dumps(record) + '\n' + json.dumps(artifact) + '\n'
                        )

                    def wait(self):
                        return 0

                    def poll(self):
                        return 0

                with patch.object(package, 'validate'), patch.object(
                    package.sys, 'platform', 'win32'
                ), patch.object(
                    package.subprocess,
                    'check_output',
                    side_effect=['release: 1.96.0\n', 'cargo 1.96.0 (fixture)'],
                ), patch.object(
                    package.subprocess, 'Popen', return_value=Process()
                ), patch.object(
                    package, 'verify_executable', return_value=[]
                ):
                    if case == 'good':
                        _, receipt = package.build(
                            package.ROOT, 'a' * 40, {'CARGO_TARGET_DIR': str(target)}
                        )
                        self.assertEqual(
                            receipt['icon_helper']['sha256'],
                            hashlib.sha256(helper.read_bytes()).hexdigest(),
                        )
                    else:
                        message = 'Icon helper escaped' if case == 'outside' else ''
                        with self.assertRaisesRegex(ValueError, message):
                            package.build(package.ROOT, 'a' * 40, {'CARGO_TARGET_DIR': str(target)})

    # Falsifier: retain only the Rust version half of the toolchain gate.
    def test_wrong_cargo_version_never_starts_a_build(self):
        with patch.object(package, 'validate'), patch.object(
            package.sys, 'platform', 'win32'
        ), patch.object(
            package.subprocess,
            'check_output',
            side_effect=['release: 1.96.0\n', 'cargo 1.95.0 (fixture)'],
        ), patch.object(
            package.subprocess, 'Popen'
        ) as launch:
            with self.assertRaisesRegex(ValueError, 'Unexpected release toolchain'):
                package.build(package.ROOT, 'a' * 40, {})
            launch.assert_not_called()

    # Falsifier: remove the staged executable hash comparison.
    def test_changed_executable_is_not_packaged(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / 'synthetic.exe'
            binary.write_bytes(b'changed after build')
            with self.assertRaisesRegex(ValueError, 'changed after the build'):
                package.package_built(
                    binary,
                    Path(directory) / 'out',
                    {'source_revision': 'a' * 40, 'binary_sha256': '0' * 64},
                )
            self.assertEqual(list((Path(directory) / 'out').glob('*.zip')), [])


if __name__ == '__main__':
    unittest.main()
