"""Exercise real Git snapshot isolation and synthetic PE imports without a release build."""
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import tarfile
import io
import tomllib
import unittest
from unittest.mock import patch
import windows_release as release


class WindowsRelease(unittest.TestCase):
    def repo(self, path):
        path.mkdir()
        def git(*args):
            return subprocess.check_output(['git','-c','user.name=Fixture','-c','user.email=test@example.invalid','-c','commit.gpgsign=false',*args],cwd=path,stderr=subprocess.DEVNULL)
        git('init','--quiet')
        (path/'input.txt').write_text('committed')
        (path/'.gitignore').write_text('ignored.txt\n')
        git('add','.'); git('commit','--quiet','-m','fixture')
        return git

    # Falsifier: build ROOT instead of the Git archive, or accept source archives.
    def test_archive_omits_ignored_and_hidden_local_edits(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)/'repo'; git=self.repo(root)
            git('update-index','--skip-worktree','input.txt')
            (root/'input.txt').write_text('hidden local edit')
            (root/'ignored.txt').write_text('must not build')
            with release.isolated_source(root) as (source,revision,env):
                self.assertEqual((source/'input.txt').read_text(),'committed')
                self.assertFalse((source/'ignored.txt').exists())
                self.assertEqual((source/'falcon/.cargo/config.toml').read_text(),release.PORTABLE_CONFIG)
                self.assertFalse(Path(env['CARGO_TARGET_DIR']).exists())
                self.assertEqual(list(Path(env['CARGO_HOME']).iterdir()),[])
                self.assertEqual(revision,git('rev-parse','HEAD').decode().strip())
            with self.assertRaises(ValueError): release.clean_revision(Path(directory))

    def test_dirty_checkout_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)/'repo'; self.repo(root)
            (root/'input.txt').write_text('edit')
            with self.assertRaisesRegex(ValueError,'clean Git'): release.clean_revision(root)

    def test_build_overrides_cannot_remove_crt_or_change_geometry(self):
        inherited={'PATH':'compiler','SystemRoot':'windows','INCLUDE':'sdk','RUSTFLAGS':'bad',
                   'CARGO_ENCODED_RUSTFLAGS':'bad','SLINT_SCALE_FACTOR':'2','FALCON_ALLOW_SLINT_SCALE':'1',
                   'CARGO_PROFILE_RELEASE_DEBUG':'true','CARGO_TARGET_DIR':'old','CARGO_HOME':'poison',
                   'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER':'bad','CXXFLAGS':'bad','CL':'bad',
                   **{key:'bad' for key in ['TARGET_CFLAGS','HOST_CFLAGS','CRATE_CC_NO_DEFAULTS','CXXSTDLIB','ARFLAGS','RC_PATH','RUSTC_BOOTSTRAP','UNKNOWN_FUTURE_FLAG']}}
        env=release.child_environment(Path('isolated'),inherited)
        self.assertEqual(env['PATH'],'compiler');self.assertEqual(env['INCLUDE'],'sdk')
        for key in ['RUSTFLAGS','SLINT_SCALE_FACTOR','FALCON_ALLOW_SLINT_SCALE','CARGO_PROFILE_RELEASE_DEBUG','CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER','CXXFLAGS','CL']:
            self.assertNotIn(key,env)
        self.assertEqual(env['CARGO_ENCODED_RUSTFLAGS'],'-C\x1ftarget-feature=+crt-static')
        self.assertEqual(env['RUSTUP_TOOLCHAIN'],'1.96.0')
        self.assertNotEqual(env['CARGO_TARGET_DIR'],'old')
        self.assertEqual(set(env),{'PATH','SystemRoot','INCLUDE','CARGO_HOME','CARGO_TARGET_DIR','RUSTUP_TOOLCHAIN','CARGO_ENCODED_RUSTFLAGS'})

    # Falsifiers: remove either Cargo-config refusal in isolated_source().
    def test_ancestor_and_nested_cargo_configs_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            parent=Path(directory);root=parent/'repo';git=self.repo(root)
            cfg=parent/'.cargo/config.toml';cfg.parent.mkdir();cfg.write_text('[env]\n')
            with patch.object(release.tempfile,'gettempdir',return_value=directory):
                with self.assertRaisesRegex(ValueError,'ancestor Cargo'):
                    with release.isolated_source(root):pass
            cfg.unlink()
            cfg=root/'falcon/native/.cargo/config.toml';cfg.parent.mkdir(parents=True);cfg.write_text('[env]\n')
            git('add','.');git('commit','--quiet','-m','nested config')
            with self.assertRaisesRegex(ValueError,'Unreviewed Cargo'):
                with release.isolated_source(root):pass

    # Falsifier: extract archive members without the regular-path checks.
    def test_unsafe_archive_entries_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)/'repo';self.repo(root)
            for name,symbolic in [('../escape.txt',False),('C:/escape.txt',False),('link',True)]:
                original_run=release.subprocess.run
                def archive(args,**kwargs):
                    if args[:2]!=['git','archive']:return original_run(args,**kwargs)
                    path=args[args.index('--output')+1]
                    with tarfile.open(path,'w') as output:
                        info=tarfile.TarInfo(name)
                        if symbolic:info.type=tarfile.SYMTYPE;info.linkname='../escape'
                        else:info.size=1
                        output.addfile(info,None if symbolic else io.BytesIO(b'x'))
                with patch.object(release.subprocess,'run',side_effect=archive),self.assertRaisesRegex(ValueError,'unsafe source archive'):
                    with release.isolated_source(root):pass

    def test_toolchain_constant_matches_source_pin(self):
        # Falsifier: update just one of the two version declarations.
        config=Path(__file__).resolve().parents[1]/'falcon/rust-toolchain.toml'
        self.assertEqual(release.TOOLCHAIN,tomllib.loads(config.read_text())['toolchain']['channel'])

    def pe(self, name=b'kernel32.dll', delay=False):
        data=bytearray(0x600);data[:2]=b'MZ';struct.pack_into('<I',data,0x3c,0x80)
        data[0x80:0x84]=b'PE\0\0';struct.pack_into('<HH',data,0x84,0x8664,1)
        struct.pack_into('<H',data,0x94,240);opt=0x98
        struct.pack_into('<H',data,opt,0x20b);struct.pack_into('<I',data,opt+108,16)
        struct.pack_into('<IIII',data,opt+240+8,0x400,0x1000,0x400,0x200)
        index,width=(13,32) if delay else (1,20)
        struct.pack_into('<II',data,opt+112+index*8,0x1000,width*2)
        if delay:struct.pack_into('<II',data,0x200,1,0x1080)
        else:struct.pack_into('<I',data,0x200+12,0x1080)
        data[0x280:0x280+len(name)]=name
        return data

    def test_normal_and_delayed_runtime_imports_are_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            binary=Path(directory)/'fixture.exe'
            for delay in [False,True]:
                binary.write_bytes(self.pe(delay=delay))
                self.assertEqual(release.verify_runtime(binary),['kernel32.dll'])
                for name in [b'VCRUNTIME140.dll',b'MSVCP140.dll',b'api-ms-win-crt-runtime-l1-1-0.dll',b'ucrtbase.dll',b'vcomp140.dll',b'concrt140.dll',b'vccorlib140.dll',b'msvcr120.dll',b'unknown.dll']:
                    binary.write_bytes(self.pe(name,delay))
                    with self.assertRaisesRegex(ValueError,'Microsoft CRT'):release.verify_runtime(binary)

    def test_the_graphics_memory_counter_import_is_allowed(self):
        # 2026-10-25: Falcon reads the whole-card memory counter through pdh.dll (Windows Vista+).
        # Falsifier: remove pdh.dll from SYSTEM_IMPORTS.
        with tempfile.TemporaryDirectory() as directory:
            binary=Path(directory)/'fixture.exe'
            binary.write_bytes(self.pe(b'pdh.dll'))
            self.assertEqual(release.verify_runtime(binary),['pdh.dll'])

    def test_malformed_pe_tables_are_refused(self):
        for offset,value in [(0x84,0x14c),(0x98+112+8,0xfffffff0),(0x200+12,0xfffffff0)]:
            data=self.pe();struct.pack_into('<I',data,offset,value)
            with self.assertRaises(ValueError):release.pe_imports(data)
        with self.assertRaises(ValueError):release.pe_imports(self.pe()[:200])


if __name__=='__main__':unittest.main()
