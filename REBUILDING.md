# Rebuilding Falcon Photo Viewer and the RAW library

Falcon's own code is Apache-2.0. RAW development uses rawler 0.8.0 under LGPL-2.1. The release
provides corresponding application source, build scripts, locked dependencies and the rawler
source. You may modify that library and rebuild the executable that uses your modified copy.
Keep the library's copyright/licence notices. This source route does not require proprietary
object files or a paid compiler/signing certificate.

## Normal build

Use the source archive corresponding to the binary release. Its source revision and SHA256
checksums are provided beside the downloads. Install Rust 1.96.0 and native C++ build tools
(Visual Studio Build Tools plus Windows SDK on Windows; Xcode command-line tools on macOS).
Follow BUILDING.md. Run Cargo inside falcon so the Windows static-runtime flags apply:

```text
cargo build --locked --release -p falcon-native
```

No CUDA or HEVC decoder DLL is bundled. Optional acceleration uses compatible installed runtimes;
CPU fallback remains available. The standard Rust toolchain and OS SDK are separate build tools.

## Modify and relink rawler

1. Extract the matching source and dependency-source archives to a new directory. Keep the supplied
   patched winit, zune-jpeg and femtovg sources and all licence files. Use a copy, not a shared
   Cargo registry cache.
2. Copy rawler's supplied source into a writable directory, for example third-party/rawler, and
   make your changes there. Keep its package version/API compatible initially.
3. In falcon/Cargo.toml, add the following entry to the EXISTING [patch.crates-io] table. Retain
   every existing entry (winit, zune-jpeg, femtovg). The path is relative to falcon/Cargo.toml:

```toml
rawler = { path = "../third-party/rawler" }
```

4. Run `cargo update -p rawler` from falcon to record that deliberate local-source override in
   your copy's lockfile. This modified build's lockfile differs intentionally from the release.
5. Run the relevant tests, then `cargo build --locked --release -p falcon-native`. The rebuilt
   executable statically links your modified library. Run that new executable on a test RAW file.

Use the application and library sources matching your downloaded version. These instructions
cover modifying rawler; substituting a different RAW decoder requires adapting Falcon's code.
A successful Windows rebuild does not verify a Mac build: build and test on the target platform.

Use the application, dependency-source and binary archives from the same release.
