"""Isolated release inputs and read-only checks of the built Windows PE."""
from contextlib import contextmanager
import os
from pathlib import Path, PurePosixPath
import struct
import subprocess
import tarfile
import tempfile

TARGET = 'x86_64-pc-windows-msvc'
TOOLCHAIN = '1.96.0'
PORTABLE_CONFIG = '[target.x86_64-pc-windows-msvc]\nrustflags = ["-C", "target-feature=+crt-static"]\n'
# Discovery/runtime variables only. Compiler options and arbitrary crate build switches are not inherited.
BUILD_ENVIRONMENT = frozenset('''PATH PATHEXT SYSTEMROOT WINDIR SYSTEMDRIVE COMSPEC TEMP TMP
USERPROFILE HOMEDRIVE HOMEPATH LOCALAPPDATA APPDATA PROGRAMFILES PROGRAMFILES(X86) PROGRAMW6432
PROCESSOR_ARCHITECTURE PROCESSOR_ARCHITEW6432 NUMBER_OF_PROCESSORS
INCLUDE LIB LIBPATH VSINSTALLDIR VCINSTALLDIR VCTOOLSINSTALLDIR VCTOOLSVERSION
WINDOWSSDKDIR WINDOWSSDKVERSION WINDOWSSDKLIBVERSION UNIVERSALCRTSDKDIR UCRTVERSION
VSCMD_ARG_HOST_ARCH VSCMD_ARG_TGT_ARCH VSCMD_VER RUSTUP_HOME
HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY SSL_CERT_FILE SSL_CERT_DIR'''.split())
SYSTEM_IMPORTS = frozenset('''advapi32.dll api-ms-win-core-synch-l1-2-0.dll
api-ms-win-core-winrt-error-l1-1-0.dll bcryptprimitives.dll combase.dll comctl32.dll d3d11.dll
dwmapi.dll dwrite.dll dxgi.dll gdi32.dll imm32.dll kernel32.dll mscms.dll ntdll.dll ole32.dll
oleaut32.dll opengl32.dll pdh.dll propsys.dll setupapi.dll shell32.dll shlwapi.dll user32.dll uxtheme.dll'''.split())


def clean_revision(root):
    root = Path(root).resolve()
    try:
        top = subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], cwd=root, stderr=subprocess.DEVNULL).decode().strip()
        if Path(top).resolve() != root or not (root/'.git').exists():
            raise ValueError('Release packaging requires this exact Git checkout')
        revision = subprocess.check_output(['git', 'rev-parse', '--verify', 'HEAD'], cwd=root).decode().strip()
        dirty = subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=normal'], cwd=root)
    except (subprocess.CalledProcessError, FileNotFoundError) as error:
        raise ValueError('Release packaging requires a Git checkout with a commit') from error
    if dirty.strip():
        raise ValueError('Release packaging requires a clean Git checkout')
    return revision


def child_environment(scratch, inherited=None):
    env = {k:v for k,v in (os.environ if inherited is None else inherited).items() if k.upper() in BUILD_ENVIRONMENT}
    env.update(CARGO_HOME=str(scratch/'cargo-home'), CARGO_TARGET_DIR=str(scratch/'target'),
               RUSTUP_TOOLCHAIN=TOOLCHAIN, CARGO_ENCODED_RUSTFLAGS='-C\x1ftarget-feature=+crt-static')
    return env


