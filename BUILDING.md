# Building Falcon from source

Use the source snapshot matching the intended release. Keep the bundled licence/notices and
source revision with any package you distribute.

Use Rust 1.96.0 (including Cargo, rustfmt and Clippy), Python 3.11 or newer, and the native compiler
for your operating system: the Visual Studio C++ build tools/Windows SDK on Windows, or Xcode
command-line tools on macOS. Run Cargo inside `falcon/`.

```text
cargo test --locked --workspace -- --nocapture
cargo clippy --locked -p falcon-native --all-targets
cargo build --locked --release --bin falcon
```

The portable Cargo configuration retains Windows static-CRT flags. Set `CARGO_TARGET_DIR` if you
want build output outside this checkout. Do not set `SLINT_SCALE_FACTOR` while compiling: that
changes generated UI geometry. The patched `vendor/winit`, `vendor/zune-jpeg` and `vendor/femtovg`
directories are required, including their licences and original-file hash inventories. zune-jpeg
includes Falcon's independently written integer transpose. Keep `Cargo.lock` unchanged for a
reproducible build.

On macOS, run the native toolbar tests before a release build:

```text
cargo test --locked --workspace --features falcon-native/mac-native-tests
```

Packaging requires macOS tools. From the source root, run:

```text
python3 scripts/test-mac-experiment-bundle.py
python3 scripts/test-winit-hosted-patch.py
python3 scripts/test-zune-jpeg-patch.py
bash scripts/mac-bundle.sh falcon/target/release/falcon dist shipping
```

For a source archive without Git metadata, set `FALCON_SOURCE_REVISION` to the full, lowercase
40-character revision of the published source. In an actual Git checkout an override must equal
its HEAD. An extracted archive inside another repository does not inherit that parent's revision.
Do not copy a private `.git` directory into an
archive. A local preparation tree has no published revision yet.

Private camera photos and decoded photo tiles are absent. Portable synthetic tests still run.
Real-camera, optional-codec and GPU checks print SKIP when their required inputs or hardware are
unavailable; the test framework can count these returns as passes. Preserve those messages and
report coverage separately. See [fixture setup](docs/testing/private-fixtures.md).

The CPU JPEG fallback works without a developer CUDA installation. Falcon does not
bundle CUDA/nvJPEG runtimes or an HEVC software decoder. Test each intended package on a clean
machine, then review its actual dependencies, notices and source correspondence before publishing.

Windows packaging, from the clean Git source checkout after review:

```text
python scripts/package-windows.py dist
```

The helper archives the committed source into a temporary directory and uses a fresh target and
Cargo home with controlled compiler flags. It checks the executable's runtime imports and icon
resources and records the toolchain, source revision and executable/lockfile/notice hashes.
It refuses a dirty or missing Git checkout and accepts no separately supplied executable. Extracted
source archives can still use the normal Cargo build instructions above; the verified Windows
package helper requires a Git checkout. Check the resulting app's launch before distributing it.

Maintainers regenerate notices using cargo-about 0.9.2 and scripts/generate-notices.py; `--check`
regenerates and compares without changing files. Unverified saved JSON is not accepted. A normal
build uses the checked-in notices. Public builds use the checked-in icon resources; the private
Figma authoring pack is not required. See REBUILDING.md for replacing and relinking rawler.
