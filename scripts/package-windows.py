"""Build the current clean source and package its Windows executable and notices.

Run only for a reviewed release. There is deliberately no arbitrary-binary input.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import zipfile
from release_materials import copy as copy_materials, validate, text_hash, MATERIAL_MARKER
from windows_release import TARGET, TOOLCHAIN, isolated_source, clean_revision, verify_runtime

ROOT = Path(__file__).resolve().parents[1]


def verify_executable(binary, root):
    imports = verify_runtime(binary)
    spec = importlib.util.spec_from_file_location('release_icons', root/'scripts/check-windows-icon-resources.py')
    checker = importlib.util.module_from_spec(spec); spec.loader.exec_module(checker)
    checker.verify(binary, root/'falcon/native/assets/icons')
    return imports


def build(root, revision, env):
    if sys.platform != 'win32':
        raise ValueError('Build the Windows release on Windows')
    validate(root)
    rustc = subprocess.check_output(['rustc', '-vV'], cwd=root/'falcon', env=env, text=True)
    cargo = subprocess.check_output(['cargo', '-V'], cwd=root/'falcon', env=env, text=True).strip()
    if 'release: '+TOOLCHAIN+'\n' not in rustc or not cargo.startswith('cargo '+TOOLCHAIN+' '):
        raise ValueError('Unexpected release toolchain')
    command = ['cargo', 'build', '--locked', '--release', '--bin', 'falcon', '--target', TARGET,
               '--message-format=json-render-diagnostics']
    binary = None
    native_package = None
    build_outputs = {}
    process = subprocess.Popen(command, cwd=root/'falcon', env=env, stdout=subprocess.PIPE,
                               text=True, encoding='utf-8')
    try:
        for line in process.stdout:
            record = json.loads(line)
            if record.get('reason') == 'build-script-executed':
                build_outputs[record.get('package_id')] = record.get('out_dir')
            if record.get('reason') == 'compiler-message':
                print(record['message'].get('rendered', ''), end='', file=sys.stderr)
            if record.get('reason') == 'compiler-artifact' and record.get('target', {}).get('name') == 'falcon' and record.get('executable'):
                binary = Path(record['executable'])
                native_package = record.get('package_id')
        if process.wait() != 0 or binary is None or not binary.is_file():
            raise RuntimeError('Cargo did not produce the Windows executable')
    finally:
        process.stdout.close()
        if process.poll() is None: process.terminate(); process.wait()
    target = Path(env['CARGO_TARGET_DIR']).resolve()
    if not binary.resolve().is_relative_to(target): raise ValueError('Cargo output escaped the fresh target')
    imports = verify_executable(binary, root)
    out_dir = build_outputs.get(native_package)
    if not native_package or not out_dir:raise ValueError('Missing production icon helper build output')
    helper = Path(out_dir)/'falcon-shell-icons.dll'
    if not helper.resolve().is_relative_to(target):raise ValueError('Icon helper escaped the fresh target')
    if not helper.is_file():raise ValueError('Missing production icon helper')
    helper_imports = verify_runtime(helper)
    helper_bytes = helper.read_bytes()
    if not helper_bytes or helper_bytes not in binary.read_bytes():raise ValueError('Built icon helper is not embedded in the executable')
    receipt = {'source_revision': revision, 'rustc': rustc.strip(), 'cargo': cargo,
               'icon_helper': {'sha256': hashlib.sha256(helper_bytes).hexdigest(), 'imports': helper_imports},
               'target': TARGET, 'rustflags': ['-C', 'target-feature=+crt-static'], 'imports': imports,
               'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
               'lock_sha256': text_hash(root/'falcon/Cargo.lock'),
               'notices_sha256': text_hash(root/'THIRD-PARTY-NOTICES.txt'), 'command': command}
    return binary, receipt


def commit_stamp(root, revision):
    """The source commit's time (UTC) as a zip entry date. Every file in the package carries it, so the
    same source always makes the same zip. A fixed 1 January 1980 00:00 read as a time before 1980 in
    time zones ahead of UTC, and Windows showed the files as dated 1899, or not at all."""
    seconds = subprocess.check_output(['git', 'log', '-1', '--format=%ct', revision], cwd=root, text=True)
    return time.gmtime(int(seconds.strip()))[:6]


def package_built(binary, destination, receipt, root=None, stamp=None):
    """Internal staging step, called only with this invocation's successful build."""
    root = root or ROOT
    version = tomllib.loads((root/'falcon/native/Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    output = Path(destination)/f'falcon-{version}-windows-x64.zip'
    if output.exists():
        raise ValueError('Refusing to overwrite a package')
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='falcon-windows-package-') as scratch:
        stage = Path(scratch)/'Falcon Photo Viewer'
        copy_materials(root, stage)
        shutil.copyfile(binary, stage/'Falcon.exe')
        if hashlib.sha256((stage/'Falcon.exe').read_bytes()).hexdigest() != receipt['binary_sha256']:
            raise ValueError('Executable changed after the build')
        (stage/'source-revision.txt').write_text(receipt['source_revision']+'\n', encoding='utf-8', newline='\n')
        (stage/'build-receipt.json').write_text(json.dumps({**receipt, 'version': version}, indent=2)+'\n', encoding='utf-8', newline='\n')
        (stage/'Read me.txt').write_text(
            f'Falcon Photo Viewer {version}\n\n'
            'Extract this whole folder, then run Falcon.exe. No installer is needed.\n'
            'This free build is unsigned. If Windows SmartScreen blocks the trusted download,\n'
            'use More info > Run anyway when offered. Keep system-wide security enabled.\n'
            'For file icons/opening, choose Settings > File associations > Add to Open With after\n'
            'placing the app in its intended folder. To update, close Falcon and extract the new\n'
            'zip into the folder that contains this one, replacing its files. If you keep it\n'
            'elsewhere, open the new copy and choose Update when it asks.\n'
            'Keep the accompanying licence/notices files.\n\n'
            'Source, issues and updates: https://github.com/HWu0101/falcon-photo-viewer\n'
            'Source rebuilding and the RAW library: REBUILDING.md\n', encoding='utf-8', newline='\n')
        if stamp is None:
            raise ValueError('The package needs its source commit time')
        with zipfile.ZipFile(output, 'x', zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for path in sorted(stage.rglob('*')):
                if path.is_file() and path.relative_to(stage).as_posix() != 'docs/licenses/'+MATERIAL_MARKER:
                    info = zipfile.ZipInfo(path.relative_to(stage.parent).as_posix(), stamp)
                    info.compress_type = zipfile.ZIP_DEFLATED; info.external_attr = 0o100644 << 16
                    archive.writestr(info, path.read_bytes(), compresslevel=9)
    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    output.with_suffix(output.suffix+'.sha256').write_text(f'{digest}  {output.name}\n', encoding='utf-8', newline='\n')
    print(output)
    return output


def package(destination):
    version = tomllib.loads((ROOT/'falcon/native/Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    if (Path(destination)/f'falcon-{version}-windows-x64.zip').exists():
        raise ValueError('Refusing to overwrite a package')
    with isolated_source(ROOT) as (source, revision, env):
        binary, receipt = build(source, revision, env)
        if clean_revision(ROOT) != revision:
            raise ValueError('Source changed during the release build')
        return package_built(binary, destination, receipt, source, commit_stamp(ROOT, revision))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('destination', type=Path)
    args = parser.parse_args()
    package(args.destination)