@contextmanager
def isolated_source(root):
    root = Path(root).resolve()
    revision = clean_revision(root)
    base = Path(tempfile.gettempdir()).resolve()
    with tempfile.TemporaryDirectory(prefix='falcon-release-', dir=base) as directory:
        scratch = Path(directory).resolve()
        if not scratch.is_relative_to(base):
            raise ValueError('Release scratch directory escaped the temporary root')
        snapshot = scratch/'source'; snapshot.mkdir()
        # Cargo also searches ancestor .cargo directories; an empty CARGO_HOME alone is not enough.
        for parent in snapshot.parents:
            if any((parent/'.cargo'/name).exists() for name in ('config', 'config.toml')):
                raise ValueError('Unexpected ancestor Cargo configuration; use a clean temporary location')
        archive = scratch/'source.tar'
        subprocess.run(['git', 'archive', '--format=tar', '--output', str(archive), revision], cwd=root, check=True)
        with tarfile.open(archive) as stream:
            for member in stream:
                parts = PurePosixPath(member.name).parts
                if (not parts or member.name.startswith('/') or any(p in ('..', '.git') or ':' in p or '\\' in p for p in parts)
                    or not (member.isdir() or member.isfile())):
                    raise ValueError('Non-regular or unsafe source archive entry')
                path = snapshot.joinpath(*parts)
                if not path.resolve().is_relative_to(snapshot): raise ValueError('Source archive escaped snapshot')
                if member.isdir(): path.mkdir(parents=True, exist_ok=True); continue
                path.parent.mkdir(parents=True, exist_ok=True)
                with stream.extractfile(member) as source, path.open('xb') as out: out.write(source.read())
                path.chmod(member.mode)
        configs = [p for p in snapshot.rglob('*') if p.parent.name.casefold() == '.cargo' and p.name.casefold() in ('config', 'config.toml')]
        expected = snapshot/'falcon/.cargo/config.toml'
        if any(p != expected for p in configs): raise ValueError('Unreviewed Cargo configuration in release source')
        expected.parent.mkdir(parents=True, exist_ok=True)
        expected.write_text(PORTABLE_CONFIG, encoding='utf-8', newline='\n')
        (scratch/'cargo-home').mkdir()
        yield snapshot, revision, child_environment(scratch)


def pe_imports(data):
    def read(fmt, offset):
        size = struct.calcsize(fmt)
        if offset < 0 or offset+size > len(data): raise ValueError('Truncated PE')
        return struct.unpack_from(fmt, data, offset)
    if data[:2] != b'MZ': raise ValueError('Not a PE executable')
    pe, = read('<I', 0x3c)
    if data[pe:pe+4] != b'PE\0\0': raise ValueError('Invalid PE signature')
    machine, count = read('<HH', pe+4)
    optional_size, = read('<H', pe+20)
    opt = pe+24
    if machine != 0x8664 or read('<H', opt)[0] != 0x20b or not 1 <= count <= 128:
        raise ValueError('Expected Windows x64 PE32+ executable')
    if optional_size < 112 or opt+optional_size+40*count > len(data): raise ValueError('Invalid PE headers')
    sections = []
    for index in range(count):
        virtual_size, address, raw_size, raw = read('<IIII', opt+optional_size+40*index+8)
        if raw+raw_size > len(data): raise ValueError('Truncated PE section')
        sections.append((address, raw_size, raw))
    def offset(rva, length):
        for address, size, raw in sections:
            if address <= rva and rva+length <= address+size: return raw+rva-address
        raise ValueError('PE import outside a raw section')
    def name(rva):
        start = offset(rva, 1)
        chars = []
        for i in range(256):
            byte = data[offset(rva+i, 1)]
            if byte == 0: return bytes(chars).decode('ascii').lower()
            chars.append(byte)
        raise ValueError('Unterminated PE import name')
    names = []
    directories, = read('<I', opt+108)
    for index, width in ((1, 20), (13, 32)):
        if directories <= index: continue
        if 112+(index+1)*8 > optional_size: raise ValueError('Truncated PE directory table')
        rva, size = read('<II', opt+112+index*8)
        if rva == size == 0: continue
        if not rva or size < width: raise ValueError('Invalid PE import table')
        terminated = False
        for pos in range(0, size-width+1, width):
            record = read('<'+'I'*(width//4), offset(rva+pos, width))
            if not any(record): terminated = True; break
            if index == 13 and record[0] != 1: raise ValueError('Unsupported delay-import address mode')
            names.append(name(record[3] if index == 1 else record[1]))
        if not terminated: raise ValueError('Unterminated PE import table')
    return sorted(set(names))


def verify_runtime(binary):
    imports = pe_imports(Path(binary).read_bytes())
    unexpected = set(imports)-SYSTEM_IMPORTS
    if unexpected:
        raise ValueError('Unreviewed runtime imports (including possible Microsoft CRT): '+', '.join(sorted(unexpected)))
    return imports
