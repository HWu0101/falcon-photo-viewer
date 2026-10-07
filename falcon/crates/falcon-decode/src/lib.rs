//! Falcon decode — folder scan + RAW+JPG pairing, and the per-frame decode paths
//! that feed the viewer. Every piece here is lifted from a verified Phase-0 spike
//! (see ../../../PLAN.md §14): JPG shrink-on-load (`spikes/shrink_bench`,
//! `real_bench`), CR3 embedded-preview extraction (`real_bench`), and true raw
//! develop (`spikes-raw/raw_develop`). No cmake/nasm — pure Rust.
//!
//! Render tiers (PLAN §6.3):
//!   * **fast**       — DCT shrink-decode the JPG (or CR3 embedded preview) to ~4K,
//!                      re-encode small JPEG. The 30/s scrub path.
//!   * **reference**  — full 45 MP decode → Lanczos downscale → high-quality JPEG.
//!   * **develop_raw**— rawler true raw develop (demosaic + camera WB + cam→sRGB).
//!   * **thumbnail**  — tiny JPEG for the filmstrip.

#[cfg(test)]
#[path = "../../test-support/fixture_paths.rs"]
mod fixture_paths;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
// Shared 3×3 helpers (row-major, `M·v` dots rows with the column vector) — one implementation for
// the whole workspace; falcon-decode's private copies were deleted in the v0.7.4 dedup (C8).
use falcon_color::{mat3_inv, mat3_mul};
use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use jpeg_decoder::{Decoder, PixelFormat};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use rawler::analyze::extract_raw_pixels;
use rawler::decoders::RawDecodeParams;
use rawler::imgop::xyz::Illuminant;
use rawler::RawImageData;
use serde::Serialize;

// v0.8.0 stage 2 — the rotation APPLY pipeline (EXIF 2-byte patch + XMP sidecars + the mirror-safe
// FILE-side composer). Kept in its own module (the app's first mutation of user originals gets its own
// clearly-sectioned, heavily-tested home) and re-exported flat so callers see `falcon_decode::…`.
pub mod file_io;
mod apply;
pub use apply::*;

// v0.9.1 (P5, PLAN §65): the finished-image decode layer behind the `ImageDecoder` trait so a future
// macOS ImageIO decoder slots in without touching the render-worker call sites. Re-exported flat so
// callers see `falcon_decode::{ImageDecoder, DecodedPixels, DecodeError, DecodeCaps, CpuDecoder, …}`.
mod decode;
pub use decode::*;

// Precision-preserving CPU RAW export and the CPU viewer's shared X-Trans development route.
mod raw_export;
pub use raw_export::{develop_raw_pixels_for_export, RawExportCancelled};
mod xtrans;

mod scan_io;

// v0.8.144 (E1) — the from-scratch ISO-BMFF HEIF grid parse: the container stage of the hardware-
// decode epic. Ships INERT: nothing on the shipping decode path calls it unless
// `FALCON_HW_HEIC_PARSE=1`, and even then it only parks a diagnostic line per file. Same flat
// re-export idiom as `apply`, so callers see `falcon_decode::…`.
mod heif_grid;
pub use heif_grid::*;

// v0.8.145 (E2) — the deterministic NV12→RGB8 kernel: the colour stage of the hardware-decode epic,
// and (HEVC decode being bit-exact) the ONLY place two backends could ever disagree about a HEIC's
// colour. Ships INERT: nothing on the shipping decode path calls it, and there is no flag to switch
// because there is nothing to switch — the only callers are the tests, `falcon-gpu`'s twin harness
// and the `yuv_kernel_probe` example.
//
// NOT flat-re-exported, deliberately. `apply`/`heif_grid` join the app's flat surface because the app
// calls them; nothing in the app calls this, and `falcon_decode::yuv_kernel::…` keeps that visible at
// every call site (and keeps names like `GOLDEN_PINS` out of a glob import).
pub mod yuv_kernel;

/// Extensions we treat as raw. The finished-image sibling (JPG/PNG/TIFF) is the "fast" copy.
/// M16 (v0.8.63) additions — each verified against rawler 0.7.2's CONTENT-based dispatch (rawler detects
/// by magic bytes / TIFF `Make`, never by extension, so the extension is only OUR routing hint into the
/// RAW path; a file that doesn't match a decoder still falls through to the normal decode-failure card):
///   iiq → decoders/mod.rs Make "Phase One" / "Phase One A/S" / "Leaf" → iiq::IiqDecoder
///   3fr → decoders/mod.rs Make "Hasselblad" → tfr::TfrDecoder (Hasselblad 3FR; testdata *.3FR)
///   x3f → decoders/mod.rs x3f::is_x3f (Sigma Foveon magic) → x3f::X3fDecoder
///   nrw → decoders/mod.rs Make "NIKON" → nrw::NrwDecoder
///   crw → decoders/mod.rs ciff::is_ciff (Canon CIFF magic) → crw::CrwDecoder
///   mrw → decoders/mod.rs mrw::is_mrw (Minolta magic) → mrw::MrwDecoder
///   erf → decoders/mod.rs Make "SEIKO EPSON CORP." → erf::ErfDecoder
///   sr2 → decoders/mod.rs Make "SONY" → arw::ArwDecoder (Sony; testdata *.SR2)
///
/// `pub` (v0.8.64, C1 audit ORANGE fix): falcon-native's Open-dialog filters + RAW association
/// family used to hand-mirror this list in support.rs, which silently went stale the moment M16
/// added the 8 tail entries here. Now it's `pub` so support.rs re-exports THIS const directly —
/// one list, everywhere "RAW file" is decided.
pub const RAW_EXTS: &[&str] = &[
    "cr3", "cr2", "nef", "arw", "raf", "rw2", "dng", "orf", "pef", "srw",
    "iiq", "3fr", "x3f", "nrw", "crw", "mrw", "erf", "sr2",
];

/// Extensions that are a decodable *finished* image (the standalone/"fast" slot). JPEG is the
/// camera default; PNG/TIFF are the Way-A additions (§30.3) for scans/screenshots/exports.
const JPEG_EXTS: &[&str] = &["jpg", "jpeg"];
/// PNG family. `.apng` is an animated PNG whose FIRST (default) frame the `png` crate decodes exactly
/// like a still PNG — we declassify it here (formats batch F4) so an `.apng` file shows its default
/// image instead of being silently dropped (it previously matched no list → ignored). No APNG playback
/// this round; the still first frame is the deliverable.
const PNG_EXTS: &[&str] = &["png", "apng"];
const TIFF_EXTS: &[&str] = &["tif", "tiff"];

/// JPEG XL — decodable everywhere via the pure-Rust `jxl-oxide` crate (formats batch F1). A modern
/// delivered/archival format; orientation lives in the codestream header (applied on render) and the
/// embedded ICC drives the source gamut, so wide-gamut JXLs colour-manage correctly.
const JXL_EXTS: &[&str] = &["jxl"];

/// BMP — decodable everywhere via the pure-Rust `image` crate's `bmp` codec (formats batch F3).
const BMP_EXTS: &[&str] = &["bmp"];

/// GIF — decodable everywhere via the pure-Rust `gif` crate (formats batch F2). Still consumers see the
/// composited FIRST frame; the viewer plays the full animation (disposal-correct frames + per-frame
/// delays) when a GIF is the current shot. Animated is the point — a static first frame isn't the value.
const GIF_EXTS: &[&str] = &["gif"];

/// WebP — decodable everywhere via the pure-Rust `image-webp` crate (lossy VP8 + lossless VP8L, and
/// it exposes the ICCP colour profile). A delivered/web export format; no OS Store codec needed, so
/// it decodes identically on every machine (§48 #75, unlike the WIC-gated HEIC path).
const WEBP_EXTS: &[&str] = &["webp"];

/// HEIC/HEIF — decodable via the OS codec: WIC on Windows (Way C §30.3), Image I/O (CGImageSource)
/// on macOS (v0.9.9). On a platform with no HEVC/HEIF system decoder they classify as Unsupported.
const HEIC_EXTS: &[&str] = &["heic", "heif"];

/// Recognised image types we do NOT yet decode. They are surfaced with an "unsupported" badge
/// (not silently dropped) so a folder of them doesn't read as "No photos", and an odd stray image
/// in a burst stays visible. The formats batch (v0.8.55) removed `jxl`/`gif`/`bmp` from here — they
/// now decode via their own SrcKinds. AVIF stays (its HDR→SDR design round is deferred) and TGA stays.
const UNSUPPORTED_IMG_EXTS: &[&str] = &["avif", "tga"];

/// True when `ext` (case-insensitive, no dot) is ANY decodable image extension — RAW or finished.
/// Used by [`Shot::seq_stem`] to strip stacked extensions (`IMG_0042.tif.png`) down to the real stem;
/// a non-image dotted suffix (`photo.v2`) is deliberately NOT matched, so it survives.
fn is_known_image_ext(ext: &str) -> bool {
    let e = ext.to_ascii_lowercase();
    let e = e.as_str();
    RAW_EXTS.contains(&e)
        || JPEG_EXTS.contains(&e)
        || PNG_EXTS.contains(&e)
        || TIFF_EXTS.contains(&e)
        || WEBP_EXTS.contains(&e)
        || HEIC_EXTS.contains(&e)
        || JXL_EXTS.contains(&e)
        || BMP_EXTS.contains(&e)
        || GIF_EXTS.contains(&e)
}

/// DoS bound on decoded source dimensions (every full-res decode path + the ROI native buffer):
/// a crafted header claiming an enormous size can't force a multi-GB allocation that thrashes or
/// OOMs the process. **Adaptive** (2026-07-08): a flat 200 MP wrongly rejected genuine 251.6 MP
/// files — JWST/astro exports, flatbed panoramas — that decode fine (a 24568×10240 baseline JPEG /
/// LZW TIFF); worse, nvJPEG's hardware unit rejects such a JPEG on resolution, so the CPU fallback
/// is the ONLY path and it was bailing here. Now the cap scales with physical RAM: `ram / 48`
/// pixels — one source may occupy up to ~1/6 of RAM at the ~8 B/px worst-case decode intermediate
/// (see [`cap_from_ram`]), clamped 200 MP–2000 MP — so a machine with the memory to hold a
/// gigapixel decode is allowed to, while a small machine keeps today's safe floor and every machine
/// still rejects the truly absurd. Computed once (RAM is fixed for the process).
///
/// Callers still allocate through the decoders (jpeg-decoder / tiff / png / WIC), whose buffers are
/// sized from the SAME header — this pre-check is what keeps that alloc bounded, so the cap must be
/// an amount we're actually willing to allocate.
fn max_source_pixels() -> u64 {
    use std::sync::OnceLock;
    static CAP: OnceLock<u64> = OnceLock::new();
    // 8 GB assumed if the probe fails — matches the historical floor's target machine.
    *CAP.get_or_init(|| cap_from_ram(total_ram_bytes().unwrap_or(8_000_000_000)))
}

/// THE chokepoint for the decompression-bomb DoS bound: every full-res decode path validates its
/// header-declared dimensions here BEFORE any pixel-sized allocation (JPEG SOF + scaled stop, WIC,
/// PNG, WebP, TIFF, the watermark PNG). Rejects empty (0-dim) frames and anything past
/// [`max_source_pixels`] with one uniform, informative message. Adding a decoder? Route its header
/// dims through here before allocating.
fn guard_source_dims(w: u32, h: u32, what: &str) -> Result<()> {
    if w == 0 || h == 0 || (w as u64) * (h as u64) > max_source_pixels() {
        bail!("{what} too large or empty: {w}x{h} (cap {} px)", max_source_pixels());
    }
    Ok(())
}

/// Pure cap policy (split out so it's unit-testable without a live RAM probe): one source may occupy
/// up to ~`ram / 6` for its PEAK decode buffer. The peak is NOT the 3 B/px packed-RGB output — a
/// 16-bit RGBA TIFF decodes to a `U16` intermediate at 8 B/px (and a progressive JPEG holds full
/// coefficient buffers), so budget the worst realistic case at ~8 B/px: `ram / 6 / 8 = ram / 48`
/// pixels. Floor 200 MP = the old flat cap (never regress a small machine BELOW it); ceiling 2000 MP
/// is the "no real single-buffer image is this big" absurd-bomb boundary. A machine with ≥ ~12 GB RAM
/// therefore accepts the 251.6 MP edge-case files at full resolution; every machine still rejects a
/// crafted gigapixel bomb, and a smaller machine still BROWSES a huge JPEG via the DCT-scaled detail
/// tier (which decodes a ≤ ddim stop, well under the floor) even when its full source exceeds the cap.
fn cap_from_ram(ram: u64) -> u64 {
    (ram / 48).clamp(200_000_000, 2_000_000_000)
}

/// Total physical RAM in bytes (`GlobalMemoryStatusEx`), or `None` if the probe fails / off Windows.
/// Drives [`max_source_pixels`]; a `None` there falls back to an 8 GB assumption. Public so the
/// native app's RAM L2 cache (PLAN §57) can size its budget off the SAME probe — no new dependency.
#[cfg(windows)]
pub fn total_ram_bytes() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut m = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        GlobalMemoryStatusEx(&mut m).ok()?;
        Some(m.ullTotalPhys)
    }
}
// macOS (v0.9.7, Phase 6): total physical RAM = the `hw.memsize` sysctl (a uint64), the analogue of
// GlobalMemoryStatusEx's ullTotalPhys. Feeds max_source_pixels() so the bomb cap tracks the real
// machine instead of the 8 GB `None` fallback. `None` on any sysctl error (never a panic).
#[cfg(target_os = "macos")]
pub fn total_ram_bytes() -> Option<u64> {
    let mut val: u64 = 0;
    let mut len: libc::size_t = std::mem::size_of::<u64>();
    let rc = unsafe {
        libc::sysctlbyname(
            b"hw.memsize\0".as_ptr() as *const libc::c_char,
            &mut val as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && val > 0).then_some(val)
}
#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn total_ram_bytes() -> Option<u64> {
    None
}

/// AVAILABLE physical RAM in bytes (`GlobalMemoryStatusEx`'s `ullAvailPhys`), or `None` if the
/// probe fails / off Windows. The same probe pattern as [`total_ram_bytes`] — no new dependency.
/// Public for the native app's RAM-L2 pressure controller (PLAN §57 stage 2): sampled ~1/s to
/// degrade/restore the L2 budget when the SYSTEM (not just this process) runs low on memory.
#[cfg(windows)]
pub fn avail_ram_bytes() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut m = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        GlobalMemoryStatusEx(&mut m).ok()?;
        Some(m.ullAvailPhys)
    }
}
// macOS (v0.9.23, one-pool round — closes logic.md M3): AVAILABLE RAM via Mach host statistics.
// `host_statistics64(HOST_VM_INFO64)` fills a `vm_statistics64` (mach/vm_statistics.h); the accepted
// "available" approximation on macOS is `(free_count + inactive_count) × page_size` — inactive pages
// are reclaimable cache the kernel hands back under pressure, the same definition `vm_stat`-derived
// monitors build "available memory" on (Apple has no direct `ullAvailPhys` analogue; see the
// host_statistics64 docs in <mach/mach_host.h> + the vm_statistics64 layout in <mach/vm_statistics.h>).
// This turns the app's ~1 Hz RAM-L2 pressure valve ON for macOS — before this arm the `None` made the
// valve silently inert (the M3 finding). `None` on any Mach error (never a panic).
#[cfg(target_os = "macos")]
pub fn avail_ram_bytes() -> Option<u64> {
    // Byte-layout mirror of <mach/vm_statistics.h>'s `vm_statistics64` (natural_t = u32, 8-byte
    // aligned). Only the first three u32s are read here, but the FULL struct is declared so the
    // `count` handshake below matches HOST_VM_INFO64_COUNT and the kernel fills a correctly-sized
    // buffer (a short count would truncate silently on older kernels; a wrong one errors).
    #[repr(C, align(8))]
    #[derive(Default)]
    struct VmStatistics64 {
        free_count: u32,
        active_count: u32,
        inactive_count: u32,
        wire_count: u32,
        zero_fill_count: u64,
        reactivations: u64,
        pageins: u64,
        pageouts: u64,
        faults: u64,
        cow_faults: u64,
        lookups: u64,
        hits: u64,
        purges: u64,
        purgeable_count: u32,
        speculative_count: u32,
        decompressions: u64,
        compressions: u64,
        swapins: u64,
        swapouts: u64,
        compressor_page_count: u32,
        throttled_count: u32,
        external_page_count: u32,
        internal_page_count: u32,
        total_uncompressed_pages_in_compressor: u64,
    }
    const HOST_VM_INFO64: libc::c_int = 4; // <mach/host_info.h>
    extern "C" {
        // Both live in libSystem (always linked) — no new framework/link flag.
        fn mach_host_self() -> u32; // mach_port_t
        fn host_statistics64(
            host: u32,
            flavor: libc::c_int,
            info: *mut i32, // host_info64_t = integer_t*
            count: *mut u32,
        ) -> libc::c_int; // kern_return_t (0 = KERN_SUCCESS)
    }
    // mach_host_self() allocates a SEND RIGHT per call — cache the port once (the standard
    // monitor-tool discipline) so the ~1 Hz valve never leaks Mach rights over a long session.
    static HOST_PORT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let host = *HOST_PORT.get_or_init(|| unsafe { mach_host_self() });
    let mut stats = VmStatistics64::default();
    let mut count = (std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<i32>()) as u32;
    let rc = unsafe {
        host_statistics64(host, HOST_VM_INFO64, &mut stats as *mut VmStatistics64 as *mut i32, &mut count)
    };
    if rc != 0 {
        return None;
    }
    // Counts are in HOST pages (16 KiB on Apple Silicon) — sysconf(_SC_PAGESIZE) reports the same
    // host page size, so the units match.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return None;
    }
    Some((stats.free_count as u64 + stats.inactive_count as u64) * page as u64)
}
#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn avail_ram_bytes() -> Option<u64> {
    None
}

// ───────────────────────── cloud (OneDrive) placeholder detection ─────────────────────────
// OneDrive "Files-On-Demand" (and other cloud providers) keep files as PLACEHOLDERS: the directory
// entry exists with full metadata but zero local bytes, and any DATA read triggers a download
// ("hydration"). The placeholder state rides the file's ATTRIBUTES, so it is detectable from metadata
// ALONE — no open, no read, no download. Falcon tags such shots at scan (v0.8.24, D1) so an offline /
// sync-paused placeholder that can't decode gets an honest "not downloaded" notice + a timed retry
// instead of latching into a permanent generic-failure state, and Copy/Move can warn before they pull
// multi-GB down. CRITICAL: detection is metadata-only — the normal decode pipeline stays the ONLY code
// that reads a placeholder's bytes (the sole hydration trigger), exactly as before this feature.

/// `FILE_ATTRIBUTE_OFFLINE` (0x1000): the file's data is not immediately available — a secondary cloud signal.
pub const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
/// `FILE_ATTRIBUTE_RECALL_ON_OPEN` (0x40000): opening the file recalls it — a secondary signal used by
/// full-file cloud providers.
pub const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` (0x400000): the Files-On-Demand marker — a dehydrated
/// placeholder that hydrates on the first DATA read. The primary OneDrive signal.
pub const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// True if a Windows `dwFileAttributes` bitmask marks a cloud (not-locally-hydrated) placeholder. Pure
/// over the raw bits so it unit-tests on synthetic values; the primary marker is
/// `RECALL_ON_DATA_ACCESS`, with `OFFLINE` / `RECALL_ON_OPEN` as secondary signals.
pub fn is_cloud_placeholder(attrs: u32) -> bool {
    attrs
        & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN)
        != 0
}

/// v1.0.0-rc TAIL (skeptic A, R1): `SF_DATALESS` (0x4000_0000, `<sys/stat.h>`) — the macOS
/// analogue of `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`. The kernel sets it on a file whose data has
/// been evicted by a File Provider: iCloud Drive's "Optimise Mac Storage", and — since macOS 12.3 —
/// OneDrive and Dropbox, which both moved onto the same File Provider eviction. Reading the DATA
/// materialises the file, exactly as on Windows.
///
/// It is a `st_flags` bit, so it is metadata-only and rides the `entry.metadata()` the scan already
/// takes: no `libc` call, no extra stat, no open.
pub const SF_DATALESS: u32 = 0x4000_0000;

/// True if a macOS `st_flags` bitmask marks a dataless (not-locally-materialised) file. Pure over
/// the raw bits, exactly like [`is_cloud_placeholder`], so it unit-tests on synthetic values on any
/// host — which matters, because the only machine that can reach its CALLER is a Mac.
pub fn is_dataless(st_flags: u32) -> bool {
    st_flags & SF_DATALESS != 0
}

/// Metadata-only cloud-placeholder probe of ONE path.
///
/// Windows reads the file attributes (`file_attributes()` — no open-for-data, no read, so a OneDrive
/// placeholder is NOT hydrated) and classifies via [`is_cloud_placeholder`]. macOS reads `st_flags`
/// and classifies via [`is_dataless`] (v1.0.0-rc TAIL, skeptic A R1). A stat error (missing /
/// locked) reads as "not a placeholder". On every other platform there is no such notion → `false`.
/// Used at scan (per pair file) and by the native app's timed retry sweep to notice a file that has
/// materialised since scan — which is also what makes B-Y2's hydration re-sniff work on both
/// platforms rather than one.
#[cfg(windows)]
pub fn file_is_cloud_placeholder(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|m| is_cloud_placeholder(m.file_attributes()))
        .unwrap_or(false)
}
#[cfg(target_os = "macos")]
pub fn file_is_cloud_placeholder(path: &Path) -> bool {
    use std::os::macos::fs::MetadataExt;
    std::fs::metadata(path).map(|m| is_dataless(m.st_flags())).unwrap_or(false)
}
#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn file_is_cloud_placeholder(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod cloud_placeholder_tests {
    use super::{
        is_cloud_placeholder, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN,
    };
    const ARCHIVE: u32 = 0x20; // a plain local file
    const DIRECTORY: u32 = 0x10;

    #[test]
    fn plain_local_files_are_not_placeholders() {
        assert!(!is_cloud_placeholder(0));
        assert!(!is_cloud_placeholder(ARCHIVE));
        assert!(!is_cloud_placeholder(ARCHIVE | DIRECTORY));
    }

    #[test]
    fn recall_on_data_access_is_the_primary_marker() {
        assert!(is_cloud_placeholder(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS));
        assert!(is_cloud_placeholder(ARCHIVE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS));
    }

    #[test]
    fn offline_and_recall_on_open_are_secondary_markers() {
        assert!(is_cloud_placeholder(FILE_ATTRIBUTE_OFFLINE));
        assert!(is_cloud_placeholder(FILE_ATTRIBUTE_RECALL_ON_OPEN));
        assert!(is_cloud_placeholder(ARCHIVE | FILE_ATTRIBUTE_OFFLINE));
    }

    #[test]
    fn all_markers_together_trip() {
        assert!(is_cloud_placeholder(
            FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN
        ));
    }
}

#[cfg(test)]
mod cap_tests {
    use super::cap_from_ram;
    const MP: u64 = 1_000_000;

    #[test]
    fn source_cap_tiers_with_ram() {
        // Tiny machine → clamped UP to the 200 MP floor (never regress the old flat cap).
        assert_eq!(cap_from_ram(2 * 1_000_000_000), 200 * MP);
        // 8 GB → 166 MP → clamped to the 200 MP floor (a 251 MP full-res decode is genuinely risky
        // here; the DCT-scaled detail tier still BROWSES the file — see cap_from_ram docs).
        assert_eq!(cap_from_ram(8 * 1_000_000_000), 200 * MP);
        // ≥ ~12 GB is the crossover where the 251.6 MP edge-case files are admitted at full resolution.
        assert!(cap_from_ram(12 * 1_000_000_000) < 251_600_000); // 250 MP — just rejects
        assert!(cap_from_ram(16 * 1_000_000_000) > 251_600_000); // 333 MP — accepts
        // Huge machine → clamped DOWN to the 2000 MP absurd-bomb ceiling.
        assert_eq!(cap_from_ram(128 * 1_000_000_000), 2000 * MP);
    }
}

#[cfg(test)]
mod scan_tests {
    use super::{scan_folder, SrcKind};

    fn touch(dir: &std::path::Path, name: &str) {
        std::fs::write(dir.join(name), b"").unwrap();
    }

    #[test]
    fn finished_siblings_split_only_raw_pairs() {
        let dir = std::env::temp_dir().join("falcon_scan_pairing_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // JPG + TIFF, same stem, NO raw → two SEPARATE shots (the edge-case report).
        touch(&dir, "weic2603c.jpg");
        touch(&dir, "weic2603c.tif");
        // A camera RAW+JPG burst → ONE paired shot (name = bare stem).
        touch(&dir, "HWU_0001.CR3");
        touch(&dir, "HWU_0001.JPG");
        // A lone JPG → one shot (bare stem).
        touch(&dir, "solo_5.jpg");

        let shots = scan_folder(&dir).unwrap();

        // The JPG/TIFF stem split into two distinctly-named shots (name is the rating key).
        let jpg = shots.iter().find(|s| s.name == "weic2603c.jpg").expect("split JPG shot");
        let tif = shots.iter().find(|s| s.name == "weic2603c.tif").expect("split TIFF shot");
        assert_eq!(jpg.kind, SrcKind::Jpeg);
        assert_eq!(tif.kind, SrcKind::Tiff);
        assert!(!jpg.has_raw && !tif.has_raw);
        assert_ne!(jpg.name, tif.name, "split shots must have distinct keys");

        // RAW+JPG still collapses to one shot keyed by the bare stem.
        let pair = shots.iter().find(|s| s.name == "HWU_0001").expect("RAW+JPG pair");
        assert!(pair.has_raw && pair.has_jpg);
        assert_eq!(shots.iter().filter(|s| s.name.starts_with("HWU_0001")).count(), 1);

        // The lone JPG keeps the bare stem (no ext suffix).
        assert!(shots.iter().any(|s| s.name == "solo_5"));

        // 4 shots total (2 split + 1 pair + 1 lone), ids sequential.
        assert_eq!(shots.len(), 4);
        for (i, s) in shots.iter().enumerate() {
            assert_eq!(s.id, i);
        }
        // seq_stem() strips the ext suffix so the filmstrip number still comes from the real file stem.
        assert_eq!(jpg.seq_stem(), "weic2603c");
        assert_eq!(tif.seq_stem(), "weic2603c");
        assert_eq!(super::frame_number(&tif.seq_stem()), ""); // no trailing digits in this stem
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn locally_written_files_are_not_cloud_placeholders() {
        // A freshly-written local temp file carries no cloud attributes → cloud_placeholder is false
        // end-to-end through the scan. Proves the scan populates the field and never false-tags an
        // ordinary local file (the common case; the marker-bit logic itself is in cloud_placeholder_tests).
        let dir = std::env::temp_dir().join("falcon_scan_cloud_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        touch(&dir, "HWU_0002.CR3");
        touch(&dir, "HWU_0002.JPG");
        touch(&dir, "lone.jpg");
        let shots = scan_folder(&dir).unwrap();
        assert!(!shots.is_empty());
        assert!(
            shots.iter().all(|s| !s.cloud_placeholder),
            "local files must never be tagged as cloud placeholders"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn raw_exts_pinned() {
        // M16 (v0.8.63): RAW_EXTS is a support contract — a format a tester relies on must not silently
        // drop out or change. Pins the EXACT list; the 8 tail entries are rawler-0.7.2-verified (see the
        // per-ext decoders/mod.rs dispatch evidence in the RAW_EXTS doc comment).
        let expected: &[&str] = &[
            "cr3", "cr2", "nef", "arw", "raf", "rw2", "dng", "orf", "pef", "srw",
            "iiq", "3fr", "x3f", "nrw", "crw", "mrw", "erf", "sr2",
        ];
        assert_eq!(super::RAW_EXTS, expected);
        // Every entry must be lowercase, dot-free, and unique — the scan lowercases the ext before the
        // RAW_EXTS.contains() lookup, so a stray uppercase/dotted/duplicate entry would be dead weight.
        let mut seen = std::collections::HashSet::new();
        for e in super::RAW_EXTS {
            assert_eq!(*e, e.to_ascii_lowercase(), "RAW_EXTS entries must be lowercase: {e}");
            assert!(!e.contains('.'), "RAW_EXTS entries must be dot-free: {e}");
            assert!(seen.insert(*e), "RAW_EXTS has a duplicate entry: {e}");
        }
    }

    #[test]
    fn new_raw_exts_scan_as_raw_shots() {
        // Each M16-added extension classifies as a RAW shot (has_raw) at scan — i.e. it routes into the
        // RAW decode path, never the silently-ignored path. Pins the scan CLASSIFICATION only (no decode).
        let dir = std::env::temp_dir().join("falcon_scan_newraw_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let new_exts = ["iiq", "3fr", "x3f", "nrw", "crw", "mrw", "erf", "sr2"];
        for (i, ext) in new_exts.iter().enumerate() {
            touch(&dir, &format!("SHOT_{i}.{ext}"));
        }
        let shots = scan_folder(&dir).unwrap();
        assert_eq!(shots.len(), new_exts.len(), "each new-ext file is its own RAW shot");
        assert!(shots.iter().all(|s| s.has_raw), "every new-ext file must classify as RAW");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_names_unique_seq_stem_numeric() {
        let dir = std::env::temp_dir().join("falcon_scan_unique_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Numeric-stem split → seq_stem yields the sequence number for the filmstrip badge.
        touch(&dir, "IMG_0042.jpg");
        touch(&dir, "IMG_0042.tif");
        // Contrived double-extension collision: A.tif (a split of stem "A") vs A.tif.png's file_stem
        // "A.tif" (a lone shot) — both would be named "A.tif" without the uniqueness pass.
        touch(&dir, "A.jpg");
        touch(&dir, "A.tif");
        touch(&dir, "A.tif.png");

        let shots = scan_folder(&dir).unwrap();

        // Numeric-stem split shots keep the digit sequence via seq_stem despite the ext-suffixed name.
        let j = shots.iter().find(|s| s.name == "IMG_0042.jpg").expect("numeric split JPG");
        assert_eq!(super::frame_number(&j.seq_stem()), "0042");

        // EVERY shot name is unique (no rating-key collision) — the double-ext case is disambiguated.
        let names: Vec<&str> = shots.iter().map(|s| s.name.as_str()).collect();
        let uniq: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), uniq.len(), "shot names (rating keys) must be unique: {names:?}");
        // The collision produced a suffixed name so both "A.tif" shots survive as distinct keys.
        assert!(shots.iter().filter(|s| s.name.starts_with("A.tif")).count() >= 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seq_stem_strips_stacked_image_extensions() {
        let dir = std::env::temp_dir().join("falcon_scan_double_ext_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Double-extension file: file_stem is "IMG_0042.tif", whose trailing ".tif" made
        // frame_number find no trailing digits → blank filmstrip badge (G6).
        touch(&dir, "IMG_0042.tif.png");
        // Normal single-extension shot alongside it → regression guard: one strip, digits intact.
        touch(&dir, "IMG_0042.jpg");

        let shots = scan_folder(&dir).unwrap();
        assert_eq!(shots.len(), 2); // different stems ("IMG_0042.tif" vs "IMG_0042") → two lone shots

        let dbl = shots.iter().find(|s| s.kind == SrcKind::Png).expect("double-ext PNG shot");
        assert_eq!(dbl.seq_stem(), "IMG_0042");
        assert_eq!(super::frame_number(&dbl.seq_stem()), "0042");

        let single = shots.iter().find(|s| s.kind == SrcKind::Jpeg).expect("single-ext JPG shot");
        assert_eq!(single.seq_stem(), "IMG_0042");
        assert_eq!(super::frame_number(&single.seq_stem()), "0042");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod webp_tests {
    use super::{decode_webp_rgb, scan_folder, webp_color_tag, Keep, Pixels, SrcKind};
    use image_webp::{ColorType, WebPEncoder};
    use std::path::Path;

    /// Encode `data` (RGB8 or RGBA8, exactly w*h*channels bytes) to a LOSSLESS (VP8L) WebP file,
    /// optionally with an embedded ICCP profile — so the decode round-trip is byte-exact and we can
    /// exercise the real `image-webp` container path without a committed binary fixture or an external
    /// encoder. (Real lossy VP8 files are covered by the argv boot-test on ffmpeg-produced WebPs.)
    fn write_webp(path: &Path, data: &[u8], w: u32, h: u32, color: ColorType, icc: Option<Vec<u8>>) {
        let mut out = Vec::new();
        let mut enc = WebPEncoder::new(&mut out);
        if let Some(icc) = icc {
            enc.set_icc_profile(icc);
        }
        enc.encode(data, w, h, color).expect("webp encode");
        std::fs::write(path, &out).expect("write webp");
    }

    /// A minimal but valid ICC carrying a v2 `desc` (textDescriptionType) tag == `name`. Not a full
    /// colour profile — just the 128-byte header, a one-entry tag table, and the `desc` tag, which is
    /// all `icc_description()` reads. Lets the ICCP round-trip assert a real, parseable description.
    fn tiny_icc(name: &str) -> Vec<u8> {
        let ascii = name.as_bytes();
        let count = ascii.len() + 1; // ASCII count includes the trailing NUL
        let tag_off = 132 + 12; // header(128) + tag-count(4) + one 12-byte tag entry
        let mut v = vec![0u8; tag_off];
        v[128..132].copy_from_slice(&1u32.to_be_bytes()); // one tag
        v[132..136].copy_from_slice(b"desc"); // tag signature
        v[136..140].copy_from_slice(&(tag_off as u32).to_be_bytes()); // offset
        v[140..144].copy_from_slice(&((12 + count) as u32).to_be_bytes()); // size
        v.extend_from_slice(b"desc"); // tag type
        v.extend_from_slice(&[0u8; 4]); // reserved
        v.extend_from_slice(&(count as u32).to_be_bytes()); // ASCII count
        v.extend_from_slice(ascii);
        v.push(0); // NUL terminator
        v
    }

    #[test]
    fn webp_lossless_roundtrip_rgb() {
        let dir = std::env::temp_dir().join("falcon_webp_rgb_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grad.webp");
        let (w, h) = (4u32, 2u32);
        let mut data = Vec::with_capacity((w * h * 3) as usize);
        for i in 0..(w * h) {
            data.extend_from_slice(&[(i * 10) as u8, (i * 20) as u8, (i * 5) as u8]);
        }
        write_webp(&path, &data, w, h, ColorType::Rgb8, None);
        let (px, dw, dh) = decode_webp_rgb(&path, Keep::NONE).expect("decode webp");
        let rgb = px.into_rgb8().expect("Keep::NONE decodes to RGB8");
        assert_eq!((dw, dh), (w, h));
        assert_eq!(rgb.len(), (w * h * 3) as usize);
        assert_eq!(rgb, data, "lossless VP8L round-trips exactly");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn webp_alpha_flattened_over_white() {
        let dir = std::env::temp_dir().join("falcon_webp_alpha_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("alpha.webp");
        // 2x1 RGBA: opaque red, then a fully-transparent pixel (any RGB).
        let data = [255u8, 0, 0, 255, 10, 20, 30, 0];
        write_webp(&path, &data, 2, 1, ColorType::Rgba8, None);
        let (px, w, h) = decode_webp_rgb(&path, Keep::NONE).expect("decode webp alpha");
        let rgb = px.into_rgb8().expect("Keep::NONE decodes to RGB8");
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgb.len(), 6, "flattened to packed RGB8 (no alpha channel)");
        assert_eq!(&rgb[0..3], &[255, 0, 0], "opaque pixel unchanged");
        assert_eq!(&rgb[3..6], &[255, 255, 255], "transparent pixel flattened over white");
        // ROUND 35, the same file under the ./export PNG stop's request: the alpha survives.
        let (kept, kw, kh) = decode_webp_rgb(&path, Keep::ALL).expect("decode webp alpha, kept");
        assert_eq!((kw, kh), (2, 1));
        assert_eq!(
            kept,
            Pixels::Rgba8(vec![255, 0, 0, 255, 10, 20, 30, 0]),
            "Keep::ALL hands back the file's own RGBA, transparent pixel and all"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn webp_iccp_gamut_desc() {
        let dir = std::env::temp_dir().join("falcon_webp_icc_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let with = dir.join("p3.webp");
        let icc = tiny_icc("Display P3");
        write_webp(&with, &[1, 2, 3, 4, 5, 6], 2, 1, ColorType::Rgb8, Some(icc.clone()));
        // The embedded ICCP is read + parsed to its description (the wide-gamut path).
        assert_eq!(webp_color_tag(&with).desc.as_deref(), Some("Display P3"));
        // …and matches the shared ICC parser (proves the ICCP chunk passes through unchanged).
        assert_eq!(webp_color_tag(&with).desc, super::icc_description(&icc));
        // A profile-less WebP → None → the sRGB fallback in shot_source_gamut.
        let without = dir.join("plain.webp");
        write_webp(&without, &[1, 2, 3, 4, 5, 6], 2, 1, ColorType::Rgb8, None);
        assert_eq!(webp_color_tag(&without), super::ColorTag::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn webp_oversized_iccp_is_bounded_not_allocated() {
        // A WebP whose ICCP chunk exceeds the 4 MB cap must be REJECTED by the memory bound (→ None),
        // not read into a giant allocation — the DoS guard for the metadata path (image-webp's default
        // limit is usize::MAX and it would otherwise alloc the declared chunk size up front).
        let dir = std::env::temp_dir().join("falcon_webp_bigicc_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bigicc.webp");
        let big = vec![0u8; 4_000_001]; // one byte over the cap
        write_webp(&path, &[1, 2, 3, 4, 5, 6], 2, 1, ColorType::Rgb8, Some(big));
        assert_eq!(webp_color_tag(&path), super::ColorTag::default(), "over-cap ICCP fails closed, not OOM");
        // …and the PIXELS still decode (the cap gates only the metadata read, not the image).
        let (px, w, h) = decode_webp_rgb(&path, Keep::NONE).expect("pixels still decode past a big ICCP");
        assert_eq!(((w, h), px.byte_len()), ((2, 1), 6));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_classifies_and_splits_webp() {
        let dir = std::env::temp_dir().join("falcon_webp_scan_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Classification is by extension (no decode), so empty files suffice here.
        std::fs::write(dir.join("shot.webp"), b"").unwrap();
        std::fs::write(dir.join("pair.webp"), b"").unwrap();
        std::fs::write(dir.join("pair.jpg"), b"").unwrap();
        let shots = scan_folder(&dir).unwrap();
        // A lone WebP is a real, decodable shot (NOT Unsupported) and badges as WEBP.
        let lone = shots.iter().find(|s| s.name == "shot").expect("lone webp shot");
        assert_eq!(lone.kind, SrcKind::Webp);
        assert!(!lone.is_unsupported());
        assert_eq!(lone.finished_format().as_deref(), Some("WEBP"));
        // A same-stem jpg+webp are two DISTINCT shots (finished siblings never collapse).
        assert!(shots.iter().any(|s| s.name == "pair.jpg" && s.kind == SrcKind::Jpeg));
        assert!(shots.iter().any(|s| s.name == "pair.webp" && s.kind == SrcKind::Webp));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ─────────────────────────── formats batch (v0.8.55) tests ────────────────────────────

#[cfg(test)]
mod guard_tests {
    use super::{guard_source_dims, max_source_pixels};

    /// THE shared bomb guard every new decode route (JXL/BMP/GIF) calls before allocating. Pin its three
    /// rejection behaviours + the accept path in one place, since the per-format decoders delegate to it.
    #[test]
    fn guard_rejects_empty_and_oversized_accepts_sane() {
        assert!(guard_source_dims(100, 100, "t").is_ok(), "a sane image passes");
        assert!(guard_source_dims(0, 10, "t").is_err(), "zero width rejected");
        assert!(guard_source_dims(10, 0, "t").is_err(), "zero height rejected");
        // One pixel past the adaptive cap → rejected BEFORE any allocation.
        let cap = max_source_pixels();
        // Pick w,h whose product exceeds the cap (cap ≥ 200 MP, so 200_001 × (cap/200_000 + 1) overflows it).
        let side = ((cap as f64).sqrt() as u32) + 2;
        assert!(
            guard_source_dims(side, side, "t").is_err(),
            "{side}×{side} = {} px must exceed cap {cap}",
            side as u64 * side as u64
        );
    }
}

#[cfg(test)]
mod jxl_tests {
    use super::{decode_jxl_rgb, scan_folder, Keep, SrcKind};

    /// Classification is by extension (no decode) — a lone `.jxl` is a real, decodable shot (was
    /// Unsupported before the formats batch) and badges as JXL.
    #[test]
    fn scan_classifies_jxl() {
        let dir = std::env::temp_dir().join("falcon_jxl_scan_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("shot.jxl"), b"").unwrap();
        let shots = scan_folder(&dir).unwrap();
        let lone = shots.iter().find(|s| s.name == "shot").expect("lone jxl shot");
        assert_eq!(lone.kind, SrcKind::Jxl);
        assert!(!lone.is_unsupported());
        assert_eq!(lone.finished_format().as_deref(), Some("JXL"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A non-JXL / corrupt file surfaces as a clean `Err` (retryable decode), never a panic — the codec
    /// is decode-only (no in-tree encoder), so this is the plumbing pin. A real-file decode is gated in
    /// `tests/real.rs` on a fixture path.
    #[test]
    fn garbage_jxl_fails_cleanly() {
        let dir = std::env::temp_dir().join("falcon_jxl_garbage_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.jxl");
        std::fs::write(&path, b"not a real JPEG XL file, just some bytes").unwrap();
        assert!(decode_jxl_rgb(&path, Keep::NONE).is_err(), "garbage .jxl must Err, not panic");
        assert!(decode_jxl_rgb(&path, Keep::ALL).is_err(), "…and the keep arm refuses it identically");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// v0.8.141 — THE COLOR ROUND's follow-up wave: the byte-threading doors, one row each, and the
/// strictly-additive HEIF widening's case table.
///
/// WHY THIS MODULE EXISTS (R4/M4): v0.8.140 rerouted six format doors to hand the resolver a
/// profile's BYTES instead of its name, and pinned exactly two of them (PNG, and the HEIF preview).
/// The other four — JPEG APP2, TIFF 34675, WebP `ICCP`, JXL original-ICC — had no test that bytes
/// reach the resolver at all, so reverting any one of them to a name-only read passed the entire
/// suite. Each row here uses the SAME fixture: a genuine Display-P3 matrix/TRC profile renamed
/// "Display", the macOS screen capture's shape, which a name matcher cannot place and colorimetry
/// can. A door that stops threading its bytes answers sRGB and its row reddens.
#[cfg(test)]
mod color_round_v141_tests {
    use super::{heic_color_tag, jxl_tag_from, shot_source_gamut, ColorTag, Shot, SrcKind};
    use falcon_color::Gamut;
    use std::path::{Path, PathBuf};

    // ───────────────────────────── fixtures ─────────────────────────────

    /// Re-describe a serialized profile: append a fresh ICC v2 `desc` (textDescriptionType) block
    /// and re-point the `desc` tag-table entry at it. An ICC's tag DATA need not be ordered or
    /// contiguous — the tag TABLE locates every block — so the colorimetry tags stay byte-identical
    /// while the name becomes anything, at any length.
    fn icc_named(g: Gamut, name: &str) -> Vec<u8> {
        let mut icc = falcon_color::icc_bytes_for_gamut(g).expect("a modeled gamut serializes");
        let e = desc_entry(&icc);
        while icc.len() % 4 != 0 {
            icc.push(0);
        }
        let off = icc.len();
        let mut tag = b"desc".to_vec();
        tag.extend_from_slice(&[0u8; 4]);
        tag.extend_from_slice(&((name.len() + 1) as u32).to_be_bytes()); // ASCII count incl. NUL
        tag.extend_from_slice(name.as_bytes());
        tag.push(0);
        icc.extend_from_slice(&tag);
        icc[e + 4..e + 8].copy_from_slice(&(off as u32).to_be_bytes());
        icc[e + 8..e + 12].copy_from_slice(&(tag.len() as u32).to_be_bytes());
        let size = icc.len() as u32;
        icc[0..4].copy_from_slice(&size.to_be_bytes());
        icc
    }

    /// The same profile with NO readable name at all — its `desc` tag signature clobbered, every
    /// colorimetry byte untouched. This is the HEIF `prof` shape R2 is about: a profile that
    /// answers the gamut question perfectly and cannot answer the name question.
    fn icc_descless(g: Gamut) -> Vec<u8> {
        let mut icc = falcon_color::icc_bytes_for_gamut(g).expect("a modeled gamut serializes");
        let e = desc_entry(&icc);
        icc[e..e + 4].copy_from_slice(b"XXXX");
        icc
    }

    /// The tag-table offset of the `desc` entry.
    fn desc_entry(icc: &[u8]) -> usize {
        let n = u32::from_be_bytes([icc[128], icc[129], icc[130], icc[131]]) as usize;
        (0..n)
            .map(|k| 132 + k * 12)
            .find(|&e| &icc[e..e + 4] == b"desc")
            .expect("icc_bytes_for_gamut writes a desc tag")
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("falcon_v141_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn shot_at(path: &Path, kind: SrcKind) -> Shot {
        Shot {
            id: 0,
            name: "fixture".into(),
            has_raw: false,
            has_jpg: true,
            raw: None,
            jpg: Some(path.to_path_buf()),
            kind,
            cloud_placeholder: false,
            sniffed: None,
        }
    }

    /// The gamut a file of `kind` at `path` resolves to, through the real door.
    fn door(path: &Path, kind: SrcKind) -> Gamut {
        shot_source_gamut(&shot_at(path, kind))
    }

    // ───────────────────────── R4 — the four unpinned doors ─────────────────────────

    /// R4 — JPEG APP2, SPLIT ACROSS TWO SEGMENTS AND OUT OF ORDER. An ICC over ~64 KB must be split
    /// across APP2 segments, and the door's job is to sort them by sequence number and concatenate.
    /// The halves are written seq 2 THEN seq 1 so a door that simply appends in file order produces
    /// a scrambled profile that will not parse, and answers sRGB.
    #[test]
    fn jpeg_app2_threads_a_two_segment_profile_to_the_resolver() {
        let icc = icc_named(Gamut::DisplayP3, "Display");
        let (a, b) = icc.split_at(icc.len() / 2);
        let seg = |seq: u8, part: &[u8]| {
            let mut data = b"ICC_PROFILE\0".to_vec();
            data.push(seq);
            data.push(2); // chunk count
            data.extend_from_slice(part);
            let mut out = vec![0xFF, 0xE2];
            out.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(&data);
            out
        };
        let mut jpg = vec![0xFFu8, 0xD8];
        jpg.extend_from_slice(&seg(2, b)); // deliberately out of order
        jpg.extend_from_slice(&seg(1, a));
        jpg.extend_from_slice(&[0xFF, 0xDA]); // SOS — the door stops here
        let dir = scratch("jpeg");
        let path = dir.join("two_segment.jpg");
        std::fs::write(&path, &jpg).expect("write");
        let got = door(&path, SrcKind::Jpeg);
        eprintln!("R4 JPEG APP2 (2 segments, reversed) → {got:?}");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, Gamut::DisplayP3, "the JPEG door must reassemble and thread its APP2 bytes");
    }

    /// R4 — TIFF tag 34675. A minimal little-endian TIFF: header, one-entry IFD naming the ICC, and
    /// the profile itself. No pixel data — the door is a bounded IFD walk and never decodes.
    #[test]
    fn tiff_34675_threads_its_profile_to_the_resolver() {
        let icc = icc_named(Gamut::DisplayP3, "Display");
        let mut tif = b"II\x2a\x00".to_vec();
        tif.extend_from_slice(&8u32.to_le_bytes()); // first IFD at offset 8
        tif.extend_from_slice(&1u16.to_le_bytes()); // one entry
        tif.extend_from_slice(&34675u16.to_le_bytes()); // ICCProfile
        tif.extend_from_slice(&7u16.to_le_bytes()); // type UNDEFINED
        tif.extend_from_slice(&(icc.len() as u32).to_le_bytes());
        tif.extend_from_slice(&26u32.to_le_bytes()); // data offset: 8 + 2 + 12 + 4
        tif.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        assert_eq!(tif.len(), 26, "the ICC must start exactly where the entry points");
        tif.extend_from_slice(&icc);
        let dir = scratch("tiff");
        let path = dir.join("tagged.tif");
        std::fs::write(&path, &tif).expect("write");
        let got = door(&path, SrcKind::Tiff);
        eprintln!("R4 TIFF 34675 → {got:?}");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, Gamut::DisplayP3, "the TIFF door must thread tag 34675's bytes");
    }

    /// R4 — WebP `ICCP`, through the real `image-webp` container (a lossless VP8L encode, so no
    /// fixture binary and no external encoder).
    #[test]
    fn webp_iccp_threads_its_profile_to_the_resolver() {
        use image_webp::{ColorType, WebPEncoder};
        let icc = icc_named(Gamut::DisplayP3, "Display");
        let mut out = Vec::new();
        let mut enc = WebPEncoder::new(&mut out);
        enc.set_icc_profile(icc);
        enc.encode(&[200u8, 90, 40, 10, 20, 30], 2, 1, ColorType::Rgb8).expect("webp encode");
        let dir = scratch("webp");
        let path = dir.join("tagged.webp");
        std::fs::write(&path, &out).expect("write");
        let got = door(&path, SrcKind::Webp);
        eprintln!("R4 WebP ICCP → {got:?}");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, Gamut::DisplayP3, "the WebP door must thread its ICCP bytes");
    }

    /// R4 — the JXL door's PRECEDENCE, as far as this tree can reach it.
    ///
    /// STATED LIMIT (reported, not faked): `jxl-oxide` is decode-only and there is no JXL encoder in
    /// the tree or the dependency graph, so no fixture can carry an original ICC — a JXL's embedded
    /// profile is brotli-compressed under the format's own ICC prediction encoding, and hand-rolling
    /// one to prove a single `image.original_icc()` hop would be a fixture larger than the door. So
    /// the row drives [`jxl_tag_from`], which is that door minus the one call: it pins that the
    /// BYTES survive, that a profile-supplied name wins over the CICP code, and — the R7 half — that
    /// a CICP name standing in for an unreadable `desc` is never attributed to the profile.
    #[test]
    fn jxl_door_threads_bytes_and_never_calls_a_cicp_name_the_profiles() {
        let named = icc_named(Gamut::DisplayP3, "Display");
        // (1) a profile with a name of its own: bytes + that name, and the CICP code is ignored.
        let t = jxl_tag_from(Some(&named), Some("sRGB".into()));
        assert_eq!(t.desc.as_deref(), Some("Display"), "the profile's own name wins over CICP");
        assert!(t.desc_from_profile, "…and it IS the profile's");
        assert_eq!(falcon_color::resolve_source_gamut(t.icc.as_deref(), t.desc.as_deref()).gamut, Gamut::DisplayP3);
        // (2) a profile whose name will not read: the CICP code supplies a name, the BYTES still
        //     answer the gamut, and the borrowed name is NOT the profile's to quote.
        let t = jxl_tag_from(Some(&icc_descless(Gamut::DisplayP3)), Some("sRGB".into()));
        assert_eq!(t.desc.as_deref(), Some("sRGB"), "the CICP code names it");
        assert!(!t.desc_from_profile, "…but that name is the container's, not the profile's");
        assert!(t.icc.is_some(), "the bytes must survive the name fallback");
        let r = falcon_color::resolve_source_gamut(t.icc.as_deref(), t.desc.as_deref());
        assert_eq!(r.gamut, Gamut::DisplayP3, "the colorants answer, not the borrowed name");
        assert_eq!(r.route, falcon_color::GamutRoute::Colorimetry);
        // (3) no profile at all — the CICP name is the whole answer, exactly as before.
        let t = jxl_tag_from(None, Some("Rec. 2020".into()));
        assert_eq!((t.icc.as_ref(), t.desc.as_deref()), (None, Some("Rec. 2020")));
        assert_eq!(jxl_tag_from(None, None), ColorTag::default());
    }

    // ───────────────── R2 — the strictly-additive HEIF widening, case table ─────────────────

    /// A HEIF-shaped byte sequence carrying `colr` boxes in the given order — all `heic_color_tag`'s
    /// byte SCAN needs (it finds the `colr` fourcc and reads the box size from the 4 bytes before
    /// it, which is exactly what an ISO-BMFF box header puts there).
    fn heif_with_colr(bodies: &[Vec<u8>]) -> Vec<u8> {
        let bx = |t: &[u8; 4], body: &[u8]| {
            let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(t);
            v.extend_from_slice(body);
            v
        };
        let mut out = bx(b"ftyp", b"heic\0\0\0\0heic");
        for b in bodies {
            out.extend_from_slice(&bx(b"colr", b));
        }
        out
    }

    /// An `nclx` colr body naming its primaries by enumerated code.
    fn nclx(primaries: u16) -> Vec<u8> {
        let mut b = b"nclx".to_vec();
        b.extend_from_slice(&primaries.to_be_bytes());
        b.extend_from_slice(&13u16.to_be_bytes()); // transfer
        b.extend_from_slice(&6u16.to_be_bytes()); // matrix
        b.push(0x80);
        b
    }

    /// A `prof` colr body carrying an ICC profile.
    fn prof(icc: &[u8]) -> Vec<u8> {
        let mut b = b"prof".to_vec();
        b.extend_from_slice(icc);
        b
    }

    /// R2 — THE CASE TABLE. The widening must be STRICTLY ADDITIVE: every container that answered
    /// before answers identically, and only a container where NOTHING answered may gain an answer.
    ///
    /// FALSIFIERS, both measured: with the second pass removed, EXACTLY TWO rows move — row 3 falls
    /// back to sRGB (the round's own defect, alive in this door) and row 9 with it (both are
    /// all-descless-`prof` containers, so only pass 2 can reach them) — and the other eight are
    /// byte-identical, which is what "strictly additive" means and is why the table is printed whole
    /// before it is judged. (The v0.8.141 commit message says "one row / nine stay" — miscounted;
    /// the narrow verify re-derived it: two move, eight stay, a STRONGER additivity proof.) With the pass-2 predicate loosened from "places within τ" to
    /// "colorants parse", row 9 answers sRGB: the garbled profile in front takes the answer and the
    /// good one behind it never gets asked. A 20,000-trial fuzz measured that 40-77% of profiles
    /// corrupted past their `desc` still yield parseable colorants, while ZERO of 40,000 landed
    /// within τ — which is exactly why the tolerance, not the parse, is the predicate.
    #[test]
    fn the_heif_widening_is_strictly_additive() {
        let p3_named = prof(&icc_named(Gamut::DisplayP3, "Display P3"));
        let p3_descless = prof(&icc_descless(Gamut::DisplayP3));
        // A profile that PARSES and MISSES τ: true cinema DCI-P3 sits 0.08 from the nearest thing we
        // model, so it is measurable-but-unplaceable — the deterministic stand-in for the fuzz's
        // corrupted profiles, and the row that proves "parses" is not the predicate.
        let unplaceable_descless = prof(&icc_descless(Gamut::DciP3));
        let rows: &[(&str, Vec<Vec<u8>>, Gamut)] = &[
            // ── UNCHANGED: containers today's rule already answered ──
            ("1 nclx sRGB alone", vec![nclx(1)], Gamut::Srgb),
            ("2 named prof alone", vec![p3_named.clone()], Gamut::DisplayP3),
            // ── THE WIDENING: nothing answered before; the colorants place, so they may now ──
            ("3 descless prof alone", vec![p3_descless.clone()], Gamut::DisplayP3),
            // ── THE SAFETY ROWS: an nclx is never shadowed, in either order, and an nclx whose
            //    primaries CODE we do not model still IS the container's answer — the deliberate
            //    pre-v0.8.140 rule ("the container said something we don't model" → sRGB), which
            //    the widening must not quietly overturn by preferring the profile behind it.
            ("4 descless prof, then unknown nclx", vec![p3_descless.clone(), nclx(2)], Gamut::Srgb),
            ("5 nclx sRGB, then descless prof", vec![nclx(1), p3_descless.clone()], Gamut::Srgb),
            ("6 descless prof, then nclx sRGB", vec![p3_descless.clone(), nclx(1)], Gamut::Srgb),
            // ── THE PREDICATE: measurable is not placeable ──
            ("7 descless UNPLACEABLE prof", vec![unplaceable_descless.clone()], Gamut::Srgb),
            ("8 descless unplaceable, then nclx 2020", vec![unplaceable_descless.clone(), nclx(9)], Gamut::Rec2020),
            // Row 9 is what makes the predicate load-bearing rather than decorative: two descless
            // `prof` boxes, the unplaceable one FIRST. Pass 2 must skip it and answer from the one
            // that really places — `colorants.is_some()` would stop at the first and land on sRGB.
            ("9 descless unplaceable, then descless P3", vec![unplaceable_descless, p3_descless.clone()], Gamut::DisplayP3),
            ("10 no colr at all", vec![], Gamut::Srgb),
        ];
        let dir = scratch("heif");
        // Every row is measured and PRINTED before anything is asserted, so an inversion of the
        // widening shows the whole table (which rows moved and which did not) instead of stopping
        // at the first one — "strictly additive" is a claim about all nine rows at once.
        let mut wrong: Vec<String> = Vec::new();
        for (name, bodies, want) in rows {
            let path = dir.join(format!("{}.heic", name.replace(' ', "_")));
            std::fs::write(&path, heif_with_colr(bodies)).expect("write");
            let tag = heic_color_tag(&path);
            let got = door(&path, SrcKind::Heic);
            eprintln!(
                "R2 {name:<38} → {got:?}{}  (tag: desc {:?}, {} bytes of profile)",
                if got == *want { "" } else { " ← WRONG" },
                tag.desc,
                tag.icc.as_ref().map(|v| v.len()).unwrap_or(0)
            );
            if got != *want {
                wrong.push(format!("{name:?}: expected {want:?}, got {got:?}"));
            }
        }
        assert!(wrong.is_empty(), "R2 case table: {}", wrong.join(" | "));
        // Row 3's answer must be COLORIMETRY, not a name that happened to agree — otherwise the row
        // would pass for the wrong reason.
        let path = dir.join("3_descless_prof_alone.heic");
        let tag = heic_color_tag(&path);
        assert!(tag.desc.is_none(), "the answering box carries NO name — that is the whole point");
        let r = falcon_color::resolve_source_gamut(tag.icc.as_deref(), tag.desc.as_deref());
        assert_eq!(r.route, falcon_color::GamutRoute::Colorimetry, "…so only the bytes can have answered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R2 — the same widening at the second site: the HEIC PREVIEW item's `colr` property filter.
    /// The container is the `heic_preview_colr` walk's real shape (`pitm` → `iref thmb` → `ipma` →
    /// `ipco`), with the MASTER declaring sRGB by `nclx` so the preview's answer is distinguishable
    /// from a fall-back to the master.
    #[test]
    fn the_heif_preview_filter_widens_the_same_way() {
        use super::{frame_source_gamut, FrameSource};
        let bx = |t: &[u8; 4], body: &[u8]| -> Vec<u8> {
            let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(t);
            v.extend_from_slice(body);
            v
        };
        let full = |t: &[u8; 4], ver: u8, flags: u32, body: &[u8]| -> Vec<u8> {
            let mut b = vec![ver, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
            b.extend_from_slice(body);
            bx(t, &b)
        };
        let build = |preview_body: Vec<u8>| -> Vec<u8> {
            let ipco = bx(b"ipco", &[bx(b"colr", &nclx(1)), bx(b"colr", &preview_body)].concat());
            let ipma = full(b"ipma", 0, 0, &[0, 0, 0, 2, 0, 1, 1, 1, 0, 2, 1, 2]);
            let iprp = bx(b"iprp", &[ipco, ipma].concat());
            let pitm = full(b"pitm", 0, 0, &[0, 1]);
            let thmb = bx(b"thmb", &[2u16.to_be_bytes(), 1u16.to_be_bytes(), 1u16.to_be_bytes()].concat());
            let meta = full(b"meta", 0, 0, &[pitm, full(b"iref", 0, 0, &thmb), iprp].concat());
            let mut bytes = bx(b"ftyp", b"heic\0\0\0\0heic");
            bytes.extend_from_slice(&meta);
            bytes
        };
        let dir = scratch("heif_preview");
        for (name, body, want) in [
            // The widening: a descless `prof` whose colorants place is now the preview's answer.
            ("descless prof", prof(&icc_descless(Gamut::DisplayP3)), Gamut::DisplayP3),
            // Unchanged: a named `prof` answered before and answers identically.
            ("named prof", prof(&icc_named(Gamut::AdobeRgb, "Adobe RGB (1998)")), Gamut::AdobeRgb),
            // Unchanged: a property that places NOWHERE is still not an answer — the master stands.
            ("descless unplaceable prof", prof(&icc_descless(Gamut::DciP3)), Gamut::Srgb),
        ] {
            let path = dir.join(format!("{}.heic", name.replace(' ', "_")));
            std::fs::write(&path, build(body)).expect("write");
            let shot = shot_at(&path, SrcKind::Heic);
            let master = frame_source_gamut(&shot, FrameSource::MainImage);
            let preview = frame_source_gamut(&shot, FrameSource::EmbeddedPreview);
            eprintln!("R2 preview {name:<28} → master {master:?}, preview {preview:?}");
            assert_eq!(master, Gamut::Srgb, "{name}: the master declares sRGB by nclx — the control");
            assert_eq!(preview, want, "{name}: the preview door");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **v0.8.177 (G-Y1) — THE SECOND DESC PARSER LEARNS THE TAG BOUND.** `falcon-decode`'s own
    /// [`super::icc_description`] bounded both of its reads by the WHOLE PROFILE BUFFER, so a `desc`
    /// tag declaring 5000 bytes of text inside a 40-byte tag walked straight past its own end and
    /// returned whatever followed it as the profile's name. That was already a lie on the panel; since
    /// v0.8.177 it is worse, because this string feeds route (3) — `Gamut::from_description` — so
    /// bytes from a NEIGHBOURING tag could CHOOSE the gamut every pixel is then converted from.
    ///
    /// RED-FIRST, both arms MEASURED against the pre-fix parser: the v2 row returned its 28 in-tag
    /// bytes followed by 5000 bytes of the neighbour ("…xxxxDisplay P3 Display P3 …"), and the v4
    /// row returned a 2500-character name on which `Gamut::from_description` answered
    /// `Some(DisplayP3)` — the fabricated gamut, from bytes the tag does not own. The fix is
    /// `falcon-color`'s shape: bound every read by the tag's DECLARED extent, proved once to lie
    /// inside the buffer.
    #[test]
    fn a_desc_tag_cannot_name_a_profile_with_its_neighbours_bytes() {
        /// Re-point the `desc` tag-table entry at `body`, DECLARING `declared` bytes for it, then
        /// append `forged` immediately after — inside the buffer, outside the tag. Exactly the shape
        /// a whole-buffer bound cannot tell from honest text.
        fn hostile(body: &[u8], declared: usize, forged: &[u8]) -> Vec<u8> {
            let mut icc = falcon_color::icc_bytes_for_gamut(Gamut::Srgb).expect("sRGB serializes");
            let e = desc_entry(&icc);
            while icc.len() % 4 != 0 {
                icc.push(0);
            }
            let off = icc.len();
            icc.extend_from_slice(body);
            icc.extend_from_slice(forged);
            icc[e + 4..e + 8].copy_from_slice(&(off as u32).to_be_bytes());
            icc[e + 8..e + 12].copy_from_slice(&(declared as u32).to_be_bytes());
            let size = icc.len() as u32;
            icc[0..4].copy_from_slice(&size.to_be_bytes());
            icc
        }

        // ── row 1: the ICC v2 `desc` (textDescriptionType, ASCII) ──
        // 28 bytes of in-tag text (a 40-byte tag, 12 of header), deliberately WITHOUT a NUL, so only
        // the bound can stop the read. The forged trailer names a gamut, which is the teeth.
        const IN_TAG_V2: &str = "in-tag name xxxxxxxxxxxxxxxx";
        assert_eq!(IN_TAG_V2.len(), 28, "the in-tag text must exactly fill the declared tag");
        let mut body = b"desc".to_vec();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&5000u32.to_be_bytes()); // the lie: 5000 ASCII bytes in a 40-byte tag
        body.extend_from_slice(IN_TAG_V2.as_bytes());
        assert_eq!(body.len(), 40);
        let forged: Vec<u8> = b"Display P3 ".iter().copied().cycle().take(5000).collect();
        let v2 = hostile(&body, 40, &forged);
        let got = super::icc_description(&v2).expect("the tag's OWN bytes still name it");
        eprintln!("G-Y1 v2 desc (count 5000 in a 40-byte tag) → {got:?}");
        assert_eq!(got, IN_TAG_V2, "the read must stop at the tag's declared end");
        assert!(!got.contains("Display P3"), "a neighbour's bytes are never this profile's name");
        assert_eq!(
            falcon_color::Gamut::from_description(&got),
            None,
            "…and therefore cannot CHOOSE a gamut the file was never encoded in"
        );

        // ── row 2: the ICC v4 `mluc` (multiLocalizedUnicodeType, UTF-16BE) ──
        // Header 16 + one 12-byte record + 22 bytes of string = a 50-byte tag; the record's own
        // length field claims 5000.
        const IN_TAG_V4: &str = "In-Tag Name";
        let mut body = b"mluc".to_vec();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&1u32.to_be_bytes()); // record count
        body.extend_from_slice(&12u32.to_be_bytes()); // record size
        body.extend_from_slice(b"enUS");
        body.extend_from_slice(&5000u32.to_be_bytes()); // the lie: 5000 bytes of UTF-16
        body.extend_from_slice(&28u32.to_be_bytes()); // string offset, relative to the tag start
        for u in IN_TAG_V4.encode_utf16() {
            body.extend_from_slice(&u.to_be_bytes());
        }
        assert_eq!(body.len(), 50, "16 header + 12 record + 22 string");
        let forged: Vec<u8> = "Display P3 ".encode_utf16().flat_map(u16::to_be_bytes).cycle().take(5000).collect();
        let v4 = hostile(&body, 50, &forged);
        let got = super::icc_description(&v4).expect("the tag's OWN bytes still name it");
        eprintln!("G-Y1 v4 mluc (len 5000 in a 50-byte tag) → {got:?}");
        assert_eq!(got, IN_TAG_V4, "the read must stop at the tag's declared end");
        assert!(!got.contains("Display P3"), "a neighbour's bytes are never this profile's name");
        assert_eq!(falcon_color::Gamut::from_description(&got), None);

        // ── row 3: a `desc` tag whose DECLARED extent cannot hold even its own header is a refusal,
        // never a read of what follows it.
        let short = hostile(b"desc\0\0\0\0", 8, &forged);
        assert_eq!(super::icc_description(&short), None, "an 8-byte desc tag names nothing");

        // ── row 4 (v0.8.181): …AND THE OTHER END OF THE SAME BOUND — a `desc` tag whose OFFSET
        // points into the 128-byte HEADER. `falcon-color`'s `icc_find_tag` has carried an
        // `off >= 128` floor since it was written; this loop had only the extent check, so a tag
        // aimed backwards at header bytes was read as text. The teeth are route (3) again: the
        // string feeds `Gamut::from_description`, so header bytes an attacker chose could pick the
        // gamut every pixel is then converted from.
        //
        // RED-FIRST (measured against the pre-fix parser): `Some("Display P3")`.
        let mut into_header = falcon_color::icc_bytes_for_gamut(Gamut::Srgb).expect("sRGB serializes");
        let e = desc_entry(&into_header);
        let mut forged_header = b"desc".to_vec();
        forged_header.extend_from_slice(&[0u8; 4]);
        forged_header.extend_from_slice(&11u32.to_be_bytes()); // ASCII count
        forged_header.extend_from_slice(b"Display P3\0");
        assert_eq!(forged_header.len(), 23, "12 bytes of textDescriptionType header + 11 of ASCII");
        into_header[12..12 + forged_header.len()].copy_from_slice(&forged_header);
        into_header[e + 4..e + 8].copy_from_slice(&12u32.to_be_bytes()); // offset: INSIDE the header
        into_header[e + 8..e + 12].copy_from_slice(&(forged_header.len() as u32).to_be_bytes());
        assert_eq!(
            super::icc_description(&into_header),
            None,
            "a desc tag pointing into the 128-byte header names nothing — the floor falcon-color \
             has always had"
        );
    }

}

#[cfg(test)]
mod bmp_tests {
    use super::{bmp_dimensions, decode_bmp_rgb, scan_folder, Keep, Pixels, SrcKind};
    use image::codecs::bmp::BmpEncoder;
    use image::ExtendedColorType;
    use std::path::Path;

    fn write_bmp(path: &Path, data: &[u8], w: u32, h: u32, color: ExtendedColorType) {
        let mut out = Vec::new();
        BmpEncoder::new(&mut out).encode(data, w, h, color).expect("bmp encode");
        std::fs::write(path, &out).expect("write bmp");
    }

    #[test]
    fn bmp_roundtrip_rgb() {
        let dir = std::env::temp_dir().join("falcon_bmp_rgb_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grad.bmp");
        let (w, h) = (4u32, 2u32);
        let mut data = Vec::with_capacity((w * h * 3) as usize);
        for i in 0..(w * h) {
            data.extend_from_slice(&[(i * 10) as u8, (i * 20) as u8, (i * 5) as u8]);
        }
        write_bmp(&path, &data, w, h, ExtendedColorType::Rgb8);
        assert_eq!(bmp_dimensions(&path), Some((w, h)));
        let (px, dw, dh) = decode_bmp_rgb(&path, Keep::NONE).expect("decode bmp");
        let rgb = px.into_rgb8().expect("Keep::NONE decodes to RGB8");
        assert_eq!((dw, dh), (w, h));
        assert_eq!(rgb, data, "24-bit BMP round-trips exactly");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bmp_alpha_flattened_over_white() {
        let dir = std::env::temp_dir().join("falcon_bmp_alpha_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("alpha.bmp");
        // 2x1 RGBA: opaque red, then a fully-transparent pixel (any RGB).
        let data = [255u8, 0, 0, 255, 10, 20, 30, 0];
        write_bmp(&path, &data, 2, 1, ExtendedColorType::Rgba8);
        let (px, w, h) = decode_bmp_rgb(&path, Keep::NONE).expect("decode bmp alpha");
        let rgb = px.into_rgb8().expect("Keep::NONE decodes to RGB8");
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgb.len(), 6, "flattened to packed RGB8");
        assert_eq!(&rgb[0..3], &[255, 0, 0], "opaque pixel unchanged");
        assert_eq!(&rgb[3..6], &[255, 255, 255], "transparent pixel flattened over white");
        // ROUND 35: the 32-bpp BMP's alpha under the ./export PNG stop's request.
        let (kept, _, _) = decode_bmp_rgb(&path, Keep::ALL).expect("decode bmp alpha, kept");
        assert_eq!(
            kept,
            Pixels::Rgba8(vec![255, 0, 0, 255, 10, 20, 30, 0]),
            "Keep::ALL hands back the file's own RGBA"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_classifies_bmp() {
        let dir = std::env::temp_dir().join("falcon_bmp_scan_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("shot.bmp"), b"").unwrap();
        let shots = scan_folder(&dir).unwrap();
        let lone = shots.iter().find(|s| s.name == "shot").expect("lone bmp shot");
        assert_eq!(lone.kind, SrcKind::Bmp);
        assert!(!lone.is_unsupported());
        assert_eq!(lone.finished_format().as_deref(), Some("BMP"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod gif_tests {
    use super::{
        clamp_gif_delay, decode_gif_animation, decode_gif_first_frame_rgb, gif_is_animated,
        precompose_fits, scan_folder, GifPlayback, GifStream, Keep, Pixels, SrcKind,
        GIF_PRECOMPOSE_CAP_BYTES,
    };
    use gif::{DisposalMethod, Encoder, Frame, Repeat};
    use std::borrow::Cow;
    use std::path::Path;

    // Global palette: 0=red, 1=green, 2=blue, 3=black (used as the transparent slot).
    const PALETTE: &[u8] = &[255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0];
    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    #[allow(clippy::too_many_arguments)]
    fn idx_frame(
        left: u16,
        top: u16,
        w: u16,
        h: u16,
        indices: Vec<u8>,
        dispose: DisposalMethod,
        delay: u16,
        transparent: Option<u8>,
    ) -> Frame<'static> {
        let mut f = Frame::default();
        f.left = left;
        f.top = top;
        f.width = w;
        f.height = h;
        f.dispose = dispose;
        f.delay = delay;
        f.transparent = transparent;
        f.buffer = Cow::Owned(indices);
        f
    }

    fn write_gif(path: &Path, w: u16, h: u16, frames: &[Frame<'static>]) {
        let file = std::fs::File::create(path).unwrap();
        let mut enc = Encoder::new(std::io::BufWriter::new(file), w, h, PALETTE).unwrap();
        enc.set_repeat(Repeat::Infinite).unwrap();
        for f in frames {
            enc.write_frame(f).unwrap();
        }
        // enc drops here → trailer written.
    }

    fn px(rgba: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let o = ((y * w + x) * 4) as usize;
        [rgba[o], rgba[o + 1], rgba[o + 2], rgba[o + 3]]
    }

    fn frames_of(path: &Path) -> super::AnimFrames {
        match decode_gif_animation(path).expect("gif animation") {
            GifPlayback::InMemory(a) => a,
            GifPlayback::Streamed { .. } => panic!("small test GIF should precompose"),
        }
    }

    /// v0.8.187 (X1) — **THE ROTATION REFUSAL'S PREDICATE, ON REAL BYTES.**
    ///
    /// Three rows, because the viewer acts differently on each: a STILL GIF must answer `false` (it
    /// rotates correctly today and its menu rows stay live — ledger L42, the name is not the bytes),
    /// an ANIMATED one must answer `true` (the refusal), and a TRUNCATED/garbage file must answer
    /// `Err` so the caller refuses conservatively instead of guessing "still" and recording a
    /// rotation the viewer will never show.
    ///
    /// The BOUND is the fourth row: a 40-frame clip and a 2-frame clip must both answer from the
    /// second frame, because this probe runs synchronously on the UI thread under a keypress.
    ///
    /// FALSIFIER (L28): drop the `n >= 2` early return and the bound row still passes but the probe
    /// becomes O(frames) — so the row asserts the ANSWER, and the early return is what the comment
    /// and the `gif_frame_count` contrast document. Remove `skip_frame_decoding(true)` and every row
    /// still passes while the probe starts allocating pixel buffers: that is a REVIEW obligation,
    /// stated here rather than pinned with a green assert that cannot see it.
    #[test]
    fn gif_is_animated_answers_still_animated_and_unreadable() {
        let dir = std::env::temp_dir().join(format!("falcon_gif_anim_probe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // (1) ONE frame — a still GIF. The viewer rotates these and must keep rotating them.
        let still = dir.join("still.gif");
        write_gif(&still, 2, 2, &[idx_frame(0, 0, 2, 2, vec![0, 0, 0, 0], DisposalMethod::Keep, 5, None)]);
        assert_eq!(gif_is_animated(&still).unwrap(), false, "a single-frame GIF is NOT animated");

        // (2) TWO frames — animated. This is the refusal's whole subject.
        let anim = dir.join("anim.gif");
        write_gif(
            &anim,
            2,
            2,
            &[
                idx_frame(0, 0, 2, 2, vec![0, 0, 0, 0], DisposalMethod::Keep, 5, None),
                idx_frame(0, 0, 2, 2, vec![1, 1, 1, 1], DisposalMethod::Keep, 5, None),
            ],
        );
        assert_eq!(gif_is_animated(&anim).unwrap(), true, "two frames IS animated");

        // (3) …and the bound: a long clip answers from the SECOND frame, not the fortieth.
        let long = dir.join("long.gif");
        let many: Vec<_> = (0..40)
            .map(|i| idx_frame(0, 0, 2, 2, vec![i % 2, 0, 0, 0], DisposalMethod::Keep, 5, None))
            .collect();
        write_gif(&long, 2, 2, &many);
        assert_eq!(gif_is_animated(&long).unwrap(), true, "a 40-frame clip answers the same question");

        // (4) TRUNCATED — the header parses but the block chain does not, or nothing parses at all.
        //     Either way the answer is Err, and the caller must refuse rather than assume.
        let bad = dir.join("bad.gif");
        std::fs::write(&bad, b"GIF89a\x02\x00\x02\x00\x00\x00\x00").unwrap();
        assert!(gif_is_animated(&bad).is_err(), "a truncated GIF cannot be answered, and says so");
        let junk = dir.join("junk.gif");
        std::fs::write(&junk, b"not a gif at all").unwrap();
        assert!(gif_is_animated(&junk).is_err(), "…nor can a file that is not a GIF");
        assert!(gif_is_animated(&dir.join("nope.gif")).is_err(), "…nor one that is not there");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_classifies_gif_and_first_frame() {
        let dir = std::env::temp_dir().join("falcon_gif_scan_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.gif");
        // 2x2 all-red single frame.
        write_gif(&path, 2, 2, &[idx_frame(0, 0, 2, 2, vec![0, 0, 0, 0], DisposalMethod::Keep, 5, None)]);
        let shots = scan_folder(&dir).unwrap();
        let lone = shots.iter().find(|s| s.name == "shot").expect("lone gif shot");
        assert_eq!(lone.kind, SrcKind::Gif);
        assert!(!lone.is_unsupported());
        assert_eq!(lone.finished_format().as_deref(), Some("GIF"));
        // First-frame still (the thumb/fast/scan/ROI consumers) is composited RGB8.
        let (px, w, h) = decode_gif_first_frame_rgb(&path, Keep::NONE).expect("first frame");
        let rgb = px.into_rgb8().expect("Keep::NONE decodes to RGB8");
        assert_eq!((w, h), (2, 2));
        assert_eq!(rgb.len(), (w * h * 3) as usize);
        assert_eq!(&rgb[0..3], &[255, 0, 0], "first frame is red");
        // ROUND 35: the same frame under the ./export PNG stop's request is the compositor's own
        // canvas, whose alpha is binary (0 or 255) because GIF transparency is a palette index.
        let (kept, _, _) =
            decode_gif_first_frame_rgb(&path, Keep::ALL).expect("first frame, kept");
        assert!(matches!(kept, Pixels::Rgba8(_)), "Keep::ALL keeps the canvas's own alpha");
        assert_eq!(kept.byte_len(), (w * h * 4) as usize);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disposal_background_clears_previous_rect() {
        let dir = std::env::temp_dir().join("falcon_gif_bg_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bg.gif");
        write_gif(
            &path,
            2,
            2,
            &[
                // Frame 0: full 2x2 red, dispose = Background (clear the whole canvas after showing).
                idx_frame(0, 0, 2, 2, vec![0, 0, 0, 0], DisposalMethod::Background, 5, None),
                // Frame 1: 1x1 green at (0,0), Keep.
                idx_frame(0, 0, 1, 1, vec![1], DisposalMethod::Keep, 5, None),
            ],
        );
        let a = frames_of(&path);
        assert_eq!(a.frames.len(), 2);
        assert_eq!(px(&a.frames[0].rgba, 2, 0, 0), RED, "frame 0 all red");
        // Background disposal of frame 0 cleared the canvas → frame 1 shows green only at (0,0), the rest
        // reverts to (transparent→) white, NOT the previous red.
        assert_eq!(px(&a.frames[1].rgba, 2, 0, 0), GREEN, "frame 1 (0,0) green");
        assert_eq!(px(&a.frames[1].rgba, 2, 1, 0), WHITE, "cleared → white, not red");
        assert_eq!(px(&a.frames[1].rgba, 2, 1, 1), WHITE, "cleared → white, not red");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v0.8.56 (audit YELLOW 1): the Background-disposal test above uses a FULL-CANVAS frame 0, so a
    /// bug that cleared the WHOLE canvas (instead of only the disposed frame's rect) would still pass
    /// it. Pin the sub-rect semantics: an established base, then a SMALL Background-disposal frame —
    /// only that frame's rect clears; the surrounding base persists.
    #[test]
    fn disposal_background_clears_only_its_rect() {
        let dir = std::env::temp_dir().join("falcon_gif_bgrect_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bgrect.gif");
        write_gif(
            &path,
            2,
            2,
            &[
                // f0: full 2x2 red base, Keep (persists under everything).
                idx_frame(0, 0, 2, 2, vec![0, 0, 0, 0], DisposalMethod::Keep, 5, None),
                // f1: 1x1 green at (1,0), dispose = Background (clear ONLY that pixel afterwards).
                idx_frame(1, 0, 1, 1, vec![1], DisposalMethod::Background, 5, None),
                // f2: 1x1 blue at (0,1), Keep.
                idx_frame(0, 1, 1, 1, vec![2], DisposalMethod::Keep, 5, None),
            ],
        );
        let a = frames_of(&path);
        assert_eq!(a.frames.len(), 3);
        // Frame 1 shows green at (1,0) over the red base.
        assert_eq!(px(&a.frames[1].rgba, 2, 1, 0), GREEN, "frame 1 (1,0) green");
        assert_eq!(px(&a.frames[1].rgba, 2, 0, 0), RED, "frame 1 base persists");
        // Frame 2: f1's 1x1 rect was cleared (transparent → white); EVERY other base pixel persists —
        // a whole-canvas clear would turn (0,0)/(1,1) white and fail here.
        assert_eq!(px(&a.frames[2].rgba, 2, 1, 0), WHITE, "only f1's rect cleared → white");
        assert_eq!(px(&a.frames[2].rgba, 2, 0, 1), BLUE, "frame 2 (0,1) blue");
        assert_eq!(px(&a.frames[2].rgba, 2, 0, 0), RED, "base survives OUTSIDE the disposed rect");
        assert_eq!(px(&a.frames[2].rgba, 2, 1, 1), RED, "base survives OUTSIDE the disposed rect");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disposal_keep_retains_previous() {
        let dir = std::env::temp_dir().join("falcon_gif_keep_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keep.gif");
        write_gif(
            &path,
            2,
            2,
            &[
                idx_frame(0, 0, 2, 2, vec![0, 0, 0, 0], DisposalMethod::Keep, 5, None),
                idx_frame(0, 0, 1, 1, vec![1], DisposalMethod::Keep, 5, None),
            ],
        );
        let a = frames_of(&path);
        // Keep disposal → frame 1 overlays green at (0,0), the rest STAYS red.
        assert_eq!(px(&a.frames[1].rgba, 2, 0, 0), GREEN, "frame 1 (0,0) green");
        assert_eq!(px(&a.frames[1].rgba, 2, 1, 0), RED, "kept red under");
        assert_eq!(px(&a.frames[1].rgba, 2, 1, 1), RED, "kept red under");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disposal_previous_restores_snapshot() {
        let dir = std::env::temp_dir().join("falcon_gif_prev_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prev.gif");
        write_gif(
            &path,
            2,
            2,
            &[
                // f0: full red, Keep.
                idx_frame(0, 0, 2, 2, vec![0, 0, 0, 0], DisposalMethod::Keep, 5, None),
                // f1: full green, Previous (after showing, restore canvas to pre-f1 = all red).
                idx_frame(0, 0, 2, 2, vec![1, 1, 1, 1], DisposalMethod::Previous, 5, None),
                // f2: 1x1 blue at (0,0), Keep.
                idx_frame(0, 0, 1, 1, vec![2], DisposalMethod::Keep, 5, None),
            ],
        );
        let a = frames_of(&path);
        assert_eq!(a.frames.len(), 3);
        assert_eq!(px(&a.frames[1].rgba, 2, 0, 0), GREEN, "frame 1 shows green");
        assert_eq!(px(&a.frames[1].rgba, 2, 1, 1), GREEN, "frame 1 shows green");
        // Previous disposal restored the pre-f1 canvas (all red) → f2 draws blue at (0,0), rest RED.
        assert_eq!(px(&a.frames[2].rgba, 2, 0, 0), BLUE, "frame 2 (0,0) blue");
        assert_eq!(px(&a.frames[2].rgba, 2, 1, 0), RED, "restored red under");
        assert_eq!(px(&a.frames[2].rgba, 2, 1, 1), RED, "restored red under");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transparency_composites_binary() {
        let dir = std::env::temp_dir().join("falcon_gif_trans_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trans.gif");
        write_gif(
            &path,
            2,
            2,
            &[
                idx_frame(0, 0, 2, 2, vec![0, 0, 0, 0], DisposalMethod::Keep, 5, None),
                // f1: 2x2, green at (0,0), transparent (idx 3) elsewhere → only (0,0) overwrites.
                idx_frame(0, 0, 2, 2, vec![1, 3, 3, 3], DisposalMethod::Keep, 5, Some(3)),
            ],
        );
        let a = frames_of(&path);
        assert_eq!(px(&a.frames[1].rgba, 2, 0, 0), GREEN, "opaque pixel drawn");
        assert_eq!(px(&a.frames[1].rgba, 2, 1, 0), RED, "transparent pixel kept red");
        assert_eq!(px(&a.frames[1].rgba, 2, 0, 1), RED, "transparent pixel kept red");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delay_clamp_pin() {
        // Direct pin of the browser-conventional clamp (independent of any encoder round-trip).
        assert_eq!(clamp_gif_delay(0), 100, "0 cs → 100 ms");
        assert_eq!(clamp_gif_delay(1), 100, "1 cs → 100 ms");
        assert_eq!(clamp_gif_delay(2), 20, "2 cs → 20 ms");
        assert_eq!(clamp_gif_delay(5), 50, "5 cs → 50 ms");
        assert_eq!(clamp_gif_delay(100), 1000, "100 cs → 1000 ms");
    }

    #[test]
    fn precompose_cap_decision() {
        let cap = GIF_PRECOMPOSE_CAP_BYTES;
        // A small clip fits (100×100×4×10 = 400 KB).
        assert!(precompose_fits(100, 100, 10, cap), "small clip precomposes");
        // Exactly at the cap fits; one frame more does not.
        let per = 100u64 * 100 * 4;
        let n_at_cap = (cap / per) as usize;
        assert!(precompose_fits(100, 100, n_at_cap, cap), "exactly at cap fits");
        assert!(!precompose_fits(100, 100, n_at_cap + 1, cap), "one frame over cap streams");
        // A big canvas × many frames overflows the cap → stream (and the math never panics).
        assert!(!precompose_fits(4000, 4000, 100, cap), "large animation streams");
        assert!(!precompose_fits(u32::MAX, u32::MAX, usize::MAX, cap), "overflow-safe → streams");
    }

    #[test]
    fn animation_precomposes_in_memory() {
        let dir = std::env::temp_dir().join("falcon_gif_inmem_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("anim.gif");
        write_gif(
            &path,
            2,
            2,
            &[
                idx_frame(0, 0, 2, 2, vec![0, 0, 0, 0], DisposalMethod::Keep, 3, None),
                idx_frame(0, 0, 2, 2, vec![1, 1, 1, 1], DisposalMethod::Keep, 0, None),
            ],
        );
        match decode_gif_animation(&path).expect("animation") {
            GifPlayback::InMemory(a) => {
                assert_eq!((a.width, a.height), (2, 2));
                assert_eq!(a.frames.len(), 2, "two frames precomposed");
            }
            GifPlayback::Streamed { .. } => panic!("small GIF should be InMemory"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stream_loops_forever() {
        let dir = std::env::temp_dir().join("falcon_gif_stream_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("loop.gif");
        // Two full-canvas opaque frames (red, green), both Keep, so each displayed frame is deterministic.
        write_gif(
            &path,
            2,
            2,
            &[
                idx_frame(0, 0, 2, 2, vec![0, 0, 0, 0], DisposalMethod::Keep, 5, None),
                idx_frame(0, 0, 2, 2, vec![1, 1, 1, 1], DisposalMethod::Keep, 5, None),
            ],
        );
        let mut s = GifStream::open(&path).expect("open stream");
        assert_eq!(s.dimensions(), (2, 2));
        let f0 = s.next_frame().unwrap();
        let f1 = s.next_frame().unwrap();
        let f2 = s.next_frame().unwrap(); // wraps → frame 0 again
        let f3 = s.next_frame().unwrap();
        let f4 = s.next_frame().unwrap();
        assert_eq!(px(&f0.rgba, 2, 0, 0), RED);
        assert_eq!(px(&f1.rgba, 2, 0, 0), GREEN);
        assert_eq!(f0.rgba, f2.rgba, "stream loops: frame 2 == frame 0 (red)");
        assert_eq!(f1.rgba, f3.rgba, "stream loops: frame 3 == frame 1 (green)");
        assert_eq!(f0.rgba, f4.rgba, "stream loops: frame 4 == frame 0 (red)");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The format of a shot's finished-image slot — decides which decoder the render tiers use, and
/// whether the shot is displayable at all. A RAW-only shot is `Jpeg` (its embedded preview is a
/// JPEG). A standalone image we can't decode is `Unsupported` (shows a badge, never decoded).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum SrcKind {
    Jpeg,
    Png,
    Tiff,
    /// WebP, decoded via the pure-Rust `image-webp` crate (lossy VP8 + lossless VP8L; the ICCP
    /// profile drives the source gamut). Decodable on every platform — no OS codec dependency.
    Webp,
    /// HEIC/HEIF, decoded via the OS codec — Windows WIC or macOS Image I/O (v0.9.9). Only produced
    /// on platforms with a `decode_heic` implementation; elsewhere HEIC files classify as `Unsupported`.
    Heic,
    /// JPEG XL, decoded via the pure-Rust `jxl-oxide` crate. Orientation (codestream header) is applied
    /// on render; the embedded ICC drives the source gamut. HDR (PQ/HLG) JXLs route to Unsupported at
    /// decode time (the HDR→SDR design round is deferred). Formats batch F1.
    Jxl,
    /// BMP, decoded via the pure-Rust `image` crate's `bmp` codec. Universal (incl. the macOS port).
    /// Formats batch F3.
    Bmp,
    /// GIF, decoded via the pure-Rust `gif` crate. Still consumers get the composited first frame; the
    /// viewer plays the full animation. Formats batch F2.
    Gif,
    Unsupported,
}

/// One logical shot. A RAW+JPG pair with the same filename stem collapses into a
/// single shot so navigation advances over *shots*, not files (PLAN §6.9).
#[derive(Clone, Debug, Serialize)]
pub struct Shot {
    pub id: usize,
    /// Filename stem, e.g. `HWU_7781`.
    pub name: String,
    pub has_raw: bool,
    /// True when the finished-image slot holds a *decodable* sibling (JPG/PNG/TIFF). Kept the
    /// historical name; `kind` disambiguates the format. False for a RAW-only or unsupported shot.
    pub has_jpg: bool,
    // Absolute paths stay server-side; never serialized to the webview.
    #[serde(skip)]
    pub raw: Option<PathBuf>,
    /// The finished-image path (JPG/PNG/TIFF, or an unsupported standalone image so the badge can
    /// name/reveal it). `None` for a RAW-only shot (decoded via the RAW's embedded JPEG preview).
    #[serde(skip)]
    pub jpg: Option<PathBuf>,
    /// Format of the finished-image slot — the decoder dispatch key.
    pub kind: SrcKind,
    /// v1.0.0-rc TAIL (skeptic B, R2): the container the file's own BYTES declared, when it
    /// DISAGREED with the extension. `None` when the two agreed, when the magic was unrecognised
    /// (rule (3)), and for a cloud placeholder — which is never read at all. Runtime state, like
    /// `cloud_placeholder`, so it is never serialized to the UI.
    ///
    /// IT EXISTS BECAUSE `Unsupported` HAS NO FORMAT NOUN OF ITS OWN. Without it,
    /// [`Shot::finished_format`]'s `Unsupported` arm falls back to the EXTENSION — and for an AVIF
    /// named `holiday.jpg` that put **"JPG is currently unsupported"** on the full stage
    /// (`tick.rs` `step_stage`), `Copy JPG` in the context menu, `holiday JPG copied to clipboard`
    /// in the toast and `JPG 214 KB` in the info panel: five surfaces telling a photographer that
    /// the app does not support a format it fully supports. This round exists because a card said
    /// something false about a file; this field is what stops it minting a new one.
    #[serde(skip)]
    pub sniffed: Option<SrcKind>,
    /// v0.8.24 (D1): EITHER pair file is a cloud (OneDrive Files-On-Demand) placeholder — present in the
    /// listing but not hydrated locally. Detected at scan from the file ATTRIBUTES only (no data read →
    /// no download). Drives the "not downloaded" failure notice + the timed retry sweep + the Copy/Move
    /// download advisory. Server-side runtime state (like `raw`/`jpg`) → never serialized; always `false`
    /// off Windows.
    #[serde(skip)]
    pub cloud_placeholder: bool,
}

/// v0.8.99 (H2a/H3a): the SHORT format tag a log line names a source by.
///
/// `Jpeg` deliberately renders as **`JPG`** — that is the exact token the detail and ROI tiers have
/// always printed (`"JPG / CPU"`, `"JPG via nvJPEG"`) and every historical log, doc and boot-verify
/// grep in the tree reads it, so it is byte-pinned. The other arms exist because those lines used to
/// print `JPG` for a HEIC/PNG/TIFF too: a hard-coded lie that would have misread the v0.8.99 HEIC
/// measurement round as "JPEG is slow". Also the bucket key for the per-tier decode statistics.
pub fn kind_tag(k: SrcKind) -> &'static str {
    match k {
        SrcKind::Jpeg => "JPG", // byte-pinned: the historical token
        SrcKind::Png => "PNG",
        SrcKind::Tiff => "TIFF",
        SrcKind::Webp => "WEBP",
        SrcKind::Heic => "HEIC",
        SrcKind::Jxl => "JXL",
        SrcKind::Bmp => "BMP",
        SrcKind::Gif => "GIF",
        SrcKind::Unsupported => "UNSUP",
    }
}

/// v0.8.99 (H1): is this path a HEIC/HEIF by EXTENSION — the same rule the scanner classifies by?
///
/// Deliberately extension-only and deliberately NOT `kind == Heic`: on a codec-less Windows box the
/// scan now classifies HEIC as `Unsupported` (H3b), and the capability probe must still fire for
/// exactly those machines — "codec=missing" is the most informative probe line there is.
pub fn is_heic_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| HEIC_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// v0.8.99 (H3b): the scan-time classification of a HEIC file, given whether this machine can
/// actually decode one. Pure so the rule is testable without a codec (or a Windows box) in the loop.
///
/// Before v0.8.99 a HEIC on Windows was `Heic` unconditionally — an assumption, not a fact. With no
/// HEVC/HEIF Image Extension installed, all three render tiers then discovered the truth
/// independently, each by failing a decode and latching its own failure set, and the user got a
/// decode-failure card instead of the honest "this format isn't supported on this machine" one.
pub fn heic_scan_kind(decodable: bool) -> SrcKind {
    if decodable {
        SrcKind::Heic
    } else {
        SrcKind::Unsupported
    }
}

impl Shot {
    /// A recognised image we cannot decode (AVIF/JXL/GIF/…, or a genuinely bad file): show a
    /// placeholder + badge instead of attempting a decode that would only fail.
    pub fn is_unsupported(&self) -> bool {
        self.kind == SrcKind::Unsupported
    }
    /// The finished source is JPEG bytes (a real .jpg, or a RAW-only shot's embedded preview) — the
    /// only case the nvJPEG GPU decoder can handle. PNG/TIFF must take the CPU decode path.
    pub fn is_jpeg_source(&self) -> bool {
        self.kind == SrcKind::Jpeg
    }

    /// The uppercased format of the finished-image slot for the info panel / unsupported notice
    /// (e.g. `JPG`, `PNG`, `TIFF`, `HEIC`). `None` for a RAW-only shot (its finished image is the
    /// embedded preview, which has no standalone file extension).
    ///
    /// v1.0.0-rc (BYTES OVER NAMES): the label names the **kind**, and since the scan sniffs, the
    /// kind is the container the file's own bytes declare — so a PNG named `53d879f01a3f481c.JPG`
    /// badges **PNG** while the header still shows the user their real filename. This is the L26
    /// rule applied to a format noun: gate a shared answer and you owe the mirror on every invoker.
    /// It also removes a second format table — [`kind_tag`] is now the only one, so the badge, the
    /// panel's Format row and every decode-completion line can no longer drift apart.
    ///
    /// `Unsupported` is the one arm that still reads the extension, and must be: that kind has no
    /// format noun of its own (`kind_tag` renders it `UNSUP`), and it is what a HEIC becomes on a
    /// Windows box with no HEVC Image Extension — where the panel must still say **HEIC**, which is
    /// the fact the user needs in order to go and install the codec. A `.avif`/`.tga` reads its own
    /// name for the same reason.
    pub fn finished_format(&self) -> Option<String> {
        // v1.0.0-rc MICRO-TAIL (A-Y3b): the `Unsupported` shot is the one whose CARD has to name a
        // format — that is the whole point of the arm below, and of A-Y1 — so it answers from the
        // file. Every other shot answers about THE PICTURE, and a passenger is not the picture: a
        // RAW carrying an undecodable sibling badges `RAW`, offers "Copy image", and shows the
        // RAW's photograph, exactly as it did before this round touched it.
        if self.kind == SrcKind::Unsupported {
            return self.finished_file_format();
        }
        if !self.has_jpg {
            return None;
        }
        Some(kind_tag(self.kind).to_string())
    }

    /// v1.0.0-rc MICRO-TAIL (A-Y3b): the container noun of the FILE in the finished slot, whether
    /// or not that file is the picture on screen.
    ///
    /// This is the question the info panel asks — it lists what is on disk — while
    /// [`Shot::finished_format`] asks what is being shown. They were one function until a RAW with
    /// an undecodable sibling made the difference matter: the panel must say
    /// `RAW 45 MB · HEIC 3 MB` (the partner named with its true container, on the surface that
    /// already lists partners) while the badge says `RAW`, because that is what the user is looking
    /// at. No new sentence family: `fmt_files` already renders exactly that shape.
    pub fn finished_file_format(&self) -> Option<String> {
        let path = self.jpg.as_ref()?;
        if self.kind != SrcKind::Unsupported && self.has_jpg {
            return Some(kind_tag(self.kind).to_string());
        }
        // v1.0.0-rc TAIL (skeptic B, R2): `Unsupported` names something OTHER than itself, and
        // WHICH something depends on whether the name was believed. This is the L26 mirror the
        // first cut owed and did not walk: five invokers read this one answer.
        match self.sniffed {
            // The bytes were read and they named a container this MACHINE cannot decode — a HEIC on
            // a Windows box with no HEVC Image Extension. Say that: it is true, and it is the one
            // fact that tells the user what to go and install (v0.8.99 H3b's whole point).
            Some(s) if s != SrcKind::Unsupported => Some(kind_tag(s).to_string()),
            // The bytes were read and this BUILD has no decoder for what they are (today: AVIF).
            // There is no honest noun to give — `kind_tag` renders it `UNSUP`, which is a log-column
            // token and not a format — so the answer is NONE, and every invoker already has a
            // generic form for that: the stage says "This format is currently unsupported"
            // (already written, already reachable), the badge reads `IMG`, the menu `Copy image`,
            // the toast `<name> image copied`. Nothing false is said on any surface.
            Some(_) => None,
            // The name was never overridden: a genuine `.avif`/`.tga`, a `.heic` on a codec-less
            // box, or a cloud placeholder that rule (1) forbade us to read. The extension is the
            // best evidence there is here, and naming it is correct.
            None => path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| match e.to_ascii_lowercase().as_str() {
                    "jpg" | "jpeg" => "JPG".to_string(),
                    "tif" | "tiff" => "TIFF".to_string(),
                    "heic" | "heif" => "HEIC".to_string(),
                    "webp" => "WEBP".to_string(),
                    other => other.to_ascii_uppercase(),
                }),
        }
    }

    /// v1.0.0-rc TAIL (skeptic B, O1 — owner ruling): the extension this file WEARS, when its bytes
    /// say something else — `Some("JPG")` for the field file, `None` whenever name and bytes agree.
    ///
    /// The info panel is the one surface whose whole job is answering "what IS this file", so it is
    /// the one surface that discloses the disagreement (`PNG 872 KB (named .JPG)`). The tile badge
    /// stays a single clean noun: it is scanned at a glance across a filmstrip and a parenthetical
    /// there would cost the photograph.
    pub fn named_ext_when_bytes_disagree(&self) -> Option<String> {
        self.sniffed?;
        let e = self.jpg.as_ref()?.extension()?.to_str()?;
        Some(e.to_ascii_uppercase())
    }

    /// Short filetype badge for a tile (JPG / PNG / TIFF / RAW / R+J / HEIC …). A RAW paired with a
    /// finished image reads `R+<initial>` (R+J / R+P / R+T); a RAW with no finished sibling is `RAW`.
    pub fn badge_label(&self) -> String {
        match (self.has_raw, self.finished_format()) {
            (true, Some(f)) => format!("R+{}", f.chars().next().unwrap_or('J')),
            (true, None) => "RAW".to_string(),
            (false, Some(f)) => f,
            (false, None) => "IMG".to_string(),
        }
    }

    /// The underlying FILE's stem for the filmstrip sequence number. A split shot's `name` carries an
    /// extension suffix (`IMG_0042.tif`) for a unique rating key, but `frame_number(name)` would then
    /// see the trailing `.tif` and return empty — so derive the sequence digits from the actual file
    /// stem (`IMG_0042` → `0042`). Double-extension files (`IMG_0042.tif.png`) leave a trailing image
    /// ext in `file_stem` too, so KEEP stripping while a known image extension remains (G6 — the badge
    /// went blank); a dotted non-image suffix (`photo.v2.jpg` → `photo.v2`) is kept. Falls back to
    /// `name` for a shot with no path (defensive).
    pub fn seq_stem(&self) -> String {
        self.jpg
            .as_ref()
            .or(self.raw.as_ref())
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .map(|s| {
                let mut stem = s;
                while let Some(dot) = stem.rfind('.') {
                    if dot > 0 && is_known_image_ext(&stem[dot + 1..]) {
                        stem = &stem[..dot];
                    } else {
                        break;
                    }
                }
                stem.to_string()
            })
            .unwrap_or_else(|| self.name.clone())
    }
}

/// An encoded image ready to hand to the webview over the `falcon://` protocol.
#[derive(Clone)]
pub struct Frame {
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

/// Decoded EXIF for the info panel. All optional — cameras vary.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Exif {
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub focal: Option<String>,
    pub aperture: Option<String>,
    pub shutter: Option<String>,
    pub iso: Option<String>,
    pub date: Option<String>,
    pub dimensions: Option<String>,
    pub files: Option<String>,
}

/// Precedence among finished-image formats sharing a stem — higher wins (deterministic pairing).
fn finished_rank(k: SrcKind) -> u8 {
    match k {
        SrcKind::Jpeg => 8,
        SrcKind::Tiff => 7,
        SrcKind::Png => 6,
        // Modern pure-Rust delivered/archival format — below the lossless PNG/TIFF and the camera-default
        // JPEG, above the other delivered formats for a rare same-stem collision.
        SrcKind::Jxl => 5,
        // A pure-Rust-decodable delivered format.
        SrcKind::Webp => 4,
        SrcKind::Bmp => 3,
        // Animated/indexed delivered format — the lowest of the pure-Rust decoders (a same-stem GIF is
        // the least likely to be the "real" deliverable), still above HEIC + Unsupported.
        SrcKind::Gif => 2,
        // Below the pure-Rust decoders: for a rare same-stem collision prefer a format we can decode
        // without the OS HEVC codec. Still above Unsupported (HEIC is decodable on Windows).
        SrcKind::Heic => 1,
        SrcKind::Unsupported => 0,
    }
}

// ───────────────────────────── folder scan / pairing ─────────────────────────

/// v0.9.63 (B-R5-1, mac round-5 field report) — is this file name an APPLEDOUBLE SIDECAR?
///
/// macOS writes one of these beside every file it copies onto a filesystem that cannot hold an
/// HFS+ resource fork or extended attributes — exFAT, FAT32, most SMB shares. The sidecar is named
/// `._<original>`, so a folder of iPhone HEICs on a T7 SSD carries `._IMG_7737.HEIC` beside
/// `IMG_7737.HEIC`. It is not a photograph: it is a few KB of AppleDouble metadata that happens to
/// wear a `.HEIC` extension, and Finder hides it.
///
/// THE FIELD SYMPTOM (the tester's log, v0.9.62): `._IMG_7737.HEIC` sorted FIRST, so it became
/// shot #0 — the folder's landing photograph — the HEIC probe ran on it, and both the thumbnail
/// and the fast decode failed loudly (`thumbnail #0 decode FAILED — ._IMG_7737`). The user's first
/// sight of the folder was a broken frame for a file they cannot see in Finder.
///
/// THE PREDICATE IS THE NAME, AND ONLY THE NAME. Requiring the original to be present beside it
/// was considered and rejected: on a drive that has travelled, the original may have been deleted,
/// renamed or never copied, and the sidecar is still not a photograph. A real photograph whose
/// name begins with `._` is not a case worth protecting — macOS hides such a file from its own
/// user, and no camera or export tool produces one.
///
/// INTENDED SHARED CHANGE, stated: this is trunk-shared code and the skip is deliberately NOT
/// gated to macOS. AppleDouble files travel — an exFAT card culled on a Mac and then opened on
/// Windows carries them, and Falcon scanned them there too, with the same two loud failures and
/// the same phantom shot. The same class as the v0.9.62 B4-a un-gatings: the defect is not the
/// platform, it is the file.
pub fn is_appledouble_sidecar(file_name: &str) -> bool {
    // `._` alone (length 2) is not a sidecar for anything — and it has no extension, so the scan
    // would ignore it regardless. Requiring a third byte keeps the predicate about real sidecars.
    file_name.len() > 2 && file_name.starts_with("._")
}

// ───────────── v1.0.0-rc (BYTES OVER NAMES): the container the BYTES declare ─────────────
//
// Ledger L42 (a NAME deciding what BYTES are, when the bytes are right there) with a file
// EXTENSION as the named case. The field report: `53d879f01a3f481c.JPG` is a complete, valid,
// uncorrupted PNG — 1649×2337 RGBA, 219 IDAT chunks, a clean IEND. Falcon classified it by
// `Path::extension()` and by nothing else, stamped `SrcKind::Jpeg`, and every tier then fed PNG
// bytes to `jpeg-decoder`, which refused them at the first two ("first two bytes are not an SOI
// marker"). Three tiers, three latches, and a card telling the user the file "may be corrupt or
// unreadable" — the one thing it demonstrably is not. The same bytes renamed `.png` opened in
// 33 ms. The extension is now the FALLBACK, not the authority.

/// How many bytes the scan reads to ask a file what it IS.
///
/// 32 is the smallest round number that covers every signature in [`sniff_kind`]. The longest
/// single magic is the 12-byte JPEG XL container box, but the ISO-BMFF arm reads a compatible-brand
/// list that starts at offset 16 and the BMP arm reads the DIB header size at 14..18. One `read` of
/// 32 bytes costs exactly what one `read` of 20 does, so the head is sized by the reader rather
/// than shaved to the parser.
pub const SNIFF_HEAD_BYTES: usize = 32;

/// The container a file's OWN BYTES declare.
///
/// `None` means "a magic this function does not recognise" — which is the caller's instruction to
/// keep the EXTENSION's answer. It is never a guess and never `Unsupported`: a silence here must
/// not be able to reclassify anything (rule (c) at [`scan_classify`]). Pure over a byte prefix, so
/// the whole table is testable with no filesystem in the loop, and so a caller that must not read
/// (a cloud placeholder) simply never calls it.
///
/// PRIOR ART — this extends a habit, it does not invent one. `largest_embedded_jpeg` scans for
/// `FF D8 FF` inside RAW buffers; `extract_icc_from_jpeg` and `locate_jpeg_orientation` both check
/// SOI before parsing; `heic_colr_scan`/`bmff_children` walk ISO-BMFF boxes; kamadak-exif's
/// `read_from_container` sniffs, which is precisely why `read_exif` was the ONE door in the tree
/// that behaved correctly on the field file. `RAW_EXTS`' own doc states the principle outright:
/// rawler detects by magic bytes, never by extension, so the extension is only OUR routing hint.
///
/// THE TABLE, and why each entry is the length it is:
///   * PNG      `89 50 4E 47 0D 0A 1A 0A` — the full 8-byte signature, never the short `\x89PNG`.
///   * JPEG     `FF D8 FF` — SOI plus the first byte of the marker that must follow it.
///   * TIFF     `II*\0` / `MM\0*`, **and** the BigTIFF `II+\0` / `MM\0+` forms: `tiff` 0.9.1 reads
///     version 43 (`decoder/mod.rs`), so a BigTIFF genuinely takes the `SrcKind::Tiff` arm — and
///     even where the crate declines a particular BigTIFF, that arm falls back to the OS codec,
///     which is a strictly better answer than the extension's guess.
///   * WebP     `RIFF` at 0 **and** `WEBP` at 8 — the RIFF fourcc alone is also WAV/AVI.
///   * GIF      `GIF87a` / `GIF89a` — both full 6-byte signatures, not the 4-byte `GIF8` prefix.
///   * BMP      `BM` is only two bytes, so it is qualified by the DIB header size at 14..18 being
///     one of the eight defined values. A two-byte magic is the one entry in this table that could
///     plausibly fire on junk, and a false POSITIVE is the failure mode rule (c) exists to avoid.
///   * ISO-BMFF `ftyp` at offset 4, with the BRANDS deciding — see [`bmff_image_kind`].
///   * JXL      the 12-byte container signature box, and the naked codestream's `FF 0A`.
///
/// WHAT IS DELIBERATELY ABSENT: TGA (its v1 header carries no signature at all, so a TGA can only
/// ever be recognised by its name — `.tga` keeps the name's answer and that is correct); RAW
/// containers (the scan routes those by extension BEFORE this is reached, and rawler sniffs from
/// there); and every ISO-BMFF brand that is not a still image this tree has an arm for (`isom`,
/// `mp41`, `qt  `, `crx ` = CR3), which return `None` so a video or a RAW wearing an image
/// extension degrades to the name instead of being mis-decoded as a HEIC.
pub fn sniff_kind(head: &[u8]) -> Option<SrcKind> {
    let at = |i: usize| head.get(i..i + 4);
    // Longest / least ambiguous first. The two-byte magics are last on purpose.
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(SrcKind::Png);
    }
    // The JXL ISO-BMFF signature box is 12 bytes and always the first box, so it is decided before
    // the `ftyp` arm below ever sees the container.
    if head.starts_with(&[0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A]) {
        return Some(SrcKind::Jxl);
    }
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(SrcKind::Jpeg);
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some(SrcKind::Gif);
    }
    if at(0) == Some(b"RIFF") && at(8) == Some(b"WEBP") {
        return Some(SrcKind::Webp);
    }
    if head.starts_with(&[0x49, 0x49, 0x2A, 0x00])
        || head.starts_with(&[0x4D, 0x4D, 0x00, 0x2A])
        || head.starts_with(&[0x49, 0x49, 0x2B, 0x00])
        || head.starts_with(&[0x4D, 0x4D, 0x00, 0x2B])
    {
        return Some(SrcKind::Tiff);
    }
    if at(4) == Some(b"ftyp") {
        return bmff_image_kind(head);
    }
    if head.starts_with(&[0xFF, 0x0A]) {
        return Some(SrcKind::Jxl);
    }
    if head.starts_with(b"BM") && bmp_dib_header_plausible(head) {
        return Some(SrcKind::Bmp);
    }
    None
}

/// The still-image kind an ISO-BMFF `ftyp` box declares, or `None` for a container whose brands
/// name something this tree has no arm for (video, CR3, an unknown brand).
///
/// The brand list is walked in FILE order — major brand at 8..12, a 4-byte minor version, then the
/// compatible brands from 16 — and `avif`/`avis` SHORT-CIRCUIT, because an AVIF legally carries
/// `mif1`/`miaf` among its compatible brands and must not be read as a HEIC on the strength of one.
/// AVIF answers [`SrcKind::Unsupported`] rather than `None`, which is the same verdict
/// `UNSUPPORTED_IMG_EXTS` gives a file NAMED `.avif`: there is no AVIF decoder in the tree (its
/// HDR→SDR design round is deferred), so the honest answer is the badge, not a decode attempt.
///
/// `mif1`/`msf1`/`miaf` are the generic HEIF brands and only set the fallback answer, so a file
/// that carries one of them AND a decisive brand is decided by the decisive one wherever it sits.
///
/// v1.0.0-rc TAIL (skeptic A, O3): THE BOX'S OWN SIZE BOUNDS THE BRAND LIST; the 32-byte head
/// still bounds the READ. Before the tail this walked five brand slots with no reference to the
/// `ftyp` box's declared length, so a legal 16-byte `ftyp mif1` followed by an `mdat` whose payload
/// happens to begin `avif` answered `Some(Unsupported)` — from four bytes that are not a brand and
/// belong to a different box entirely. The size field is the first four bytes of every ISO-BMFF
/// box; `0` means "to end of file" and `1` means "the real size is a 64-bit `largesize` after the
/// type", and in both of those cases the head is the only bound available.
fn bmff_image_kind(head: &[u8]) -> Option<SrcKind> {
    let declared = head.get(..4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let end = match declared {
        Some(0) | Some(1) | None => head.len(),
        Some(n) => n.min(head.len()),
    };
    let mut fallback: Option<SrcKind> = None;
    let mut i = 8usize; // the major brand; the minor version occupies 12..16
    while i + 4 <= end {
        let brand = &head[i..i + 4];
        match brand {
            b"avif" | b"avis" => return Some(SrcKind::Unsupported),
            b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"hevm" | b"hevs" => {
                return Some(SrcKind::Heic)
            }
            b"jxl " => return Some(SrcKind::Jxl),
            b"mif1" | b"msf1" | b"miaf" => fallback = Some(SrcKind::Heic),
            _ => {}
        }
        i += if i == 8 { 8 } else { 4 };
    }
    fallback
}


/// Does a `BM`-headed buffer carry a DIB header size the BMP format actually defines?
///
/// The eight legal values are BITMAPCOREHEADER (12), OS22XBITMAPHEADER (16 and 64),
/// BITMAPINFOHEADER (40), BITMAPV2/V3INFOHEADER (52, 56), BITMAPV4HEADER (108) and BITMAPV5HEADER
/// (124). Two bytes of magic are not enough to reclassify a file on; these four are.
fn bmp_dib_header_plausible(head: &[u8]) -> bool {
    match head.get(14..18) {
        Some(b) => matches!(
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            12 | 16 | 40 | 52 | 56 | 64 | 108 | 124
        ),
        None => false,
    }
}

/// The first [`SNIFF_HEAD_BYTES`] of a file, or `None` when it cannot be opened at all.
///
/// Bounded by construction: a fixed 32-byte stack buffer, filled by `read` — never `read_to_end`,
/// never a length taken from the file. A SHORT file returns the short slice it is, because
/// [`sniff_kind`] answers `None` for anything it cannot match in full, and a short read on a
/// network share is retried rather than accepted, so an SMB hiccup degrades to the name only when
/// the file really is that small.
fn read_head(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; SNIFF_HEAD_BYTES];
    let mut n = 0usize;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    Some(buf[..n].to_vec())
}

/// The HEIC classification this machine can actually honour, asked once per scan (see the
/// `heic_kind` cell in [`scan_folder_counted`]).
///
/// Extracted at v1.0.0-rc because the sniff gave it a SECOND caller: a file whose bytes say HEIC
/// must reach the same verdict whether its name said `.heic` or `.jpg`, and two hand-copies of a
/// codec probe is exactly the drift this round is about.
fn heic_scan_kind_here() -> SrcKind {
    heic_scan_kind(cfg!(target_os = "macos") || (cfg!(windows) && wic_heif_codec_present()))
}

/// The scan-time classification of ONE finished-image candidate — the three rules of the
/// BYTES OVER NAMES round, stated where they are enforced.
///
///  1. **NEVER READ A CLOUD PLACEHOLDER.** A dehydrated OneDrive file is present in the listing and
///     absent from the disk; opening it triggers a DOWNLOAD, per file, during a scan whose whole
///     design (`entry.file_type()`, `entry.metadata()`, no data read anywhere) exists to avoid
///     exactly that. `head` is a CLOSURE for this reason and no other: the row
///     `the_sniff_never_touches_a_cloud_placeholder` proves the arm is taken by proving the closure
///     is never called. A placeholder keeps its extension kind; when it hydrates, the next scan of
///     that folder sniffs it (see the stated residue in the round record: the 30 s
///     `step_cloud_retry` sweep clears the failure LATCHES but does not re-classify in place).
///  2. **THE BYTES WIN.** A recognised magic that disagrees with the extension replaces the kind
///     for every consumer, because there is exactly one `kind` and all nine subsystems read it.
///  3. **UNKNOWN MAGIC DEGRADES TO THE NAME.** An unreadable file, a short read, or a magic
///     [`sniff_kind`] does not know keeps the extension's answer — so no existing corpus can be
///     reclassified by this change, and a `.tga` (a format with no signature) is unaffected.
///
/// Returns the kind to STAMP and, when the bytes disagreed with the name, the container the bytes
/// DECLARED. Those two are not always the same value, which is why both are returned: a HEIC named
/// `.jpg` on a Windows box with no HEVC Image Extension installed is stamped `Unsupported`, and the
/// honesty line still has to be able to say the word HEIC.
fn scan_classify(
    ext_kind: SrcKind,
    cloud_placeholder: bool,
    head: impl FnOnce() -> Option<Vec<u8>>,
    heic_kind: impl FnOnce() -> SrcKind,
) -> (SrcKind, Option<SrcKind>) {
    if cloud_placeholder {
        return (ext_kind, None); // rule (1) — `head` is not called
    }
    let Some(sniffed) = head().as_deref().and_then(sniff_kind) else {
        return (ext_kind, None); // rule (3)
    };
    // A sniffed HEIC goes through the same capability gate a NAMED one does (v0.8.99 H3b): on a
    // codec-less box the honest answer is "we cannot decode this", not "we will fail three times".
    let stamped = if sniffed == SrcKind::Heic { heic_kind() } else { sniffed };
    if stamped == ext_kind {
        (stamped, None)
    } else {
        (stamped, Some(sniffed)) // rule (2)
    }
}

/// v1.0.0-rc TAIL (skeptic B, Y2): the ONE line a cloud placeholder gets for taking the NAME arm.
///
/// Rule (1) is right and is untouched — a dehydrated file is not read, because opening it is a
/// download. What was wrong is that the arm was also SILENT: a dehydrated OneDrive PNG named `.JPG`
/// got the exact pre-round experience — three tier failures, three latches, "may be corrupt" — with
/// no diagnostic of any kind anywhere, on a project whose own tree and whose own test corpus live
/// on OneDrive. This costs nothing, it is true, and it is the sentence that makes the class
/// self-diagnosing. `cloud_hint`-free on purpose: falcon-decode is a library the spikes link and
/// has no platform noun table; the app's own "not downloaded" notice already names the provider.
fn placeholder_naming_line(file_name: &str, ext_kind: SrcKind) -> String {
    format!(
        "{file_name}: not downloaded yet — classified by its NAME as {} until it arrives",
        kind_tag(ext_kind)
    )
}

/// v1.0.0-rc TAIL (skeptic B, Y2): re-classify a shot whose placeholder has HYDRATED.
///
/// The companion to rule (1). The scan could not read the file, so it took the name; when the file
/// arrives, the bytes become available and the answer can be corrected in place — otherwise the
/// 30 s `step_cloud_retry` sweep clears the latches and every tier re-fails **as a JPEG**,
/// indefinitely, until the user thinks to leave the folder and come back (a step he has no way to
/// discover — the THINKING-FLOW break the skeptic named).
///
/// Returns `true` when the kind actually changed, so the caller can bound its own logging and knows
/// whether anything downstream needs to notice. Emits the same honesty line the chokepoint does,
/// through the same capped family and the same per-path key — so a file that is re-classified here
/// says it once, and never says it twice if the folder is later rescanned.
pub fn reclassify_hydrated(shot: &mut Shot) -> bool {
    let Some(path) = shot.jpg.clone() else { return false };
    // v1.0.0-rc TAIL 3 (verifier Y1): the kind of the FILE, which is not always the kind of the
    // SHOT. For a passenger the shot's `kind` describes the RAW, so the file's last-known kind is
    // "undecodable" — the only thing that could have made it a passenger in the first place.
    let file_kind = if shot.has_jpg { shot.kind } else { SrcKind::Unsupported };
    let (file_now, sniffed) = scan_classify(file_kind, false, || read_head(&path), heic_scan_kind_here);
    // …and the SHOT's state comes from the same mint the scan uses, never from the sniff alone.
    let (kind, has_jpg) = mint_finished(Some(file_now), shot.has_raw);
    if kind == shot.kind && has_jpg == shot.has_jpg && sniffed.is_none() {
        return false;
    }
    let changed = kind != shot.kind || has_jpg != shot.has_jpg;
    shot.kind = kind;
    shot.has_jpg = has_jpg;
    shot.sniffed = sniffed;
    if let Some(bytes_say) = sniffed {
        let shown = sanitize_one_line(path.file_name().and_then(|s| s.to_str()).unwrap_or_default());
        note_sniff_disagreement(&path, &shown, bytes_say, file_now, file_kind);
    }
    changed
}

/// v1.0.0-rc TAIL (skeptic B, Y1 + Y2): the ONE place the disagreement line is parked, so the
/// chokepoint and the hydration re-classify cannot drift apart in wording, key or cap.
fn note_sniff_disagreement(
    path: &Path,
    shown: &str,
    sniffed: SrcKind,
    stamped: SrcKind,
    ext_kind: SrcKind,
) -> NoteVerdict {
    static SNIFFED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    note_path_capped(&format!("sniff:{}", path.display()), &SNIFFED, SNIFF_NOTE_CAP, || {
        sniff_disagreement_line(shown, sniffed, stamped, ext_kind)
    })
}

/// v1.0.0-rc TAIL (skeptic A, O6): the ONE trailer a scan posts for everything its capped note
/// families swallowed, keyed on the FOLDER so a later folder that also overflows says so again.
///
/// This is what makes the cap honest rather than merely quiet: a reader who sees 200 lines and then
/// nothing has no way to tell a folder with exactly 200 disagreements from one with 20 000.
fn note_scan_suppressed(dir: &Path, m: usize) {
    if m == 0 {
        return;
    }
    note_once(
        &format!("scan-suppressed:{}", dir.display()),
        format!(
            "scan: …and {m} more like it in this folder — per-file name/bytes lines are capped at {SNIFF_NOTE_CAP} for this session"
        ),
    );
}

/// The bound on every per-path note family this round adds. 200 is `colour-resolve`'s own number,
/// chosen for the same reason: a bounded log beats a complete one, and 200 disagreeing files is
/// already far past the point where a reader learns anything new from the 201st.
const SNIFF_NOTE_CAP: usize = 200;

/// v1.0.0-rc TAIL (skeptic B, Y1): a per-PATH note family, capped the way `colour-resolve` is.
///
/// `note_once`'s own doc says its return value exists *"so a caller that also bounds ITS OWN family
/// of notes counts real lines"*, and the tree has two families that do exactly that
/// (`colour-resolve:<path>` at 200, `colour-miss:<profile>` at 8). This round shipped two families
/// that did not: `sniff:<path>` and `jpeg-truncated:<path>`, both always-on and both keyed on a
/// path, in a process whose key set is never cleared for the session. §C accepted 500 lines for a
/// 500-file folder as a stated decision; what it did not state is that the KEYS accumulate across
/// every folder browsed, and that the population producing them — a Downloads / messenger-export
/// folder where most files are misnamed — is exactly where the field file came from. Measured by
/// the skeptic: four folders of 300 mismatched files each produced 1 200 lines and no cap.
///
/// The composer is a closure so a capped family costs nothing once it is full: the string is never
/// built.
///
/// v1.0.0-rc TAIL (skeptic A, O6): the cap bounds **residency**, not merely output. Once the family
/// is full this returns before `note_once`, so no further KEY is inserted into the session-wide set
/// either — which is the half that matters, because A measured 300 keys from one folder drained on
/// a single ~1.5 s report tick and resident for the life of the process, and this is the channel's
/// first user-data-derived key family. It also returns a VERDICT, so the caller can count what was
/// swallowed and say so at whatever boundary it owns (a folder, for the scan; the session, for the
/// per-decode families) — a cap that goes silent without saying how much it hid is its own small
/// dishonesty.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NoteVerdict {
    /// A new fact: logged, and this key is now resident.
    Logged,
    /// This exact key has already been logged this session — not a new fact, and not suppressed.
    Repeat,
    /// The family is full. Nothing was logged, nothing was composed, and NO KEY WAS INSERTED.
    Suppressed,
}

fn note_path_capped(
    key: &str,
    counted: &'static std::sync::atomic::AtomicUsize,
    cap: usize,
    line: impl FnOnce() -> String,
) -> NoteVerdict {
    use std::sync::atomic::Ordering;
    if counted.load(Ordering::Relaxed) >= cap {
        return NoteVerdict::Suppressed;
    }
    if note_once(key, line()) {
        counted.fetch_add(1, Ordering::Relaxed);
        NoteVerdict::Logged
    } else {
        NoteVerdict::Repeat
    }
}

/// The ONE sentence a file gets when its bytes and its name disagree.
///
/// Pure so the wording is pinned by a row instead of by reading a log, and composed from
/// [`kind_tag`] so the format nouns here can never drift from the ones every completion line and
/// every badge already print. `file_name` must arrive sanitised — a file name is attacker-supplied
/// text and may legally contain a newline, which would forge a second log line (`sanitize_one_line`,
/// v0.8.141 R11).
///
/// v1.0.0-rc TAIL (skeptic B, R1 — L30 turned inside out): **IT SAYS WHAT THE SCAN KNOWS, WHICH IS
/// A ROUTING AND NOT AN OUTCOME.** The first cut wrote "decoded as PNG" from inside
/// `scan_folder_counted`, where nothing has been decoded — 32 bytes have been read and a kind has
/// been stamped. That is not a wording preference, it is false on a reachable path: a PNG with a
/// broken IHDR CRC named `.JPG` sniffs cleanly and never decodes, and because the tier workers log
/// `decode FAILED` immediately while this channel drains on the ~1.5 s report cadence, the tester's
/// log read three failures and THEN a line claiming the file had been decoded. The decode's own
/// outcome is already reported, by the tier that actually has it (`full-res #N … (PNG / CPU)`, or
/// `decode FAILED`); this line's job is the fact those lines cannot carry — which decoder the file
/// was sent to, and why. The two `Unsupported` arms were already right, because "not decodable on
/// this machine" is a classification too; only the success arm overreached.
fn sniff_disagreement_line(
    file_name: &str,
    sniffed: SrcKind,
    stamped: SrcKind,
    ext_kind: SrcKind,
) -> String {
    let bytes_say = kind_tag(sniffed);
    let name_says = kind_tag(ext_kind);
    if sniffed == SrcKind::Unsupported {
        // The sniff READ the container and this build has no decoder for it (today: AVIF, whose
        // HDR→SDR design round is deferred). `kind_tag` renders that `UNSUP`, which is a token for a
        // log column and not a format noun, so the sentence says the thing in words instead.
        return format!(
            "{file_name}: its bytes are a container this build does not decode (the name says {name_says})"
        );
    }
    if stamped == SrcKind::Unsupported {
        // The bytes were read and understood; this machine has no decoder for what they are.
        format!("{file_name}: {bytes_say} by its bytes — not decodable on this machine (the name says {name_says})")
    } else {
        format!(
            "{file_name}: {bytes_say} by its bytes — routing to the {} decoder (the name says {name_says})",
            kind_tag(stamped)
        )
    }
}

/// v1.0.0-rc TAIL 3 (verifier Y1): THE MINT — the one place a finished candidate becomes a
/// `(kind, has_jpg)` pair, and therefore the one place the PASSENGER rule is written.
///
/// `sniffed` is the kind the classification produced for the shot's best finished candidate, or
/// `None` when the stem yielded no finished file at all. The four arms:
///
/// | finished candidate | RAW beside it | `kind` | `has_jpg` | the picture |
/// |---|---|---|---|---|
/// | decodable | either | that format | `true` | the finished file |
/// | undecodable | **yes** | `Jpeg` | `false` | **the RAW** (a PASSENGER) |
/// | undecodable | no | `Unsupported` | `false` | none — the "install the codec" card |
/// | none | yes | `Jpeg` | `false` | the RAW |
///
/// It is pure and it is shared because the round already shipped this table twice and got it wrong
/// the second time: `reclassify_hydrated` re-stamped `kind` from the sniff alone, so a RAW whose
/// same-stem sibling was a cloud placeholder at scan — rule (1), the name's kind stood, `has_jpg`
/// true — could hydrate into `kind = Unsupported` WITH `has_jpg` still true. That is a state this
/// mint cannot produce, and it cost the RAW its picture to the codec card until the next rescan.
fn mint_finished(sniffed: Option<SrcKind>, has_raw: bool) -> (SrcKind, bool) {
    match sniffed {
        Some(k) if k != SrcKind::Unsupported => (k, true),
        // A passenger: the file rides along (the caller keeps its path) and the RAW is the picture,
        // so the kind is the RAW-preview kind and `has_jpg` — "the finished slot is the picture" —
        // is false.
        Some(_) if has_raw => (SrcKind::Jpeg, false),
        Some(_) => (SrcKind::Unsupported, false),
        None => (SrcKind::Jpeg, false),
    }
}

/// Scan a folder and pair RAW+JPG by stem (logic from `spikes/real_bench`).
/// Result is sorted by name and assigned stable sequential ids.
///
/// Pairing rule (user 2026-07-08): a RAW pairs with its BEST finished sibling — the camera CR3+JPG
/// burst is ONE logical shot. But two *finished* images of the same stem (e.g. a JPG and a TIFF/PNG
/// export) are DISTINCT deliverables and must each appear as their own shot — only RAW+finished
/// collapses. When a stem yields more than one shot, the shot NAME (which is also the rating/selection
/// key) is disambiguated by extension (`weic2603c.jpg` / `weic2603c.tif`); a lone shot keeps the bare
/// stem, so the common RAW+JPG burst / single-image folder is unchanged.
pub fn scan_folder(dir: &Path) -> std::io::Result<Vec<Shot>> {
    scan_folder_counted(dir).map(|(shots, _)| shots)
}

/// [`scan_folder`], plus the number of AppleDouble sidecars it skipped (v0.9.63 / B-R5-1).
///
/// The count exists so the skip is not SILENT. A file vanishing from a folder listing with nothing
/// said anywhere is the shape this codebase keeps closing, and "where did IMG_7737 go?" has a very
/// different answer from "there was never a photograph there". The three app scan paths log one
/// line when it is non-zero; every other caller (tests, the posture bench, the probe example) wants
/// the plain `Vec` and takes the wrapper above.
pub fn scan_folder_counted(dir: &Path) -> std::io::Result<(Vec<Shot>, usize)> {
    scan_folder_with_metadata(dir).map(|scan| (scan.shots, scan.appledouble))
}

/// One coherent directory scan. Metadata is the same snapshot used for the cloud
/// gate, available to the caller's sort without reopening every directory entry.
/// Timings exclude the caller's review-data load, sort and UI application.
pub struct FolderScan {
    pub shots: Vec<Shot>,
    pub appledouble: usize,
    pub metadata: BTreeMap<PathBuf, std::fs::Metadata>,
    pub enumerate_ms: u128,
    pub headers_ms: u128,
    pub finish_ms: u128,
    pub header_reads: usize,
    pub readers: usize,
    pub changed_sources: std::collections::HashSet<PathBuf>,
}

/// A directory snapshot for one opening, shared by clicked/nearby/full classification.
/// Header results live only for this open; this is not a persistent file cache.
pub struct FolderCatalogue {
    dir: PathBuf,
    entries: Vec<CatalogueEntry>,
    heads: std::collections::HashMap<PathBuf, Option<Vec<u8>>>,
    touched: std::collections::HashSet<PathBuf>,
    changed: std::collections::HashSet<PathBuf>,
    simple_keys: std::cell::Cell<Option<bool>>,
    pub enumerate_ms: u128,
}

struct CatalogueEntry {
    path: PathBuf,
    metadata: Option<std::fs::Metadata>,
}

impl FolderCatalogue {
    pub fn read(dir: &Path, cancelled: impl Fn() -> bool) -> std::io::Result<Self> {
        scan_io::check_cancelled(&cancelled)?;
        let start = std::time::Instant::now();
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            scan_io::check_cancelled(&cancelled)?;
            let entry = entry?;
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                entries.push(CatalogueEntry { path: entry.path(), metadata: entry.metadata().ok() });
            }
        }
        scan_io::check_cancelled(&cancelled)?;
        Ok(Self { dir: dir.to_path_buf(), entries, heads: std::collections::HashMap::new(), touched: std::collections::HashSet::new(), changed: std::collections::HashSet::new(), simple_keys: std::cell::Cell::new(None), enumerate_ms: start.elapsed().as_millis() })
    }

    pub fn requested_shot(&mut self, path: &Path, cancelled: impl Fn() -> bool + Sync) -> std::io::Result<Option<Shot>> {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { return Ok(None) };
        if path.parent() != Some(self.dir.as_path()) || is_appledouble_sidecar(stem)
            || (!self.has_simple_keys() && (stem.contains('.') || stem.contains('('))) { return Ok(None); }
        let stems = std::collections::HashSet::from([stem.to_owned()]);
        let scan = scan_catalogue_subset(self, Some(1), &cancelled, Some(&stems))?;
        Ok(scan.shots.into_iter().find(|s| s.jpg.as_deref() == Some(path) || s.raw.as_deref() == Some(path)))
    }

    /// Exact one-shot groups: one finished image with an optional RAW, or one RAW.
    /// Complex splitting/collision namespaces defer to full classification.
    pub fn simple_candidates(&self) -> Option<Vec<SinglePhotoCandidate>> {
        self.simple_keys.set(Some(false));
        let mut groups: BTreeMap<String, (Option<&CatalogueEntry>, Option<&CatalogueEntry>)> = BTreeMap::new();
        for entry in &self.entries {
            let path = &entry.path;
            if path.file_name().and_then(|n| n.to_str()).is_some_and(is_appledouble_sidecar) { continue; }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
            let raw = RAW_EXTS.contains(&ext.as_str());
            let finished = [JPEG_EXTS, PNG_EXTS, TIFF_EXTS, WEBP_EXTS, HEIC_EXTS, JXL_EXTS, BMP_EXTS, GIF_EXTS, UNSUPPORTED_IMG_EXTS]
                .iter().any(|exts| exts.contains(&ext.as_str()));
            if !raw && !finished { continue; }
            let stem = path.file_stem()?.to_str()?;
            let group = groups.entry(stem.to_owned()).or_default();
            let slot = if raw { &mut group.0 } else { &mut group.1 };
            if slot.replace(entry).is_some() { return None; }
        }
        self.simple_keys.set(Some(true));
        Some(groups.into_iter().filter_map(|(name, (raw, finished))| {
            let primary = finished.or(raw)?;
            Some(SinglePhotoCandidate { name, path: primary.path.clone(), metadata: primary.metadata.clone() })
        }).collect())
    }

    fn has_simple_keys(&self) -> bool {
        // With exactly one shot per unique stem, no generated suffix can collide
        // with a dotted/parenthesized filename. This is a proof, not an extension guess.
        self.simple_keys.get().unwrap_or_else(|| self.simple_candidates().is_some())
    }

    pub fn scan_batch(&mut self, paths: &[PathBuf], cancelled: impl Fn() -> bool + Sync) -> std::io::Result<Option<FolderScan>> {
        if paths.is_empty() { return Ok(None); }
        let mut stems = std::collections::HashSet::new();
        for path in paths {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { return Ok(None) };
            if path.parent() != Some(self.dir.as_path()) || (!self.has_simple_keys() && (stem.contains('.') || stem.contains('('))) { return Ok(None); }
            stems.insert(stem.to_owned());
        }
        let scan = scan_catalogue_subset(self, Some(paths.len().clamp(1, 4)), &cancelled, Some(&stems))?;
        let complete = scan.shots.len() == paths.len() && paths.iter().all(|p| scan.shots.iter().any(|s| s.jpg.as_ref() == Some(p) || s.raw.as_ref() == Some(p)));
        Ok(complete.then_some(scan))
    }

    pub fn finish(&mut self, cancelled: impl Fn() -> bool + Sync) -> std::io::Result<FolderScan> {
        // Only sources used by an earlier stage need a second metadata check. Do
        // this off the UI thread before promotion can retain their decoded pixels.
        for entry in &mut self.entries {
            scan_io::check_cancelled(&cancelled)?;
            if !self.touched.contains(&entry.path) { continue; }
            let fresh = std::fs::symlink_metadata(&entry.path).ok();
            let stamp = |m: &std::fs::Metadata| {
                #[cfg(windows)]
                let cloud = { use std::os::windows::fs::MetadataExt; is_cloud_placeholder(m.file_attributes()) };
                #[cfg(target_os = "macos")]
                let cloud = { use std::os::macos::fs::MetadataExt; is_dataless(m.st_flags()) };
                #[cfg(not(any(windows, target_os = "macos")))]
                let cloud = false;
                (m.len(), m.modified().ok(), m.is_file(), cloud)
            };
            if fresh.as_ref().map(stamp) != entry.metadata.as_ref().map(stamp) {
                self.changed.insert(entry.path.clone());
                self.heads.remove(&entry.path);
                entry.metadata = fresh;
            }
        }
        self.entries.retain(|e| !self.changed.contains(&e.path) || e.metadata.as_ref().is_some_and(|m| m.is_file()));
        scan_catalogue_subset(self, None, &cancelled, None)
    }
}

pub fn scan_folder_with_metadata(dir: &Path) -> std::io::Result<FolderScan> {
    scan_folder_with_metadata_cancellable(dir, || false)
}

/// The complete scan contract above, with cooperative cancellation between file
/// operations. A superseded request returns Interrupted, never a partial shot list.
/// Workers stop before their next read after observing cancellation; an OS read
/// already in progress must finish.
pub fn scan_folder_with_metadata_cancellable(
    dir: &Path,
    is_cancelled: impl Fn() -> bool + Sync,
) -> std::io::Result<FolderScan> {
    scan_folder_with_readers(dir, None, &is_cancelled)
}

fn scan_folder_with_readers(
    dir: &Path,
    readers: Option<usize>,
    is_cancelled: &(impl Fn() -> bool + Sync),
) -> std::io::Result<FolderScan> {
    scan_folder_filtered(dir, readers, is_cancelled, None)
}

/// Resolve only the opened file's complete same-stem group, using the normal
/// content classification, pairing and cloud gates. Other files are listed but
/// never stat'ed or opened. Ambiguous dotted rating namespaces defer to the full
/// scan: an early picture must never borrow another photo's persisted edits.
pub fn scan_requested_shot(
    path: &Path,
    is_cancelled: impl Fn() -> bool + Sync,
) -> std::io::Result<Option<Shot>> {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { return Ok(None) };
    if stem.contains('.') || stem.contains('(') || is_appledouble_sidecar(stem) {
        return Ok(None);
    }
    let Some(dir) = path.parent() else { return Ok(None) };
    let scan = scan_folder_filtered(dir, Some(1), &is_cancelled, Some(stem))?;
    Ok(scan.shots.into_iter().find(|s| {
        s.jpg.as_deref() == Some(path) || s.raw.as_deref() == Some(path)
    }))
}

fn scan_folder_filtered(
    dir: &Path,
    readers: Option<usize>,
    is_cancelled: &(impl Fn() -> bool + Sync),
    only_stem: Option<&str>,
) -> std::io::Result<FolderScan> {
    let stems = only_stem.map(|s| std::collections::HashSet::from([s.to_owned()]));
    scan_folder_subset(dir, readers, is_cancelled, stems.as_ref())
}

/// A metadata-only candidate, never a classified/displayable Shot.
pub struct SinglePhotoCandidate {
    pub name: String,
    pub path: PathBuf,
    pub metadata: Option<std::fs::Metadata>,
}

/// The nearby stage is safe to rank without other files' bytes only when every
/// image has a unique stem and no RAW partner needs classification to choose its
/// primary file. Return None for those mixed groups; the full scanner owns them.
pub fn single_photo_candidates(
    dir: &Path,
    is_cancelled: impl Fn() -> bool,
) -> std::io::Result<Option<Vec<SinglePhotoCandidate>>> {
    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        scan_io::check_cancelled(&is_cancelled)?;
        let entry = entry?;
        if !entry.file_type().is_ok_and(|t| t.is_file()) { continue; }
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()).is_some_and(is_appledouble_sidecar) { continue; }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        if RAW_EXTS.contains(&ext.as_str()) { return Ok(None); }
        if ![JPEG_EXTS, PNG_EXTS, TIFF_EXTS, WEBP_EXTS, HEIC_EXTS, JXL_EXTS, BMP_EXTS, GIF_EXTS, UNSUPPORTED_IMG_EXTS]
            .iter().any(|exts| exts.contains(&ext.as_str())) { continue; }
        let Some(name) = path.file_stem().and_then(|n| n.to_str()) else { return Ok(None) };
        if !seen.insert(name.to_owned()) { return Ok(None); }
        candidates.push(SinglePhotoCandidate { name: name.to_owned(), path, metadata: entry.metadata().ok() });
    }
    Ok(Some(candidates))
}

/// Classify a small candidate window through the very same minting/pairing code.
/// The caller must still sort the returned shots and retain the full review map.
pub fn scan_requested_batch(
    paths: &[PathBuf], is_cancelled: impl Fn() -> bool + Sync,
) -> std::io::Result<Option<FolderScan>> {
    let Some(dir) = paths.first().and_then(|p| p.parent()) else { return Ok(None) };
    let mut stems = std::collections::HashSet::new();
    for path in paths {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { return Ok(None) };
        if path.parent() != Some(dir) || stem.contains('.') || stem.contains('(') { return Ok(None); }
        stems.insert(stem.to_owned());
    }
    let scan = scan_folder_subset(dir, None, &is_cancelled, Some(&stems))?;
    let complete = scan.shots.len() == paths.len() && paths.iter().all(|p| scan.shots.iter()
        .any(|s| s.jpg.as_ref() == Some(p) || s.raw.as_ref() == Some(p)));
    Ok(complete.then_some(scan))
}

fn scan_folder_subset(
    dir: &Path,
    readers: Option<usize>,
    is_cancelled: &(impl Fn() -> bool + Sync),
    only_stems: Option<&std::collections::HashSet<String>>,
) -> std::io::Result<FolderScan> {
    let mut catalogue = FolderCatalogue::read(dir, is_cancelled)?;
    // Legacy subset APIs keep their conservative namespace contract. The new
    // opening catalogue opts into its stronger whole-directory simple-key proof.
    if only_stems.is_some() { catalogue.simple_keys.set(Some(false)); }
    scan_catalogue_subset(&mut catalogue, readers, is_cancelled, only_stems)
}

fn scan_catalogue_subset(
    catalogue: &mut FolderCatalogue,
    readers: Option<usize>,
    is_cancelled: &(impl Fn() -> bool + Sync),
    only_stems: Option<&std::collections::HashSet<String>>,
) -> std::io::Result<FolderScan> {
    let simple_keys = only_stems.is_some() && catalogue.has_simple_keys();
    let dir = catalogue.dir.as_path();
    scan_io::check_cancelled(is_cancelled)?;
    let phase = std::time::Instant::now();
    let mut metadata = BTreeMap::new();
    let mut candidates = Vec::new();
    type Finished = (PathBuf, SrcKind);
    // Per stem: ALL raw files + ALL finished-image candidates (path + format). v0.8.36: raws are a Vec
    // (was a single slot that silently OVERWROTE) so two RAWs sharing a stem — IMG_0001.CR3 + IMG_0001.DNG
    // — both become visible/deletable shots instead of one shadowing the other (a later delete-shot then
    // recycled only the tracked file, orphaning the invisible one the user believed deleted).
    let mut map: BTreeMap<String, (Vec<PathBuf>, Vec<Finished>)> = BTreeMap::new();
    // v0.8.24 (D1): paths whose ATTRIBUTES mark a cloud (OneDrive) placeholder — read off the dir-entry's
    // metadata (Windows: the WIN32_FIND_DATA the scan ALREADY walked → no extra stat and, crucially, no
    // open/read → no hydration). A shot is tagged if EITHER of its pair files is here. Off Windows there is
    // no such notion → the set stays empty (and non-`mut`, so no unused-mut lint on the macOS port).
    #[cfg(windows)]
    let mut cloud_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // v1.0.0-rc TAIL (skeptic A, R1): macOS populates this too, from `st_flags`. Before the tail
    // this arm was the empty non-mut set, which meant rule (1) — "never read a cloud placeholder" —
    // was a WINDOWS-ONLY guarantee: on a Mac `scan_classify` sniffed everything, so opening an
    // iCloud Drive folder under "Optimise Mac Storage" would have MATERIALISED every image in it at
    // folder open. That is a direct hit on the merge round's own §6.1 rule, "a new platform probe
    // must answer the other platform's 'I don't know' arm".
    #[cfg(target_os = "macos")]
    let mut cloud_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let cloud_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // v0.8.99 (H3b): the HEIC classification, decided ONCE per scan and only if the folder actually
    // holds one. `wic_heif_codec_present()` is a read-only walk of WIC's codec registry on its own
    // throwaway thread (OnceLock-cached for the process) — cheap, but not free, and a HEIC-less
    // folder must not pay for it at all, so the cell is filled lazily at the first HEIC extension.
    // Once per SCAN rather than once per FILE keeps a 500-HEIC folder to one decision.
    let mut heic_kind: Option<SrcKind> = None;
    // v0.9.63 (B-R5-1): AppleDouble sidecars skipped this scan — see `is_appledouble_sidecar`.
    let mut appledouble = 0usize;
    // v1.0.0-rc TAIL (skeptic B, R2): the container the BYTES declared, per finished-image path,
    // carried from the chokepoint to the `Shot` that is built from it further down. `None` for a
    // file whose name and bytes agreed — which is almost every file, so the map stays tiny.
    let mut sniffed_of: BTreeMap<PathBuf, Option<SrcKind>> = BTreeMap::new();
    // v1.0.0-rc TAIL (skeptic A, O6): how many per-file honesty lines this scan's capped families
    // swallowed, so the folder can say so once at the end instead of going silently quiet.
    let mut suppressed_notes = 0usize;
    for entry in &catalogue.entries {
        scan_io::check_cancelled(is_cancelled)?;
        // Use the dir-entry's cached file type — no extra stat per file, and (unlike
        // Path::is_file) it does NOT follow reparse points / junctions, so a junction-heavy
        // folder can't stall the scan chasing links.
        // v0.9.63 (B-R5-1): BEFORE anything else touches this entry — before the extension is
        // classified, before the cloud-placeholder probe adds its path to a set. An AppleDouble
        // sidecar is not a photograph, so nothing downstream should ever learn its name.
        if entry.path.file_name().and_then(|n| n.to_str()).is_some_and(is_appledouble_sidecar) {
            appledouble += 1;
            continue;
        }
        let p = entry.path.clone();
        if let Some(wanted) = only_stems {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            if !wanted.contains(stem) {
                // Split-shot names may collide with a foreign dotted stem. The
                // full scan owns the global uniqueness pass in that rare case.
                if !simple_keys && wanted.iter().any(|w| stem.strip_prefix(w.as_str()).is_some_and(|s| s.starts_with('.') || s.starts_with(" ("))) {
                    return Ok(FolderScan {
                        shots: Vec::new(), metadata: BTreeMap::new(), appledouble: 0,
                        enumerate_ms: phase.elapsed().as_millis(), headers_ms: 0,
                        finish_ms: 0, header_reads: 0, readers: 1,
                        changed_sources: catalogue.changed.clone(),
                    });
                }
                continue;
            }
        }
        let entry_meta = entry.metadata.clone();
        if only_stems.is_some() { catalogue.touched.insert(p.clone()); }
        // v0.8.24 (D1): metadata-only placeholder probe (no data read — see the cloud_paths note above).
        // Windows-only; the dir-entry's cached metadata carries dwFileAttributes, so this adds no stat.
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if entry_meta.as_ref().map(|m| is_cloud_placeholder(m.file_attributes())).unwrap_or(false) {
                cloud_paths.insert(p.clone());
            }
        }
        // v1.0.0-rc TAIL (skeptic A, R1): the same probe on the same already-taken metadata, through
        // the macOS bit. `st_flags` is a plain `u32` field of `stat`, so this costs what the Windows
        // arm costs — nothing beyond the dir-entry metadata the loop already reads.
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            if entry_meta.as_ref().map(|m| is_dataless(m.st_flags())).unwrap_or(false) {
                cloud_paths.insert(p.clone());
            }
        }
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        let e = ext.as_str();
        let slot = map.entry(stem).or_default();
        if RAW_EXTS.contains(&e) {
            if let Some(m) = entry_meta { metadata.insert(p.clone(), m); }
            slot.0.push(p); // collect EVERY same-stem RAW (was `= Some(p)`, which dropped all but the last)
        } else {
            let kind = if JPEG_EXTS.contains(&e) {
                Some(SrcKind::Jpeg)
            } else if PNG_EXTS.contains(&e) {
                Some(SrcKind::Png)
            } else if TIFF_EXTS.contains(&e) {
                Some(SrcKind::Tiff)
            } else if WEBP_EXTS.contains(&e) {
                Some(SrcKind::Webp)
            } else if HEIC_EXTS.contains(&e) {
                // Decodable via the OS codec — WIC on Windows, Image I/O (CGImageSource) on macOS
                // (v0.9.9). A badge-only Unsupported on platforms with no HEVC/HEIF system decoder.
                // v0.8.99 (H3b): on WINDOWS that is now a fact, not an assumption — the Store
                // HEVC/HEIF Image Extension is not shipped with Windows, so the codec registry is
                // asked (once per scan, lazily). macOS needs no such gate: Image I/O decodes HEIC
                // natively with nothing to install, so it short-circuits to "decodable".
                Some(*heic_kind.get_or_insert_with(heic_scan_kind_here))
            } else if JXL_EXTS.contains(&e) {
                Some(SrcKind::Jxl)
            } else if BMP_EXTS.contains(&e) {
                Some(SrcKind::Bmp)
            } else if GIF_EXTS.contains(&e) {
                Some(SrcKind::Gif)
            } else if UNSUPPORTED_IMG_EXTS.contains(&e) {
                Some(SrcKind::Unsupported)
            } else {
                None // not an image file — ignore
            };
            if let Some(ext_kind) = kind {
                if let Some(m) = entry_meta { metadata.insert(p.clone(), m); }
                let placeholder = cloud_paths.contains(&p);
                candidates.push((p, ext_kind, placeholder));
            }
        }
    }
    let enumerate_ms = catalogue.enumerate_ms + phase.elapsed().as_millis();
    let phase = std::time::Instant::now();
    let unread: Vec<_> = candidates.iter().filter(|c| !catalogue.heads.contains_key(&c.0)).cloned().collect();
    let header_reads = unread.iter().filter(|c| !c.2).count();
    let readers = readers.unwrap_or_else(|| scan_io::reader_count(header_reads));
    let new_heads = scan_io::read_heads(&unread, readers, is_cancelled)?;
    for ((path, _, placeholder), head) in unread.into_iter().zip(new_heads) {
        // A transient failed read is not a reusable classification; a later stage
        // may succeed. Known placeholders are cached without ever opening them.
        if head.is_some() || placeholder { catalogue.heads.insert(path, head); }
    }
    let heads: Vec<_> = candidates.iter().map(|c| catalogue.heads.get(&c.0).cloned().flatten()).collect();
    let headers_ms = phase.elapsed().as_millis();
    let phase = std::time::Instant::now();
    // Resolve in the original listing order, on one thread. Parallel reads cannot
    // reorder format diagnostics, codec capability decisions or RAW partner choice.
    for ((p, ext_kind, placeholder), head) in candidates.into_iter().zip(heads) {
        scan_io::check_cancelled(is_cancelled)?;
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        let slot = map.entry(stem).or_default();
        // v1.0.0-rc (BYTES OVER NAMES): THE CHOKEPOINT. This is the single classification
        // authority in the tree — nine subsystems read the `SrcKind` minted here — so it is
        // the one place the file's own bytes get a vote. `scan_classify` states the three
        // rules; the two closures are what make rule (1) provable and rule (3) cheap.
        let (kind, disagreed) = scan_classify(
            ext_kind,
            placeholder,
            || head,
            || *heic_kind.get_or_insert_with(heic_scan_kind_here),
        );
        let shown = || sanitize_one_line(p.file_name().and_then(|s| s.to_str()).unwrap_or_default());
        let mut verdict = NoteVerdict::Repeat;
        if let Some(sniffed) = disagreed {
            // ONE line per disagreeing FILE (the key is the path), on the existing
            // once-per-session decode-note channel the app drains on its ~1.5 s report
            // cadence — so a 500-file folder logs 500 facts and not 500 × 18 workers.
            // An AGREEING file says nothing at all: silence is the normal case and a line
            // per scanned file would bury the ones that matter. v1.0.0-rc TAIL (skeptic B,
            // Y1): and the family is CAPPED, because the keys outlive the folder.
            verdict = note_sniff_disagreement(&p, &shown(), sniffed, kind, ext_kind);
        } else if placeholder {
            // v1.0.0-rc TAIL (skeptic B, Y2): rule (1) took the NAME, and now says so.
            // Same cap, same per-path key discipline — a folder of 500 placeholders is the
            // ordinary OneDrive case and must not be 500 uncapped lines either.
            static NAMED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            verdict = note_path_capped(
                &format!("placeholder-kind:{}", p.display()),
                &NAMED,
                SNIFF_NOTE_CAP,
                || placeholder_naming_line(&shown(), ext_kind),
            );
        }
        if verdict == NoteVerdict::Suppressed {
            suppressed_notes += 1;
        }
        sniffed_of.insert(p.clone(), disagreed);
        slot.1.push((p, kind));
    }
    note_scan_suppressed(dir, suppressed_notes);
    let mut shots: Vec<Shot> = Vec::new();
    for (stem, (mut raws, mut finished)) in map {
        scan_io::check_cancelled(is_cancelled)?;
        // Deterministic order: rank DESC (JPEG > TIFF > PNG > HEIC > Unsupported), then path — so a
        // RAW's partner is the camera-default JPEG and split-shot names/keys are stable across rescans
        // regardless of read_dir()'s OS/filesystem order.
        finished.sort_by(|a, b| finished_rank(b.1).cmp(&finished_rank(a.1)).then_with(|| a.0.cmp(&b.0)));
        raws.sort(); // v0.8.36: stable RAW order (no quality rank between CR3/DNG) so the JPG-partner pick is deterministic
        // The FIRST raw (lowest path) takes the best finished as its partner (mirrors the finished-multi
        // rule where the sole RAW claims the best finished — the JPG pairs with exactly one RAW); every
        // REMAINING raw becomes its own RAW-only shot (decoded from its embedded preview), and every
        // remaining finished its own finished-only shot. A stem with neither yields no specs → no shot.
        let mut specs: Vec<(Option<PathBuf>, Option<Finished>)> = Vec::new();
        let mut raws_it = raws.into_iter();
        if let Some(first_raw) = raws_it.next() {
            let partner = if finished.is_empty() { None } else { Some(finished.remove(0)) };
            specs.push((Some(first_raw), partner));
        }
        for r in raws_it {
            specs.push((Some(r), None)); // extra same-stem RAW → visible/deletable RAW-only shot
        }
        for f in finished {
            specs.push((None, Some(f)));
        }
        let multi = specs.len() > 1;
        for (raw, finished) in specs {
            let has_raw = raw.is_some();
            // Resolve the display slot + decoder kind:
            //  * a decodable finished image → use it;
            //  * else if there's a RAW → decode its embedded JPEG preview (kind Jpeg, no jpg path);
            //  * else a standalone image we can't decode → keep its path so the badge can reveal it.
            // v1.0.0-rc TAIL (skeptic A, Y3): AN `Unsupported` SIBLING IS KEPT, NEVER DROPPED.
            // The middle arm used to carry `if !has_raw`, so a RAW paired with a finished sibling
            // that this round newly re-stamps `Unsupported` — an AVIF named `.jpg`, or a `.HEIC` on
            // a box with no HEVC codec — fell to the `_` arm, the finished PATH was thrown away, and
            // the file became INVISIBLE AND UNDELETABLE in a culler whose whole job is deciding what
            // to keep. Before the round that same file was visible and failing, which is worse to
            // look at and better to live with. `finished_rank` already ranks `Unsupported` last, so
            // a genuine sibling still wins the RAW's partner slot; this only decides what happens
            // when the unsupported file is the best (or only) finished candidate there is.
            //
            // v1.0.0-rc MICRO-TAIL (A-Y3b, the flag upheld): THE PICTURE WINS, AND THE FILE STILL
            // RIDES ALONG. The tail's A-Y3 kept the undecodable sibling's PATH — which is what makes
            // it visible, deletable with the shot and listable on the panel, all for free, because
            // every file operation in the tree already enumerates `shot.jpg`. What it also did, and
            // should not have, was stamp the SHOT `Unsupported`, so a RAW + `X.HEIC` on a codec-less
            // box lost its photograph to an "install the codec" card. The ruling was about
            // visibility, never about which picture the shot shows.
            //
            // So the two questions are finally separated, and `has_jpg` is where — its doc has said
            // "False for a RAW-only or unsupported shot" since it was written, i.e. "there may be a
            // file here, but it is not the picture". A PASSENGER is exactly that state: the path is
            // kept, the kind is the RAW-preview kind the pre-round scan gave this shot, and
            // `has_jpg` is false. Everything that asks "what do I decode / probe / patch / badge?"
            // reads `has_jpg` and gets the RAW; everything that asks "what files are here?" reads
            // `jpg` and gets both. The card is reached only where the undecodable file is the shot's
            // SOLE source, which is the case it was written for.
            //
            // v1.0.0-rc TAIL 3 (verifier Y1): the four arms live in `mint_finished` because there
            // are TWO invokers — this one and `reclassify_hydrated` — and the second did not apply
            // the rule (L26: one mint, every caller).
            let (kind, has_jpg) = mint_finished(finished.as_ref().map(|(_, k)| *k), has_raw);
            let jpg = finished.map(|(p, _)| p);
            // Lone shot → bare stem (unchanged); a split stem → suffix the finished extension so the
            // name (== the rating key) never collides with its sibling. v0.8.36: a RAW-only split member
            // (no finished side, e.g. the extra same-stem DNG) has no jpg ext, so fall back to the RAW's
            // ext → `stem.cr3` / `stem.dng` stay distinct + meaningful (the final uniqueness pass below is
            // then a no-op rather than papering over a `stem` vs `stem (2)` collapse).
            let name = if multi {
                jpg.as_ref()
                    .and_then(|p| p.extension())
                    .or_else(|| raw.as_ref().and_then(|p| p.extension()))
                    .and_then(|e| e.to_str())
                    .map(|e| format!("{stem}.{}", e.to_ascii_lowercase()))
                    .unwrap_or_else(|| stem.clone())
            } else {
                stem.clone()
            };
            // v0.8.24 (D1): tagged if EITHER pair file is a cloud placeholder (borrow before the move).
            let cloud_placeholder = raw.as_deref().map_or(false, |rp| cloud_paths.contains(rp))
                || jpg.as_deref().map_or(false, |jp| cloud_paths.contains(jp));
            // v1.0.0-rc TAIL (skeptic B, R2): the sniffed container travels with the shot, because
            // `Unsupported` has no noun of its own and five surfaces need one that is not the name.
            let sniffed = jpg.as_ref().and_then(|p| sniffed_of.get(p).copied()).flatten();
            shots.push(Shot { id: 0, name, has_raw, has_jpg, raw, jpg, kind, cloud_placeholder, sniffed });
        }
    }
    // Stable order by name (the BTreeMap's stem order, with split shots landing adjacently by their
    // ext suffix).
    shots.sort_by(|a, b| a.name.cmp(&b.name));
    // Guarantee UNIQUE names — the name is the rating/selection/resume key, and a contrived double-
    // extension file (e.g. `A.tif.png`, whose file_stem() is `A.tif`, beside `A.jpg` + `A.tif`) could
    // make a split shot's `stem.ext` name coincide with a lone dotted-stem shot, silently merging their
    // ratings on save/load. Suffix any collision so no two shots ever share a key.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for s in shots.iter_mut() {
        scan_io::check_cancelled(is_cancelled)?;
        if !seen.insert(s.name.clone()) {
            let base = s.name.clone();
            let mut n = 2;
            while !seen.insert(format!("{base} ({n})")) {
                n += 1;
            }
            s.name = format!("{base} ({n})");
        }
    }
    // Sequential ids after the final naming.
    for (i, s) in shots.iter_mut().enumerate() {
        s.id = i;
    }
    scan_io::check_cancelled(is_cancelled)?;
    Ok(FolderScan {
        shots, appledouble, metadata, enumerate_ms, headers_ms,
        finish_ms: phase.elapsed().as_millis(), header_reads, readers,
        changed_sources: catalogue.changed.clone(),
    })
}

/// The trailing run of digits in a filename stem — the per-shot sequence number
/// cameras increment (`HWU_7841` → `"7841"`, `DSC_0123` → `"0123"`, `P1000123` →
/// `"1000123"`). Works across brand conventions since the counter is always the
/// suffix. Empty string if the stem ends in no digit (caller falls back to `name`).
pub fn frame_number(stem: &str) -> String {
    let mut rev: Vec<char> = stem.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
    rev.reverse();
    rev.into_iter().collect()
}

/// v0.8.57 (owner rider), extraction improved v0.8.61 (owner bug: `HWU_0544E` showed the
/// first-5 fallback because only a TRAILING digit run was detected; owner amendment: dotted
/// date/time names must yield something genuinely distinguishing, not the bare seconds):
/// the thumbnail NUMBER-CHIP label — never empty for a real shot. THE RULE, in order:
///  1. GROUP digit runs: consecutive runs separated by exactly ONE date/time punctuation
///     char (`.`, `-`, `:`) fuse into one logical number (`21.24.40` = one time, not three
///     scraps; a space or any text BREAKS the group). Take the LAST group carrying ≥ 2
///     digits, rendered as its digits CONCATENATED (separators dropped; a single-run group
///     is the run as written, leading zeros preserved). So: `HWU_0544E` → `0544`,
///     `IMG_1234` → `1234`, `7O7A1389` → `1389`, `DSC00042` → `00042`,
///     `Screenshot 2026-07-16 125538` → `125538` (the date group is passed over for the
///     LAST group, the time), and `截屏2025-12-13 21.24.40` → `212440` — the SAME time
///     field the undotted screenshot shape shows, exactly what differs between burst
///     neighbours (`IMG_1234 (2)` still → `1234`: the copy-suffix `(2)` is a 1-digit group).
///  2. else the TRAILING digit run (a single digit by now — `shot_1` → `1`, `a1b2` → `2`;
///     a lone MID-name digit is treated as noise, not a counter: `vacation2milan` stays
///     on the text fallback, matching v0.8.57);
///  3. else the v0.8.57 fallback: up to the first 5 characters, "…"-suffixed when longer
///     (the terminal case — the chip is never empty).
/// Every stem ENDING in an un-punctuated digit run is byte-identical to the old
/// trailing-run behaviour. ONE source: make_film_item feeds `FilmItem.number` from here,
/// so the filmstrip, the grid dock and the Selection tiles all inherit it uniformly.
/// (`frame_number` itself is BYTE-IDENTICAL — its trailing-digits contract still serves
/// seq_stem's tests and any counter-semantics caller; only this display extraction changed.)
pub fn tile_number_label(stem: &str) -> String {
    // The "numbers" of a stem are its fused digit GROUPS in order (see `scan_fused_groups`); the
    // folder-blind counter is the LAST group carrying ≥ 2 digits, rendered as its digits as written.
    let (groups, _) = scan_fused_groups(stem);
    if let Some(d) = groups.into_iter().rev().find(|g| g.len() >= 2) {
        return d;
    }
    let trailing = frame_number(stem); // a single digit at most here (any ≥2 run formed a group)
    if !trailing.is_empty() {
        return trailing;
    }
    if stem.chars().count() <= 5 {
        stem.to_string()
    } else {
        let head: String = stem.chars().take(5).collect();
        format!("{head}…")
    }
}

/// Scan a stem into its fused digit GROUPS in order, plus the stem's SKELETON — ONE pass shared by
/// [`tile_number_label`] (which takes the last ≥2-digit group) and the per-folder planner
/// ([`tile_number_plan`]) so both read the SAME tokenization. A GROUP is one logical number:
/// consecutive digit runs FUSED across a single `.`/`-`/`:` when another digit follows (`21.24.40`
/// = one group `"212440"`, `2026-07-16` = `"20260716"`); a space or any other char breaks the group.
/// Each group is its digits CONCATENATED (separators dropped, leading zeros kept); single-digit
/// groups are INCLUDED (`(2)` → `"2"`). The SKELETON replaces every group's whole span (digits +
/// any fused separators) with a single NUL (`'\0'`) marker and keeps all other characters verbatim
/// (`"10001_v1_2048"` → `"\0_v\0_\0"`): files sharing a skeleton form a group, and the j-th marker
/// is fused-group column j (the marker count always equals the group count).
/// WHY NUL rather than a printable like `#`: the marker is an IN-BAND sentinel, so it MUST live
/// outside the data alphabet — otherwise a LITERAL copy of it in a filename is indistinguishable
/// from a marker (`"1#2"` and `"##3"` would BOTH skeletonize to `"###"` yet carry 2 vs 1 groups, a
/// ragged collision that panicked `choose_number_column`'s `files[0].len()` indexing). NUL is the
/// one byte NO supported filesystem allows in a name (Windows forbids it; POSIX paths are
/// NUL-terminated), so a literal collision is STRUCTURALLY impossible — identical skeleton now truly
/// implies identical group count.
fn scan_fused_groups(stem: &str) -> (Vec<String>, String) {
    // Byte scan is UTF-8-safe: digit runs + the separator set are pure ASCII, and the two slice
    // cursors (`last`, `i`) only ever land on an ASCII (digit) boundary or a string end — never
    // mid-codepoint. A non-ASCII byte simply advances `i` in the "other" arm (breaks a group).
    let b = stem.as_bytes();
    let mut groups: Vec<String> = Vec::new();
    let mut skeleton = String::new();
    let mut i = 0usize;
    let mut last = 0usize; // start of the plain-text run not yet copied into the skeleton
    while i < b.len() {
        if b[i].is_ascii_digit() {
            skeleton.push_str(&stem[last..i]); // the text between the previous group and this one
            let mut digits = String::new();
            loop {
                let start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                digits.push_str(&stem[start..i]);
                // Fuse across exactly one '.', '-' or ':' when another digit follows.
                if i + 1 < b.len() && matches!(b[i], b'.' | b'-' | b':') && b[i + 1].is_ascii_digit() {
                    i += 1; // consume the separator; the loop continues the group
                } else {
                    break;
                }
            }
            groups.push(digits);
            skeleton.push('\0'); // NUL group marker: the ONE byte no filesystem allows in a name, so
                                 // a literal filename char can never collide with it (see doc above).
            last = i;
        } else {
            i += 1;
        }
    }
    skeleton.push_str(&stem[last..]);
    (groups, skeleton)
}

/// A per-FOLDER decision for the thumbnail number chip: for each filename SKELETON shared by ≥ 2
/// files, WHICH fused-group column best distinguishes those files. Built once per scan by
/// [`tile_number_plan`], applied per file by [`tile_number_label_planned`]. Skeletons with no
/// entry (singletons, or groups no column can distinguish) fall back to the folder-blind
/// [`tile_number_label`]. Default/empty for a no-folder / one-file / no-plannable-group scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TileNumberPlan {
    /// skeleton → the 0-based fused-group column each matching file should display.
    chosen: std::collections::HashMap<String, usize>,
}

impl TileNumberPlan {
    /// How many skeletons carry a chosen column (diagnostics/tests).
    pub fn len(&self) -> usize {
        self.chosen.len()
    }
    /// No skeleton has a plan — every file falls back to [`tile_number_label`].
    pub fn is_empty(&self) -> bool {
        self.chosen.is_empty()
    }
}

/// Build the per-folder number-chip plan over every shot's `seq_stem`. Groups the stems by
/// SKELETON; for each group of n ≥ 2 files, classifies each fused-group column (constant /
/// always-unique / varying) and picks the display column by the owner's ruling — see
/// [`choose_number_column`]. Pure + order-independent (the plan depends only on the SET of stems),
/// so it is safe to rebuild on every folder scan / delete-recover reconciliation.
pub fn tile_number_plan(stems: &[&str]) -> TileNumberPlan {
    let folder_n = stems.len();
    // skeleton → the per-file fused-group columns (each inner Vec is one file's groups, in order).
    let mut by_skeleton: std::collections::HashMap<String, Vec<Vec<String>>> =
        std::collections::HashMap::new();
    for s in stems {
        let (groups, skeleton) = scan_fused_groups(s);
        by_skeleton.entry(skeleton).or_default().push(groups);
    }
    let mut chosen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (skeleton, files) in &by_skeleton {
        if let Some(col) = choose_number_column(files, folder_n) {
            chosen.insert(skeleton.clone(), col);
        }
    }
    TileNumberPlan { chosen }
}

/// The column pick for ONE skeleton group. `files` are the group's per-file fused-group columns —
/// identical skeleton ⇒ identical group count (the NUL skeleton marker cannot collide with a
/// literal filename char, so the lengths genuinely match); `folder_n` is the whole folder's file
/// count (for the P2 majority gate). Defence in depth: should a future tokenizer change ever feed
/// RAGGED groups, this returns `None` (per-file fallback) rather than index-panicking on `f[j]`.
/// Owner ruling, in order:
///   • **P1** — the RIGHTMOST always-unique column whose every value has ≥ 2 digits. Applies to
///     ANY group with n ≥ 2 (`10001_v1_2048`: the constant `2048` tail and the 1-digit `v#` are
///     rejected, the unique 5-digit head wins; two different-base copies `IMG_1234 (2)` /
///     `IMG_1235 (2)` → their base column).
///   • **P2** (only if no P1 column) — the column with the MOST distinct values among the
///     varying/always-unique columns, NO length floor; ties go RIGHTMOST. GATED: applies only when
///     the group is a STRICT MAJORITY of the folder (`group_n * 2 > folder_n`) — a folder FILLED
///     with `IMG_1234 (1)/(2)/(3)` shows `1/2/3`, but a FEW same-base copy outliers in a plain
///     `IMG_####` folder (or a 50/50 split) do NOT, falling through to P3.
///   • **P3** — no column (returns `None`); the file uses the folder-blind `tile_number_label`.
fn choose_number_column(files: &[Vec<String>], folder_n: usize) -> Option<usize> {
    let n = files.len();
    if n < 2 {
        return None; // P3: a singleton skeleton can't be distinguished by any column
    }
    let ncols = files[0].len();
    // BELT (defence in depth): the NUL marker makes identical-skeleton ⇒ identical group count
    // structurally guaranteed, so this never trips today — but a future tokenizer change MUST
    // degrade to the per-file fallback, never panic on the unguarded `f[j]` below. Ragged ⇒ None.
    if files.iter().any(|f| f.len() != ncols) {
        return None; // P3: ragged groups share no common column — every file falls back per file
    }
    // P1: the RIGHTMOST always-unique column whose every value carries ≥ 2 digits.
    let mut p1: Option<usize> = None;
    for j in 0..ncols {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut all_ge2 = true;
        for f in files {
            if f[j].len() < 2 {
                all_ge2 = false;
            }
            seen.insert(f[j].as_str());
        }
        if all_ge2 && seen.len() == n {
            p1 = Some(j); // ascending j ⇒ the last match kept is the RIGHTMOST
        }
    }
    if p1.is_some() {
        return p1;
    }
    // P2: gated on a strict folder majority. Most-distinct column wins; ties go rightmost.
    if n * 2 <= folder_n {
        return None; // P3
    }
    let mut best: Option<(usize, usize)> = None; // (distinct_count, column)
    for j in 0..ncols {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for f in files {
            seen.insert(f[j].as_str());
        }
        let distinct = seen.len();
        // ≥ 2 distinct = a candidate; `>=` with ascending j keeps the RIGHTMOST on a tie.
        let take = match best {
            Some((bd, _)) => distinct >= bd,
            None => true,
        };
        if distinct >= 2 && take {
            best = Some((distinct, j));
        }
    }
    best.map(|(_, j)| j)
}

/// Apply a folder [`TileNumberPlan`] to ONE stem: if the plan has a column for this stem's
/// skeleton, show the file's OWN digit string in that column (as written, leading zeros kept);
/// otherwise fall back to the folder-blind [`tile_number_label`]. The SOLE behavioural difference
/// from `tile_number_label` — it needs the whole folder's stems (via the plan) to know which
/// column actually varies across neighbours.
pub fn tile_number_label_planned(stem: &str, plan: &TileNumberPlan) -> String {
    if !plan.is_empty() {
        let (groups, skeleton) = scan_fused_groups(stem);
        if let Some(&col) = plan.chosen.get(&skeleton) {
            if let Some(g) = groups.get(col) {
                return g.clone();
            }
        }
    }
    tile_number_label(stem)
}

// ───────────────────────────────── frame paths ───────────────────────────────

/// Fast scrub frame: DCT shrink-decode to ~`max_dim` long side, re-encode JPEG.
/// Uses the JPG when present, else the CR3's largest embedded preview.
pub fn fast_frame(shot: &Shot, max_dim: u32) -> Result<Frame> {
    let (rgb, w, h) = decode_source_rgb(shot, Some(max_dim))?;
    Ok(Frame { mime: "image/jpeg", bytes: encode_jpeg(&rgb, w, h, 88, None)? })
}

/// Reference frame: full 45 MP decode of the JPG, Lanczos downscale to `max_dim`,
/// high-quality JPEG. (True raw fidelity lives in [`develop_raw`].)
pub fn reference_frame(shot: &Shot, max_dim: u32) -> Result<Frame> {
    let (rgb, w, h) = decode_source_rgb(shot, None)?;
    let (rgb, w, h) = resize_to_long(rgb, w, h, max_dim)?;
    Ok(Frame { mime: "image/jpeg", bytes: encode_jpeg(&rgb, w, h, 95, None)? })
}

/// Filmstrip thumbnail (~`px` long side).
pub fn thumbnail(shot: &Shot, px: u32) -> Result<Frame> {
    let (rgb, w, h) = decode_source_rgb(shot, Some(px.saturating_mul(2).max(128)))?;
    let (rgb, w, h) = resize_to_long(rgb, w, h, px)?;
    Ok(Frame { mime: "image/jpeg", bytes: encode_jpeg(&rgb, w, h, 82, None)? })
}

/// True raw develop (on-demand "Develop RAW"): decode + demosaic + camera WB +
/// cam→sRGB matrix + gamma via rawler, then Lanczos downscale to `max_dim`.
pub fn develop_raw(shot: &Shot, max_dim: u32) -> Result<Frame> {
    let raw = shot.raw.as_ref().context("shot has no raw file to develop")?;
    let dynimg = raw_export::develop_raw_image_for_viewer(raw)
        .map_err(|e| anyhow::anyhow!("raw develop failed: {e:?}"))?;
    let (w, h) = (dynimg.width(), dynimg.height());
    // rawler returns its own (possibly 16-bit) image type; drop to an 8-bit RGB
    // byte vec so we stay decoupled from rawler's `image` crate version.
    let rgb = dynimg.to_rgb8().into_raw();
    let (rgb, w, h) = resize_to_long(rgb, w, h, max_dim)?;
    Ok(Frame { mime: "image/jpeg", bytes: encode_jpeg(&rgb, w, h, 92, None)? })
}

/// Camera-native Bayer plane + everything needed to develop it. The CRX
/// decompress (rawler) stays on the CPU; the per-pixel develop math runs
/// elsewhere (see `falcon-gpu`). `rggb == false` tells callers to fall back to the
/// CPU [`develop_raw`] path (the GPU shader only implements the RGGB phase).
pub struct Cfa {
    pub data: Vec<u16>,
    pub width: u32,
    pub height: u32,
    /// Recommended crop (the JPG framing), in full-sensor pixels.
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_w: u32,
    pub crop_h: u32,
    pub black: f32,
    pub white: f32,
    /// Per-channel white-balance multipliers (R, G, B), applied before the matrix.
    pub wb: [f32; 3],
    /// Camera-RGB -> linear sRGB for the original GPU preview calibration.
    pub cam_to_srgb: [[f32; 3]; 3],
    pub rggb: bool,
}

/// v0.8.143 (A1) — DOES THIS SENSOR'S FILTER ARRAY MATCH THE ONE THE GPU SHADER DRAWS?
///
/// The shader in `falcon-gpu` is a 2×2 bilinear demosaic with the RGGB phase hard-coded:
/// `(col,row) (0,0)=R (1,0)=G (0,1)=G (1,1)=B`. This predicate is that shader's own precondition,
/// written in the CFA's terms — a 2×2 repeat whose top-left is red and whose bottom-right is blue.
///
/// IT REPLACES A SUBSTRING TEST THAT SILENTLY DEVELOPED X-TRANS SENSORS WRONG. The old line was
/// `format!("{:?}", raw.camera.cfa).contains("RGGB")`, and rawler's `Debug for CFA` prints the
/// PATTERN STRING — which for a 6×6 Fuji X-Trans array is 36 characters of R/G/B. All five X-Trans
/// layouts in rawler 0.7.2's camera database (35 Fuji bodies: `GGRGGBGGBGGRBRGRBGGGBGGRGGRGGBRBGBRG`,
/// `RBGBRGGGRGGBGGBGGRBRGRBGGGBGGRGGRGGB`, `GBGGRGRGRBGBGBGGRGGRGGBGBGBRGRGRGGBG`,
/// `GRBGBRBGGRGGRGGBGGGBRGRBRGGBGGBGGRGG`, `GGRGGBGGBGGRBRGRGBGGBGGRGGRGGBRBGBRG`) contain "RGGB"
/// somewhere inside them, so every Fuji RAF answered TRUE and took the 2×2 shader — which cannot
/// demosaic a 6×6 array at all. This predicate fixes that GPU routing error. The historical claim
/// that the CPU fallback stayed correct was field-falsified: rawler 0.7.2 also sent X-Trans to
/// Bayer PPG. The CPU wrappers now share the explicit Markesteijn route in `xtrans.rs`.
///
/// Every case this narrows falls to CPU development; it is not evidence of that developer's quality.
///
/// The original predicate regression was derived from rawler 0.7.2's camera database and CFA size
/// table without a RAF fixture. The later X-Trans round adds separate real-file and pixel evidence;
/// [`the_rggb_predicate_is_the_shaders_own`] still proves only the GPU admission predicate.
pub(crate) fn cfa_is_rggb(cfa: &rawler::CFA) -> bool {
    // rawler's `color_at(row, col)`; colour indices R=0, G=1, B=2 (cfa.rs CFA_COLOR_*).
    cfa.width == 2 && cfa.height == 2 && cfa.color_at(0, 0) == 0 && cfa.color_at(1, 1) == 2
}

/// Decompress a raw file to its Bayer CFA plane and original GPU preview parameters.
/// The GPU's bilinear demosaic, scalar levels and highlight clipping differ from CPU development;
/// this is not a pixel/color equivalence guarantee. Manufactured RAW exports use the CPU path.
pub fn extract_cfa(shot: &Shot) -> Result<Cfa> {
    let raw_path = shot.raw.as_ref().context("shot has no raw file")?;
    let raw = extract_raw_pixels(raw_path, &RawDecodeParams::default())
        .map_err(|e| anyhow::anyhow!("raw decompress failed: {e:?}"))?;
    let data = match raw.data {
        RawImageData::Integer(v) => v,
        RawImageData::Float(_) => bail!("float raw not supported by GPU develop"),
    };
    let (width, height) = (raw.width as u32, raw.height as u32);
    // The GPU develop uploads `data` as a tightly-packed width×height R16 texture, so a
    // padded/short rawler buffer would panic the worker (no fallback). Bail here instead so
    // the caller falls back to CPU develop (B13).
    if data.len() != (width as usize) * (height as usize) {
        bail!(
            "unexpected CFA buffer length {} for {}×{}",
            data.len(),
            width,
            height
        );
    }

    let black = raw.blacklevel.as_vec().first().copied().unwrap_or(0.0);
    let white = *raw.whitelevel.0.first().unwrap_or(&16383) as f32;
    let wb = [
        if raw.wb_coeffs[0].is_nan() { 1.0 } else { raw.wb_coeffs[0] },
        if raw.wb_coeffs[1].is_nan() { 1.0 } else { raw.wb_coeffs[1] },
        if raw.wb_coeffs[2].is_nan() { 1.0 } else { raw.wb_coeffs[2] },
    ];

    let fm = raw
        .color_matrix
        .get(&Illuminant::D65)
        .or_else(|| raw.color_matrix.values().next())
        .context("no colour matrix in raw")?;
    if fm.len() < 9 {
        bail!("unexpected colour matrix length {}", fm.len());
    }
    let xyz2cam = [[fm[0], fm[1], fm[2]], [fm[3], fm[4], fm[5]], [fm[6], fm[7], fm[8]]];
    const SRGB_TO_XYZ_D65: [[f32; 3]; 3] = [
        [0.4124564, 0.3575761, 0.1804375],
        [0.2126729, 0.7151522, 0.0721750],
        [0.0193339, 0.1191920, 0.9503041],
    ];
    let mut rgb2cam = mat3_mul(xyz2cam, SRGB_TO_XYZ_D65);
    for row in rgb2cam.iter_mut() {
        let sum = row[0] + row[1] + row[2];
        if sum.abs() > 1e-9 {
            for v in row.iter_mut() {
                *v /= sum;
            }
        }
    }
    let cam_to_srgb = mat3_inv(rgb2cam);

    let (crop_x, crop_y, crop_w, crop_h) = match raw.crop_area.or(raw.active_area) {
        Some(r) => (r.p.x as u32, r.p.y as u32, r.d.w as u32, r.d.h as u32),
        None => (0, 0, width, height),
    };
    // v0.8.143 (A1): the SHADER'S precondition, not a substring of the pattern string — see
    // `cfa_is_rggb`. The old `format!("{:?}", …).contains("RGGB")` answered TRUE for all five
    // 6×6 X-Trans layouts (35 Fuji bodies), sending them to a 2×2 demosaic that cannot read them.
    let rggb = cfa_is_rggb(&raw.camera.cfa);

    Ok(Cfa {
        data, width, height, crop_x, crop_y, crop_w, crop_h, black, white, wb, cam_to_srgb, rggb,
    })
}

/// Estimate the as-shot white-balance colour temperature (Kelvin) from a RAW's WB
/// multipliers + colour matrix. Brand-agnostic — works for any RAW rawler decodes
/// (Canon/Sony/Nikon/…) because it derives the value from the recorded WB rather than
/// reading a proprietary MakerNote tag (standard EXIF has no colour temperature). This
/// is the number photographers want (the actual K), not the WhiteBalance mode flag.
///
/// Method: the camera-space colour of the as-shot illuminant is the inverse of the WB
/// multipliers; map it back to XYZ via the (inverse) colour matrix, take the xy
/// chromaticity, and convert to a correlated colour temperature with McCamy's formula.
/// Uses a metadata-only (`dummy`) decode, so there's NO pixel decompression — only the
/// header / MakerNote is parsed. Returns the CCT rounded to 50 K, or None if unavailable.
pub fn wb_kelvin(raw_path: &Path) -> Option<u32> {
    let src = rawler::rawsource::RawSource::new(raw_path).ok()?;
    let decoder = rawler::get_decoder(&src).ok()?;
    let raw = decoder.raw_image(&src, &RawDecodeParams::default(), true).ok()?;

    let (r, g, b) = (raw.wb_coeffs[0], raw.wb_coeffs[1], raw.wb_coeffs[2]);
    if !(r.is_finite() && g.is_finite() && b.is_finite()) || r <= 0.0 || g <= 0.0 || b <= 0.0 {
        return None;
    }
    let fm = raw.color_matrix.get(&Illuminant::D65).or_else(|| raw.color_matrix.values().next())?;
    if fm.len() < 9 {
        return None;
    }
    // xyz2cam (XYZ→camera); invert to map the camera-space illuminant back to XYZ.
    let xyz2cam = [[fm[0], fm[1], fm[2]], [fm[3], fm[4], fm[5]], [fm[6], fm[7], fm[8]]];
    let cam2xyz = mat3_inv(xyz2cam);
    let cam = [1.0 / r, 1.0 / g, 1.0 / b]; // as-shot illuminant in camera space
    let xyz = [
        cam2xyz[0][0] * cam[0] + cam2xyz[0][1] * cam[1] + cam2xyz[0][2] * cam[2],
        cam2xyz[1][0] * cam[0] + cam2xyz[1][1] * cam[1] + cam2xyz[1][2] * cam[2],
        cam2xyz[2][0] * cam[0] + cam2xyz[2][1] * cam[1] + cam2xyz[2][2] * cam[2],
    ];
    let sum = xyz[0] + xyz[1] + xyz[2];
    if sum <= 0.0 {
        return None;
    }
    let (x, y) = (xyz[0] / sum, xyz[1] / sum);
    let denom = 0.1858 - y;
    if denom.abs() < 1e-6 {
        return None;
    }
    let n = (x - 0.3320) / denom; // McCamy 1992
    let cct = 449.0 * n * n * n + 3525.0 * n * n + 6823.3 * n + 5520.33;
    if !cct.is_finite() || cct < 1500.0 || cct > 20000.0 {
        return None;
    }
    Some(((cct / 50.0).round() as u32) * 50)
}

// ──────────────────────── raw RGBA output (GPU-present path) ─────────────────
// These skip JPEG encode entirely: the bytes go to a <canvas> texture, so there's
// no Rust encode and no browser re-decode (PLAN §16, GPU present).

/// Expand packed RGB8 to RGBA8 with opaque alpha (canvas/ImageData wants RGBA).
pub fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    // Pre-fill alpha with a single memset, then write only the RGB bytes — no per-pixel
    // alpha store, and the fixed-index 3-byte copy over zipped exact chunks auto-vectorizes.
    // The old `chunks + copy_from_slice + set alpha` ran a few ms/frame slower (scrub spike).
    let n = rgb.len() / 3;
    let mut out = vec![255u8; n * 4];
    for (dst, src) in out.chunks_exact_mut(4).zip(rgb.chunks_exact(3)) {
        dst[0] = src[0];
        dst[1] = src[1];
        dst[2] = src[2];
        // dst[3] already 255 from the fill
    }
    out
}

/// Lanczos-downscale packed RGB8 so its long side ≤ `long` (no-op if already ≤).
/// Lets the nvJPEG path finish a GPU-decoded RGB buffer through the same SIMD
/// resize the CPU decoders use. nvJPEG already scales on-device to ≈ the target,
/// so this is usually a no-op; it only trims a source that overshoots `long`.
pub fn downscale_rgb(rgb: Vec<u8>, w: u32, h: u32, long: u32) -> (Vec<u8>, u32, u32) {
    if w == 0 || h == 0 || w.max(h) <= long {
        return (rgb, w, h);
    }
    // resize_to_long only errs on a zero dimension (excluded above); on the
    // practically unreachable resizer error, keep the (larger) source frame.
    let unscaled = (rgb.clone(), w, h);
    resize_to_long(rgb, w, h, long).unwrap_or(unscaled)
}

/// As [`downscale_rgb`], then expand to canvas-ready RGBA8 (the rawframe path).
pub fn downscale_rgb_to_rgba(rgb: Vec<u8>, w: u32, h: u32, long: u32) -> (Vec<u8>, u32, u32) {
    let (rgb, w, h) = downscale_rgb(rgb, w, h, long);
    (rgb_to_rgba(&rgb), w, h)
}

/// Lanczos-downscale packed RGBA8 so the long side ≤ `long` (no-op if already ≤).
/// Used to make a tiny "frosted-glass backdrop" from a fast frame: shrunk hard
/// here, then up-scaled by the GPU under a panel, the loss of detail reads as blur.
pub fn downscale_rgba(rgba: &[u8], w: u32, h: u32, long: u32) -> (Vec<u8>, u32, u32) {
    if w == 0 || h == 0 || w.max(h) <= long {
        return (rgba.to_vec(), w, h);
    }
    let (nw, nh) = if w >= h {
        (long, ((long as u64 * h as u64) / w as u64).max(1) as u32)
    } else {
        (((long as u64 * w as u64) / h as u64).max(1) as u32, long)
    };
    let Ok(src) = Image::from_vec_u8(w, h, rgba.to_vec(), PixelType::U8x4) else {
        return (rgba.to_vec(), w, h);
    };
    let mut dst = Image::new(nw, nh, PixelType::U8x4);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    if Resizer::new().resize(&src, &mut dst, &opts).is_err() {
        return (rgba.to_vec(), w, h);
    }
    (dst.into_vec(), nw, nh)
}

/// Positive area reduction for blur-only inputs. Lanczos is retained for displayed
/// photo/thumbnail pixels; its negative lobes must not add ringing to frosted glass.
pub fn downscale_frost_rgba(rgba: &[u8], w: u32, h: u32, long: u32) -> (Vec<u8>, u32, u32) {
    if w == 0 || h == 0 || w.max(h) <= long {
        return (rgba.to_vec(), w, h);
    }
    let (nw, nh) = if w >= h {
        (long, ((long as u64 * h as u64) / w as u64).max(1) as u32)
    } else {
        (((long as u64 * w as u64) / h as u64).max(1) as u32, long)
    };
    let Ok(src) = Image::from_vec_u8(w, h, rgba.to_vec(), PixelType::U8x4) else {
        return (rgba.to_vec(), w, h);
    };
    let mut dst = Image::new(nw, nh, PixelType::U8x4);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Box)).use_alpha(true);
    if Resizer::new().resize(&src, &mut dst, &opts).is_err() {
        return (rgba.to_vec(), w, h);
    }
    (dst.into_vec(), nw, nh)
}

// ───────────── v0.8.101 (S1/S2): the HEIC browse-lane strategy — lanes and tags ─────────────
// Two levers, both HEIC-only, both reversible with ONE environment switch ([`classic_heic`]):
//   * S1 — the 256 px THUMB tier reads the file's EMBEDDED PREVIEW (WIC `GetThumbnail`) instead of
//     full-decoding a 12–48 MP HEVC frame for a tile the size of a postage stamp.
//   * S2 — the FAST/scrub tier decodes AT SCALE through `IWICBitmapSourceTransform`, so `scale_to`
//     finally buys something for HEIC (it was a pure no-op: decode full, then Lanczos).
// The DETAIL/full-res tier and the ROI source buffer are deliberately OUT of both: 1:1 needs
// native pixels, and the sharp tier's fidelity is not a place to trade resamplers for milliseconds.

/// Which browse tier is asking for a frame. The decoder reads it for exactly two HEIC decisions,
/// and both are STRUCTURAL — a tier cannot opt into a lane it was not handed, so "the scrub tier
/// accidentally shows a 576 px preview" is not a bug that can be written here, it is a call that
/// does not typecheck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The 256 px filmstrip / Selection-grid tier. The ONLY lane that may be served from the file's
    /// embedded preview (S1) — and therefore the only lane that can ever return
    /// [`FrameSource::EmbeddedPreview`]. Also takes S2's decode-at-scale when the preview declines.
    Thumb,
    /// The fast / scrub tier. Decode-at-scale is welcome (S2); the MAIN image, always.
    Fast,
    /// Everything else — the detail/full-res tier, the ROI source buffer, and the legacy
    /// `fast_frame`/`reference_frame`/`thumbnail` helpers. The MAIN image at NATIVE resolution,
    /// Lanczos-downscaled afterwards if asked: byte-for-byte the pre-v0.8.101 path.
    Native,
}

/// Where a decoded browse frame's pixels actually came from. Returned by [`browse_frame_rgba`] so
/// the caller can never MISTAKE a preview-sourced frame for a decode of the real image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSource {
    /// The container's MAIN image, decoded (at scale or in full). Every tier's normal answer.
    MainImage,
    /// v0.8.101 (S1): the file's EMBEDDED preview item. Cheap, but bounded by whatever size the
    /// camera chose to write (576×432 on an iPhone HEIC) — so it is fit for the thumb tier and
    /// NOTHING else. See [`FrameSource::cache_dim`].
    EmbeddedPreview,
}

/// The dim-bucket sentinel a preview-sourced frame is filed under: `0`, which is OUTSIDE the
/// bucket alphabet (every real want is ≥ 1, and `l2::dim_satisfies` refuses this value outright).
/// A frame tagged with it can never satisfy ANY fast-tier request — that is the point.
pub const PREVIEW_CACHE_DIM: u32 = 0;

impl FrameSource {
    /// The FastCache/L2 dim bucket a frame from this source may be filed under, given the `want`
    /// the tier asked for. `MainImage` keeps the existing convention (the want, never the pixel
    /// long side). `EmbeddedPreview` collapses to [`PREVIEW_CACHE_DIM`] — the structural half of
    /// the S1 caveat: even if a preview frame were somehow handed to the fast tier, its bucket
    /// could never satisfy a scrub request.
    #[inline]
    pub fn cache_dim(self, want: u32) -> u32 {
        match self {
            FrameSource::MainImage => want,
            FrameSource::EmbeddedPreview => PREVIEW_CACHE_DIM,
        }
    }
}

/// v0.8.101: the ONE-SWITCH revert. `FALCON_CLASSIC_HEIC=1` puts BOTH S1 and S2 back to sleep and
/// restores the v0.8.100 HEIC paths exactly — every tier full-decodes the main image and
/// Lanczos-downscales afterwards. Read once per process (`OnceLock`): a decode lane must not be
/// able to change its mind mid-session, and an env read per decode would be a syscall on the hot
/// path. The native app logs the state at boot, so a log answers "which paths ran?" with no guessing.
pub fn classic_heic() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| classic_heic_from_env(std::env::var("FALCON_CLASSIC_HEIC").ok().as_deref()))
}

/// The switch's PARSE, split out so it is testable without touching the process environment —
/// [`classic_heic`] latches in a `OnceLock`, so a test that set the variable would poison every
/// later test in the same binary. Exactly `"1"` arms it: a user who writes `FALCON_CLASSIC_HEIC=0`
/// means "off", and an any-non-empty-value reading would hand them the opposite of what they asked.
#[inline]
pub fn classic_heic_from_env(v: Option<&str>) -> bool {
    v == Some("1")
}

/// v0.8.148 (E5): the HARDWARE HEIC LANE'S DOOR, and the only wire from this crate into it.
///
/// # Why a function pointer and not a call
///
/// The hardware lane lives in `falcon-hwdec`, which DEPENDS ON THIS CRATE (E1 tells it where the
/// tiles are). The dependency cannot run both ways, so the shipping app — which depends on both —
/// installs the door at boot and this crate calls it without ever naming the type. That is not a
/// workaround; it is what keeps `falcon-decode` a pure library the spikes can still link, and it is
/// what makes the lane's absence the ordinary case rather than a `cfg`.
///
/// # The contract the hook must honour
///
/// * `Some((rgb, w, h))` — packed RGB8, stride `w*3`, `irot` APPLIED, UN-COLOUR-MANAGED in the
///   file's own gamut, dims equal to what [`scaled_dims`] would give for `scale_to`. Exactly the
///   contract the WIC rungs already meet, because the caller runs [`finish_source`] over the answer
///   either way and the CM chain downstream is keyed on the FILE, not on the route.
/// * `None` — DECLINE. Never an error, never a panic: the caller falls to the WIC ladder as though
///   the hook were not installed at all, which is plan non-negotiable #5 written as control flow.
///   The hook is responsible for parking its own once-per-reason note ([`note_decode_once`]).
///
/// `Lane` is passed because the router must be able to answer differently per tier; today the thumb
/// lane never reaches here at all (see [`decode_heic_lane`]).
pub type HwHeicHook = fn(&Path, Option<u32>, Lane) -> HwHeicAnswer;

/// v0.8.171 (HEIC SPEED PRIORITY) — what the hardware lane ANSWERED, and it has three values now.
///
/// v0.8.148 shipped two, as an `Option`, and that was exactly right while there were two things to
/// say. The speed-priority setting adds a third: a decode the app itself abandoned mid-grid because
/// the user had already browsed past the photograph. It is neither of the others and folding it into
/// either is a real defect:
///
///   * as `Declined` it would fall through to rung 1 and pay WIC's whole software HEVC decode for a
///     picture the app just decided nobody was waiting for — the opposite of the setting's purpose;
///   * as an `Err` it would latch the tier's decode-failure guard against a perfectly good file.
///
/// So it comes back up the ladder as itself, and the two browse workers — the only callers that can
/// arm a supersession signal in the first place — are the only code that has to know it exists.
#[derive(Debug)]
pub enum HwHeicAnswer {
    /// The finished photo, on the hook's documented contract (packed RGB8, `irot` applied, the
    /// file's own gamut, dims equal to [`scaled_dims`] for `scale_to`).
    Served { rgb: Vec<u8>, w: u32, h: u32 },
    /// This file cannot take the lane, for any of the reasons the lane declines for. The caller
    /// falls to the WIC ladder exactly as it did before rung 0 existed.
    Declined,
    /// The decode was ABANDONED mid-grid: the app moved on. Nothing failed, nothing is wrong with
    /// the file, the session was returned clean, and the shot simply has no frame yet.
    Superseded,
}

static HW_HEIC_HOOK: std::sync::OnceLock<HwHeicHook> = std::sync::OnceLock::new();

/// Install the hardware lane. Returns `false` if one was already installed — the app installs
/// exactly once, from the boot probe, and a second caller is a bug rather than a fight to win.
pub fn install_hw_heic_hook(f: HwHeicHook) -> bool {
    HW_HEIC_HOOK.set(f).is_ok()
}

/// The installed hardware lane, if any. `None` on every box that has no hardware lane, on every
/// non-Windows build, and — deliberately — while `FALCON_CLASSIC_HEIC=1`, because the app does not
/// install the hook at all in that mode. Two locks on the same door: the app declines to install it,
/// and [`decode_heic_lane`] re-checks [`classic_heic`] before it asks.
#[inline]
pub fn hw_heic_hook() -> Option<HwHeicHook> {
    HW_HEIC_HOOK.get().copied()
}

/// Park a once-per-session line on the decode-note channel [`drain_decode_notes`] drains.
///
/// Public so the hardware lane's DECLINE reasons ride the same channel — and therefore the same
/// once-per-key discipline and the same ~1.5 s drain — as S1's. A per-file decline is expected
/// behaviour on this lane; a line per file would be the 18-worker flood this codebase keeps not
/// shipping, so the key is the REASON and the line says so.
pub fn note_decode_once(key: &str, line: String) -> bool {
    note_once(key, line)
}

/// v0.8.101: the decode lanes' ONE-TIME notes. falcon-decode has no logger of its own — deliberately:
/// it is a pure library the spikes link too — so a lane that wants to say something once per session
/// parks a line here and the app drains it ([`drain_decode_notes`], called on the tick's existing
/// ~1.5 s report cadence). `key` is what makes it once-only, so a per-format key gives the
/// once-per-format-per-session line the S1 fail-soft contract asks for. Never logs per decode: an
/// 18-worker pool would turn that into a flood, which is the mistake this codebase keeps not making.
static DECODE_NOTES: std::sync::Mutex<Option<(std::collections::HashSet<String>, Vec<String>)>> =
    std::sync::Mutex::new(None);

/// Returns TRUE when the note was actually recorded (i.e. this key had not been seen) — so a caller
/// that also bounds ITS OWN family of notes counts real lines, not repeat calls about one file.
fn note_once(key: &str, line: String) -> bool {
    let mut g = DECODE_NOTES.lock().unwrap_or_else(|e| e.into_inner());
    let (seen, out) = g.get_or_insert_with(Default::default);
    let fresh = seen.insert(key.to_string());
    if fresh {
        out.push(line);
    }
    fresh
}

/// Take every one-time decode note recorded since the last drain (see [`note_once`]). The KEYS are
/// not cleared — a note stays once-per-session, not once-per-drain.
pub fn drain_decode_notes() -> Vec<String> {
    let mut g = DECODE_NOTES.lock().unwrap_or_else(|e| e.into_inner());
    match g.as_mut() {
        None => Vec::new(),
        Some((_, out)) => std::mem::take(out),
    }
}

/// The exact dims [`resize_to_long`] would produce for `long` — extracted as a PURE function so the
/// v0.8.101 scaled-decode arm can ask WIC for precisely the size the old full-decode-then-Lanczos
/// path landed on. That is what makes "S2 changes the cost, never the frame size" a fact rather
/// than a hope: the unit test `scaled_dims_matches_resize_to_long` pins the two against each other.
#[inline]
pub fn scaled_dims(w: u32, h: u32, long: u32) -> (u32, u32) {
    if w >= h {
        (long, ((long as u64 * h as u64) / w as u64).max(1) as u32)
    } else {
        (((long as u64 * w as u64) / h as u64).max(1) as u32, long)
    }
}

/// v0.8.148 (E5): which ENGINE produced a frame's pixels. Reported, never decided from — the
/// routing decision is made before the decode ([`hw_heic_hook`]) and this is what came back.
///
/// It exists because a field log has to be able to tell the two apart. Every claim this epic makes
/// ("HEIC full-res is ~10× faster now") is a claim about which route ran, and a `decode-stats fast
/// HEIC` line that does not say which one is a number nobody can act on: the same folder can serve
/// some files through the hardware lane and its neighbours through WIC, because a per-file decline
/// is the designed behaviour and not a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeRoute {
    /// The CPU/OS codec path — WIC on Windows, and EVERY non-HEIC format on every platform. Plan
    /// non-negotiable #5: this remains the fallback forever.
    Cpu,
    /// v0.8.148: the D3D11VA hardware lane (E3), reached only through [`hw_heic_hook`].
    Hardware,
}

// v0.8.149 (F9/B6) — ONE ROUTE GRAMMAR, and this is where the second one used to be.
//
// v0.8.148 shipped `DecodeRoute::tag()` returning `"cpu"`/`"hw"`, and NOTHING called it: the two
// producers that actually reach a log are `support::decode_stats_tag` (`HEIC/wic` · `HEIC/hw`) and
// the detail tier's engine label (`HEIC / CPU` · `HEIC / D3D11VA`). A third, unused, THIRD-spelling
// vocabulary sitting in the shared crate is exactly how a field report ends up quoting a string
// that no version of this app has ever printed, so it is deleted rather than wired: the enum is the
// datum, and each log line spells it for its own reader. If a future round wants one spelling for
// both lines it must change the two producers, which is a decision with a diff, not a helper.

/// One browse-lane frame: the RGBA pixels, the two timing spans the detail tier attributes
/// separately in its log line, and — v0.8.101 — WHERE the pixels came from.
pub struct BrowseFrame {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    /// The source decode plus any Lanczos downscale.
    pub dec_ms: u32,
    /// The RGB→RGBA expansion, a full-buffer pass that grows 146 MB into 195 MB at 48 MP.
    pub exp_ms: u32,
    /// v0.8.101 (S1): `EmbeddedPreview` ONLY ever from [`Lane::Thumb`]. See [`FrameSource::cache_dim`].
    pub source: FrameSource,
    /// v0.8.148 (E5): which engine decoded it. [`DecodeRoute::Cpu`] for every format but HEIC, and
    /// for HEIC whenever the hardware lane declined.
    pub route: DecodeRoute,
}

/// Fast scrub frame as raw RGBA (DCT shrink-on-load + Lanczos, no encode) — the native
/// GPU-texture scrub path (PLAN §20).
///
/// ## THE SAMPLING CONTRACT (owner-clarified 2026-07-27 — BINDING)
///
/// The decode ladder is the halving sequence off the FULL-RES frame: full/1, full/2, full/4,
/// full/8 …. A "tier COVERS an ask" when its long side is **≥** the ask. Then:
///
/// > **SUPERSAMPLE** decodes at the **smallest full-res/2ⁿ tier that COVERS the ask**, and
/// > Lanczos-downscales from there to exactly the ask.
/// > **SUBSAMPLE** takes the same rule against the **HALVED ask** (`max_dim / 2`) and serves
/// > **at least** that many pixels, never fewer.
/// > **There is no undershooting tier in either mode.** A tier below the ask is not a cheaper
/// > sample; it is a different, softer frame, and this tier never serves one.
///
/// The two sampling modes are therefore the same rule applied to two asks (`max_dim` and
/// `max_dim / 2`), which is exactly what the code does — `decode_target` below is the only place
/// the distinction lives. No tolerance band exists and none may be added: an "undershoot by a few
/// percent" rule would make the tier's sharpness a function of how the ask happens to fall against
/// the ladder, which is precisely the property a contract is for. (`near_stop_skip_resize` is NOT
/// such a band — it skips the final Lanczos when the covering tier is already within 12.5% of the
/// ask, which serves MORE pixels than asked, never fewer.)
///
/// **What SUBSAMPLE actually serves, split honestly (v0.8.112).** The clause above binds the
/// FLOOR — ≥ the halved ask — because the two decoder shapes in this crate land on different sides
/// of it and both are correct:
///  • **The decoder returns a rung** (JPEG, whose `d.scale()` reports the DCT stop's own dims):
///    the covering rung of the halved ask is served AS DECODED, so the frame is ≥ the halved ask
///    and usually larger. An 8064-px master at `max_dim` 2880 asks 1440, lands on the 2016 stop,
///    and `near_stop_skip_resize` keeps it — 2016 px served.
///  • **The chain clamps to the target** (every format reaching `finish_source` with `Some(target)`
///    — PNG/TIFF/WebP/HEIC/JXL/BMP/GIF): `resize_to_long` trims to EXACTLY the halved ask, so the
///    same 8064-px master serves 1440 px, the full decode (or a coarser rung plus Lanczos) standing
///    in for the ladder. The Windows HEIC lane does this at every rung — it may take the 1/4 stop
///    for cost, then trims to the ask.
/// They differ by up to 1.4× on the long side, which is a real difference and is why this is stated
/// rather than smoothed over. **What binds future work:** the FLOOR (never fewer pixels than the
/// halved ask) and each format's EXISTING answer — neither may be changed without a measurement,
/// because both the frame's sharpness and the tier's cost hang on it. In particular, do not "fix"
/// the clamped formats up to the covering rung to make the two shapes agree: on Windows HEIC that
/// would decode 2016 px to serve a 1440 px frame at full HEVC price, which is the cost this whole
/// posture round exists to remove.
///
/// **The one legitimate substitution.** On a format whose codec charges full price for a rung, a
/// FULL decode plus Lanczos is the same output as taking that rung and is cheaper, so it stands in
/// for it. That is a codec cost detail, not a contract exception — the frame is identical either
/// way (see [`scaled_dims`]). Windows HEIC is the measured case: the Store HEVC extension's 1/2 stop
/// ran **46% SLOWER pooled** than a plain full decode (36 frames at a 2880 target: 23.6 s via the
/// 1/2 stop against 16.2 s full-decoding — v0.8.101), which is why [`HEIC_MIN_STOP_DIV`] starts that
/// ladder at 1/4 and every shape above it substitutes the full decode.
///
/// ## Per format
///
/// `supersample` picks the tier relative to the JPEG DCT stops (jpeg_decoder only decodes at 1/1,
/// 1/2, 1/4, 1/8 and never undershoots the requested long side — the contract, enforced by the
/// decoder itself):
///  • `true`  — ask for `max_dim`, so the decode lands on the stop AT/ABOVE it, then
///    Lanczos-downscale to exactly `max_dim`. Sharpest; pays the from-larger-stop resize
///    (a 2176-px ask on an 8192-px source decodes at 4096, then shrinks to 2176).
///  • `false` — ask for `max_dim / 2`, which lands one DCT stop LOWER (the stop just below
///    `max_dim`) and skips the downscale — the cheap tier that dodges the decode cliff
///    (2176 → decode 2048 instead of 4096). See PLAN §35.14 / the scrub-perf spike.
/// PNG/TIFF have no shrink-on-load: they decode full then downscale to the target, so
/// `supersample=false` just renders them one tier smaller (no speed win, no harm) — the full
/// decode standing in for every rung, the substitution above in its most total form.
///
/// HEIC — and the answer is OPPOSITE on the two platforms, so it is stated per platform (v0.8.103,
/// V10: v0.8.102's F11 rewrite stated the Windows arithmetic as universal on this cross-platform
/// `pub fn`, which is the same over-claim shape F12 removed from the boot line two hunks away).
/// Frame dims are identical on both (see [`scaled_dims`]) — this is a cost question, never a layout
/// one.
///
/// * **Windows / WIC** (v0.8.102 F11; range corrected v0.8.103 — V8): it CAN shrink on load through
///   WIC's decode-at-scale (S2), but only where a 1/4-or-coarser stop still covers the ask, because
///   the Store HEVC codec charges FULL price for its 1/2 stop ([`HEIC_MIN_STOP_DIV`]). Worked against
///   the LIVE scrub range — `SCRUB_DIM_MIN` 2048 … `adapt_max`, the window's long side capped at
///   4096, NOT the 2880 `SCRUB_DIM_MAX` fast-tier cap — that means **no stop qualifies at either tier
///   on a 12 MP master (4032 long), and none at the supersample tier on a 48 MP one**: both take the
///   full decode, so `supersample=false` buys HEIC nothing there, exactly as it buys PNG/TIFF
///   nothing. (Both hold across the whole range, and more firmly at its top — a bigger ask can only
///   make a stop less likely to cover.) The stop DOES engage on a 48 MP master at the SUB tier
///   (2880 → decode target 1440 → 8064/4 = 2016 covers), but only for windows up to 4032 px long:
///   above that the sub tier's target passes 2016 and a maximized 5K panel full-decodes at BOTH
///   tiers. It also engages on the 256 px thumb tier whenever S1's preview door declines (1/8).
/// * **macOS / Image I/O** (v0.9.30 / H5): there is no S2 rung here because the scaled decode IS the
///   only decode — `decode_heic` passes `scale_to` straight into Image I/O's subsample ladder for
///   EVERY lane. So HEIC shrinks on load wherever [`imageio_subsample_max_px`] returns a factor ≥ 2,
///   and the Windows sentences above are inverted: a 12 MP master takes a 1/2 shrink at the SUB tier
///   (2880 → target 1440 → 2016), and a 48 MP one takes a 1/2 at the SUPERSAMPLE tier (2880 → 4032)
///   and a 1/4 at the sub tier (1440 → 2016). `supersample=false` is therefore the genuinely CHEAPER
///   tier for HEIC on Mac, not a no-op — the opposite of the Windows advice. (The ladder has no
///   minimum-divisor rule of its own: Image I/O's 1/2 subsample is a real saving, which is precisely
///   the measurement that made `HEIC_MIN_STOP_DIV` a Windows-only fact.)
pub fn fast_frame_rgba(shot: &Shot, max_dim: u32, supersample: bool) -> Result<(Vec<u8>, u32, u32)> {
    let f = browse_frame_rgba(shot, max_dim, supersample, Lane::Fast)?;
    Ok((f.rgba, f.w, f.h))
}

/// v0.8.99 (H2b) / v0.8.101 (S1/S2): `fast_frame_rgba`'s ONE body, with the timing split and the
/// source tag. `fast_frame_rgba` delegates and drops the extras, so there is exactly one
/// implementation and no variant can drift from the hot path.
///
/// `lane` decides the two HEIC-only behaviours and NOTHING else — every other format takes the
/// identical `decode_source_rgb` dispatch it took before v0.8.101, which is what byte-pins the
/// JPEG paths: the new code is unreachable from a JPEG shot.
///
/// * `dec_ms` — the source decode plus any Lanczos downscale.
/// * `exp_ms` — the RGB→RGBA expansion. Small next to a HEVC decode, first-order next to an
///   nvJPEG one; either way it stops being invisible.
///
/// Both are `u32` milliseconds, the same unit the tier's existing total uses, so `dec + exp +
/// xform ≈ ms` reads directly off one line.
pub fn browse_frame_rgba(
    shot: &Shot,
    max_dim: u32,
    supersample: bool,
    lane: Lane,
) -> Result<BrowseFrame> {
    // v0.8.171: the un-watched door. A caller that has armed no supersession signal cannot be
    // superseded, so `None` here means somebody armed one and then came in through the door that
    // cannot express the answer — a bug in the caller, reported as one rather than guessed at.
    browse_frame_or_superseded(shot, max_dim, supersample, lane)?
        .context("the hardware lane abandoned a decode for a caller that cannot be superseded")
}

/// v0.8.171 (HEIC SPEED PRIORITY) — [`browse_frame_rgba`] for the two BROWSE WORKERS, which can be
/// superseded and must be able to hear it.
///
/// `Ok(None)` = the hardware lane abandoned this decode mid-grid because the app moved on. The
/// caller's obligation is precise and short: send no frame, latch no failure, and let the shot go
/// back to being wanted. A later ask starts fresh.
pub fn browse_frame_or_superseded(
    shot: &Shot,
    max_dim: u32,
    supersample: bool,
    lane: Lane,
) -> Result<Option<BrowseFrame>> {
    let t0 = std::time::Instant::now();
    let decode_target = fast_decode_target(max_dim, supersample);
    let Some((rgb, w, h, source, route)) = decode_source_rgb_lane(shot, Some(decode_target), lane)?
    else {
        return Ok(None);
    };
    let decode_ms = t0.elapsed().as_millis() as u32;
    let (rgba, w, h, resize_ms, exp_ms) = finish_fast_rgba_timed(rgb, w, h, max_dim)?;
    Ok(Some(BrowseFrame { rgba, w, h, dec_ms: decode_ms + resize_ms, exp_ms, source, route }))
}

/// The near-stop finish that turns a decoded (DCT-stop or full) RGB8 buffer into a canvas-ready RGBA8
/// frame — split out of [`fast_frame_rgba`] (v0.9.1, P5) so the `ImageDecoder` CPU path shares ONE
/// finish rule with `fast_frame_rgba`: a call site that took a raw `decode_scaled` RGB8 off
/// [`CpuDecoder`] finishes it here and gets byte-identical pixels to `fast_frame_rgba`.
///
/// B2: skip the near-stop Lanczos (see [`near_stop_skip_resize`]). `resize_to_long` is already a
/// no-op when the decoded stop sits at/below `max_dim` (the subsample tier), so this only changes the
/// supersample case where the stop is a hair above the window (want 3840 → stop 4096): there the
/// 1.07× shrink measured PRICIER than serving the raw stop, and the ≤14% extra bytes are free on the
/// GPU (and, post-B1, on the staging-only upload thread).
pub fn finish_fast_rgba(rgb: Vec<u8>, w: u32, h: u32, max_dim: u32) -> Result<(Vec<u8>, u32, u32)> {
    let (rgba, w, h, _resize_ms, _exp_ms) = finish_fast_rgba_timed(rgb, w, h, max_dim)?;
    Ok((rgba, w, h))
}

/// v0.8.99 (H2b): `finish_fast_rgba`'s ONE body, with its two spans reported —
/// `(rgba, w, h, resize_ms, exp_ms)`.
///
/// The `ImageDecoder` seam (v0.9.1, P5) splits the detail tier's CPU arm into "decode" (the
/// decoder's own `decode_scaled`) and "finish" (this), so the branch reaches the same
/// `dec = decode + resize` / `exp = RGB→RGBA expand` attribution the Windows trunk's
/// `fast_frame_rgba_timed` reports — the caller adds its own decode span to `resize_ms`.
/// `finish_fast_rgba` delegates and drops both, so there is exactly one body.
pub fn finish_fast_rgba_timed(
    rgb: Vec<u8>,
    w: u32,
    h: u32,
    max_dim: u32,
) -> Result<(Vec<u8>, u32, u32, u32, u32)> {
    let t0 = std::time::Instant::now();
    let (rgb, w, h) = if near_stop_skip_resize(w.max(h), max_dim) {
        (rgb, w, h)
    } else {
        resize_to_long(rgb, w, h, max_dim)?
    };
    let resize_ms = t0.elapsed().as_millis() as u32;
    let t1 = std::time::Instant::now();
    let rgba = rgb_to_rgba(&rgb);
    Ok((rgba, w, h, resize_ms, t1.elapsed().as_millis() as u32))
}

/// B2 predicate: true when the decoded DCT stop's long side is close enough to the requested
/// `max_dim` that the Lanczos downscale isn't worth its cost. The JPEG DCT stops are octaves
/// (…2048, 4096, 8192…), so the supersample decode lands on the smallest stop ≥ `max_dim` and the
/// ratio stop/max_dim is 1.0 at a boundary, approaching 2.0 just above one. The real maximized-4K
/// case is want=3840 → stop 4096 (ratio 1.067): serving the 4096 stop raw measured ~234 ms vs
/// ~261 ms for the 1.067× Lanczos shrink (spikes-decode-bench/browse_cost) — the resize is pricier
/// than the extra bytes it saves. We skip up to max_dim × 1.125, which comfortably catches that
/// 1.067 boundary yet still resizes the next-octave 1.4–2.0× cases (e.g. want 2880 → stop 4096)
/// where the byte blow-up would be real. (The spec's nominal 1.05 would miss 1.067, the very case
/// the measurement targets, so the threshold is widened to the measured net-win boundary.)
///
/// The skip is ALSO hard-capped at [`TEX_SAFE_LONG`]: with the 1.125× slack a huge-source detail-
/// tier fallback (nvJPEG rejected on resolution → `fast_frame_rgba`) at/near a 16384 res-limit could
/// otherwise return an ~18432 px stop, which trips wgpu's max-texture validation → the upload fails
/// and the shot never sharpens. Above the cap we fall through to the exact-`max_dim` Lanczos clamp.
/// Every scrub tier (≤4096 stop) sits far below this, so it is untouched.
#[inline]
pub fn near_stop_skip_resize(long: u32, max_dim: u32) -> bool {
    long <= (max_dim + max_dim / 8).min(TEX_SAFE_LONG)
}

/// The common GPU max-texture-dimension (equals `RES_LIMIT_MAX`, the whole-image full-res cap). A
/// near-stop skip must keep its result within this or wgpu's create_texture validation rejects it.
pub const TEX_SAFE_LONG: u32 = 16384;

/// The long side a fast/scrub-tier decode is ASKED for, given the tier's want and its sampling mode.
/// The supersample tier asks for the want itself; the subsample tier asks for HALF it and serves
/// whatever the decoder hands back (on a format whose decoder can scale, that is a genuinely
/// half-size proxy — the sub tier's whole point).
///
/// Extracted (v0.8.115) so [`browse_frame_rgba`] and [`derive_fast_rgba`] cannot disagree about what
/// the fast tier's frame size IS. They did not disagree by accident: a derive written against
/// `max_dim` alone produces a 2176 px frame where the sub tier serves 1088, i.e. FOUR times the
/// bytes per cached frame — and the prefetch window is sized off the largest cached frame, so it
/// would have quartered the browse's runway on the sub tier while every test still passed.
#[inline]
pub fn fast_decode_target(max_dim: u32, supersample: bool) -> u32 {
    if supersample {
        max_dim
    } else {
        (max_dim / 2).max(1)
    }
}

/// v0.8.115 (DERIVE-DON'T-DECODE): the fast tier's frame, produced from pixels that are ALREADY
/// canvas-ready RGBA8 — i.e. from the native-resolution frame the DETAIL tier has just decoded.
///
/// WHY IT EXISTS. On the software-HEVC lane a fast-tier decode is not a cheap proxy for the sharp
/// frame; it is the SAME full decode plus a Lanczos reduction ("heic S2: no power-of-two stop
/// covers a 2176 px ask from a 6048x8064 master — full decode + Lanczos"), so it costs MORE than
/// the detail decode standing beside it and usually lands after the user has stepped past the shot.
/// One decode can serve both tiers: the detail frame's pixels ARE the fast frame's source, and this
/// is the reduction that turns one into the other.
///
/// IT IS [`browse_frame_rgba`]'s OWN SIZING AND FINISH, moved to the other side of the RGB→RGBA
/// expansion — the shared [`fast_decode_target`] for the ask, then the same two branches:
///   * the B2 near-stop skip ([`near_stop_skip_resize`]) — the source is already close enough to the
///     ask that the reduction costs more than the bytes it saves, so the frame is kept as it is
///     (with the same [`TEX_SAFE_LONG`] hard cap folded into the predicate);
///   * otherwise the Lanczos3 reduction to exactly the ask on the long side — the same filter and
///     the same [`scaled_dims`] result, via [`downscale_rgba`].
///
/// So a derived frame is the SAME CLASS of pixels a decoded fast frame is, at the same size. The
/// unit row `derive_fast_rgba_is_the_decoded_finish` pins the two against each other on synthetic
/// pixels; `tests/heic.rs` pins them on the real 48 MP Display-P3 iPhone masters, where the measured
/// difference is **0.000 mean-abs-byte on every same-size row** — bit-identical, because on that
/// lane both really are the master reduced by the same Lanczos3.
///
/// THE ONE HONEST DIFFERENCE, stated so nobody has to rediscover it: a decoded fast frame may come
/// from a SCALED decode (S2 / a DCT stop) and the near-stop rule may then KEEP that stop's size,
/// which can sit up to 1.125× above the ask; a derived frame always comes from the master and lands
/// exactly on the ask. Both are filed under the same `dim` WANT bucket — the bucket is the want, not
/// the pixel long side — so no consumer can tell them apart, and the derived frame is never the
/// larger (never the more expensive) of the two.
///
/// THE CALLER'S OBLIGATION: `w.max(h)` must be ≥ [`fast_decode_target`]. A source SMALLER than the
/// ask cannot produce a frame of that size, and a frame filed under a `dim` bucket its pixels do not
/// support is a soft frame served as sharp — the one failure mode this path could introduce. The
/// caller checks it (see `main.rs`'s derive worker) rather than this function silently up-scaling.
pub fn derive_fast_rgba(
    rgba: &[u8],
    w: u32,
    h: u32,
    max_dim: u32,
    supersample: bool,
) -> (Vec<u8>, u32, u32) {
    let target = fast_decode_target(max_dim, supersample);
    if near_stop_skip_resize(w.max(h), target) {
        return (rgba.to_vec(), w, h);
    }
    downscale_rgba(rgba, w, h, target)
}

/// Reference frame as raw RGBA (full decode + Lanczos downscale, no encode).
pub fn reference_frame_rgba(shot: &Shot, max_dim: u32) -> Result<(Vec<u8>, u32, u32)> {
    let (rgb, w, h) = decode_source_rgb(shot, None)?;
    let (rgb, w, h) = resize_to_long(rgb, w, h, max_dim)?;
    Ok((rgb_to_rgba(&rgb), w, h))
}

// ───────────────────── adaptive hi-res (zoomed-region / tile) ─────────────────
// For very large images (> the base full-res cap), full resolution is only needed for
// the part you're zoomed into. Rather than upload the whole giant image (which can
// exhaust GPU memory), we decode the source once to a CPU buffer and crop the visible
// region to a bounded screen-sized tile on demand. (PLAN §25 follow-up.)

/// Cheap source-dimension probe: parse only the image header (no pixel decode) to learn the full
/// pixel size — used to decide whether an image is large enough that zooming needs region hi-res.
/// Dispatches by format; JPEG reads a bounded prefix, PNG/TIFF read the header IFD/chunk, WebP reads
/// the container header via `image-webp`, HEIC asks the OS codec (WIC / Image I/O) for the frame size
/// (header-only). Returns
/// `None` for a RAW-only shot (no finished-image path — its embedded preview is small) and for
/// unsupported shots.
pub fn source_dimensions(shot: &Shot) -> Option<(u32, u32)> {
    // v1.0.0-rc MICRO-TAIL (A-Y3b): a passenger is not the picture, so it has no dimensions to
    // report — the same `None` a RAW-only shot has always answered here.
    if !shot.has_jpg {
        return None;
    }
    let path = shot.jpg.as_ref()?;
    match shot.kind {
        // v1.0.0-rc (R3): a MARKER WALK, bounded by the file's own segment lengths rather than by a
        // fixed 256 KB prefix. The old prefix silently returned `None` for any JPEG whose SOF sits
        // past 256 KB — a 500 KB embedded ICC, or an EXIF block carrying a big thumbnail — and the
        // caller reads `None` as "no size available", so the ROI hi-res tile, the focus readout and
        // the zoom decision all quietly went away on a file that decodes perfectly.
        SrcKind::Jpeg => jpeg_header_dims(path),
        SrcKind::Png => {
            let f = std::fs::File::open(path).ok()?;
            let reader = png::Decoder::new(std::io::BufReader::new(f)).read_info().ok()?;
            let info = reader.info();
            Some((info.width, info.height))
        }
        SrcKind::Tiff => {
            // The crate reads dims from the IFD tags (works even for CCITT/exotic TIFFs it can't
            // decode); fall back to the OS codec for the rare header the crate can't parse (P8).
            let crate_dims = std::fs::File::open(path)
                .ok()
                .and_then(|f| tiff::decoder::Decoder::new(std::io::BufReader::new(f)).ok())
                .and_then(|mut dec| dec.dimensions().ok());
            crate_dims.or_else(|| wic_dimensions(path))
        }
        SrcKind::Webp => webp_dimensions(path),
        SrcKind::Heic => wic_dimensions(path),
        SrcKind::Jxl => jxl_dimensions(path),
        SrcKind::Bmp => bmp_dimensions(path),
        SrcKind::Gif => gif_dimensions(path),
        SrcKind::Unsupported => None,
    }
}

/// Decode the FULL source to packed RGB8 (no downscale) — the CPU buffer the region
/// cropper works from. Heavy (full decode); the caller caches the result per shot.
pub fn decode_full_rgb(shot: &Shot) -> Result<(Vec<u8>, u32, u32)> {
    decode_source_rgb(shot, None)
}

/// v1.0.0-rc PNG EXPORT (queue item 35): [`decode_full_rgb`], keeping what `keep` asks for -- the
/// ./export export's decode, and the ONLY caller in the tree that passes anything but [`Keep::NONE`].
///
/// **THE OPAQUE-ALPHA DROP LIVES HERE** (sheet 2.2c, 0.1 (b)): "keep transparency" taken literally
/// means an alpha channel whose every sample is full is not transparency, so it is dropped and the
/// deliverable is a third smaller and pixel-identical. It is one linear scan, once, on a buffer
/// that has just cost a whole decode -- and it is done HERE, before the resize and the stamp, so
/// the rest of the pipeline runs three channels wide instead of four and the mark takes the
/// shipped opaque-destination arithmetic. It is asked only when alpha was kept at all.
pub fn decode_full_pixels(shot: &Shot, keep: Keep) -> Result<(Pixels, u32, u32)> {
    let (px, w, h) = decode_source_keep(shot, None, keep)?;
    Ok((if keep.alpha { px.drop_opaque_alpha() } else { px }, w, h))
}

/// Decode the source with DCT shrink-on-load so the long side ≈ `max_long` — far faster
/// than a full decode (a 110 MP JPG drops from ~600 ms to tens of ms). Used as the
/// progressive "preview" source: a usable tile appears immediately while the full-res
/// buffer decodes in the background.
pub fn decode_source_scaled(shot: &Shot, max_long: u32) -> Result<(Vec<u8>, u32, u32)> {
    decode_source_rgb(shot, Some(max_long))
}

/// Crop a NORMALISED region `(u0,v0)-(u1,v1)` (0..1, resolution-independent) from a full
/// RGB8 buffer and downscale to a bounded tile. Returns the tile + the actual (clamped)
/// normalised rect. Resolution-independent so the SAME request works on a preview buffer
/// and the full-res buffer (progressive hi-res). `None` on a degenerate rect.
#[allow(clippy::too_many_arguments)]
pub fn crop_region_norm(
    rgb: &[u8],
    w: u32,
    h: u32,
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
    out_long: u32,
) -> Option<(Vec<u8>, u32, u32, f32, f32, f32, f32)> {
    let (wf, hf) = (w as f32, h as f32);
    let sx = (u0.clamp(0.0, 1.0) * wf) as u32;
    let sy = (v0.clamp(0.0, 1.0) * hf) as u32;
    let sw = (((u1 - u0).clamp(0.0, 1.0) * wf) as u32).max(1);
    let sh = (((v1 - v0).clamp(0.0, 1.0) * hf) as u32).max(1);
    let (rgba, ow, oh, csx, csy, csw, csh) = crop_region_rgba(rgb, w, h, sx, sy, sw, sh, out_long)?;
    Some((
        rgba,
        ow,
        oh,
        csx as f32 / wf,
        csy as f32 / hf,
        (csx + csw) as f32 / wf,
        (csy + csh) as f32 / hf,
    ))
}

/// Crop region `[sx,sy,sw,sh]` (source px, clamped to bounds) from a full RGB8 buffer and
/// Lanczos-downscale so its long side ≤ `out_long` → canvas-ready RGBA. Returns the tile
/// plus the actual (clamped) source rect so the caller can position it exactly. `None` on
/// a degenerate/empty rect.
#[allow(clippy::too_many_arguments)]
pub fn crop_region_rgba(
    rgb: &[u8],
    w: u32,
    h: u32,
    sx: u32,
    sy: u32,
    sw: u32,
    sh: u32,
    out_long: u32,
) -> Option<(Vec<u8>, u32, u32, u32, u32, u32, u32)> {
    if w == 0 || h == 0 || rgb.len() < (w as usize * h as usize * 3) {
        return None;
    }
    let sx = sx.min(w - 1);
    let sy = sy.min(h - 1);
    let sw = sw.clamp(1, w - sx);
    let sh = sh.clamp(1, h - sy);
    // Tight-copy the crop rows out of the full buffer.
    let mut crop = vec![0u8; sw as usize * sh as usize * 3];
    let row_bytes = sw as usize * 3;
    for row in 0..sh as usize {
        let src_off = (((sy as usize + row) * w as usize) + sx as usize) * 3;
        let dst_off = row * row_bytes;
        crop[dst_off..dst_off + row_bytes].copy_from_slice(&rgb[src_off..src_off + row_bytes]);
    }
    let (rgb2, ow, oh) = resize_to_long(crop, sw, sh, out_long).ok()?;
    Some((rgb_to_rgba(&rgb2), ow, oh, sx, sy, sw, sh))
}

/// A cropped planar-YUV region (E4 zoom tiles): tight planes + the SNAPPED normalised rect
/// of the FULL frame it covers (the stage overlay maps the tile by this rect, so it must be
/// the snapped one, not the requested one).
pub struct YuvCrop {
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    /// Luma dims of the crop.
    pub w: u32,
    pub h: u32,
    /// Chroma dims of the crop.
    pub cw: u32,
    pub ch: u32,
    /// The snapped rect as normalised fractions of the full frame.
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
}

/// Crop a NORMALISED region from tight planar-YUV buffers (the E4 ROI/zoom-tile path),
/// snapping the luma rect OUTWARD to the chroma subsampling grid (`dx`,`dy` = the plane
/// divisors, e.g. 4:2:2 → 2,1) so the chroma crop covers exactly the same region — per-uv
/// GPU sampling of the cropped planes is then bit-identical to sampling the full frame
/// (PLAN §E4-SPEC). Native resolution only: no downscale — the ROI worker gates the YUV
/// route on the crop fitting the request's `out_long` and falls back to RGBI otherwise.
/// `None` on degenerate dims or undersized planes (crash-safety, like the RGB crops).
#[allow(clippy::too_many_arguments)]
pub fn crop_region_yuv(
    y: &[u8],
    cb: &[u8],
    cr: &[u8],
    w: u32,
    h: u32,
    cw: u32,
    ch: u32,
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
) -> Option<YuvCrop> {
    if w == 0 || h == 0 || cw == 0 || ch == 0 {
        return None;
    }
    if y.len() < (w as usize * h as usize)
        || cb.len() < (cw as usize * ch as usize)
        || cr.len() < (cw as usize * ch as usize)
    {
        return None;
    }
    let dx = (w.div_ceil(cw)).max(1);
    let dy = (h.div_ceil(ch)).max(1);
    // Same pixel-rect derivation as crop_region_norm/crop_region_rgba (identical clamps, so
    // the requested rect matches what the RGBI path would have cropped).
    let (wf, hf) = (w as f32, h as f32);
    let sx = ((u0.clamp(0.0, 1.0) * wf) as u32).min(w - 1);
    let sy = ((v0.clamp(0.0, 1.0) * hf) as u32).min(h - 1);
    let sw = (((u1 - u0).clamp(0.0, 1.0) * wf) as u32).clamp(1, w - sx);
    let sh = (((v1 - v0).clamp(0.0, 1.0) * hf) as u32).clamp(1, h - sy);
    // Snap OUTWARD to the chroma grid (never shrinks the requested region).
    let x0 = sx / dx * dx;
    let y0 = sy / dy * dy;
    let x1 = ((sx + sw).div_ceil(dx) * dx).min(w);
    let y1 = ((sy + sh).div_ceil(dy) * dy).min(h);
    let (tw, th) = (x1 - x0, y1 - y0);
    // Chroma rect: exact division at the snapped origin; div_ceil at the (possibly w-clamped)
    // end — mirrors the full frame's own cw=ceil(w/dx) sizing at the right/bottom edge.
    let (ccx0, ccy0) = (x0 / dx, y0 / dy);
    let ccx1 = x1.div_ceil(dx).min(cw);
    let ccy1 = y1.div_ceil(dy).min(ch);
    let (tcw, tch) = (ccx1 - ccx0, ccy1 - ccy0);
    if tw == 0 || th == 0 || tcw == 0 || tch == 0 {
        return None;
    }
    let copy_plane = |src: &[u8], stride: usize, px0: usize, py0: usize, pw: usize, ph: usize| {
        let mut out = vec![0u8; pw * ph];
        for row in 0..ph {
            let s = (py0 + row) * stride + px0;
            out[row * pw..(row + 1) * pw].copy_from_slice(&src[s..s + pw]);
        }
        out
    };
    Some(YuvCrop {
        y: copy_plane(y, w as usize, x0 as usize, y0 as usize, tw as usize, th as usize),
        cb: copy_plane(cb, cw as usize, ccx0 as usize, ccy0 as usize, tcw as usize, tch as usize),
        cr: copy_plane(cr, cw as usize, ccx0 as usize, ccy0 as usize, tcw as usize, tch as usize),
        w: tw,
        h: th,
        cw: tcw,
        ch: tch,
        u0: x0 as f32 / wf,
        v0: y0 as f32 / hf,
        u1: x1 as f32 / wf,
        v1: y1 as f32 / hf,
    })
}

/// CPU true raw develop as raw RGBA (the fallback when no GPU is present).
pub fn develop_raw_rgba(shot: &Shot, max_dim: u32) -> Result<(Vec<u8>, u32, u32)> {
    let raw = shot.raw.as_ref().context("shot has no raw file to develop")?;
    let dynimg = raw_export::develop_raw_image_for_viewer(raw)
        .map_err(|e| anyhow::anyhow!("raw develop failed: {e:?}"))?;
    let (w, h) = (dynimg.width(), dynimg.height());
    let rgb = dynimg.to_rgb8().into_raw();
    let (rgb, w, h) = resize_to_long(rgb, w, h, max_dim)?;
    Ok((rgb_to_rgba(&rgb), w, h))
}

/// CPU true raw develop to FULL-resolution packed RGB8 (NO downscale) — the source buffer
/// the ROI region cropper works from in RAW mode, so a 100% zoom / focus-check shows the
/// DEVELOPED RAW (demosaic, camera white balance and sRGB calibration), not the embedded camera JPG. This is
/// the RAW analogue of [`decode_full_rgb`]: same RGB8 layout, full native resolution, so the
/// existing `crop_region_norm` tile path works unchanged. Heavy (a full RAW develop); the ROI
/// worker caches the result per shot, exactly as it caches the JPG source.
pub fn develop_raw_rgb_full(shot: &Shot) -> Result<(Vec<u8>, u32, u32)> {
    let raw = shot.raw.as_ref().context("shot has no raw file to develop")?;
    let dynimg = raw_export::develop_raw_image_for_viewer(raw)
        .map_err(|e| anyhow::anyhow!("raw develop failed: {e:?}"))?;
    let (w, h) = (dynimg.width(), dynimg.height());
    Ok((dynimg.to_rgb8().into_raw(), w, h))
}

/// Raw JPEG bytes for a shot — the JPG file, or the largest embedded preview in a
/// raw container (CR3). Public so the native nvJPEG path can feed the GPU decoder
/// the exact same source the CPU tiers decode (wraps the private [`jpeg_source`]).
pub fn jpeg_bytes(shot: &Shot) -> Result<Vec<u8>> {
    jpeg_source(shot)
}

// ───────────────────────────────────── EXIF ──────────────────────────────────

/// Read EXIF from the JPG sibling (TIFF-based, reliable) and fall back to the raw
/// file. Also reports on-disk file sizes for the pair.
/// v1.0.0-rc TAIL 4 (re-verification O1): **NOT A PRODUCTION PATH — `exif_rows` is.**
///
/// Measured, because tail 3 put a passenger fix here and shipped nothing: this function has ZERO
/// references under `native/`. Its callers are `examples/probe.rs`, `tests/real.rs` and the rig.
/// The info panel is built by [`exif_rows`], which parses the container itself and is called from
/// `main.rs`'s EXIF worker; anything asserted here asserts about a twin the app never runs.
///
/// It is kept rather than deleted because it is a genuine public entry point of a library the
/// spikes and examples link, and `tests/real.rs`'s `exif_reads_core_fields` is a real regression
/// guard on the owner's corpus. What it is NOT is evidence about the panel — and this doc is here
/// so the next sweep cannot mistake it for one.
pub fn read_exif(shot: &Shot) -> Exif {
    use exif::{In, Tag};

    let mut ex = Exif::default();
    let raw_sz = shot.raw.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len());
    let jpg_sz = shot.jpg.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len());
    ex.files = fmt_files(
        raw_sz,
        jpg_sz,
        // v1.0.0-rc MICRO-TAIL (A-Y3b): the FILE's noun, so a RAW's undecodable passenger is listed
        // as `RAW 45 MB · HEIC 3 MB` rather than as a nameless "Image".
        &shot.finished_file_format().unwrap_or_else(|| "Image".to_string()),
        shot.named_ext_when_bytes_disagree().as_deref(),
    );

    // v1.0.0-rc TAIL 3 (verifier Y2): the panel describes the PICTURE, so a passenger's EXIF is
    // not it — the RAW's is.
    let Some(path) = shot.jpg.as_ref().filter(|_| shot.has_jpg).or(shot.raw.as_ref()) else {
        return ex;
    };
    let Ok(file) = std::fs::File::open(path) else { return ex };
    let mut br = std::io::BufReader::new(file);
    let Ok(reader) = exif::Reader::new().read_from_container(&mut br) else { return ex };

    let with_unit = |tag| {
        reader
            .get_field(tag, In::PRIMARY)
            .map(|f| f.display_value().with_unit(&reader).to_string())
    };
    let plain = |tag| {
        reader
            .get_field(tag, In::PRIMARY)
            .map(|f| f.display_value().to_string())
    };
    // String tags → first non-empty ASCII component (handles NUL-padded lens/make/model
    // across brands; see exif_string).
    let str_tag = |tag| reader.get_field(tag, In::PRIMARY).and_then(exif_string);

    ex.camera = match (str_tag(Tag::Make), str_tag(Tag::Model)) {
        (Some(mk), Some(md)) => {
            // Canon stores Model as "Canon EOS R5m2"; don't prepend Make again.
            if md.to_lowercase().starts_with(&mk.to_lowercase()) {
                Some(md)
            } else {
                Some(format!("{mk} {md}"))
            }
        }
        (None, Some(md)) => Some(md),
        (Some(mk), None) => Some(mk),
        _ => None,
    };
    ex.lens = str_tag(Tag::LensModel);
    ex.focal = with_unit(Tag::FocalLength);
    ex.aperture = plain(Tag::FNumber).map(|s| format!("f/{s}"));
    ex.shutter = plain(Tag::ExposureTime).map(|s| format!("{s} s"));
    ex.iso = plain(Tag::PhotographicSensitivity);
    // EXIF dates look like "2025:02:22 14:08:30"; dash the date, keep time colons.
    ex.date = plain(Tag::DateTimeOriginal).map(|s| match s.split_once(' ') {
        Some((d, t)) => format!("{} {}", d.replace(':', "-"), t),
        None => s,
    });
    if let (Some(x), Some(y)) = (plain(Tag::PixelXDimension), plain(Tag::PixelYDimension)) {
        ex.dimensions = Some(format!("{x} × {y}"));
    } else if let Some((w, h)) = source_dimensions(shot) {
        // EXIF PixelXDimension is optional — edited/exported JPGs and all PNG/TIFF lack it. Fall back to
        // the real pixel size from the image header so the info panel always shows dimensions.
        ex.dimensions = Some(format!("{w} × {h}"));
    }
    ex
}

/// v0.8.49 (sort): the shot's EXIF `DateTimeOriginal` as the raw fixed-width EXIF string
/// (`"YYYY:MM:DD HH:MM:SS"` — lexicographically sortable as-is). Reads the PRIMARY file (finished
/// image first, else the RAW), with a rawler metadata-only fallback for a RAW whose container
/// kamadak-exif can't parse (CR3 is ISO-BMFF). Bounded by `DATE_READ_CAP`, NOT the 4 MB
/// `EXIF_READ_CAP`: this is called folder-wide by the Date-taken sort, and camera files carry the
/// EXIF IFD in the first few KB (JPEG APP1 right after SOI) — a 4 MB slurp per file would turn a
/// 5 000-shot sort into a ~20 GB read. A file whose metadata sits beyond the cap (or has none)
/// yields `None` and the sort falls back to the file's modified time (documented, Explorer-like).
/// CALLER CONTRACT: never call this on a cloud-placeholder shot — opening the file HYDRATES it
/// (the sort layer guards on `Shot::cloud_placeholder` and uses the mtime fallback there).
pub fn read_date_taken(shot: &Shot) -> Option<String> {
    use exif::{In, Tag};
    const DATE_READ_CAP: u64 = 64 * 1024;
    let read_capped = |path: &Path| -> Option<exif::Exif> {
        use std::io::Read;
        let file = std::fs::File::open(path).ok()?;
        let mut buf = Vec::new();
        file.take(DATE_READ_CAP).read_to_end(&mut buf).ok()?;
        exif_from_bytes(&buf)
    };
    let date_of = |path: &Path| -> Option<String> {
        read_capped(path)?
            .get_field(Tag::DateTimeOriginal, In::PRIMARY)
            .and_then(exif_string)
            .filter(|s| !s.is_empty())
    };
    // v1.0.0-rc TAIL 3 (verifier Y2): a passenger's date is not the shot's date.
    if let Some(d) = shot.jpg.as_deref().filter(|_| shot.has_jpg).and_then(date_of) {
        return Some(d);
    }
    let rp = shot.raw.as_deref()?;
    if let Some(d) = date_of(rp) {
        return Some(d); // TIFF-based RAW (CR2/NEF/ARW/DNG) parsed directly
    }
    // BMFF RAW (CR3) → rawler's metadata-only decode (the same lightweight path raw_orientation uses).
    let src = rawler::rawsource::RawSource::new(rp).ok()?;
    let decoder = rawler::get_decoder(&src).ok()?;
    let md = decoder.raw_metadata(&src, &RawDecodeParams::default()).ok()?;
    md.exif.date_time_original.clone().filter(|s| !s.trim().is_empty())
}

// ─────────────────── orientation (EXIF / rawler) + upright rotation (v0.8.0) ───────────────────
// Falcon displays every source UPRIGHT. Cameras write an EXIF `Orientation` tag (JPG/TIFF), and a
// RAW carries the same in its metadata, describing how the body was held. We reduce that to a count
// of 90° CLOCKWISE quarter-turns (0..3) to apply at display; ONE convention runs end to end — the
// GPU UV transform (upload), the ROI crop mapping, and the CPU rotate used for thumbs + web export
// all speak these turns. Only the ROTATION component is honored; the mirrored orientations
// (2/4/5/7) keep their rotation and DROP the flip (mirroring is out of scope — the caller logs the
// drop once per session).

/// EXIF `Orientation` (1..8) → clockwise quarter-turns (0..3) for upright display. 1→0 (normal),
/// 3→2 (180°), 6→1 (90° CW), 8→3 (270° CW). Mirrored variants keep ONLY their rotation: 2→0, 4→2,
/// 5→1, 7→3. Any absent/invalid value → 0.
pub fn orientation_to_turns(o: u32) -> u8 {
    match o {
        3 | 4 => 2,
        5 | 6 => 1,
        7 | 8 => 3,
        _ => 0, // 1, 2, and anything out of range (mirror-only 2 has no rotation)
    }
}

/// True for the mirrored EXIF orientations (2/4/5/7). Used only so the caller can log the dropped
/// flip once per session (Falcon honors rotation, not mirroring).
pub fn orientation_is_mirrored(o: u32) -> bool {
    matches!(o, 2 | 4 | 5 | 7)
}

/// Read the raw EXIF `Orientation` (1..8) of the source a given tier decodes. `want_raw` reads the
/// RAW file's metadata (the CR3 develop tier) via rawler — specifically `raw_metadata().exif
/// .orientation`, NOT `RawImage.orientation`, which is a hard-coded stub. Otherwise it reads the
/// finished JPG/TIFF/PNG via kamadak-exif (WebP / plain PNG carry no EXIF → `None`), falling back to
/// the RAW's metadata for a RAW-only shot (whose embedded preview inherits the RAW's orientation).
/// `None` when absent/unreadable → base turns 0.
///
/// v0.8.0 S2 (findings #3/#9): an APPLIED rotation on the sidecar route lives ONLY in the shot's XMP
/// sidecar, so the sidecar is consulted FIRST and WINS over the embedded value (Adobe convention) —
/// RAW → `basename.xmp`, finished → `fullname.xmp`, never crossed — so an applied rotation round-trips
/// through this reader. An absent/oversized/unreadable/malformed sidecar falls through to the embedded
/// read. Cheap (one `fs::metadata` probe + a header/metadata parse, no pixel decode) and amortized by
/// the tick's `OrientCache`, so it is safe to call per tier — duplicate reads across tiers are harmless.
///
/// v0.8.102 (the F1/F2 RED): what this returns is the RESIDUAL orientation — the turn still
/// OUTSTANDING once the platform decoder has had its say ([`decoded_base_orientation`]) — and that is
/// now true of the SIDECAR arm too, not just the embedded one. A sidecar's `tiff:Orientation` is
/// FILE-ABSOLUTE by convention (see `apply_rotation`), so [`decoder_consumed_turns`] is subtracted
/// from it here. Without that subtraction a `.heic.xmp` — Falcon's own from a previous Apply, or a
/// third party's — reinstated the pre-v0.8.101 double rotation on the very next read, because the
/// sidecar branch bypassed S4 entirely.
pub fn read_orientation(shot: &Shot, want_raw: bool) -> Option<u32> {
    if want_raw {
        let p = shot.raw.as_deref()?;
        return apply::sidecar_orientation(p, true).map(u32::from).or_else(|| raw_orientation(p));
    }
    // v1.0.0-rc TAIL 3 (verifier R1 — this one showed the user a sideways photograph): `has_jpg`,
    // not `jpg.is_some()`. A passenger is not the picture, so the picture's orientation is the
    // RAW's — the arm above, which this fn's own doc already reserves for a RAW-only shot.
    // Measured on the owner's portrait `HWU_0141.CR3`: with a passenger this returned `None` and
    // the frame displayed AND WEB-EXPORTED sideways; RAW-only it returns `Some(6)`.
    match shot.jpg.as_deref().filter(|_| shot.has_jpg) {
        Some(p) => apply::sidecar_orientation(p, false)
            .map(|abs| {
                u32::from(apply::orientation_minus_turns(abs, decoder_consumed_turns_at(shot, p)))
            })
            .or_else(|| decoded_base_orientation(shot, p)),
        None => {
            let p = shot.raw.as_deref()?;
            apply::sidecar_orientation(p, true).map(u32::from).or_else(|| raw_orientation(p))
        }
    }
}

/// Quarter-turns already applied by the finished-image decoder. RAW decoding leaves them
/// unapplied. Windows HEIC decoders (WIC and the hardware lane) apply the container transform;
/// read its primary-item properties so 180-degree rotations, squares and mirrors are explicit.
/// macOS keeps its existing Image I/O contract, using the dimension fallback for HEIC only.
/// JPEG/PNG/TIFF/etc. must never infer decoder rotation from unrelated EXIF dimensions.
/// The native app caches the residual from `read_orientation`; Apply derives consumed turns
/// from that cached residual and the fresh absolute tag, without a second decoder probe.
pub fn decoder_consumed_turns(shot: &Shot) -> u8 {
    // v1.0.0-rc TAIL 3 (verifier Y2): a passenger was never handed to a decoder, so no decoder
    // consumed any turns for it — the RAW-only answer (0) is the true one.
    match shot.jpg.as_deref().filter(|_| shot.has_jpg) {
        Some(p) => decoder_consumed_turns_at(shot, p),
        None => 0, // RAW-only: rawler's develop output is never pre-rotated (see `apply_rotation`)
    }
}

fn decoder_consumed_turns_at(shot: &Shot, path: &Path) -> u8 {
    if shot.kind != SrcKind::Heic {
        return 0;
    }
    // Without EXIF, Apply's absolute baseline is 1. The decoder's container transform is already
    // part of those base pixels; a newly created sidecar must add to them, not undo that transform.
    let Some(o) = exif_orientation(path) else { return 0 };
    #[cfg(windows)]
    if let Some(o) = heif_grid::heif_primary_orientation(path) {
        return orientation_to_turns(o);
    }
    if orientation_to_turns(o) & 1 == 0 {
        return 0;
    }
    let (Some(stored), Some(decoded)) = (exif_stored_dims(path), source_dimensions(shot)) else {
        return 0;
    };
    decoder_consumed_turns_from_dims(o, stored, decoded)
}

/// [`decoder_consumed_turns`]'s ARITHMETIC, as a PURE function — the S4 residual subtracted from the
/// file's own tag, with no file, no codec and no platform in it.
///
/// v0.8.103 (V2/V3/V12). Two claims the tree asserted but never pinned now have a home that runs on
/// every machine:
///   * **Windows/WIC** honours the container's `irot`, so a portrait iPhone HEIC decodes TRANSPOSED
///     and one quarter-turn is consumed.
///   * **macOS/Image I/O** does not (`kCGImageSourceCreateThumbnailWithTransform = false` /
///     `CreateImageAtIndex`), so the decoded size equals the stored size and NOTHING is consumed —
///     the answer the Mac write path depends on. That answer is DERIVED here from whatever
///     `imageio_dimensions` reports, not assumed: feed this the transposed pair and it says 1 on a
///     Mac too, which is exactly the hypothesis a Mac tester's upright-HEIC check falsifies.
///
/// FALSIFIER (L28): make the residual collapse the mirror class (return a bare 1 rather than 2 for
/// 5/7) and the mirrored rows below still read 1 — the turns answer is unchanged, which is why
/// `orientation_after_decoder_transform` carries its own mirror falsifier instead. Drop the
/// transpose comparison inside it and the Windows rows return 0, which is the sideways-HEIC bug.
#[inline]
pub fn decoder_consumed_turns_from_dims(
    orientation: u32,
    stored: (u32, u32),
    decoded: (u32, u32),
) -> u8 {
    let residual = orientation_after_decoder_transform(orientation, stored, decoded);
    (orientation_to_turns(orientation) + 4 - orientation_to_turns(residual)) & 3
}

/// Residual EXIF rotation after the actual decoder's container transform. The Windows HEIC
/// metadata path handles transforms that cannot be inferred from dimensions. Other finished
/// formats keep their EXIF orientation even when their copied dimension tags are stale.
fn decoded_base_orientation(shot: &Shot, path: &Path) -> Option<u32> {
    let o = exif_orientation(path)?;
    Some(u32::from(apply::orientation_minus_turns(o as u8, decoder_consumed_turns_at(shot, path))))
}

/// [`decoded_base_orientation`]'s decision, as a PURE function — so the rule that fixed the
/// sideways-HEIC bug is unit-testable on any machine, with no codec and no real file.
///
/// v0.8.102 (F14): the residual PRESERVES THE MIRROR CLASS — 6/8 → 1, but 5/7 → **2**, not 1.
/// `orientation_to_turns(2)` is still 0, so the rotation answer is identical; what changes is that
/// `orientation_is_mirrored` still fires downstream, so `note_orientation`'s once-per-session
/// "mirroring dropped, rotation honored" line — Falcon's ONLY disclosure that it is deliberately
/// showing different pixels from what the file asks for — no longer goes silent on exactly the files
/// whose mirror is being dropped. What the dims comparison can prove is a TRANSPOSE and nothing more
/// (a HEIF container's mirror is a separate `imir` property this rule cannot observe), so collapsing
/// to a bare non-mirrored 1 was stating an assumption as a fact.
///
/// FALSIFIER (L28): drop the `stored.0 != stored.1` term and a SQUARE source with a quarter-turn
/// tag silently loses its rotation (its transpose is itself, so "the decoder already turned it" is
/// unprovable and must not be assumed). Drop the transpose comparison and every rotated JPEG stops
/// rotating — the opposite bug, and a much louder one. Replace the mirror-preserving residual with a
/// bare `1` and the 5/7 rows below return 1 — the state in which the mirror-drop disclosure cannot
/// fire for any HEIC.
pub fn orientation_after_decoder_transform(
    orientation: u32,
    stored: (u32, u32),
    decoded: (u32, u32),
) -> u32 {
    let turns = orientation_to_turns(orientation);
    let quarter = turns & 1 == 1;
    let transposed = decoded == (stored.1, stored.0) && stored.0 != stored.1;
    if quarter && transposed {
        // The decoder consumed the whole quarter-turn; what is LEFT is the same mirror class at zero
        // turns (non-mirrored → 1, mirrored → 2). Keeping the mirror bit is what keeps the drop
        // disclosable — the rotation the caller applies is 0 either way.
        u32::from(apply::orientation_minus_turns(orientation as u8, turns))
    } else {
        orientation
    }
}

/// EXIF's record of the size the image was WRITTEN at (`PixelXDimension`/`PixelYDimension`) — the
/// PRE-rotation size, which is what makes it a usable reference point for
/// [`orientation_after_decoder_transform`]. `None` when either tag is absent or zero (an edited
/// JPEG, most PNG/TIFF), in which case the caller keeps the EXIF orientation untouched.
fn exif_stored_dims(path: &Path) -> Option<(u32, u32)> {
    use exif::{In, Tag};
    let reader = read_exif_bounded(path)?;
    let x = reader.get_field(Tag::PixelXDimension, In::PRIMARY)?.value.get_uint(0)?;
    let y = reader.get_field(Tag::PixelYDimension, In::PRIMARY)?.value.get_uint(0)?;
    (x > 0 && y > 0).then_some((x, y))
}

/// A1/F2 (CODEBASE_REVIEW_2026-07 §5, defect #1): the house metadata-read cap for kamadak-exif.
/// `read_from_container` buffers the ENTIRE container (for a TIFF it `read_to_end`s the whole file),
/// so a crafted OR legitimate multi-GB `.tif` forces a multi-GB allocation — an UNcatchable Rust OOM
/// abort, not a panic the worker `catch_unwind` can rescue — and v0.8.0 made the orientation read fire
/// UNGATED on the fast/detail/thumb worker tiers per decode (not just on info-panel open). Real files
/// carry the EXIF IFD in the first few KB (JPEG APP1 / TIFF IFD0), so the cap is invisible to genuine
/// files; metadata absent within the cap degrades to `None` (documented acceptable loss). Same 4 MB
/// idiom as the WebP ICCP / TIFF ICC caps elsewhere in this file.
const EXIF_READ_CAP: u64 = 4 * 1024 * 1024;

/// Parse an EXIF reader from an in-memory container buffer (already length-capped by the caller).
/// Split out so the cap discipline is unit-testable without a real multi-GB file.
fn exif_from_bytes(buf: &[u8]) -> Option<exif::Exif> {
    exif::Reader::new().read_from_container(&mut std::io::Cursor::new(buf)).ok()
}

/// Read at most [`EXIF_READ_CAP`] bytes of `path` into memory and parse EXIF from them — the bounded
/// replacement for a raw `read_from_container` on a `BufReader<File>` (see [`EXIF_READ_CAP`]).
fn read_exif_bounded(path: &Path) -> Option<exif::Exif> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    // `take` caps the alloc: `read_to_end` on the capped reader can grow `buf` to at most the cap.
    file.take(EXIF_READ_CAP).read_to_end(&mut buf).ok()?;
    exif_from_bytes(&buf)
}

pub(crate) fn exif_orientation(path: &Path) -> Option<u32> {
    use exif::{In, Tag};
    let reader = read_exif_bounded(path)?;
    // Validate at the shared reader so display AND Apply cannot narrow 262 into orientation 6.
    reader.get_field(Tag::Orientation, In::PRIMARY).and_then(|f| f.value.get_uint(0))
        .filter(|o| (1..=8).contains(o))
}

pub(crate) fn raw_orientation(path: &Path) -> Option<u32> {
    // Metadata-only rawler decode (no pixel decompress) — the same lightweight path wb_kelvin uses.
    let src = rawler::rawsource::RawSource::new(path).ok()?;
    let decoder = rawler::get_decoder(&src).ok()?;
    let md = decoder.raw_metadata(&src, &RawDecodeParams::default()).ok()?;
    md.exif.orientation.map(|o| o as u32).filter(|o| (1..=8).contains(o))
}

/// Normalised DISPLAY-space → SOURCE-space point map for a source shown rotated `turns` × 90° CW.
/// This is the inverse of the on-screen rotation — the EXACT transform the GPU shader applies to each
/// output fragment's uv, so a tile cropped through it lines up with what the shader samples:
///   0: (u,v)   1: (v, 1-u)   2: (1-u, 1-v)   3: (1-v, u).
pub fn display_to_source_uv(turns: u8, u: f32, v: f32) -> (f32, f32) {
    match turns & 3 {
        1 => (v, 1.0 - u),
        2 => (1.0 - u, 1.0 - v),
        3 => (1.0 - v, u),
        _ => (u, v),
    }
}

/// Map a normalised axis-aligned rect from DISPLAY space to SOURCE space (corner map via
/// [`display_to_source_uv`], re-min/maxed). Returns `(u0,v0,u1,v1)` with `u0≤u1`, `v0≤v1`.
pub fn display_rect_to_source(turns: u8, u0: f32, v0: f32, u1: f32, v1: f32) -> (f32, f32, f32, f32) {
    let (au, av) = display_to_source_uv(turns, u0, v0);
    let (bu, bv) = display_to_source_uv(turns, u1, v1);
    (au.min(bu), av.min(bv), au.max(bu), av.max(bv))
}

/// Inverse of [`display_rect_to_source`]: re-express a SOURCE-space rect (the worker's clamped crop)
/// back in DISPLAY space, so the overlay positions the tile where the user is looking. A CW rotation
/// by `turns` is undone by a CW rotation by `4-turns`, so this reuses the same primitive.
pub fn source_rect_to_display(turns: u8, u0: f32, v0: f32, u1: f32, v1: f32) -> (f32, f32, f32, f32) {
    display_rect_to_source((4 - (turns & 3)) & 3, u0, v0, u1, v1)
}

/// Rotate a tightly-packed pixel buffer by `turns` × 90° CLOCKWISE (same convention as
/// [`orientation_to_turns`] / the GPU UV transform), returning the rotated buffer + its new
/// dimensions. `bpp` = bytes per pixel. turns 0 (or a degenerate/short buffer) is a cheap copy.
/// CPU-side rotation is used for the small filmstrip/Selection thumbnails and to bake rotation into
/// web exports; the large fast/detail/ROI frames rotate on the GPU at upload instead (no per-frame
/// copy on the hot path).
fn rotate_packed(src: &[u8], w: u32, h: u32, turns: u8, bpp: usize) -> (Vec<u8>, u32, u32) {
    rotate_samples(src, w, h, turns, bpp).unwrap_or_else(|| (src.to_vec(), w, h))
}

/// v1.0.0-rc PNG EXPORT (queue item 35): the ROTATION KERNEL, generic over the sample type, so the
/// 16-bit arms share it instead of reinterpreting a `[u16]` as bytes. `ch` is ELEMENTS per pixel:
/// bytes for a `[u8]` buffer (the shipped 3 and 4), samples for a `[u16]` one (3 and 4 again, at
/// twice the width) -- so the "bpp 6/8" a byte view would have needed never appears.
///
/// `None` means THERE IS NOTHING TO DO -- `turns == 0`, a degenerate size, or a buffer too short --
/// which is what lets [`rotate_pixels`] hand its own buffer straight back with no copy while
/// [`rotate_packed`] still makes the copy its `&[u8]` signature has always made.
fn rotate_samples<T: Copy + Default>(
    src: &[T],
    w: u32,
    h: u32,
    turns: u8,
    ch: usize,
) -> Option<(Vec<T>, u32, u32)> {
    let turns = turns & 3;
    let (wu, hu) = (w as usize, h as usize);
    if turns == 0 || w == 0 || h == 0 || src.len() < wu * hu * ch {
        return None;
    }
    let bpp = ch;
    let (dw, dh) = if turns & 1 == 1 { (h, w) } else { (w, h) };
    let (dwu, dhu) = (dw as usize, dh as usize);
    let mut out = vec![T::default(); dwu * dhu * bpp];
    // Dest pixel (x',y') reads source pixel:
    //   turns=1 (90° CW):  (y',        h-1 - x')
    //   turns=2 (180°):    (w-1 - x',  h-1 - y')
    //   turns=3 (270° CW): (w-1 - y',  x')
    for y in 0..dhu {
        for x in 0..dwu {
            let (sx, sy) = match turns {
                1 => (y, hu - 1 - x),
                2 => (wu - 1 - x, hu - 1 - y),
                _ => (wu - 1 - y, x), // 3
            };
            let si = (sy * wu + sx) * bpp;
            let di = (y * dwu + x) * bpp;
            out[di..di + bpp].copy_from_slice(&src[si..si + bpp]);
        }
    }
    Some((out, dw, dh))
}

/// Rotate a [`Pixels`] by `turns` x 90 degrees CW, BY VALUE (round 35). At `turns == 0` -- which is
/// every upright photograph, i.e. most of them -- the buffer is handed straight back and NOTHING is
/// copied; the shipped `rotate_rgb` allocated a full second frame there, which was one of the three
/// full-size copies the export used to hold at once (the round record's per-stage table).
pub fn rotate_pixels(px: Pixels, w: u32, h: u32, turns: u8) -> (Pixels, u32, u32) {
    let ch = px.channels();
    /// Rotate, or hand the buffer back untouched. Generic so all four layouts share one body.
    fn turned<T: Copy + Default>(
        v: Vec<T>,
        w: u32,
        h: u32,
        turns: u8,
        ch: usize,
    ) -> (Vec<T>, u32, u32) {
        match rotate_samples(&v, w, h, turns, ch) {
            Some(out) => out,
            None => (v, w, h),
        }
    }
    match px {
        Pixels::Rgb8(v) => {
            let (o, dw, dh) = turned(v, w, h, turns, ch);
            (Pixels::Rgb8(o), dw, dh)
        }
        Pixels::Rgba8(v) => {
            let (o, dw, dh) = turned(v, w, h, turns, ch);
            (Pixels::Rgba8(o), dw, dh)
        }
        Pixels::Rgb16(v) => {
            let (o, dw, dh) = turned(v, w, h, turns, ch);
            (Pixels::Rgb16(o), dw, dh)
        }
        Pixels::Rgba16(v) => {
            let (o, dw, dh) = turned(v, w, h, turns, ch);
            (Pixels::Rgba16(o), dw, dh)
        }
    }
}

/// Rotate packed RGBA8 by `turns` × 90° CW (see [`rotate_packed`]) — the thumbnail / fallback path.
pub fn rotate_rgba(rgba: &[u8], w: u32, h: u32, turns: u8) -> (Vec<u8>, u32, u32) {
    rotate_packed(rgba, w, h, turns, 4)
}

/// Rotate packed RGB8 by `turns` × 90° CW (see [`rotate_packed`]) — the web-export bake path.
pub fn rotate_rgb(rgb: &[u8], w: u32, h: u32, turns: u8) -> (Vec<u8>, u32, u32) {
    rotate_packed(rgb, w, h, turns, 3)
}

#[cfg(test)]
mod orient_tests {
    use super::*;

    #[test]
    fn stale_exif_dimensions_cannot_suppress_jpeg_orientation() {
        let mut tiff = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
        tiff.extend_from_slice(&2u16.to_le_bytes());
        for (tag, ty, value) in [(0x112u16, 3u16, 6u32), (0x8769, 4, 38)] {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&ty.to_le_bytes());
            tiff.extend_from_slice(&1u32.to_le_bytes());
            tiff.extend_from_slice(&value.to_le_bytes());
        }
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        for (tag, value) in [(0xa002u16, 3u32), (0xa003, 2)] {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&4u16.to_le_bytes());
            tiff.extend_from_slice(&1u32.to_le_bytes());
            tiff.extend_from_slice(&value.to_le_bytes());
        }
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut jpeg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpeg, 90).encode(&[90; 18], 2, 3, jpeg_encoder::ColorType::Rgb).unwrap();
        let mut app1 = b"\xff\xe1".to_vec();
        app1.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        jpeg.splice(2..2, app1);
        let path = std::env::temp_dir().join(format!("falcon_stale_exif_{}.jpg", std::process::id()));
        std::fs::write(&path, &jpeg).unwrap();
        let shot = Shot { id: 0, name: "test".into(), has_raw: false, has_jpg: true, raw: None,
            jpg: Some(path.clone()), kind: SrcKind::Jpeg, cloud_placeholder: false, sniffed: None };
        assert_eq!(exif_stored_dims(&path), Some((3, 2)));
        assert_eq!(source_dimensions(&shot), Some((2, 3)));
        assert_eq!(decoder_consumed_turns(&shot), 0);
        assert_eq!(read_orientation(&shot, false), Some(6));
        let tiff_start = jpeg.windows(6).position(|b| b == b"Exif\0\0").unwrap() + 6;
        jpeg[tiff_start + 18..tiff_start + 22].copy_from_slice(&262u32.to_le_bytes());
        std::fs::write(&path, &jpeg).unwrap();
        assert_eq!(read_orientation(&shot, false), None);
        let report = apply_rotation(&RotApplyPlan { finished: Some(path.clone()), finished_is_jpeg: true,
            raw: None, base_turns: 0, delta: 1 });
        assert!(report.ok);
        assert_eq!(sidecar_orientation(&path, false), Some(6), "Apply starts from upright, not truncated 262");
        assert_eq!(exif_orientation(&path), None, "malformed original is not patched");
        std::fs::remove_file(path.with_extension("jpg.xmp")).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn exif_orientation_maps_to_clockwise_turns() {
        // Upright + the three pure rotations.
        assert_eq!(orientation_to_turns(1), 0);
        assert_eq!(orientation_to_turns(6), 1); // 90° CW — the R5 II landscape-pixels case
        assert_eq!(orientation_to_turns(3), 2); // 180°
        assert_eq!(orientation_to_turns(8), 3); // 270° CW
        // Mirrored variants keep ONLY their rotation component.
        assert_eq!(orientation_to_turns(2), 0);
        assert_eq!(orientation_to_turns(4), 2);
        assert_eq!(orientation_to_turns(5), 1);
        assert_eq!(orientation_to_turns(7), 3);
        // Absent / invalid → upright.
        assert_eq!(orientation_to_turns(0), 0);
        assert_eq!(orientation_to_turns(9), 0);
        for m in [2, 4, 5, 7] {
            assert!(orientation_is_mirrored(m));
        }
        for r in [1, 3, 6, 8] {
            assert!(!orientation_is_mirrored(r));
        }
    }

    #[test]
    fn read_orientation_prefers_applied_sidecar_roundtrip() {
        // finding #9: an applied rotation on the sidecar route lives ONLY in the .xmp. read_orientation —
        // Falcon's sole display-orientation reader — must read it back (sidecar WINS over embedded), so the
        // rotation round-trips through the app's own reader instead of reverting on the next re-decode.
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("falcon_ro_test_{pid}_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();

        // RAW-only shot; the fake CR3's embedded orientation is unreadable → upright without a sidecar.
        let raw = dir.join("A.CR3");
        std::fs::write(&raw, [0u8; 32]).unwrap();
        let shot = Shot { id: 0, name: "A".into(), has_raw: true, has_jpg: false,
            raw: Some(raw.clone()), jpg: None, kind: SrcKind::Jpeg, cloud_placeholder: false, sniffed: None };
        assert_eq!(read_orientation(&shot, false), None, "no sidecar → embedded (None)");
        assert_eq!(read_orientation(&shot, true), None);
        // Apply writes A.xmp = 8 (270°). read_orientation now returns it on BOTH the base and develop reads.
        std::fs::write(dir.join("A.xmp"), "<rdf:Description tiff:Orientation=\"8\"/>").unwrap();
        assert_eq!(read_orientation(&shot, false), Some(8), "RAW-only base read honors the applied sidecar");
        assert_eq!(read_orientation(&shot, true), Some(8), "develop read honors the applied sidecar");

        // Pair isolation: want_raw=false reads the JPG's FULLNAME sidecar, never the RAW's basename.
        let jpg = dir.join("B.JPG");
        std::fs::write(&jpg, [0u8; 16]).unwrap(); // not a real JPEG → embedded None
        let raw_b = dir.join("B.CR3");
        std::fs::write(&raw_b, [0u8; 32]).unwrap();
        let pair = Shot { id: 1, name: "B".into(), has_raw: true, has_jpg: true,
            raw: Some(raw_b.clone()), jpg: Some(jpg.clone()), kind: SrcKind::Jpeg, cloud_placeholder: false, sniffed: None };
        std::fs::write(dir.join("B.xmp"), "<rdf:Description tiff:Orientation=\"3\"/>").unwrap();     // RAW basename
        std::fs::write(dir.join("B.JPG.xmp"), "<rdf:Description tiff:Orientation=\"6\"/>").unwrap(); // JPG fullname
        assert_eq!(read_orientation(&pair, false), Some(6), "finished read = fullname sidecar, not the RAW basename");
        assert_eq!(read_orientation(&pair, true), Some(3), "develop read = RAW basename sidecar");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bounded_exif_parse_reads_orientation_and_degrades_gracefully() {
        // A1/F2: the bounded reader parses metadata from an in-memory (length-capped) buffer instead of
        // buffering a whole multi-GB container. Feed a minimal little-endian TIFF carrying a single
        // Orientation=6 SHORT entry and confirm it reads through `exif_from_bytes` (the split-out,
        // testable half of `read_exif_bounded`), and that garbage / empty buffers degrade to None.
        let tiff: [u8; 26] = [
            0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, // "II", magic 42, IFD0 offset = 8
            0x01, 0x00, // 1 IFD entry
            0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, // tag 0x0112 Orientation, SHORT, count 1
            0x06, 0x00, 0x00, 0x00, // value = 6 (90° CW)
            0x00, 0x00, 0x00, 0x00, // next IFD = 0
        ];
        let reader = exif_from_bytes(&tiff).expect("minimal TIFF container parses");
        let o = reader
            .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0));
        assert_eq!(o, Some(6), "bounded parse reads the Orientation tag");
        assert!(exif_from_bytes(&[0u8; 4]).is_none(), "garbage buffer → None, no panic");
        assert!(exif_from_bytes(&[]).is_none(), "empty buffer → None, no panic");
        // The cap stays the house 4 MB metadata budget (the WebP ICCP / TIFF ICC idiom).
        assert_eq!(EXIF_READ_CAP, 4 * 1024 * 1024);
    }

    // A distinctly-labelled 2×3 RGB image (unique byte per pixel) so a rotation's pixel placement is
    // unambiguous. Layout (col,row), value = 10*(row+1) + (col+1):
    //   row0: 11 12    row1: 21 22    row2: 31 32   (w=2, h=3)
    fn label_img() -> (Vec<u8>, u32, u32) {
        let (w, h) = (2u32, 3u32);
        let mut v = Vec::new();
        for row in 0..h {
            for col in 0..w {
                let p = (10 * (row + 1) + (col + 1)) as u8;
                v.extend_from_slice(&[p, p, p]);
            }
        }
        (v, w, h)
    }
    fn at(buf: &[u8], w: u32, x: u32, y: u32) -> u8 {
        buf[((y * w + x) * 3) as usize]
    }

    #[test]
    fn rotate_rgb_places_pixels_clockwise() {
        let (src, w, h) = label_img();
        // 90° CW: dest is 3×2; source top-left (11) lands top-right; the top row becomes the right col.
        let (d1, dw1, dh1) = rotate_rgb(&src, w, h, 1);
        assert_eq!((dw1, dh1), (3, 2));
        assert_eq!(at(&d1, dw1, 2, 0), 11); // src (0,0) top-left → dest top-right
        assert_eq!(at(&d1, dw1, 0, 0), 31); // src (0,2) bottom-left → dest top-left
        assert_eq!(at(&d1, dw1, 2, 1), 12); // src (1,0) top-right → dest bottom-right
        // 180°: dims preserved, corners swap diagonally.
        let (d2, dw2, dh2) = rotate_rgb(&src, w, h, 2);
        assert_eq!((dw2, dh2), (2, 3));
        assert_eq!(at(&d2, dw2, 1, 2), 11); // src (0,0) → dest (1,2)
        assert_eq!(at(&d2, dw2, 0, 0), 32); // src (1,2) → dest (0,0)
        // 270° CW (= 90° CCW): dest 3×2; src top-left → bottom-left, src bottom-right → top-right.
        let (d3, dw3, dh3) = rotate_rgb(&src, w, h, 3);
        assert_eq!((dw3, dh3), (3, 2));
        assert_eq!(at(&d3, dw3, 0, 1), 11); // src (0,0) top-left → dest bottom-left
        assert_eq!(at(&d3, dw3, 2, 0), 32); // src (1,2) bottom-right → dest top-right
        assert_eq!(at(&d3, dw3, 2, 1), 31); // src (0,2) bottom-left → dest bottom-right
        // turns 0 is an exact copy; turns 4 wraps to 0.
        assert_eq!(rotate_rgb(&src, w, h, 0).0, src);
        assert_eq!(rotate_rgb(&src, w, h, 4).0, src);
    }

    #[test]
    fn rotate_then_unrotate_is_identity_rgba() {
        // A round trip through turns t then 4-t restores the original buffer + dims, for RGBA too.
        let (rgb, w, h) = label_img();
        let rgba: Vec<u8> = rgb.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
        for t in 0..4u8 {
            let (r, rw, rh) = rotate_rgba(&rgba, w, h, t);
            let (b, bw, bh) = rotate_rgba(&r, rw, rh, (4 - t) & 3);
            assert_eq!((bw, bh), (w, h), "dims restored for turns {t}");
            assert_eq!(b, rgba, "pixels restored for turns {t}");
        }
    }

    #[test]
    fn display_source_rect_mapping_all_turns() {
        // The GPU shader samples source at display_to_source_uv(turns, out_uv); the ROI worker crops
        // the source rect display_rect_to_source(turns, disp_rect) and reports it back through the
        // inverse. Verify the point map on the four unit corners + rect round-trips.
        let corners = [(0.0f32, 0.0f32), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)];
        let expect = [
            [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)],       // turns 0 identity
            [(0.0, 1.0), (0.0, 0.0), (1.0, 1.0), (1.0, 0.0)],       // turns 1: (v,1-u)
            [(1.0, 1.0), (0.0, 1.0), (1.0, 0.0), (0.0, 0.0)],       // turns 2: (1-u,1-v)
            [(1.0, 0.0), (1.0, 1.0), (0.0, 0.0), (0.0, 1.0)],       // turns 3: (1-v,u)
        ];
        for t in 0..4u8 {
            for (i, &(u, v)) in corners.iter().enumerate() {
                let got = display_to_source_uv(t, u, v);
                let ex = expect[t as usize][i];
                assert!((got.0 - ex.0).abs() < 1e-6 && (got.1 - ex.1).abs() < 1e-6,
                    "turns {t} corner {i}: got {got:?} expected {ex:?}");
            }
        }
        // A landscape sub-rect (right half, upper band) and a portrait edge tile, round-tripped.
        for &(u0, v0, u1, v1) in &[(0.5f32, 0.0f32, 1.0f32, 0.25f32), (0.0, 0.9, 0.2, 1.0)] {
            for t in 0..4u8 {
                let (su0, sv0, su1, sv1) = display_rect_to_source(t, u0, v0, u1, v1);
                assert!(su0 <= su1 && sv0 <= sv1, "turns {t}: source rect ordered");
                // Source rect maps back to the original display rect.
                let (bu0, bv0, bu1, bv1) = source_rect_to_display(t, su0, sv0, su1, sv1);
                assert!((bu0 - u0).abs() < 1e-6 && (bv0 - v0).abs() < 1e-6
                    && (bu1 - u1).abs() < 1e-6 && (bv1 - v1).abs() < 1e-6,
                    "turns {t}: rect round-trip {:?} != {:?}", (bu0, bv0, bu1, bv1), (u0, v0, u1, v1));
            }
        }
    }
}

/// Extract the embedded ICC colour profile from a JPEG (the bytes of the `APP2`
/// `ICC_PROFILE` segment(s), concatenated in chunk order). The profile sits in the file
/// header before the image scan, so a bounded prefix read covers it. `None` if the file
/// isn't a JPEG or carries no profile (e.g. a Canon sRGB JPG, which tags sRGB in EXIF only).
fn extract_icc_from_jpeg(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    // 512 KB covers every realistic ICC profile (sRGB ~3 KB, Adobe RGB / Display P3 < 1 KB) even
    // after a large EXIF thumbnail; the bulk compressed scan data (which we never need) follows.
    let mut buf = vec![0u8; 512 * 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let b = &buf;
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return None; // not a JPEG (SOI = FFD8)
    }
    let mut i = 2usize;
    let mut chunks: Vec<(u8, Vec<u8>)> = Vec::new(); // (sequence number, chunk bytes)
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            break; // marker stream misaligned
        }
        let marker = b[i + 1];
        i += 2;
        // standalone markers carry no length payload
        if marker == 0xD9 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if marker == 0xDA {
            break; // SOS — image scan begins, no more metadata segments
        }
        if i + 2 > b.len() {
            break;
        }
        let seg_len = ((b[i] as usize) << 8) | (b[i + 1] as usize);
        if seg_len < 2 || i + seg_len > b.len() {
            break;
        }
        let data = &b[i + 2..i + seg_len];
        // APP2 + "ICC_PROFILE\0" + seq(1) + count(1) + ICC chunk
        if marker == 0xE2 && data.len() > 14 && &data[0..12] == b"ICC_PROFILE\0" {
            chunks.push((data[12], data[14..].to_vec()));
        }
        i += seg_len;
    }
    if chunks.is_empty() {
        return None;
    }
    chunks.sort_by_key(|(s, _)| *s);
    Some(chunks.into_iter().flat_map(|(_, d)| d).collect())
}

/// What a file DECLARES about its colour space: the raw ICC profile when it embeds one, and the
/// human description.
///
/// v0.8.140 (THE COLOR ROUND, C2): every format door used to hand the source-gamut question a bare
/// description STRING, and `Gamut::from_description` guessed the colour space from that name. A
/// macOS screen capture embeds the DISPLAY's own profile, described "Display" — no name matched, so
/// P3 pixels were converted FROM sRGB and every output mode came out uniformly duller, silently.
/// The doors now carry the BYTES as well, and the bytes are what decide (see
/// [`falcon_color::resolve_source_gamut`]); the description is kept for the label and the log, and
/// still answers where there are no bytes to read.
///
/// `icc: None` with `desc: Some(..)` is a real and correct state, not a degraded one: a HEIF `nclx`
/// box and a JXL CICP triple name their primaries by ENUMERATED CODE, carrying no profile at all.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct ColorTag {
    /// The embedded ICC profile's bytes, when the file carries one.
    pub icc: Option<Vec<u8>>,
    /// The colour space's name — the profile's own description, or the name of an enumerated code.
    pub desc: Option<String>,
    /// v0.8.141 (R7): TRUE only when `desc` was read out of `icc`'s OWN bytes.
    ///
    /// The two are not the same claim and this tree conflated them. A PNG that carries an `iCCP`
    /// profile whose description will not parse still gets a `desc` — from the bare `sRGB` chunk,
    /// by the precedence this door has always had — and a JXL in the same situation gets one from
    /// its CICP code. Both are correct GUESSES at the name; neither is what the profile says. The
    /// log line and the panel row used to print such a name as `profile "sRGB"`, attributing to the
    /// embedded profile a description it does not contain, on exactly the files this round exists
    /// for (P3 colorants under an unreadable name). [`ColorTag::profile_desc`] is the accessor that
    /// may be quoted; `desc` is the accessor that may be MATCHED.
    pub desc_from_profile: bool,
}

impl ColorTag {
    /// A tag read from an embedded profile: the bytes, plus whatever description they carry.
    fn from_icc(icc: Vec<u8>) -> Self {
        let desc = icc_description(&icc).filter(|d| !d.is_empty());
        ColorTag { desc_from_profile: desc.is_some(), icc: Some(icc), desc }
    }
    /// A tag that is only a NAME — an enumerated colour-primaries code, or a bare PNG `sRGB` chunk.
    fn named(desc: &str) -> Self {
        ColorTag { icc: None, desc: Some(desc.to_string()), desc_from_profile: false }
    }
    /// The file declared nothing at all about its colour space.
    fn is_silent(&self) -> bool {
        self.icc.is_none() && self.desc.is_none()
    }
    /// The description ONLY when the embedded profile itself supplied it (v0.8.141, R7) — i.e. the
    /// only name anything is entitled to print as `profile '…'`. `None` for a synthesized name (an
    /// `nclx`/CICP code, a bare PNG `sRGB` chunk) and for a profile whose `desc` tag would not read.
    fn profile_desc(&self) -> Option<&str> {
        self.desc.as_deref().filter(|_| self.desc_from_profile)
    }
}

/// Make an untrusted free-text fragment safe to drop into ONE line of a log or ONE cell of the info
/// panel (v0.8.141, R11).
///
/// An ICC description and a file name are both attacker-controlled: a macOS file name may legally
/// contain a newline (which would forge a second log line — and every log reader in this project,
/// including the boot-verify greps, counts lines), and a `desc` may contain C0/C1 controls or the
/// Unicode bidi-override formatting characters, which reorder everything after them in a panel row.
/// Dep-free by construction: `char::is_control` already covers C0, C1 and DEL, and the bidi set is
/// eight code points. Everything else — every script, every emoji — passes through untouched.
fn sanitize_one_line(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    *c,
                    '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
                )
        })
        .collect()
}

/// Read the human-readable description out of an ICC profile — this is what actually NAMES the
/// colour space ("Adobe RGB (1998)", "Display P3", "sRGB IEC61966-2.1"), unlike the EXIF
/// `ColorSpace` tag which only distinguishes sRGB / Adobe RGB / Uncalibrated. Handles both the
/// ICC v2 `desc` (ASCII) and the ICC v4 `mluc` (UTF-16BE) description types.
///
/// v0.8.177 (G-Y1) — BOUNDED BY THE TAG, NOT BY THE BUFFER. Both reads used to stop at
/// `icc.len()`, so a `desc` tag declaring 5000 bytes of text inside a 40-byte tag returned the
/// NEIGHBOURING tags' bytes as the profile's name. That was always a lie on the panel; since this
/// round it also feeds route (3) — [`falcon_color::Gamut::from_description`] — so a crafted profile
/// could pick the gamut every pixel is converted from out of bytes it does not own. Every read is
/// now clipped to the tag's own DECLARED extent (`off + size` from the tag table), which is
/// `falcon-color`'s `icc_description` shape; a tag whose declared extent cannot hold what its
/// internal count promises is TRUNCATED to the tag, and one too short to hold its own header is
/// refused outright. Pinned by `a_desc_tag_cannot_name_a_profile_with_its_neighbours_bytes`.
///
/// v0.8.181 (pre-merge review) — …AND THE FLOOR AT THE OTHER END. The extent check bounded the tag
/// from ABOVE; nothing bounded it from below, so a `desc` entry whose offset pointed backwards into
/// the 128-byte HEADER was read as text and could name the profile with header bytes. That is the
/// same route-(3) hole from the opposite direction, and `falcon-color`'s `icc_find_tag`
/// (lib.rs, `off >= 128`) has always refused it. The two parsers still exist deliberately (the
/// §3 note: unification is a Phase-4 chip) — what they must not do is disagree about what a tag IS.
fn icc_description(icc: &[u8]) -> Option<String> {
    if icc.len() < 132 {
        return None;
    }
    let be32 = |o: usize| -> usize {
        ((icc[o] as usize) << 24) | ((icc[o + 1] as usize) << 16) | ((icc[o + 2] as usize) << 8) | (icc[o + 3] as usize)
    };
    let tag_count = be32(128);
    for k in 0..tag_count {
        let e = 132 + k * 12; // tag table starts right after the 4-byte count
        if e + 12 > icc.len() {
            break;
        }
        if &icc[e..e + 4] != b"desc" {
            continue;
        }
        let off = be32(e + 4);
        let size = be32(e + 8);
        // THE TAG'S OWN EXTENT — every read below is clipped to `end`, and `end` is proved to lie
        // inside the buffer here, once. A tag too short for the 12-byte header of either type names
        // nothing at all, and (v0.8.181) one aimed BELOW 128 is aimed at the profile header rather
        // than at tag data — `falcon-color`'s `icc_find_tag` floor, spelled here too.
        let end = off.checked_add(size)?;
        if off < 128 || end > icc.len() || size < 12 {
            return None;
        }
        let tag_type = &icc[off..off + 4];
        if tag_type == b"desc" {
            // v2 textDescriptionType: ['desc'][reserved 4][ASCII count 4][ASCII string…]
            let count = be32(off + 8);
            let s = off + 12;
            let e = s.checked_add(count)?.min(end);
            if s >= e {
                return None;
            }
            let txt: String = icc[s..e].iter().take_while(|&&c| c != 0).map(|&c| c as char).collect();
            return Some(txt.trim().to_string());
        } else if tag_type == b"mluc" {
            // v4 multiLocalizedUnicodeType: [...][rec count 4][rec size 4][record: lang2 country2 len4 off4]…
            if be32(off + 8) == 0 || off + 28 > end {
                return None;
            }
            let r = off + 16; // first record
            let len = be32(r + 4); // string length in bytes
            let str_off = off.checked_add(be32(r + 8))?; // offset is relative to the start of the mluc tag
            let e = str_off.checked_add(len)?.min(end);
            if str_off >= e || str_off < off {
                return None;
            }
            let u16s: Vec<u16> =
                icc[str_off..e].chunks_exact(2).map(|c| ((c[0] as u16) << 8) | c[1] as u16).collect();
            return Some(String::from_utf16_lossy(&u16s).trim().to_string());
        }
        return None;
    }
    None
}

/// Map an ICC profile description (or EXIF value) to a short, consistent colour-space name. sRGB
/// variants ALL normalise to exactly "sRGB" so the UI's "is it sRGB?" supported-check stays simple.
fn normalize_cs(desc: &str) -> String {
    let d = desc.to_ascii_lowercase();
    if d.contains("srgb") {
        "sRGB".into()
    } else if d.contains("adobe rgb") || d.contains("adobergb") {
        "Adobe RGB".into()
    } else if d.contains("display p3") || d.contains("display-p3") {
        "Display P3".into()
    } else if d.contains("dci-p3") || d.contains("dci p3") {
        "DCI-P3".into()
    } else if d.contains("p3") {
        "Display P3".into()
    } else if d.contains("prophoto") {
        "ProPhoto RGB".into()
    } else if d.contains("2020") {
        "Rec. 2020".into()
    } else {
        desc.trim().to_string()
    }
}

/// The panel's colour-space name for a file, now that the file's own COLORIMETRY can disagree with
/// its profile's name (v0.8.140 C3).
///
/// [`normalize_cs`] shortens the names it recognises and otherwise hands the raw description
/// straight through — so a screen capture whose profile is described "Display" put the literal word
/// "Display" in the info panel, while the render pipeline (correctly, after C1/C2) converted the
/// pixels FROM Display P3. A panel that contradicts the render is worse than one that says nothing.
///
/// The rule: when the resolution lands on a named gamut and the NAME does not already say that
/// gamut, show the RESOLVED space and keep the raw profile name beside it — "Display P3 — profile
/// 'Display'". Nothing else changes: a profile whose name and colorimetry agree (every ordinary
/// sRGB / Adobe RGB / Display P3 file) reads exactly as it did before.
///
/// v0.8.141 (R8) — THE LABEL NOW CARRIES THE ROUTE, and takes the resolution rather than re-deriving
/// it. Two things were wrong with the v0.8.140 shape:
/// - it called `Gamut::from_icc_bytes` a SECOND time, parsing the same profile the caller had just
///   parsed, purely to ask a question the caller already had the answer to;
/// - it could only distinguish "measured" from "not placed", so a measured-panel profile that MISSED
///   τ and fell through to sRGB was labelled with the same flat confidence as one whose colorants
///   were read: "sRGB — profile 'DELL U2723…'" reads as a measurement, and it is a guess.
///   `GamutRoute` already knows which happened, so the guess now says `(assumed)`.
///
/// v0.8.141 (R7): only a name the PROFILE ITSELF carries may be printed as `profile '…'`. When the
/// name was synthesized (a PNG `sRGB` chunk standing in for an unreadable `desc`, an `nclx`/CICP
/// code) there is no profile name to quote, so the row is the resolved space alone — the panel
/// still stops contradicting the render, it just does not put words in the profile's mouth. The raw
/// description survives in full in the log line ([`note_color_resolution`]).
///
/// [VD] the exact copy — the em-dash form, the quoting, and the "(assumed)" word — is the
/// executor's judgement.
fn color_space_label_for(tag: &ColorTag, desc: &str, r: &falcon_color::GamutResolution) -> String {
    // v0.8.177 — THE FAITHFUL ROUTE NAMES ITSELF, and it must, because there is no modeled gamut to
    // name it with. The resolved space IS this profile, so the honest row is the profile's OWN
    // description ("ProPhoto RGB", "LStar-RGB-v2.icc") — read by falcon-color from the same bytes
    // the transform was built from, sanitized and length-bounded there. No hedge: nothing was
    // assumed. No `— profile '…'` suffix either: it would repeat the name it just printed.
    if r.route == falcon_color::GamutRoute::Faithful {
        return r.gamut.display_name();
    }
    let named = normalize_cs(desc);
    // No profile-supplied name ⇒ nothing to annotate WITH. (R10 pins that every modeled gamut's
    // own label is a fixed point of `normalize_cs`, so an agreeing file compares equal here.)
    let Some(profile_name) = tag.profile_desc() else { return named };
    if r.gamut.label() == named {
        return named; // name and resolution agree — byte-identical to the pre-round panel row
    }
    let hedge = match r.route {
        falcon_color::GamutRoute::Colorimetry => "", // measured from the profile's own colorants
        _ => " (assumed)",                           // the name, or the sRGB floor, decided
    };
    format!("{}{hedge} — profile {}", r.gamut.label(), quoted_profile_name(profile_name))
}

/// A raw ICC description as a quoted, LENGTH-BOUNDED fragment for the panel. ICC descriptions are
/// free text and real ones run long ("Dell S2725QS Native, D6500, 2.2, MHC2 calibrated 2026-01-14");
/// the panel row is a fixed-width cell, so an unbounded name would push the row off the panel.
///
/// v0.8.141 (R11): the cap counts DISPLAY CELLS, not `char`s, and the text is sanitized first. A
/// description is untrusted free text: 32 CJK characters occupy roughly 64 cells and would push the
/// row off the panel exactly as an uncapped ASCII name did, and a bidi override inside it would
/// reorder the whole row. The width estimate is deliberately crude and dep-free — ASCII costs one
/// cell, everything else two — because it only has to bound the row, not typeset it.
fn quoted_profile_name(desc: &str) -> String {
    const MAX_CELLS: usize = 32;
    let clean = sanitize_one_line(desc);
    let mut out = String::with_capacity(MAX_CELLS + 3);
    let (mut cells, mut clipped) = (0usize, false);
    for c in clean.trim().chars() {
        let w = if c.is_ascii() { 1 } else { 2 };
        if cells + w > MAX_CELLS {
            clipped = true;
            break;
        }
        cells += w;
        out.push(c);
    }
    if clipped {
        out.push('…');
    }
    format!("'{out}'")
}

/// The shot's REAL colour space: the embedded ICC profile's description first (authoritative —
/// names Adobe RGB / Display P3 / sRGB variants), falling back to the EXIF `ColorSpace` tag when
/// there's no profile (Canon sRGB JPGs). `None` = unknown (the UI then assumes its sRGB default).
fn color_space_label(path: &std::path::Path, reader: &exif::Exif) -> Option<String> {
    let tag = extract_icc_from_jpeg(path).map(ColorTag::from_icc).unwrap_or_default();
    if let Some(desc) = tag.desc.as_deref().filter(|d| !d.is_empty()) {
        // v0.8.141 (R8): resolve ONCE and hand the answer to the label, which used to re-parse the
        // same profile to ask a question this call already answers.
        let r = falcon_color::resolve_source_gamut(tag.icc.as_deref(), tag.desc.as_deref());
        return Some(color_space_label_for(&tag, desc, &r));
    }
    use exif::{In, Tag, Value};
    let cs = reader.get_field(Tag::ColorSpace, In::PRIMARY).and_then(|f| match &f.value {
        Value::Short(v) => v.first().copied(),
        _ => None,
    });
    match cs {
        Some(1) => Some("sRGB".into()),
        Some(2) => Some("Adobe RGB".into()),
        _ => None, // Uncalibrated/absent with no ICC → unknown; the panel falls back to a dim "sRGB"
    }
}

/// The colour-space DESCRIPTION of a PNG: its embedded iCCP profile (the wide-gamut case — Photoshop /
/// Affinity exports in Display P3 / Adobe RGB carry one), else an `sRGB` chunk → "sRGB", else `None`.
/// Header-only (read_info stops at IDAT), so it's a cheap probe. (N2.)
/// v0.8.140 (C2): the door now yields the `iCCP` profile ITSELF, so the source gamut is MEASURED
/// from its colorants instead of guessed from its name. The precedence is unchanged: an embedded
/// profile first, else the bare `sRGB` chunk. A profile whose DESCRIPTION will not parse still
/// yields its bytes — that file used to fall all the way to sRGB; now the colorants still answer.
fn png_color_tag(path: &Path) -> ColorTag {
    let Ok(file) = std::fs::File::open(path) else { return ColorTag::default() };
    let Ok(reader) = png::Decoder::new(std::io::BufReader::new(file)).read_info() else {
        return ColorTag::default();
    };
    let info = reader.info();
    let mut tag = info.icc_profile.as_ref().map(|icc| ColorTag::from_icc(icc.to_vec())).unwrap_or_default();
    if tag.desc.is_none() {
        // Unchanged precedence: the bare `sRGB` chunk names the space when no profile description does.
        // v0.8.141 (R7/A4): note that this reaches PAST the case it was written for. It fires not
        // only for a PNG with no profile at all, but also for one whose `iCCP` profile is present
        // and whose `desc` tag will not read — where the chunk's "sRGB" is a name the CONTAINER
        // declared, over colorants that may say something else entirely. The precedence stays (it is
        // still the best available NAME, and the colorants outrank it anyway); what changes is that
        // `desc_from_profile` stays false here, so nothing downstream prints it as `profile "sRGB"`.
        tag.desc = info.srgb.map(|_| "sRGB".to_string());
    }
    tag
}

/// The colour-space description of a TIFF: its ICC profile (tag 34675). A minimal, bounded IFD walk of
/// the FIRST directory — self-contained (no dependency on the `tiff` crate's tag enum), fails closed to
/// `None` on anything unexpected. (N2.)
/// v0.8.140 (C2): tag 34675's profile ITSELF, so the gamut is measured from it, not from its name.
fn tiff_color_tag(path: &Path) -> ColorTag {
    tiff_icc(path).map(ColorTag::from_icc).unwrap_or_default()
}

/// The ICC profile bytes out of a TIFF's tag 34675 (the walk this door has always done).
fn tiff_icc(path: &Path) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut hdr = [0u8; 8];
    f.read_exact(&mut hdr).ok()?;
    let le = match &hdr[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16at = |b: &[u8], o: usize| {
        if le { u16::from_le_bytes([b[o], b[o + 1]]) } else { u16::from_be_bytes([b[o], b[o + 1]]) }
    };
    let u32at = |b: &[u8], o: usize| {
        if le {
            u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
        } else {
            u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
        }
    };
    f.seek(SeekFrom::Start(u32at(&hdr, 4) as u64)).ok()?;
    let mut cnt = [0u8; 2];
    f.read_exact(&mut cnt).ok()?;
    let n = u16at(&cnt, 0) as usize;
    if n == 0 || n > 4096 {
        return None; // sanity bound on a hostile/garbage header
    }
    let mut entries = vec![0u8; n * 12];
    f.read_exact(&mut entries).ok()?;
    for k in 0..n {
        let e = &entries[k * 12..k * 12 + 12];
        if u16at(e, 0) != 34675 {
            continue; // not the ICCProfile tag
        }
        let typ = u16at(e, 2);
        let count = u32at(e, 4) as usize;
        if !(typ == 7 || typ == 1) || count == 0 || count > 4_000_000 {
            return None;
        }
        let icc = if count <= 4 {
            e[8..8 + count].to_vec()
        } else {
            f.seek(SeekFrom::Start(u32at(e, 8) as u64)).ok()?;
            let mut buf = vec![0u8; count];
            f.read_exact(&mut buf).ok()?;
            buf
        };
        return Some(icc);
    }
    None
}

/// The shot's SOURCE colour gamut — what the decoded pixels are encoded in, so the colour-management
/// pass knows what to convert FROM. Reads the finished-image sibling's embedded colour profile PER
/// FORMAT (JPEG APP2 ICC, PNG iCCP, TIFF tag 34675, HEIC via the WIC colour context) — the fix for
/// wide-gamut PNG/TIFF/HEIC (e.g. iPhone Display P3) previously rendered + labelled sRGB (N2). A
/// profile-less file, a RAW-only shot's embedded preview, or an unrecognised profile all fall back to
/// sRGB. (RAW *develop* output is sRGB regardless — the caller passes `Gamut::Srgb` and never calls this.)
pub fn shot_source_gamut(shot: &Shot) -> falcon_color::Gamut {
    // v1.0.0-rc MICRO-TAIL (A-Y3b): `has_jpg` — a RAW carrying an undecodable sibling is
    // rendering its RAW's embedded preview, which is sRGB, and the sibling's profile describes a
    // picture nobody is looking at.
    let Some(path) = shot.jpg.as_ref().filter(|_| shot.has_jpg) else {
        return falcon_color::Gamut::Srgb; // RAW-only (or a passenger) → embedded preview is sRGB
    };
    resolve_source_gamut_noting(&file_color_tag(path, shot.kind), path)
}

/// The colour tag a finished-image file DECLARES, per format — the ONE door every source-gamut and
/// colour-space-label question goes through (v0.8.140 C2). Each arm returns the embedded profile's
/// BYTES where the format has any, and a name where the format only carries an enumerated code.
fn file_color_tag(path: &Path, kind: SrcKind) -> ColorTag {
    match kind {
        SrcKind::Jpeg => extract_icc_from_jpeg(path).map(ColorTag::from_icc).unwrap_or_default(),
        SrcKind::Png => png_color_tag(path),
        SrcKind::Tiff => tiff_color_tag(path),
        SrcKind::Webp => webp_color_tag(path),
        SrcKind::Heic => heic_color_tag(path),
        SrcKind::Jxl => jxl_color_tag(path),
        // BMP carries no colour profile we read (the common v3 header has none; the rare v5 ICC is not
        // exposed by the `image` bmp codec) → sRGB fallback. GIF is 8-bit palette, effectively sRGB.
        SrcKind::Bmp | SrcKind::Gif => ColorTag::default(),
        SrcKind::Unsupported => ColorTag::default(),
    }
}

/// Resolve a file's source gamut from its tag — and, when the answer is one the OLD name-only path
/// would not have given, park ONE line saying so (v0.8.140 C5).
fn resolve_source_gamut_noting(tag: &ColorTag, path: &Path) -> falcon_color::Gamut {
    let r = falcon_color::resolve_source_gamut(tag.icc.as_deref(), tag.desc.as_deref());
    note_color_resolution(tag, path, &r);
    r.gamut
}

/// C5 — the colour telemetry, in TWO independent arms (v0.8.141, R1). Both are called from here so
/// no door can wire up one and forget the other.
fn note_color_resolution(tag: &ColorTag, path: &Path, r: &falcon_color::GamutResolution) {
    note_answer_changed(tag, path, r);
    note_profile_missed_tolerance(tag, r);
}

/// ARM 1 (the v0.8.140 line, gate unchanged) — the per-FILE line, exactly when it earns its place.
///
/// It fires only where the NAME alone would have answered differently or not at all: an ordinary
/// sRGB JPEG whose profile says "sRGB IEC61966-2.1" resolves sRGB by both routes and stays silent,
/// so a folder of ordinary photographs writes nothing. What DOES speak is the case this round
/// exists for — a profile named "Display" over P3 colorants.
///
/// v0.8.141 (R1) — THE DOC OVERCLAIM, WITHDRAWN. This comment used to go on to promise that a
/// measured display profile outside τ "now says so WITH ITS DISTANCE". That is true of the tester's
/// profile and false as a class statement: this gate keys on the ANSWER CHANGING, so a
/// measured-panel profile whose NAME happens to contain a token the matcher knows — a Dell
/// "…, sRGB" sitting 0.0334 out, an "sRGB display profile…" 1.23τ out — resolves sRGB by both
/// routes and is silent here. Three of seven measured display profiles on the owner's machine are
/// in that class. Widening THIS gate is the wrong fix (a route-or-miss gate logs 31 of the 37
/// profiles installed on his machine, and one ProPhoto export folder would spend the whole 200-line
/// budget on identical lines before an interesting file ever spoke). The class telemetry belongs to
/// [`note_profile_missed_tolerance`] below, which keys on the PROFILE and carries its own small cap.
///
/// [`note_once`] keys on the path, so the several tiers that ask about one file cost one line, not
/// one per decode; the counter bounds a pathological folder to a stated number of them.
///
/// v0.8.141 (R3) — ON THE SHARED KEY. The HEIC PREVIEW door and the MASTER door both note under
/// `colour-resolve:{path}`, so a HEIC whose preview answers costs ONE line, not two. That is the C5
/// spec verbatim ("one line per FILE-load resolution"), not an accident: `note_once` is Mutex'd
/// first-writer-wins, so a race between the two tiers is resolved, not undefined, and either line
/// carries the same file's answer. `frame_source_gamut`'s Fallback branch deliberately does not
/// note at all — it has DECLINED to answer and is deferring to the master, which notes for itself.
/// v0.8.177 — A BUDGET CONSEQUENCE, STATED RATHER THAN DISCOVERED. The gate is unchanged, but its
/// population is not: a faithful-route file can never satisfy `by_name == Some(r.gamut)` (its answer
/// is a registry entry, and a name can only ever produce a modeled gamut), so every file in that
/// class now writes a line where — before this round — a ProPhoto export was SILENT here, its name
/// and its Rec.2020 answer agreeing. A folder of 400 ProPhoto exports therefore spends `NOTE_CAP`
/// where it used to spend nothing. That is accepted, not overlooked: the cap is exactly the
/// mechanism for bounding it, the per-file answer really did change and `route=faithful` is what the
/// tester greps for, and the per-PROFILE class line in [`note_profile_missed_tolerance`] (cap 8, its
/// own counter) survives the cap and carries the fact that matters at folder scale.
fn note_answer_changed(tag: &ColorTag, path: &Path, r: &falcon_color::GamutResolution) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    /// Distinct files that may each contribute a colour-resolution line in one session.
    ///
    /// v0.8.177 (H-O) — SAID PLAINLY: this counter is PER PROCESS and is NEVER RESET. Not per
    /// folder, not per open, not per hour. Once 200 distinct files in one run of the app have
    /// written a colour line, no file writes another until the app is restarted — so a tester who
    /// browses a big folder first and THEN opens the file he is investigating will find no line for
    /// it, and nothing in the log says why beyond the one suppression notice below. That is the
    /// deliberate trade (a bounded log beats a complete one), stated so it is never mistaken for a
    /// bug in the resolver.
    const NOTE_CAP: usize = 200;
    static NOTED: AtomicUsize = AtomicUsize::new(0);

    let by_name = tag.desc.as_deref().and_then(falcon_color::Gamut::from_description);
    if tag.is_silent() || by_name == Some(r.gamut) {
        return; // the file said nothing, or the name already agreed — nothing to report
    }
    if NOTED.load(Ordering::Relaxed) >= NOTE_CAP {
        return;
    }
    // v0.8.141 (R11): a file name is untrusted and may legally contain a newline on macOS, which
    // would forge a second log line. (The description below goes through `{:?}`, whose escaping
    // already covers it.)
    let file = sanitize_one_line(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
    let nearest = match r.nearest {
        Some((g, d)) => format!(", nearest {} at {d:.5} (τ {})", g.label(), falcon_color::GAMUT_MATCH_TOL),
        None => String::new(),
    };
    let named = by_name.map(|g| g.label()).unwrap_or("nothing");
    // v0.8.141 (R7): say where the name came from. `profile "X"` is a quotation from the embedded
    // profile; `declared "X"` is a name the CONTAINER supplied (a PNG `sRGB` chunk, an `nclx`/CICP
    // code) — printing the latter as the former attributed to a P3 profile a description it does
    // not contain, on precisely the files this round is about.
    let source = if tag.desc_from_profile { "profile" } else { "declared" };
    // v0.8.177 (plan point 5): the line NAMES THE ROUTE — `modeled-colorants | faithful |
    // name-fallback | default` — beside the miss distance it already printed. Before this, "the
    // name alone said X" was the only hint at provenance, and a reader could not tell a τ-matched
    // colorant answer from a faithful-profile one at all (they now differ in what the pixels are
    // actually transformed by). `display_name` rather than `label` so a faithful answer prints the
    // profile's own name, never a modeled one it is not.
    let line = format!(
        "colour: {file} — {source} {:?} → {} [route={} {}{nearest}; the name alone said {named}]",
        tag.desc.as_deref().unwrap_or("<none>"),
        r.gamut.display_name(),
        r.route.token(),
        r.why(),
    );
    if note_once(&format!("colour-resolve:{}", path.display()), line)
        && NOTED.fetch_add(1, Ordering::Relaxed) + 1 == NOTE_CAP
    {
        note_once(
            "colour-resolve-cap",
            format!("colour: {NOTE_CAP} files have now reported a colour resolution — further per-file colour lines are suppressed for this session"),
        );
    }
}

/// ARM 2 (v0.8.141, R1) — the per-PROFILE line: a profile we could MEASURE but could not PLACE.
///
/// This is the class the source-side Custom-from-profile path is still owed (the v0.8.140 C1.3
/// stop): a profile measured off a real panel sits 0.01-0.07 from every modeled gamut, so no τ that
/// keeps the modeled gamuts unambiguous can reach it, and the file falls through to its name. Arm 1
/// cannot report that class, because for many such profiles the name and the fallback agree and
/// nothing "changed". This arm reports it directly, and the shape is what makes it affordable:
/// - the key is the PROFILE (its description and its distance), not the file — a folder of 400
///   exports carrying one profile costs ONE line, which is what killed the idea of widening arm 1;
/// - the cap is its own and small (8 distinct profiles), and it NEVER touches arm 1's counter, so
///   this telemetry can never crowd out the per-file lines that answer the tester's question.
/// v0.8.177 — AND THE SENTENCE IT ENDS ON IS NOW THE GOOD NEWS. "Files carrying it resolve by name
/// instead of by colorimetry" was the honest report of a gap; the gap is closed for the matrix-TRC
/// half of this class, so the line says what actually happens to those files: they are rendered
/// through the profile's own colorants and curves. A profile that is measurable, unplaceable AND
/// still not faithfully renderable (its TRC tags are missing or malformed, or the faithful registry
/// is at [`falcon_color::SOURCE_PROFILE_CAP`]) keeps the original sentence, because for it the
/// original sentence is still true.
fn note_profile_missed_tolerance(tag: &ColorTag, r: &falcon_color::GamutResolution) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    /// Distinct unplaceable profiles that may each contribute a line in one session.
    const MISS_CAP: usize = 8;
    static MISSED: AtomicUsize = AtomicUsize::new(0);

    // `nearest` is `Some` exactly when the bytes PARSED as matrix/TRC — so this arm is, by
    // construction, only ever about a profile we really did measure.
    let Some((near, d)) = r.nearest else { return };
    if d <= falcon_color::GAMUT_MATCH_TOL {
        return; // it was placed; arm 1 owns whatever else is interesting about it
    }
    if MISSED.load(Ordering::Relaxed) >= MISS_CAP {
        return;
    }
    // R7 again: only a name the profile itself carries may be quoted as the profile's.
    let name = tag.profile_desc().unwrap_or("<unnamed profile>");
    let key = format!("colour-miss:{}:{d:.4}", sanitize_one_line(name));
    // v0.8.177 (G-O4): a REFUSAL names its cause. The three are not the same news — `cap-full` says
    // this session has simply seen too many profiles and a restart would render the file faithfully;
    // `non-adaptable-white` says this profile will never take that route; `no-usable-TRC` says its
    // curve tags are missing or malformed. One sentence for all three told the tester none of it.
    let outcome = match (falcon_color::source_profile(r.gamut), r.faithful_refusal) {
        (Some(p), _) => format!(
            "files carrying it are rendered FAITHFULLY, through its own colorants and curves ({})",
            p.trc_summary
        ),
        (None, Some(why)) => format!(
            "the faithful route refused it [{}] — files carrying it resolve by name instead of by colorimetry",
            why.token()
        ),
        (None, None) => "files carrying it resolve by name instead of by colorimetry".to_string(),
    };
    let line = format!(
        "colour: profile {:?} is measurable but unplaceable — {d:.5} from the nearest modeled gamut \
         ({}, τ {}); {outcome}",
        name,
        near.label(),
        falcon_color::GAMUT_MATCH_TOL,
    );
    if note_once(&key, line) && MISSED.fetch_add(1, Ordering::Relaxed) + 1 == MISS_CAP {
        note_once(
            "colour-miss-cap",
            format!("colour: {MISS_CAP} distinct unplaceable profiles have now been reported — further per-profile colour lines are suppressed for this session"),
        );
    }
}

/// Full ordered list of photography-relevant EXIF entries `(label, value)` for the
/// info panel — the first handful are the headline fields; the rest (GPS, exposure
/// program, metering, white balance, flash, 35 mm focal, colour space, software,
/// date, file sizes…) fill the expanded view. Only non-empty fields are returned.
/// `turns` (v0.8.0) = the shot's composed DISPLAY turns (auto-orient × EXIF base + manual delta,
/// resolved by the caller): the Dimensions row shows the ORIENTED W × H (user gate: "Oriented"),
/// so odd turns swap the two values. 0 leaves the file's stored order untouched.
pub fn exif_rows(shot: &Shot, turns: u8) -> Vec<(String, String)> {
    use exif::{In, Tag};

    let mut rows: Vec<(String, String)> = Vec::new();
    let raw_sz = shot.raw.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len());
    let jpg_sz = shot.jpg.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len());

    // v1.0.0-rc TAIL 4 (re-verification O1): `has_jpg`. THIS is the production EXIF path — the
    // info panel is built from here (`main.rs`'s EXIF worker) — and tail 3 put the passenger term on
    // `read_exif` instead, which has ZERO callers under `native/`. So a passenger shot's panel
    // still read `Camera: Apple iPhone 17 Pro` off the file beside the photograph, where the same
    // RAW alone reads nothing. A live regression against `a51da1d`, and the reason §C.TAIL4 states
    // the method: enumerate picture readers by PRODUCTION CALLER, never by function name.
    let Some(path) = shot.jpg.as_ref().filter(|_| shot.has_jpg).or(shot.raw.as_ref()) else {
        return rows;
    };
    // A1/F2 (defect #1): bounded read — the info-panel parse fires off-thread on the EXIF worker when
    // the user settles on a shot, so an unbounded `read_from_container` here would OOM-abort on a
    // crafted multi-GB TIFF exactly like the per-decode orientation read. Same 4 MB cap.
    let reader = match read_exif_bounded(path) {
        Some(r) => r,
        None => {
            // v0.8.90: the EXIF container is absent or unparseable (a PNG/WebP with no EXIF chunk, a
            // stripped web-export JPG) — but the FILE itself may be perfectly readable. Emit the rows
            // that never needed EXIF (Dimensions from the image header, Files from fs metadata),
            // GATED on the header actually parsing (`source_dimensions`): a mid-copy / 0-byte /
            // locked file still returns EMPTY, so the v0.8.89 bounded retry keeps self-healing
            // exactly those, while a readable EXIF-less file gets its honest panel IMMEDIATELY
            // instead of 5 blank retries (~11 s) first. (A RAW-only shot has no finished path →
            // `source_dimensions` is None → empty → retry, unchanged — real raws carry EXIF.)
            if let Some((w, h)) = source_dimensions(shot) {
                // Same oriented-swap rule as the EXIF-present Dimensions fallback below.
                let (w, h) = if turns & 1 == 1 { (h, w) } else { (w, h) };
                rows.push(("Dimensions".to_string(), format!("{w} × {h}")));
                let fin = shot.finished_file_format().unwrap_or_else(|| "Image".to_string());
                if let Some(f) = fmt_files(raw_sz, jpg_sz, &fin, shot.named_ext_when_bytes_disagree().as_deref()) {
                    rows.push(("Files".to_string(), f));
                }
            }
            return rows;
        }
    };

    let plain = |tag| reader.get_field(tag, In::PRIMARY).map(|f| f.display_value().to_string());
    let unit = |tag| {
        reader.get_field(tag, In::PRIMARY).map(|f| f.display_value().with_unit(&reader).to_string())
    };
    // String tags (make/model/lens/artist/software/date) → first non-empty ASCII
    // component, so NUL-padding / empty fields don't leak `", "", "", …` (see exif_string).
    let str_tag = |tag| reader.get_field(tag, In::PRIMARY).and_then(exif_string);
    let add = |rows: &mut Vec<(String, String)>, k: &str, v: Option<String>| {
        if let Some(v) = v {
            let v = trim_quotes(&v);
            if !v.is_empty() {
                rows.push((k.to_string(), v));
            }
        }
    };

    let camera = match (str_tag(Tag::Make), str_tag(Tag::Model)) {
        (Some(mk), Some(md)) => {
            if md.to_lowercase().starts_with(&mk.to_lowercase()) { Some(md) } else { Some(format!("{mk} {md}")) }
        }
        (None, Some(md)) => Some(md),
        (Some(mk), None) => Some(mk),
        _ => None,
    };
    add(&mut rows, "Camera", camera);
    // Model alone (e.g. "NIKON 1 J3", without the long "NIKON CORPORATION" make
    // prefix) for the folded grid's compact camera cell — the make would otherwise
    // crowd out the shutter/ISO cells beside it. The expanded view drops this row;
    // it already shows the full make+model as "Camera".
    add(&mut rows, "Model", str_tag(Tag::Model));
    add(&mut rows, "Lens", str_tag(Tag::LensModel));
    // Focal/aperture/shutter rows carry the ORIGINAL (possibly long-decimal) values — the expanded
    // panel shows the unrounded truth. The folded grid rounds for display via brief_shutter /
    // brief_round at ExifBrief-build time instead (user request 2026-07-06).
    add(&mut rows, "Focal length", unit(Tag::FocalLength));
    add(&mut rows, "Aperture", plain(Tag::FNumber).map(|s| format!("f/{}", trim_quotes(&s))));
    add(&mut rows, "Shutter", plain(Tag::ExposureTime).map(|s| format!("{} s", trim_quotes(&s))));
    add(&mut rows, "ISO", plain(Tag::PhotographicSensitivity));
    // Exposure bias: cameras use 1/3-EV steps, so the raw rational is e.g. -0.33333…;
    // round to 2 decimals (trim trailing zeros) instead of dumping a dozen 3s.
    let ev = reader
        .get_field(Tag::ExposureBiasValue, In::PRIMARY)
        .and_then(|f| match &f.value {
            exif::Value::SRational(v) => v.first().map(|r| r.to_f64()),
            _ => None,
        })
        .map(|v| {
            if v.abs() < 0.005 {
                "0 EV".to_string()
            } else {
                let s = format!("{v:.2}");
                let s = s.trim_end_matches('0').trim_end_matches('.');
                format!("{s} EV")
            }
        });
    add(&mut rows, "Exposure bias", ev);
    add(&mut rows, "Exposure program", plain(Tag::ExposureProgram));
    add(&mut rows, "Metering", plain(Tag::MeteringMode));
    add(&mut rows, "White balance", plain(Tag::WhiteBalance));
    add(&mut rows, "Flash", plain(Tag::Flash));
    add(&mut rows, "Focal (35mm)", unit(Tag::FocalLengthIn35mmFilm));
    // Colour space (#23). Use the REAL space from the embedded ICC profile (which alone can tell
    // Adobe RGB from Display P3 — the EXIF ColorSpace tag reports both as "Uncalibrated"), falling back
    // to the EXIF tag for profile-less sRGB JPGs. The render pipeline is sRGB-assumed, so the always-
    // visible RAW/JPG panel turns this into a dim "sRGB" or a yellow warning for a non-sRGB file.
    add(&mut rows, "Color space", color_space_label(path, &reader));
    // v0.8.0: ORIENTED dimensions — swap W×H when the display turns are odd (90°/270°).
    let swap = turns & 1 == 1;
    // v0.8.101 (S4): HEIC reads its size from the DECODED frame, never from EXIF
    // `PixelXDimension`. Those tags record the pre-rotation size, and the Windows HEIF decoder
    // hands back the frame with the container's `irot` already applied — so for a portrait iPhone
    // HEIC the EXIF pair says 8064 × 6048 while the photo on screen is 6048 × 8064. Before S4 the
    // row happened to look right for the wrong reason (the double-applied orientation made `swap`
    // true); with the rotation fixed, reading the tags would print the transpose of what the user
    // is looking at. `source_dimensions` is the frame the tiers actually decode, on both platforms.
    let heic_dims = (shot.kind == SrcKind::Heic).then(|| source_dimensions(shot)).flatten();
    if let Some((w, h)) = heic_dims {
        let (w, h) = if swap { (h, w) } else { (w, h) };
        rows.push(("Dimensions".to_string(), format!("{w} × {h}")));
    } else if let (Some(x), Some(y)) = (plain(Tag::PixelXDimension), plain(Tag::PixelYDimension)) {
        let (x, y) = (trim_quotes(&x).to_string(), trim_quotes(&y).to_string());
        let (x, y) = if swap { (y, x) } else { (x, y) };
        rows.push(("Dimensions".to_string(), format!("{x} × {y}")));
    } else if let Some((w, h)) = source_dimensions(shot) {
        // EXIF PixelXDimension is optional (edited JPGs, all PNG/TIFF) — fall back to the header size.
        let (w, h) = if swap { (h, w) } else { (w, h) };
        rows.push(("Dimensions".to_string(), format!("{w} × {h}")));
    }
    add(&mut rows, "GPS", format_gps(&reader));
    add(&mut rows, "Altitude", unit(Tag::GPSAltitude));
    add(&mut rows, "Date", str_tag(Tag::DateTimeOriginal).map(|s| match s.split_once(' ') {
        Some((d, t)) => format!("{} {}", d.replace(':', "-"), t),
        None => s,
    }));
    add(&mut rows, "Software", str_tag(Tag::Software));
    add(&mut rows, "Artist", str_tag(Tag::Artist));
    if let Some(f) = fmt_files(
        raw_sz,
        jpg_sz,
        &shot.finished_file_format().unwrap_or_else(|| "Image".to_string()),
        shot.named_ext_when_bytes_disagree().as_deref(),
    ) {
        rows.push(("Files".to_string(), f));
    }
    rows
}

/// Decimal lat/long from EXIF GPS rationals, e.g. `33.85920°S 151.21500°E`.
fn format_gps(reader: &exif::Exif) -> Option<String> {
    use exif::{In, Tag, Value};
    let dms = |v: &Value| -> Option<f64> {
        if let Value::Rational(r) = v {
            if r.len() >= 3 {
                return Some(r[0].to_f64() + r[1].to_f64() / 60.0 + r[2].to_f64() / 3600.0);
            }
        }
        None
    };
    let lat = dms(&reader.get_field(Tag::GPSLatitude, In::PRIMARY)?.value)?;
    let lon = dms(&reader.get_field(Tag::GPSLongitude, In::PRIMARY)?.value)?;
    let lr = reader.get_field(Tag::GPSLatitudeRef, In::PRIMARY).map(|f| f.display_value().to_string()).unwrap_or_default();
    let or = reader.get_field(Tag::GPSLongitudeRef, In::PRIMARY).map(|f| f.display_value().to_string()).unwrap_or_default();
    Some(format!("{:.5}°{} {:.5}°{}", lat, trim_quotes(&lr), lon, trim_quotes(&or)))
}

// ─────────────────────────────────── helpers ─────────────────────────────────

/// Raw JPEG bytes to feed the decoder: the JPG file, or the largest embedded
/// JPEG inside a raw container (CR3 preview).
fn jpeg_source(shot: &Shot) -> Result<Vec<u8>> {
    // v1.0.0-rc MICRO-TAIL (A-Y3b): `has_jpg`, not `jpg.is_some()` — a shot may CARRY a finished
    // file that is not its picture (an undecodable sibling riding with a RAW), and handing those
    // bytes to `jpeg-decoder` is the failure this round exists to stop. The RAW arm below is then
    // reached exactly as it was before the round, so the photograph is the pre-round one.
    if let (true, Some(j)) = (shot.has_jpg, &shot.jpg) {
        // Byte cap (mirrors the RAW arm's MAX_RAW_BYTES below): a crafted / corrupt ".jpg" must not be
        // able to force a multi-GB allocation — `std::fs::read` sizes its Vec from the file length, so an
        // absurdly-sized file would abort the process on the alloc. Guard the length first and, over the
        // cap, return a clean Err (never an abort/panic) that names the file + cap; the decode caller
        // logs the failure WITH the filename (M4). A genuine >600 MB JPEG does not exist in practice.
        const MAX_JPG_BYTES: u64 = 600_000_000;
        let len = std::fs::metadata(j).map(|m| m.len()).unwrap_or(0);
        if len > MAX_JPG_BYTES {
            bail!("jpeg source {} is {len} bytes (> {MAX_JPG_BYTES} cap) — skipped", j.display());
        }
        Ok(std::fs::read(j)?)
    } else if let Some(r) = &shot.raw {
        // Bounded read: real raws are ≤ a few hundred MB (a 102 MP CR3 ≈ 130 MB); cap so a crafted
        // or corrupt "raw" can't make us allocate gigabytes. The embedded previews sit near the start.
        const MAX_RAW_BYTES: u64 = 600_000_000;
        use std::io::Read;
        let mut buf = Vec::new();
        std::fs::File::open(r)?.take(MAX_RAW_BYTES).read_to_end(&mut buf)?;
        let off = largest_embedded_jpeg(&buf).context("no embedded JPEG preview in raw")?;
        // Reuse the RAW allocation rather than duplicating its sensor-data tail. Only the
        // selected JPEG survives into decoding. If malformed/truncated, preserve the old
        // decoder's error/fallback behaviour by keeping the remaining bytes.
        let end = embedded_jpeg_end(&buf, off).unwrap_or(buf.len());
        buf.copy_within(off..end, 0);
        buf.truncate(end - off);
        buf.shrink_to_fit();
        Ok(buf)
    } else {
        bail!("shot {} has no files", shot.id)
    }
}

/// v1.0.0-rc TAIL (skeptic A, O1): the ceiling on everything this walk reads or seeks before it
/// finds a frame header.
///
/// 16 MiB is chosen against what a real JPEG's pre-SOF metadata can legitimately be: the largest
/// thing that lives there is an embedded ICC profile, chunked across APP2 segments, and even a
/// full-gamut printer profile is ~3 MB; XMP, EXIF (with its thumbnail) and MPF add tens of KB. So
/// 16 MiB is roughly five times the worst honest case and still bounds the UI thread at a few
/// milliseconds of sequential read. Past it, the answer is `None` — the same answer a file with no
/// SOF at all gets, and one the caller already handles.
const JPEG_HEADER_BUDGET: usize = 16 * 1024 * 1024;

/// v1.0.0-rc (R3): the frame size a JPEG's SOF marker declares — a header-only probe with no
/// pixel decode and no fixed byte window.
///
/// It walks the marker chain, and the walk is bounded THREE ways: by each segment's own declared
/// `u16` length, by [`MAX_SEGMENTS`](jpeg_sof_dims), and — v1.0.0-rc TAIL, skeptic A O1 — by
/// [`JPEG_HEADER_BUDGET`] over every byte read OR seeked, which is the bound that actually holds
/// when a file declares no segments at all. A legitimate JPEG with a 500 KB chunked ICC before its
/// SOF is followed to the end of the chunks; a crafted one that is `FF D8` and then gigabytes of
/// filler is abandoned after 16 MiB. Pure over `BufRead + Seek`, so the table below it exercises
/// real byte layouts through a `Cursor` with no filesystem in the loop.
///
/// `None` for: a file that is not a JPEG at all, a SOF whose declared height is 0 (the DNL case —
/// the height only becomes known after the first scan, and this probe deliberately does not decode),
/// and a chain that reaches SOS or EOI without a SOF.
fn jpeg_sof_dims<R: std::io::BufRead + std::io::Seek>(r: &mut R) -> Option<JpegFrame> {
    /// How many marker segments the walk will follow before giving up. Real JPEGs carry a handful;
    /// 1024 is far past any legitimate file.
    const MAX_SEGMENTS: usize = 1024;
    let mut budget = JPEG_HEADER_BUDGET;
    let mut soi = [0u8; 2];
    r.read_exact(&mut soi).ok()?;
    if soi != [0xFF, 0xD8] {
        return None;
    }
    let mut byte = [0u8; 1];
    for _ in 0..MAX_SEGMENTS {
        // A marker is 0xFF, then any number of 0xFF fill bytes, then the marker code. A 0x00 there
        // is a stuffed byte (entropy data), not a marker, so the search restarts.
        //
        // v1.0.0-rc TAIL (skeptic A, O1): the search for that 0xFF is BLOCK-SCANNED over the
        // reader's own buffer instead of `read_exact`-ing one byte at a time, and every byte it
        // passes is charged to `budget`. The old spelling walked byte-by-byte through the whole
        // `Read` stack — A measured `FF D8` followed by 32 MB of `0x00` at 36.9 ms, ~1.15 ms/MB —
        // and `source_dimensions` runs on the UI THREAD, while rule (2) makes ANY file whose first
        // three bytes are `FF D8 FF` a `Jpeg` by content. A 2 GB such file was a ~2.3 s UI stall.
        // `fill_buf`/`consume` are std `BufRead`, so this costs no dependency and no allocation.
        loop {
            // The borrow of `r` ends with this block, so `consume` below can take its own.
            let found: Result<usize, usize> = {
                let buf = r.fill_buf().ok()?;
                if buf.is_empty() {
                    return None; // EOF before any marker
                }
                match buf.iter().position(|&b| b == 0xFF) {
                    Some(i) => Ok(i + 1),  // consume up to AND INCLUDING the 0xFF
                    None => Err(buf.len()), // no marker in this block — charge it and refill
                }
            };
            let step = found.unwrap_or_else(|n| n);
            r.consume(step);
            budget = budget.checked_sub(step)?;
            if found.is_ok() {
                break;
            }
        }
        let marker = loop {
            r.read_exact(&mut byte).ok()?;
            budget = budget.checked_sub(1)?;
            if byte[0] != 0xFF {
                break byte[0];
            }
        };
        match marker {
            0x00 => continue,                      // a stuffed byte, not a marker
            0x01 | 0xD0..=0xD8 => continue,        // standalone: TEM / RSTn / SOI, no length field
            0xD9 | 0xDA => return None,            // EOI or the start of the scan — no SOF ahead
            // SOF0..SOF15 minus the three codes in that range that are NOT frame headers:
            // 0xC4 DHT, 0xC8 JPG (reserved), 0xCC DAC.
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                let mut sof = [0u8; 8]; // length(2) precision(1) height(2) width(2) components(1)
                r.read_exact(&mut sof).ok()?;
                return Some(JpegFrame {
                    h: u32::from(u16::from_be_bytes([sof[3], sof[4]])),
                    w: u32::from(u16::from_be_bytes([sof[5], sof[6]])),
                    components: sof[7],
                });
            }
            _ => {
                let mut len = [0u8; 2];
                r.read_exact(&mut len).ok()?;
                let len = u16::from_be_bytes(len);
                if len < 2 {
                    return None; // a segment shorter than its own length field
                }
                // The SEEK is charged too: a chain of maximal segments is as good a way to spend
                // the UI thread as a fill run, and the budget is about the WALK, not about one loop.
                budget = budget.checked_sub(usize::from(len))?;
                r.seek(std::io::SeekFrom::Current(i64::from(len) - 2)).ok()?;
            }
        }
    }
    None
}

/// v1.0.0-rc TAIL (owner CMYK ruling): what a JPEG's frame header DECLARES. One walker, two
/// readers — the dimension probe and the 4-component route — because the alternative was a second
/// marker walk, which is the duplication this whole round is about.
struct JpegFrame {
    w: u32,
    h: u32,
    /// `Nf` — 1 = grayscale, 3 = YCbCr/RGB, 4 = CMYK/YCCK.
    components: u8,
}

/// [`jpeg_sof_dims`] over a file. Header-only: it opens, walks and closes without reading a pixel.
///
/// `None` for a zero dimension as well as for a missing frame: a SOF whose declared height is 0 is
/// the DNL case, where the height only becomes known after the first scan and this probe
/// deliberately does not decode.
fn jpeg_header_dims(path: &Path) -> Option<(u32, u32)> {
    let f = std::fs::File::open(path).ok()?;
    let fr = jpeg_sof_dims(&mut std::io::BufReader::new(f))?;
    (fr.w > 0 && fr.h > 0).then_some((fr.w, fr.h))
}

/// v1.0.0-rc TAIL (owner CMYK ruling): how many colour channels this JPEG's frame header declares,
/// read from bytes already in hand. 4 means CMYK or YCCK — the print-production family.
fn jpeg_component_count(bytes: &[u8]) -> Option<u8> {
    jpeg_sof_dims(&mut std::io::Cursor::new(bytes)).map(|f| f.components)
}

/// v1.0.0-rc TAIL (OWNER RULING, CMYK): route 4-component (CMYK / YCCK) JPEGs through the OS's own
/// colour-managed decode instead of Falcon's portable ink arithmetic?
///
/// **Default OFF — Falcon's own conversion is what ships**, on the reasoning §C recorded: our
/// answer lands within 5 of the source picture on every committed fixture where the OS's lands 96
/// away, it is identical on Windows and macOS, and it keeps DCT shrink-on-load for the browse
/// tiers. The option exists because the OWNER'S colour ground truth is Photoshop, and Photoshop
/// does what Windows does — it assigns a working CMYK profile (U.S. Web Coated (SWOP) v2 under the
/// default settings) to an untagged file — so on that oracle the OS's family of answers is the
/// closer one. Neither is reading anything from the file; both are assumptions. This lever lets the
/// owner pick which assumption his eyes prefer.
///
/// L43 — WHAT UN-SETS IT: the toggle, and nothing else. It is READ PER DECODE (never memoised —
/// the `heic_speed_priority` discipline), so a flip applies to the next photograph with no armed
/// posture to stand down and no folder to reopen.
static CMYK_OS_ROUTE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Is the OS colour-managed route for 4-component JPEGs on? Read per decode; never memoised.
#[inline]
pub fn cmyk_os_route() -> bool {
    CMYK_OS_ROUTE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Apply the setting — the boot seed from `settings.json`, and every click on the row.
pub fn set_cmyk_os_route(on: bool) {
    CMYK_OS_ROUTE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// v1.0.0-rc (R2): is this [`decode_jpeg`] failure the TRUNCATION class — the file simply ENDS
/// mid-scan — rather than genuine corruption?
///
/// `jpeg-decoder` reports both the missing-EOI case and the cut-mid-MCU case as
/// `Error::Io(UnexpectedEof)` ("failed to fill whole buffer"); a malformed marker, an unsupported
/// feature and a bad Huffman table all report as `Format`/`Unsupported`, which stay a hard failure.
/// The distinction matters because a truncated JPEG is a file every other viewer on the machine
/// renders — WIC and nvJPEG both decode the portion that arrived — and it is what a scan-to-email,
/// an interrupted sync or a half-copied card produces.
fn jpeg_err_is_truncation(e: &anyhow::Error) -> bool {
    e.downcast_ref::<jpeg_decoder::Error>().is_some_and(|j| {
        matches!(j, jpeg_decoder::Error::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof)
    })
}

/// Decode a JPEG to packed RGB8. `scale_to` requests DCT shrink-on-load (1/2,
/// 1/4, 1/8) to the given long side; `None` decodes full resolution.
fn decode_jpeg(bytes: &[u8], scale_to: Option<u32>) -> Result<(Vec<u8>, u32, u32)> {
    let mut d = Decoder::new(Cursor::new(bytes));
    let (w, h) = match scale_to {
        Some(max) => {
            let (sw, sh) = d.scale(max as u16, max as u16)?;
            // V1/N13 bomb guard, scaled branch: jpeg_decoder's scale() never UNDERSHOOTS — it returns
            // the smallest DCT stop whose long side is ≥ `max`, so a LARGE request (the detail tier's
            // ddim = res_limit, up to 16384, when adaptive hi-res is toggled OFF) on a huge source can
            // still land on the 1/1 or 1/2 stop = a multi-hundred-MP decode. Guard the ACTUAL decoded
            // (stop) size so `d.decode()` below can't allocate past what RAM can back: the default
            // detail cap (≤8192 → a ≤~63 MP stop here) always passes, while a low-RAM machine asked for
            // a genuinely huge stop fails CLEANLY instead of aborting on the unbounded alloc.
            guard_source_dims(sw as u32, sh as u32, "JPEG (scaled)")?;
            (sw as u32, sh as u32)
        }
        None => {
            d.read_info()?;
            let i = d.info().context("jpeg has no frame header")?;
            // V1/N13 decompression-bomb guard: reject an oversized frame from the SOF header
            // BEFORE `decode()` allocates from it — mirrors the PNG/TIFF/HEIC siblings. A crafted
            // JPEG can declare huge dims yet compress tiny (a progressive JPEG sizes its coefficient
            // buffers from the header before reading scan data), so without this a stop/zoom on such
            // a file forces a multi-GB alloc on a worker. (The `Some(dim)` scaled branch above has the
            // matching guard on its decoded-stop size — a large `dim` is not exempt.)
            guard_source_dims(i.width as u32, i.height as u32, "JPEG")?;
            (i.width as u32, i.height as u32)
        }
    };
    let pixels = d.decode()?;
    let fmt = d.info().context("jpeg has no frame header")?.pixel_format;
    Ok((to_rgb(pixels, fmt)?, w, h))
}

/// `jpeg-decoder`'s output, in whatever pixel format the file turned out to have, converted to the
/// packed RGB8 every render tier consumes.
///
/// v1.0.0-rc (R1) — THE 4-COMPONENT ARM. Before this round `to_rgb` handled `RGB24` and `L8` and
/// `bail!`ed on everything else, which meant a CMYK or YCCK JPEG — the classic
/// Acrobat / Photoshop / print-RIP export — failed at EVERY tier with `unsupported JPEG pixel
/// format: CMYK32`, while WIC opened all four flavours of it. `jpeg-decoder` has already done the
/// hard half (it applies the YCCK→CMYK colour transform itself); only the 4→3 channel conversion
/// was missing.
///
/// ONE arithmetic, not two, and the measurement that settles it. `PixelFormat::CMYK32` arrives for
/// BOTH flavours and `jpeg-decoder` does not report which path produced it — but it does not have
/// to, because both paths deliver TRUE INK (0 = no ink):
///   * transform 0, or no APP14 at all — `color_convert_line_cmyk` returns `255 - stored`, and an
///     Adobe-written file stores CMYK inverted, so what lands here is the ink.
///   * transform 2 (YCCK) — `color_convert_line_ycck` runs the YCbCr→RGB matrix over the first
///     three channels, and in Adobe's YCCK those three encode the CMY INK triple, so it lands here
///     as ink too; the fourth channel is `255 - stored` in both arms, i.e. true K in both.
///
/// So: ink → light is `255 - ink`, and the black plate multiplies it. The row
/// `a_cmyk_jpeg_decodes_to_the_same_picture_as_its_rgb_twin` proves this on all three flavours by
/// decoding the SAME source image encoded four ways and comparing the pictures — which is a
/// stronger oracle than re-typed arithmetic and than an OS codec that colour-manages (see §C).
///
/// No ICC is applied here, deliberately: this is the container's own naive CMYK, it is identical on
/// every platform (the property the pure-Rust WebP/JXL/BMP arms exist for), and the
/// colour-management pass downstream reads any embedded profile through [`file_color_tag`] exactly
/// as it does for every other format.
fn to_rgb(px: Vec<u8>, fmt: PixelFormat) -> Result<Vec<u8>> {
    match fmt {
        PixelFormat::RGB24 => Ok(px),
        PixelFormat::L8 => Ok(px.iter().flat_map(|&v| [v, v, v]).collect()),
        PixelFormat::CMYK32 => {
            let mut out = Vec::with_capacity(px.len() / 4 * 3);
            for q in px.chunks_exact(4) {
                let k = u16::from(255 - q[3]);
                for &ink in &q[..3] {
                    out.push(((u16::from(255 - ink) * k + 127) / 255) as u8);
                }
            }
            Ok(out)
        }
        other => bail!("unsupported JPEG pixel format: {other:?}"),
    }
}

// ─────────────────────── format-aware finished-image decode (Way A) ───────────────────────
// The JPG-centric spine (`jpeg_source` → `decode_jpeg`) generalised to PNG/TIFF. Every render tier
// (fast / reference / full / scaled / thumbnail) decodes through this one dispatch, so adding a
// format is one arm here, not a change per tier. Output is packed RGB8 to match the ROI cropper.

/// ROUND 35 (queue item 35), R9's INSTRUMENT: a thread-local allocation meter.
///
/// `PeakWorkingSetSize` cannot answer R9's question. It is process-wide and monotonic, so under a
/// parallel `cargo test` a row asking "did this call hold more than N bytes at once" would read a
/// peak some other test set, and the red-first mutation could not redden it -- the L28 vacuous
/// class, in the one row whose whole job is a measurement. What the question needs is the bytes
/// THIS THREAD holds at once, which a global allocator can count exactly.
///
/// It is `#[cfg(test)]`, so it exists only while this crate is compiled as its own test harness and
/// nothing in the shipped binary ever sees it. It allocates nothing itself: a `Cell<usize>` in a
/// const-initialised `thread_local!` has no destructor, so there is no lazy registration to
/// allocate for, and `try_with` means an allocation during thread teardown is skipped rather than
/// panicked on.
#[cfg(test)]
mod alloc_probe {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static LIVE: Cell<usize> = const { Cell::new(0) };
        static PEAK: Cell<usize> = const { Cell::new(0) };
    }

    /// The meter itself. Every allocation is `System`'s; the counters are the only addition.
    pub struct Counting;

    fn note_alloc(n: usize) {
        let _ = LIVE.try_with(|live| {
            let v = live.get() + n;
            live.set(v);
            let _ = PEAK.try_with(|peak| {
                if v > peak.get() {
                    peak.set(v);
                }
            });
        });
    }

    fn note_free(n: usize) {
        let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(n)));
    }

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let p = System.alloc(l);
            if !p.is_null() {
                note_alloc(l.size());
            }
            p
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            let p = System.alloc_zeroed(l);
            if !p.is_null() {
                note_alloc(l.size());
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            note_free(l.size());
            System.dealloc(p, l)
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            let q = System.realloc(p, l, new);
            if !q.is_null() {
                note_free(l.size());
                note_alloc(new);
            }
            q
        }
    }

    /// Run `f` and return its value beside the HIGH-WATER MARK, in bytes, of what this thread held
    /// live at once above where it started. Other threads are invisible to it, which is the point.
    pub fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
        let base = LIVE.with(|l| l.get());
        PEAK.with(|p| p.set(base));
        let out = f();
        let peak = PEAK.with(|p| p.get());
        (out, peak.saturating_sub(base))
    }
}

#[cfg(test)]
#[global_allocator]
static R35_COUNTING: alloc_probe::Counting = alloc_probe::Counting;

/// v1.0.0-rc PNG EXPORT (queue item 35, sheet 2.2c): **WHAT A DECODE MAY KEEP FROM THE FILE.**
///
/// Every decoder in this crate flattens. Alpha is composited over opaque white ([`over_white`]) and
/// 16-bit samples are narrowed to 8, one statement per format, because [`decode_source_rgb`]'s
/// contract is packed RGB8 and the stage, the thumbnails, the fast tier and the ROI cropper all
/// want exactly that. `Keep` is the request that suspends those two statements and NOTHING ELSE.
///
/// It rides down [`decode_source_keep`] from ONE caller -- the ./export export at its PNG stop
/// ([`Keep::for_web`]) -- and every other caller in the tree passes [`Keep::NONE`], under which each
/// decoder executes the identical statements it executed before this round. The golden row
/// `the_two_testkit_goldens_export_byte_for_byte_as_they_did` is what makes that a MEASUREMENT
/// rather than a promise: four deliverables, hashed at the parent commit.
///
/// The two fields are INDEPENDENT because the two facts are -- a 16-bit opaque TIFF has depth and
/// no alpha, an 8-bit logo has alpha and no depth. And nothing is ever UP-converted: `Keep` cannot
/// make a JPEG 16-bit or give a photograph an alpha channel, it can only decline to throw away what
/// the file already holds. The decision is taken from the DECODED DATA, never from the name or the
/// header alone (ledger L42).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Keep {
    /// Return the source's alpha channel instead of compositing it over opaque white.
    pub alpha: bool,
    /// Return the source's samples at their own depth instead of narrowing them to 8 bits.
    pub depth: bool,
}

impl Keep {
    /// The display path's request, and every path in the tree that is not the ./export PNG export:
    /// flatten everything, exactly as it was flattened before this round.
    pub const NONE: Keep = Keep { alpha: false, depth: false };
    /// Keep both. The ./export export's PNG stop, and nothing else.
    pub const ALL: Keep = Keep { alpha: true, depth: true };

    /// Is this request asking for anything at all? Every per-format decoder branches on it BEFORE
    /// its flattening statements, so a `false` here is the byte-pin for every other caller.
    #[inline]
    pub fn any(self) -> bool {
        self.alpha || self.depth
    }

    /// **ONLY THE PNG STOP KEEPS** (round 35, Q3). JPEG carries neither an alpha channel nor more
    /// than 8 bits per sample, so asking a JPG export to keep them would mean decoding wider only
    /// to throw the width away one stage later -- and the JPG stop's composite over opaque WHITE is
    /// a ruled behaviour (sheet 2.2c, 0.2), not an accident of the decoder.
    pub fn for_web(fmt: WebFormat) -> Keep {
        match fmt {
            WebFormat::Jpeg => Keep::NONE,
            WebFormat::Png => Keep::ALL,
        }
    }
}

/// v1.0.0-rc PNG EXPORT (queue item 35): **A DECODED IMAGE AT THE WIDTH THE FILE HELD IT.**
///
/// Four layouts, which is every combination the ./export PNG deliverable can be written in: alpha or
/// no alpha, 8 bits or 16. The 8-bit arms carry BYTES (one per sample, the packing every other
/// buffer in this crate uses); the 16-bit arms carry NATIVE-ENDIAN `u16` SAMPLES, because the
/// resize, the colour transform and the watermark all do arithmetic on them and only the PNG
/// encoder cares about byte order (PNG is network order -- [`write_png`] is the one place that
/// swaps, and the one place that has to know).
///
/// There is no `Gray*` variant on purpose: grey and grey+alpha expand to RGB / RGBA at their own
/// depth inside the decoder that produced them, so the pipeline below has three stages fewer to
/// think about and the deliverable is never a colour type a photographer did not ask for.
///
/// `w` and `h` travel BESIDE this enum as a `(Pixels, u32, u32)` triple -- the same shape
/// `(Vec<u8>, u32, u32)` has had in this crate since Way A, so every call site reads the same.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Pixels {
    /// Packed RGB8 -- what every caller outside the ./export PNG stop gets, and what this crate
    /// returned everywhere before this round.
    Rgb8(Vec<u8>),
    /// Packed RGBA8, STRAIGHT (not premultiplied) alpha.
    Rgba8(Vec<u8>),
    /// Packed RGB16, native-endian samples.
    Rgb16(Vec<u16>),
    /// Packed RGBA16, native-endian samples, STRAIGHT alpha.
    Rgba16(Vec<u16>),
}

impl Pixels {
    /// Samples per pixel: 3 or 4.
    pub fn channels(&self) -> usize {
        match self {
            Pixels::Rgb8(_) | Pixels::Rgb16(_) => 3,
            Pixels::Rgba8(_) | Pixels::Rgba16(_) => 4,
        }
    }

    /// Does this buffer carry an alpha channel at all? (Whether any pixel is actually transparent
    /// is a different question -- [`Pixels::drop_opaque_alpha`] is where that one is asked.)
    pub fn has_alpha(&self) -> bool {
        self.channels() == 4
    }

    /// Are the samples 16 bits wide?
    pub fn is_16bit(&self) -> bool {
        matches!(self, Pixels::Rgb16(_) | Pixels::Rgba16(_))
    }

    /// The layout as the export log names it (round 35, Q10) -- and as the PNG header spells it.
    pub fn layout(&self) -> &'static str {
        match self {
            Pixels::Rgb8(_) => "RGB8",
            Pixels::Rgba8(_) => "RGBA8",
            Pixels::Rgb16(_) => "RGB16",
            Pixels::Rgba16(_) => "RGBA16",
        }
    }

    /// Bytes per pixel: 3, 4, 6 or 8. The number [`write_png`]'s length guard multiplies by, and
    /// the one the memory budget is counted in.
    pub fn bytes_per_px(&self) -> usize {
        self.channels() * if self.is_16bit() { 2 } else { 1 }
    }

    /// Samples held (bytes for the 8-bit arms, `u16`s for the 16-bit ones).
    pub fn sample_len(&self) -> usize {
        match self {
            Pixels::Rgb8(v) | Pixels::Rgba8(v) => v.len(),
            Pixels::Rgb16(v) | Pixels::Rgba16(v) => v.len(),
        }
    }

    /// Bytes held -- the figure the per-stage allocation table counts.
    pub fn byte_len(&self) -> usize {
        self.sample_len() * if self.is_16bit() { 2 } else { 1 }
    }

    /// **0.1 (b): REAL TRANSPARENCY ONLY.** An alpha channel every one of whose samples is full is
    /// not transparency, it is four bytes where three would do -- so it is dropped, and the
    /// deliverable is a third smaller and pixel-identical. One linear scan, once, right after the
    /// decode; against a decode this is free.
    ///
    /// The testkit's two PNGs are exactly this case (colour type 6, minimum alpha over every pixel
    /// 255), which is why the R0 golden's four hashes can hold across this round at all.
    pub fn drop_opaque_alpha(self) -> Pixels {
        match self {
            Pixels::Rgba8(v) if v.chunks_exact(4).all(|p| p[3] == u8::MAX) => {
                Pixels::Rgb8(v.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect())
            }
            Pixels::Rgba16(v) if v.chunks_exact(4).all(|p| p[3] == u16::MAX) => {
                Pixels::Rgb16(v.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect())
            }
            other => other,
        }
    }

    /// Composite the alpha channel over opaque white and drop it -- the house flatten, at whichever
    /// depth the buffer is. A buffer with no alpha is returned untouched, with no allocation.
    pub fn flatten_over_white(self) -> Pixels {
        match self {
            Pixels::Rgba8(v) => Pixels::Rgb8(
                v.chunks_exact(4)
                    .flat_map(|p| [over_white(p[0], p[3]), over_white(p[1], p[3]), over_white(p[2], p[3])])
                    .collect(),
            ),
            Pixels::Rgba16(v) => Pixels::Rgb16(
                v.chunks_exact(4)
                    .flat_map(|p| {
                        [over_white16(p[0], p[3]), over_white16(p[1], p[3]), over_white16(p[2], p[3])]
                    })
                    .collect(),
            ),
            other => other,
        }
    }

    /// Narrow 16-bit samples to 8 by taking the HIGH BYTE -- `v >> 8`, which is what `png`'s own
    /// `STRIP_16` does (`png-0.17.16/src/decoder/transform.rs`, `transform_row_strip16` at :83 and
    /// `output_buffer[i] = row[2 * i]` at :85, over big-endian rows) and what the TIFF arm's `to8`
    /// has always done. An 8-bit buffer is returned untouched, with no allocation.
    pub fn narrow_to_8(self) -> Pixels {
        match self {
            Pixels::Rgb16(v) => Pixels::Rgb8(v.iter().map(|&s| (s >> 8) as u8).collect()),
            Pixels::Rgba16(v) => Pixels::Rgba8(v.iter().map(|&s| (s >> 8) as u8).collect()),
            other => other,
        }
    }

    /// The [`Keep::NONE`] unwrap. Unreachable by construction -- under `Keep::NONE` every arm of
    /// [`decode_source_keep`] returns `Rgb8` -- so it is an `Err`, never a panic, and it names what
    /// it got so a future arm that forgets says so in the log instead of in the pixels.
    fn into_rgb8(self) -> Result<Vec<u8>> {
        let layout = self.layout();
        match self {
            Pixels::Rgb8(v) => Ok(v),
            _ => bail!("internal: a Keep::NONE decode returned {layout}, not RGB8"),
        }
    }
}

/// Narrow a KEPT decode to what `keep` actually asked for -- the ONE place the two independent
/// requests are applied, so no per-format arm has to spell the four combinations out. Under
/// [`Keep::ALL`] (the only request the tree makes) both steps are the identity and neither
/// allocates; the mixed requests exist because the type expresses them.
///
/// Order matters: flatten FIRST, narrow second. The other order would composite 8-bit samples that
/// had just lost their low byte, i.e. it would round twice.
fn apply_keep(px: Pixels, keep: Keep) -> Pixels {
    let px = if keep.alpha { px } else { px.flatten_over_white() };
    if keep.depth {
        px
    } else {
        px.narrow_to_8()
    }
}

/// [`over_white`] at 16 bits -- the identical arithmetic, at the identical scale relative to full.
#[inline]
fn over_white16(v: u16, a: u16) -> u16 {
    ((v as u32 * a as u32 + 65535 * (65535 - a as u32)) / 65535) as u16
}

/// PNG hands 16-bit rows back BIG-ENDIAN (network byte order, PNG spec 7.1); every other 16-bit
/// source in this crate hands back native `u16`s already. This is the one place that says so.
fn be16_samples(buf: &[u8]) -> Vec<u16> {
    buf.chunks_exact(2).map(|p| u16::from_be_bytes([p[0], p[1]])).collect()
}

/// v0.8.101 (S1/S2): [`decode_source_rgb`] with the browse LANE attached — the ONE place a lane can
/// change what gets decoded, and it changes it for exactly one format.
///
/// Every non-HEIC shot is delegated to `decode_source_rgb` unchanged and tagged
/// [`FrameSource::MainImage`]. That is not a convention, it is the byte-pin: a JPEG (or PNG, TIFF,
/// WebP, JXL, BMP, GIF, RAW-preview) shot cannot REACH the v0.8.101 code, so no diff-reading is
/// required to believe those paths are untouched.
fn decode_source_rgb_lane(
    shot: &Shot,
    scale_to: Option<u32>,
    lane: Lane,
) -> Result<Option<(Vec<u8>, u32, u32, FrameSource, DecodeRoute)>> {
    if !matches!(shot.kind, SrcKind::Heic) {
        // v0.8.171: a non-HEIC shot can never be superseded — nothing but the hardware HEIC lane
        // has a mid-decode abort — so this arm is `Some` unconditionally, which is the byte-pin
        // that keeps every other format's path unchanged.
        let (rgb, w, h) = decode_source_rgb(shot, scale_to)?;
        return Ok(Some((rgb, w, h, FrameSource::MainImage, DecodeRoute::Cpu)));
    }
    let path = shot.jpg.as_ref().context("HEIC shot has no path")?;
    decode_heic_lane(path, scale_to, lane)
}

/// Decode a shot's finished-image source to packed RGB8 -- the contract this crate has had since
/// Way A, and the one every caller but the ./export PNG export wants. Delegates to
/// [`decode_source_keep`] with [`Keep::NONE`], under which every per-format arm runs the identical
/// flattening statements it ran before round 35.
fn decode_source_rgb(shot: &Shot, scale_to: Option<u32>) -> Result<(Vec<u8>, u32, u32)> {
    let (px, w, h) = decode_source_keep(shot, scale_to, Keep::NONE)?;
    Ok((px.into_rgb8()?, w, h))
}

/// Decode a shot's finished-image source, keeping what `keep` asks for. `scale_to` requests a smaller decode where
/// the format supports it cheaply — JPEG DCT shrink-on-load, and (v0.8.101 / S2, browse lanes only)
/// HEIC via WIC's decode-at-scale; PNG/TIFF have no shrink-on-load, so they decode full then
/// Lanczos-downscale to `scale_to` (the fast tier pays a full decode for them, mitigated by the
/// frame cache — see PLAN §30.2). `Unsupported` never decodes.
///
/// This entry point is the [`Lane::Native`] one: the HEIC arm below full-decodes, exactly as it did
/// before v0.8.101. The browse tiers go through [`decode_source_rgb_lane`].
/// ROUND 35 (queue item 35): `keep` is the ./export PNG export's request to be handed the alpha
/// channel and the sample depth the FILE holds instead of the flattened RGB8 every other caller
/// wants. [`Keep::NONE`] -- what [`decode_source_rgb`] passes, and what every tier in the app
/// reaches this dispatch with -- takes each arm's shipped path; see [`Keep`].
fn decode_source_keep(shot: &Shot, scale_to: Option<u32>, keep: Keep) -> Result<(Pixels, u32, u32)> {
    match shot.kind {
        SrcKind::Jpeg => {
            // JPEG has no alpha channel and this decoder is 8-bit, so `keep` has nothing to ask
            // for: the arm is the shipped one, lifted into `decode_jpeg_arm` unchanged.
            let (rgb, w, h) = decode_jpeg_arm(shot, scale_to)?;
            Ok((Pixels::Rgb8(rgb), w, h))
        }
        SrcKind::Png => {
            let path = shot.jpg.as_ref().context("PNG shot has no path")?;
            let (px, w, h) = decode_png_rgb(path, keep)?;
            finish_source_keep(apply_keep(px, keep), w, h, scale_to)
        }
        SrcKind::Tiff => {
            let path = shot.jpg.as_ref().context("TIFF shot has no path")?;
            let (px, w, h) = decode_tiff_rgb(path, keep)?;
            finish_source_keep(apply_keep(px, keep), w, h, scale_to)
        }
        SrcKind::Webp => {
            let path = shot.jpg.as_ref().context("WebP shot has no path")?;
            let (px, w, h) = decode_webp_rgb(path, keep)?;
            finish_source_keep(apply_keep(px, keep), w, h, scale_to)
        }
        SrcKind::Heic => {
            let path = shot.jpg.as_ref().context("HEIC shot has no path")?;
            // v0.8.99 (H5): `scale_to` goes IN, not just into the post-hoc finish. On macOS that
            // buys Image I/O's subsample ladder for the fast/thumb tiers (a 256 px thumb no longer
            // costs a full 48 MP decode); on Windows the WIC arm still ignores it and the finish
            // below does all the work, exactly as before. `finish_source` is unchanged either way —
            // it trims whatever came back to the exact target, so no arm double-scales.
            let (px, w, h) = decode_heic_keep(path, scale_to, keep)?;
            finish_source_keep(apply_keep(px, keep), w, h, scale_to)
        }
        SrcKind::Jxl => {
            let path = shot.jpg.as_ref().context("JXL shot has no path")?;
            let (px, w, h) = decode_jxl_rgb(path, keep)?;
            finish_source_keep(apply_keep(px, keep), w, h, scale_to)
        }
        SrcKind::Bmp => {
            let path = shot.jpg.as_ref().context("BMP shot has no path")?;
            let (px, w, h) = decode_bmp_rgb(path, keep)?;
            finish_source_keep(apply_keep(px, keep), w, h, scale_to)
        }
        SrcKind::Gif => {
            let path = shot.jpg.as_ref().context("GIF shot has no path")?;
            // Still consumers (thumb / fast tier / scan / ROI source) get the composited FIRST frame;
            // animated playback is a separate path in the viewer (decode_gif_animation / GifStream).
            let (px, w, h) = decode_gif_first_frame_rgb(path, keep)?;
            finish_source_keep(apply_keep(px, keep), w, h, scale_to)
        }
        SrcKind::Unsupported => bail!("unsupported image format (no decoder)"),
    }
}

/// The `SrcKind::Jpeg` arm of [`decode_source_keep`], lifted out of the dispatch by round 35 and
/// otherwise UNCHANGED -- every statement below is the one that stood in the `match` before, at the
/// same order and the same indent minus two levels.
///
/// It is a function rather than an arm because JPEG is the one format that can keep NOTHING (no
/// alpha channel exists in the container, and this decoder is 8-bit), so it is also the one arm
/// whose result is `Pixels::Rgb8` unconditionally -- and its two early `return`s would otherwise
/// have had to learn the [`Pixels`] type for a value that can only ever be one variant.
fn decode_jpeg_arm(shot: &Shot, scale_to: Option<u32>) -> Result<(Vec<u8>, u32, u32)> {
    let src = jpeg_source(shot)?;
    // v1.0.0-rc TAIL (OWNER RULING, CMYK): the opt-in colour-managed route for the
    // 4-component family, and the thing that finally gives `WicDecoder` — a first-class
    // `ImageDecoder` that has sat in `decode.rs` since it was written with `new()` called
    // NOWHERE in the workspace — a live consumer. It is asked PER DECODE (`cmyk_os_route`),
    // it is scoped to files whose own frame header declares four channels, and it is
    // Windows-only in the same breath as its Settings row: the label names Windows, so the
    // route must be Windows too (L26 — a platform noun and its behaviour travel together).
    // If the OS codec declines, the pure-Rust arm below still runs, so the option can never
    // turn a file that opens into a file that does not.
    #[cfg(windows)]
    if cmyk_os_route() && jpeg_component_count(&src) == Some(4) {
        if let Some(path) = shot.jpg.as_deref() {
            use crate::decode::{DecodedPixels, ImageDecoder};
            let mut wic = crate::decode::WicDecoder::new();
            if let Ok(DecodedPixels::Rgb8 { data, w, h }) =
                wic.decode_scaled(shot, scale_to.unwrap_or(0))
            {
                static CMYK_OS: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                let shown = path.file_name().and_then(|s| s.to_str()).unwrap_or_default();
                if note_path_capped(
                    &format!("cmyk-os:{}", path.display()),
                    &CMYK_OS,
                    SNIFF_NOTE_CAP,
                    || format!(
                        "{}: 4-component JPEG — decoded by Windows' colour-managed codec (Settings: CMYK JPEG decoding)",
                        sanitize_one_line(shown)
                    ),
                ) == NoteVerdict::Suppressed
                {
                    note_once(
                        "cmyk-os-cap",
                        format!("cmyk: {SNIFF_NOTE_CAP} 4-component JPEGs have now taken the OS route — further per-file lines are suppressed for this session"),
                    );
                }
                return Ok((data, w, h));
            }
        }
    }
    match decode_jpeg(&src, scale_to) {
        Ok(out) => Ok(out),
        // v1.0.0-rc (R2): a JPEG that simply ENDS mid-scan — a missing 2-byte EOI, an
        // interrupted sync, a scan-to-email cut in the last MCU — is refused outright by the
        // pure-Rust decoder ("failed to fill whole buffer") and rendered by every other
        // viewer on the machine, WIC and nvJPEG included. So the OS codec gets a second
        // chance at exactly that error class, and at no other: genuine corruption
        // (`Format`/`Unsupported`) still fails here and still classifies as `Corrupt`.
        // A RAW-only shot has no standalone file for the OS codec to open (its preview
        // rides `jpeg_source`), so it keeps the original error — and so does a shot whose
        // finished slot is a PASSENGER, for the same reason: the truncated bytes came from
        // the RAW, and the file beside it is not this picture (TAIL 4, re-verification Y1).
        Err(e) if jpeg_err_is_truncation(&e) => {
            // v1.0.0-rc TAIL 4 (re-verification Y1): `has_jpg`. When the truncated bytes are
            // a RAW's EMBEDDED PREVIEW there is no file the OS codec could open as a JPEG —
            // `shot.jpg` on such a shot is either absent or a PASSENGER, and handing a
            // passenger's AVIF/HEIC to WIC decodes the wrong picture entirely (or, more
            // likely, wastes a COM round trip to be refused). The comment below says "a
            // RAW-only shot has no standalone file"; a passenger has one that is not the
            // picture, which is the same thing for this purpose.
            let Some(path) = shot.jpg.as_deref().filter(|_| shot.has_jpg) else {
                return Err(e);
            };
            match os_codec_decode_rgb(path)
                .and_then(|(rgb, w, h)| finish_source(rgb, w, h, scale_to))
            {
                Ok(out) => {
                    // v1.0.0-rc TAIL (skeptic B, Y1): capped like every other per-path
                    // family. A card of half-copied JPEGs is a real folder shape.
                    static TRUNCATED: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    let shown = path.file_name().and_then(|s| s.to_str()).unwrap_or_default();
                    // A per-DECODE family has no folder to trail, so it announces its own
                    // fullness once (`note_once` makes the fixed key once-only for free).
                    if note_path_capped(
                        &format!("jpeg-truncated:{}", path.display()),
                        &TRUNCATED,
                        SNIFF_NOTE_CAP,
                        || format!(
                            "{}: the JPEG ends mid-scan — decoded by the OS codec (the pure-Rust decoder needs a whole scan)",
                            sanitize_one_line(shown)
                        ),
                    ) == NoteVerdict::Suppressed
                    {
                        note_once(
                            "jpeg-truncated-cap",
                            format!("jpeg: {SNIFF_NOTE_CAP} truncated files have now been served by the OS codec — further per-file lines are suppressed for this session"),
                        );
                    }
                    Ok(out)
                }
                // The second chance did not take it either: report the ORIGINAL refusal,
                // which is the one that names what is wrong with the file.
                Err(_) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// Optionally downscale a freshly-decoded full-res RGB8 buffer to `scale_to` (long side). `None`
/// keeps full resolution (the ROI-cropper source buffer).
fn finish_source(rgb: Vec<u8>, w: u32, h: u32, scale_to: Option<u32>) -> Result<(Vec<u8>, u32, u32)> {
    match scale_to {
        Some(long) => resize_to_long(rgb, w, h, long),
        None => Ok((rgb, w, h)),
    }
}

/// [`finish_source`] over a [`Pixels`] of any layout (round 35). `None` -- what the ./export export
/// passes, since its own long-edge cap is applied inside [`export_web_to`] -- hands the buffer
/// straight back without touching it, so the keep path allocates nothing here.
fn finish_source_keep(px: Pixels, w: u32, h: u32, scale_to: Option<u32>) -> Result<(Pixels, u32, u32)> {
    match scale_to {
        Some(long) => resize_pixels_to_long(px, w, h, long),
        None => Ok((px, w, h)),
    }
}

/// Composite one 8-bit sample over an opaque-white background by its alpha — the graceful way to
/// flatten a transparent PNG/TIFF for a photo viewer (photos are opaque; a stray logo PNG stays
/// legible instead of showing garbage RGB in its transparent pixels).
#[inline]
fn over_white(v: u8, a: u8) -> u8 {
    ((v as u16 * a as u16 + 255 * (255 - a as u16)) / 255) as u8
}

/// Decode a HEIC/HEIF to packed RGB8 via the OS image codec (Windows WIC → the installed HEVC/HEIF
/// Image Extension). No in-tree HEVC decoder, so no cmake/nasm/patent surface. `Err` when the codec
/// isn't installed or the file is malformed — the caller surfaces that as a retryable decode (#15).
/// Decode any image WIC can open (via its installed codec) to packed RGB8. The pipeline is
/// format-agnostic — `CreateDecoderFromFilename` picks the codec from the file — so it serves both
/// HEIC (HEVC Image Extension) and the exotic-TIFF fallback (CCITT G3/G4 fax, 1-bit bilevel,
/// old-JPEG-in-TIFF, YCbCr, CMYK…). `no_codec_hint` is the message for the "no installed codec" case.
#[cfg(windows)]
fn wic_decode_rgb24(path: &Path, no_codec_hint: &str) -> Result<(Vec<u8>, u32, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat24bppRGB, IWICImagingFactory,
        WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    unsafe {
        // COM must be initialised on this worker thread. Idempotent (S_FALSE if already init); we
        // don't CoUninitialize — the decode threads keep serving for the app's lifetime.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .context("WIC imaging factory")?;
        let wpath: Vec<u16> =
            path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let decoder = factory
            .CreateDecoderFromFilename(
                PCWSTR(wpath.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .context(no_codec_hint.to_string())?;
        let frame = decoder.GetFrame(0).context("WIC: frame 0")?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).context("WIC: GetSize")?;
        guard_source_dims(w, h, "WIC frame")?;
        let converter = factory.CreateFormatConverter().context("WIC format converter")?;
        // Format-convert to 24bpp RGB — this is where WIC applies the CCITT/bilevel/YCbCr/CMYK/
        // palette expansion, so any codec-supported TIFF or HEIC lands as plain RGB here.
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat24bppRGB,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .context("WIC: convert to RGB24")?;
        // Row stride in usize: `w * 3` in u32 could wrap for a very wide strip — the pixel cap bounds
        // w*h, not w alone (a 2e9×1 image passes it). CopyPixels takes the stride as u32 over FFI, so
        // reject anything that type can't express before casting.
        if (w as u64) * 3 > u32::MAX as u64 {
            bail!("WIC row stride overflows u32 ({w} px wide)");
        }
        let stride = w as usize * 3;
        let mut buf = vec![0u8; stride * h as usize];
        converter
            .CopyPixels(std::ptr::null(), stride as u32, &mut buf)
            .context("WIC: CopyPixels")?;
        Ok((buf, w, h))
    }
}

/// The "no installed codec" message every HEIC decode door reports with. One constant so the
/// entry points below cannot drift into three different wordings of the same failure.
#[cfg_attr(not(windows), allow(dead_code))] // the Windows/WIC doors are its only consumers
const HEIC_NO_CODEC: &str = "HEIC: no OS decoder (HEVC/HEIF Image Extension may not be installed)";

// v0.8.99 (H5): `scale_to` is threaded IN. It is the render tier's requested long side — the same
// value `decode_source_rgb` would otherwise only apply as a post-hoc Lanczos. Whether an arm can
// USE it is per-platform, and the difference is the whole point of the parameter:
//   * Windows/WIC — ignored HERE, deliberately. This is the CLASSIC door (`Lane::Native` and the
//     `FALCON_CLASSIC_HEIC=1` revert): full native frame, then the caller's Lanczos. The browse
//     lanes' decode-at-scale is v0.8.101's S2 and lives in `wic_decode_rgb24_scaled`, reached
//     through `decode_heic_lane` — keeping it out of THIS function is what makes the revert switch
//     a one-line restoration rather than a set of scattered conditionals.
//   * macOS/Image I/O — HONOURED, since v0.9.30. The subsample ladder already existed and the
//     detail tier's accel slot already used it; the CPU tiers (fast + thumb, i.e. the whole browse
//     lane) were passing `None` and paying a full native decode for a 256 px thumbnail. That
//     remains true in classic mode: v0.8.101's switch reverts S1/S2, not the Mac's H5.
#[cfg(windows)]
fn decode_heic(path: &Path, _scale_to: Option<u32>) -> Result<(Vec<u8>, u32, u32)> {
    wic_decode_rgb24(path, HEIC_NO_CODEC)
}
// macOS (v0.9.9, MACOS_PORT §2): HEIC is native to Image I/O since macOS 10.13 — CGImageSource is the
// direct analogue of WIC (same zero-install, patent-sidestepping OS decode).
//
// v0.8.99 (H5, investigation S6): this passed `None` — a FULL native decode on every fast and thumb
// decode, then a Lanczos down. The ladder (`imageio_subsample_max_px`) existed and was already used
// by the detail tier's Image I/O accel slot; the CPU path simply never asked for it. Now it does,
// and the browse lane gets a decode-at-scale for the first time on this platform.
//
// NO DOUBLE-SCALING: the ladder returns a subsample stop that never UNDERSHOOTS the target, and the
// caller's `finish_source` then Lanczos-trims that stop to exactly `scale_to` — the identical
// two-step (cheap stop → exact resize) the JPEG DCT path has always used. Output pixels are the same
// size as before; only the decode got cheaper.
#[cfg(target_os = "macos")]
fn decode_heic(path: &Path, scale_to: Option<u32>) -> Result<(Vec<u8>, u32, u32)> {
    imageio_decode_rgb(path, scale_to)
}
#[cfg(all(not(windows), not(target_os = "macos")))]
fn decode_heic(_path: &Path, _scale_to: Option<u32>) -> Result<(Vec<u8>, u32, u32)> {
    bail!("HEIC decode requires a system HEVC/HEIF codec (Windows WIC / macOS Image I/O)")
}

/// ROUND 35: the HEIC arm of [`decode_source_keep`].
///
/// **Windows keeps NOTHING, and that is stated residue** (Q7 (i)): the decode is WIC's, whose format
/// converter target is `GUID_WICPixelFormat24bppRGB`, so a 10-bit sample and an auxiliary alpha item
/// are gone before Falcon owns a byte. Keeping them means a second converter target behind a
/// pixel-format query, and no HEIC on this machine can prove it -- so it is not built, and the
/// export logs one line per run when a Windows HEIC is written to a PNG.
///
/// **macOS keeps ALPHA**, straight (un-premultiplied), through [`imageio_decode_rgb_as`]; its
/// 10/12-bit depth is residue for the same reason Windows' is -- the 16-bpc bitmap context cannot be
/// executed or measured on this host (see §C.7 of the round record).
#[cfg(target_os = "macos")]
fn decode_heic_keep(path: &Path, scale_to: Option<u32>, keep: Keep) -> Result<(Pixels, u32, u32)> {
    imageio_decode_rgb_as(path, scale_to, false, keep)
}

/// See [`decode_heic_keep`]: on every platform but macOS the HEIC decode goes through an OS format
/// converter that has already flattened, so `keep` has nothing to act on.
#[cfg(not(target_os = "macos"))]
fn decode_heic_keep(path: &Path, scale_to: Option<u32>, keep: Keep) -> Result<(Pixels, u32, u32)> {
    let _ = keep;
    let (rgb, w, h) = decode_heic(path, scale_to)?;
    Ok((Pixels::Rgb8(rgb), w, h))
}

/// v1.0.0-rc PNG EXPORT (queue item 35, Q7 (i)): **THE HEIC RESIDUE, IN ONE SENTENCE.**
///
/// HEIC is the one source whose alpha and depth this round cannot fully keep, and WHY is
/// per-platform -- so the sentence lives here, beside [`decode_heic_keep`], and not in the export
/// walk that prints it (ledger L26: a platform noun and its behaviour travel together, and the
/// merge is what turns a typed noun into another platform's lie).
///
/// The ./export run logs it ONCE, and only when a HEIC is actually written to the PNG stop -- so a
/// photographer whose transparent or 10-bit HEIC came out flat reads the reason in the same file as
/// the deliverable's name, instead of filing it as this round's failure. No toast: the sheet says
/// nothing about depth (sheet 2.2c, 0.4 (a)) and this is the field report.
#[cfg(windows)]
pub fn heic_keep_note() -> &'static str {
    "web-export: a HEIC decodes through Windows' own format converter at 24bppRGB, so its transparency and its 10/12-bit depth are gone before Falcon owns a byte — a HEIC's PNG deliverable is RGB8"
}

/// See [`heic_keep_note`]. Everywhere else the decode is macOS's Image I/O, which Falcon draws
/// ITSELF -- so a HEIC's alpha reaches the deliverable, and only its depth does not (the bitmap
/// context is 8 bits per component; the 16-bpc one is this round's stated residue). On a platform
/// with neither decoder a HEIC never reaches the encoder at all, so the line cannot be printed.
#[cfg(not(windows))]
pub fn heic_keep_note() -> &'static str {
    "web-export: a HEIC is drawn into an 8-bit-per-component bitmap, so its transparency reaches the PNG deliverable and its 10/12-bit depth does not"
}

/// v0.8.101 (S1 + S2): the HEIC arm of the browse lanes. The ONLY function in the tree that reads
/// [`Lane`], and the only one the two strategies live behind.
///
/// The ladder, in order, and every rung falls SOFT to the next (a COM error, an absent preview, a
/// codec with no decode-at-scale — none of them is a decode failure, they are just a slower answer):
///   1. **S1, `Lane::Thumb` only** — the file's embedded preview, if it exists and actually covers
///      the tier target at the right aspect ([`heic_embedded_preview_rgb`], WIC on Windows and
///      Image I/O on macOS). Tagged [`FrameSource::EmbeddedPreview`] so it can never be mistaken
///      for a decode of the real image — and so the CALLER can ask [`frame_source_gamut`] for the
///      PREVIEW item's own colour description rather than the master's (v0.8.105 / W5).
///   2. **S2, `Lane::Thumb`/`Lane::Fast`, WINDOWS ONLY** — WIC's decode-at-scale
///      ([`wic_decode_rgb24_scaled`]), which internally degrades to the plain full decode when no
///      cheap stop covers the ask OR the codec can't actually shrink (v0.8.102 / F4 made the second
///      half true of the code, not just of this sentence). This is also where a DECLINED S1 preview
///      lands on Windows — a Thumb decline falls to the stop ladder here, not to rung 3. macOS has
///      no rung 2 because it does not need one: since v0.9.30 its `decode_heic` scale-throughs on
///      the subsample ladder for EVERY lane, so rung 3 already IS the scaled decode there (and is
///      therefore also where a macOS S1 decline lands). Two rungs doing the same job would be two
///      chances to double-scale.
///   3. **`Lane::Native`, or `FALCON_CLASSIC_HEIC=1`** — the platform's plain `decode_heic` and
///      then `finish_source`'s Lanczos. On Windows that is the v0.8.100 path verbatim; on macOS it
///      keeps the v0.9.30 scale-through, because this round's switch reverts S1/S2, not H5.
///
/// `finish_source` runs at the end of EVERY rung, so the returned dims are identical to what the
/// pre-v0.8.101 path produced for the same `scale_to` — rungs 1 and 2 change the cost, never the
/// frame size ([`scaled_dims`] is the shared arithmetic; the integration tests assert the equality
/// on the real testkit files).
///
/// v0.8.148 (E5) — **RUNG 0, THE HARDWARE LANE**, and it sits above all three because it replaces
/// the decode rather than the resampling. It is asked only on [`Lane::Fast`] and [`Lane::Native`];
/// the thumb tier is deliberately untouched, because S1's embedded preview costs no HEVC frame at
/// all and 58 ms of hardware decode is not a thumbnail budget (E3-M2's own timing split says so).
/// It declines to `None` for every reason there is — no capability, a container it cannot parse, a
/// mosaic past the device's limits, a VUI it will not guess at, a driver error mid-photo — and a
/// decline lands on the very next rung with nothing else changed. `FALCON_CLASSIC_HEIC=1` never
/// reaches it: the switch is checked here AND the app declines to install the hook, so the classic
/// mode is the v0.8.100 path with no hardware code on the stack at all.
/// v0.8.171: `Ok(None)` = the hardware lane ABANDONED this decode mid-grid because the app moved on
/// (see [`HwHeicAnswer::Superseded`]). It is not an error and it is not a decline: no rung below
/// rung 0 runs, and the caller's shot simply has no frame yet.
fn decode_heic_lane(
    path: &Path,
    scale_to: Option<u32>,
    lane: Lane,
) -> Result<Option<(Vec<u8>, u32, u32, FrameSource, DecodeRoute)>> {
    // v0.8.144 (E1): THE ONLY WIRE FROM THE SHIPPING PATH INTO THE NEW CONTAINER PARSER, and it is
    // a no-op unless `FALCON_HW_HEIC_PARSE=1`. The whole stage is inert by construction: this call
    // returns on a `OnceLock` bool read before it touches the file, and it can neither change what
    // this function returns nor fail it — the parser's verdict is parked as a diagnostic line and
    // discarded. It sits at the TOP of the lane rather than beside one rung because the question it
    // answers ("what does this container actually say?") has nothing to do with which rung ran.
    heif_grid::note_hw_heic_parse(path);
    if !classic_heic() {
        // RUNG 0 (v0.8.148 / E5): the hardware lane. `Some` returns the finished photo; `None` is a
        // decline and falls through to exactly the ladder that ran before this rung existed.
        if hw_heic_lane_applies(lane) {
            if let Some(hw) = hw_heic_hook() {
                // v0.8.171: THREE answers. `Superseded` returns `Ok(None)` from here and every rung
                // below is skipped — the whole point is not to pay WIC for a photograph the app has
                // just decided nobody is waiting for.
                match hw(path, scale_to, lane) {
                    HwHeicAnswer::Superseded => return Ok(None),
                    HwHeicAnswer::Declined => {}
                    HwHeicAnswer::Served { rgb, w, h } => {
                    // `finish_source` is a NO-OP over a correct hardware answer — E3-M2's dims gate
                    // pins `output_dims` to `scaled_dims` on every corpus file at every tier the
                    // browse lanes ask for, and `resize_to_long` does nothing when the long side
                    // already fits. It is run anyway, and that is the point: the contract "rung 0
                    // changes the cost, never the frame size" is then structural rather than a
                    // property two crates have to keep agreeing about.
                        let (rgb, w, h) = finish_source(rgb, w, h, scale_to)?;
                        return Ok(Some((rgb, w, h, FrameSource::MainImage, DecodeRoute::Hardware)));
                    }
                }
            }
        }
        if let (Lane::Thumb, Some(target)) = (lane, scale_to) {
            if let Some((rgb, w, h)) = heic_embedded_preview_rgb(path, target) {
                let (rgb, w, h) = finish_source(rgb, w, h, scale_to)?;
                return Ok(Some((rgb, w, h, FrameSource::EmbeddedPreview, DecodeRoute::Cpu)));
            }
        }
        #[cfg(windows)]
        if let (Lane::Thumb | Lane::Fast, Some(target)) = (lane, scale_to) {
            let (rgb, w, h) = wic_decode_rgb24_scaled(path, target, HEIC_NO_CODEC)?;
            let (rgb, w, h) = finish_source(rgb, w, h, scale_to)?;
            return Ok(Some((rgb, w, h, FrameSource::MainImage, DecodeRoute::Cpu)));
        }
    }
    let (rgb, w, h) = decode_heic(path, scale_to)?;
    let (rgb, w, h) = finish_source(rgb, w, h, scale_to)?;
    Ok(Some((rgb, w, h, FrameSource::MainImage, DecodeRoute::Cpu)))
}

/// v0.8.148 (E5): may the hardware lane be ASKED for this tier at all?
///
/// A pure predicate, separated from the ladder so the tier policy is one line a test can pin rather
/// than a shape inside a function that also does I/O. FALSIFIER: widen it to `Lane::Thumb` and
/// `the_thumb_tier_never_reaches_the_hardware_lane` reddens — which is the row that keeps the S1
/// embedded-preview door being the thumb tier's answer, as the plan's E5 charter requires.
#[inline]
pub fn hw_heic_lane_applies(lane: Lane) -> bool {
    matches!(lane, Lane::Fast | Lane::Native)
}

/// S1's platform seam: the file's EMBEDDED preview at ≥ `min_long`, or `None` to decline.
/// Windows reads it through WIC's `GetThumbnail`; macOS through Image I/O's
/// `CreateThumbnailFromImageIfAbsent`. Neither exists elsewhere, so elsewhere always declines and
/// the caller decodes the main image exactly as it always has.
#[cfg(windows)]
fn heic_embedded_preview_rgb(path: &Path, min_long: u32) -> Option<(Vec<u8>, u32, u32)> {
    wic_thumbnail_rgb24(path, min_long)
}
#[cfg(target_os = "macos")]
fn heic_embedded_preview_rgb(path: &Path, min_long: u32) -> Option<(Vec<u8>, u32, u32)> {
    imageio_embedded_preview_rgb(path, min_long)
}
#[cfg(all(not(windows), not(target_os = "macos")))]
fn heic_embedded_preview_rgb(_path: &Path, _min_long: u32) -> Option<(Vec<u8>, u32, u32)> {
    None
}

/// v0.8.101 (S1): the two checks a candidate preview must pass before it may stand in for a decode
/// of the master. PURE, so both platform arms share one rule and it unit-tests on any machine —
/// the Mac arm's behaviour is otherwise unverifiable from this box.
///
/// * **Coverage** — the preview's long side must be at least the tier target. Without this a
///   camera that writes a 160 px preview would hand a 256 px filmstrip a blurry tile, which is a
///   fidelity regression dressed as a speed-up.
/// * **Aspect** — the preview's shape must match the master's within 2%. That rejects a
///   letterboxed/padded preview, and — the reason it is not paranoia — a codec that applies the
///   container's `irot` to the master but not to the preview item, which would otherwise put a
///   sideways tile in the strip. A 90° mismatch inverts the ratio, so this catches it outright.
#[inline]
#[cfg_attr(all(not(windows), not(target_os = "macos")), allow(dead_code))]
fn preview_is_usable(tw: u32, th: u32, fw: u32, fh: u32, min_long: u32) -> bool {
    if tw == 0 || th == 0 || fw == 0 || fh == 0 || tw.max(th) < min_long {
        return false;
    }
    // Cross-multiplied, so no floats and no division by zero.
    let (a, b) = (tw as u64 * fh as u64, th as u64 * fw as u64);
    a.abs_diff(b) * 50 <= a.max(b)
}

/// v0.8.103 (V11): WHICH of [`preview_is_usable`]'s two rules a decline is attributable to — for the
/// once-per-session NOTE KEY only, never for the decision.
///
/// The shared-predicate refactor (v0.9.x) merged what were two distinct decline reasons on main
/// (`coverage` / `aspect`) into one `usable` key on this trunk. [`note_once`] gates on the key alone
/// and never clears it, so a session containing one preview that is too SMALL and another that is the
/// wrong SHAPE logged exactly ONE line, frozen at whichever file arrived first — which is the
/// swallowed-reason complaint v0.8.102's F3 was written against, surviving for that pair. Splitting
/// the key here restores the eight-key set on both platforms while keeping `preview_is_usable` the
/// sole decision, so the two can never disagree about whether a preview is used.
///
/// Ordering matches the predicate's own: coverage is tested first, so a preview that fails BOTH rules
/// is reported as `coverage`, and a degenerate `0x0` (which fails coverage vacuously) never lands in
/// `aspect` where its ratio is undefined.
#[inline]
#[cfg_attr(all(not(windows), not(target_os = "macos")), allow(dead_code))]
fn preview_decline_reason(tw: u32, th: u32, min_long: u32) -> &'static str {
    if tw.max(th) < min_long {
        "coverage"
    } else {
        "aspect"
    }
}

/// v0.8.101 (S1): the EMBEDDED-PREVIEW door. Every iPhone HEIC carries one (the v0.8.99 probe
/// measured 576×432 beside a 4032×3024 master), and Falcon has never read it — so the 256 px thumb
/// tier has been paying a full 12–48 MP HEVC decode per tile, ~2200× the cost of the JPEG thumb it
/// sits next to in the same filmstrip.
///
/// `min_long` is the tier target the preview must actually COVER. This is the second structural
/// guard behind the caveat "a 576 px preview must never be served where a 2880 px scrub frame is
/// expected": even with the lane gate bypassed, a preview smaller than the ask is refused by
/// arithmetic, so the tile can never come back softer than the path it replaced.
///
/// The ASPECT guard is the third: a preview whose shape disagrees with the master by >2% is
/// refused. That covers the letterboxed/padded preview, and — the reason it is not paranoia — a
/// codec that applies the container's `irot` to the master but not to the preview item, which would
/// otherwise hand the filmstrip a sideways tile.
///
/// Returns `None` for EVERY decline (no preview, too small, wrong shape, any COM error): fail-soft
/// is the whole contract. v0.8.102 (F6/F10): what the caller then does is decode the main image
/// **through the S2 rung** — [`decode_heic_lane`]'s very next arm catches `Lane::Thumb` too, and at
/// a 256 px ask the 1/8 stop qualifies on any master ≥ 2048 px — NOT `Lane::Native`'s classic full
/// decode, which only runs when no stop covers the ask. Same picture, same size, different resample
/// chain; the integration test's decline branch is bounded perceptually for exactly that reason.
/// Each distinct outcome parks ONE line via [`note_once`], so a session says which door it used
/// without a word per decode.
#[cfg(windows)]
fn wic_thumbnail_rgb24(path: &Path, min_long: u32) -> Option<(Vec<u8>, u32, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat24bppRGB, IWICBitmapSource, IWICImagingFactory,
        WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    /// One place to record a decline, so every early return reads the same.
    ///
    /// v0.8.102 (F3): the line states a PER-FILE outcome, because that is what a decline is — there
    /// is no latch anywhere on this path, and the very next HEIC re-enters the preview door and can
    /// succeed. The old wording ("falling back to the main-image decode for HEIC this session") read
    /// as a fleet-wide fact, so one preview-less file made the log claim S1 was off for a folder it
    /// was serving 99% of. `reason` is a SHORT FIXED key (one of the eight below) rather than the
    /// formatted `why`: it keys the once-per-session note, so each distinct decline reason gets its
    /// own line — the old single key froze the first file's reason and swallowed every other kind.
    /// Bounded by construction: eight keys, not one per file — `none` / `nosize` / `dims` /
    /// `coverage` / `aspect` / `noconv` / `convert` / `copy`. (v0.8.103, V11: the merge to this trunk
    /// had collapsed `coverage` and `aspect` into a single `usable` key while this sentence still
    /// said eight; [`preview_decline_reason`] restores the split for the KEY without touching the
    /// shared [`preview_is_usable`] decision, so both platforms now carry the same eight.)
    fn decline<T>(reason: &'static str, why: String) -> Option<T> {
        note_once(
            &format!("s1-heic-decline-{reason}"),
            format!(
                "heic thumb S1: this file's embedded preview declined ({why}) — its tile came from \
                 the main-image decode instead. Per FILE, not a session-wide fallback: the next \
                 HEIC still tries the preview door."
            ),
        );
        None
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let wpath: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let Ok(decoder) = factory.CreateDecoderFromFilename(
            PCWSTR(wpath.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        ) else {
            // No codec / unreadable file. Do NOT note this as an S1 decline — the main-image decode
            // is about to fail the same way and report it honestly with the filename.
            return None;
        };
        let Ok(frame) = decoder.GetFrame(0) else { return None };
        let (mut fw, mut fh) = (0u32, 0u32);
        if frame.GetSize(&mut fw, &mut fh).is_err() || fw == 0 || fh == 0 {
            return None;
        }
        // The per-FRAME thumbnail is the one an iPhone HEIC carries; the container-level one is the
        // fallback door some codecs use instead (both were exercised by the v0.8.99 probe).
        let thumb: Option<IWICBitmapSource> =
            frame.GetThumbnail().ok().or_else(|| decoder.GetThumbnail().ok());
        let Some(t) = thumb else { return decline("none", "codec surfaced none".into()) };
        let (mut tw, mut th) = (0u32, 0u32);
        if t.GetSize(&mut tw, &mut th).is_err() || tw == 0 || th == 0 {
            return decline("nosize", "preview reports no size".into());
        }
        // Bomb guard, same chokepoint as every other decode door.
        if guard_source_dims(tw, th, "WIC thumbnail").is_err() {
            return decline("dims", "preview dims out of bounds".into());
        }
        // The shared coverage + aspect rule (see `preview_is_usable`) — ONE decision, but v0.8.103
        // (V11) keys the note by WHICH rule refused it, so a folder containing both a too-small
        // preview and a wrong-shaped one logs both lines instead of freezing on the first.
        if !preview_is_usable(tw, th, fw, fh, min_long) {
            let reason = preview_decline_reason(tw, th, min_long);
            let why = if reason == "coverage" {
                format!("preview {tw}x{th} is smaller than the {min_long} px tier")
            } else {
                format!("preview {tw}x{th} does not match the master's {fw}x{fh} shape")
            };
            return decline(reason, why);
        }
        let Ok(converter) = factory.CreateFormatConverter() else {
            return decline("noconv", "no WIC format converter".into());
        };
        if converter
            .Initialize(
                &t,
                &GUID_WICPixelFormat24bppRGB,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .is_err()
        {
            return decline("convert", "preview will not convert to RGB24".into());
        }
        // v0.8.152 (R3-L8): the u32 stride guard this door's two siblings already carry
        // (`wic_decode_rgb24`, `wic_decode_rgb24_scaled`). `guard_source_dims` bounds `tw × th`, not
        // `tw` alone, so a 2 000 000 000 × 1 preview passes it and then WRAPS the `stride as u32`
        // that goes over FFI to `CopyPixels`. Expressed as a decline rather than a `bail!` because
        // that is this door's vocabulary — every refusal here is a named S1 decline key.
        if (tw as u64) * 3 > u32::MAX as u64 {
            return decline("stride", format!("preview row stride overflows u32 ({tw} px wide)"));
        }
        let stride = tw as usize * 3;
        let mut buf = vec![0u8; stride * th as usize];
        if converter.CopyPixels(std::ptr::null(), stride as u32, &mut buf).is_err() {
            return decline("copy", "preview CopyPixels failed".into());
        }
        note_once(
            "s1-heic",
            format!(
                "heic thumb S1: serving the {min_long} px tier from the file's embedded \
                 {tw}x{th} preview (master {fw}x{fh}) — no HEVC frame decoded"
            ),
        );
        Some((buf, tw, th))
    }
}

/// v0.8.101 (S2): the STOP LADDER. Which power-of-two decode stop to ask the WIC HEIF codec for,
/// given a native long side and the long side we actually want. `1` means "no usable stop — decode
/// full", which is a real and frequent answer, not a failure.
///
/// This exists because of a measurement, and the measurement was a surprise — twice.
///
/// **First surprise: asking for an arbitrary size is a trap.** `GetClosestSize` on the Store
/// HEVC/HEIF extension accepts ANY size — ask for 2880 from a 4032 master and it answers 2880. But
/// *accepting* a size is not *decoding at* it: the codec decodes at the smallest power-of-two stop
/// that still covers the request and then resamples internally, with a slow resampler. Asking it
/// for arbitrary sizes made things WORSE, badly so where no stop qualified and it decoded 1:1 and
/// then resampled (4032→2880: 498 ms against 193 ms for the plain full decode; 5712→2880: 1321 ms
/// against 376 ms). So we ask for the STOP and do the last hop ourselves with the SIMD Lanczos the
/// browse lane already uses — the same two-step the JPEG DCT path has always run.
///
/// **Second surprise: the 1/2 stop is not cheaper than 1/1.** Measured single-threaded on the
/// 5-file iPhone testkit, decoding a 8064×6048 master at its 1/2 stop cost ~705–720 ms against
/// ~611–647 ms for the full frame — a 12% LOSS, not a saving. Under the real 18-worker pool the
/// verdict was blunter: 36 frames at a 2880 target took 23.6 s through the 1/2 stop against 16.2 s
/// full-decoding (a 46% regression), and at 2048 it was 23.5 s against 15.0 s. The same benchmark
/// at a 1440 target — where the 1/4 stop qualifies — ran 8.2 s against 15.0 s, a 1.84× WIN.
///
/// Hence [`HEIC_MIN_STOP_DIV`]: the ladder starts at 1/4. A stop the codec charges full price for
/// is not a stop worth taking, and the honest answer for those shapes is the path we already had,
/// which `1` selects.
pub const HEIC_MIN_STOP_DIV: u32 = 4;

/// v0.8.102 (F4/F5): rung 1's ACCEPTANCE TEST, as a pure predicate — is the size `GetClosestSize`
/// answered with a stop worth taking? Extracted from the middle of the COM ladder so the rule that
/// decides rung 2's reachability is unit-testable on a machine with no codec at all.
///
/// `closest` is the codec's answer, `native` the frame's real size, `target_long` the ask. Two
/// substantive tests, and they are the whole safety argument for decode-at-scale:
///   * **A REAL saving** — `closest` must be strictly smaller than native. A codec that exposes
///     `IWICBitmapSourceTransform` but hands back the native size is telling us it has no
///     decode-at-scale (the v0.8.99 probe's finding); taking that answer would decode 1:1 and then
///     resample, the 2.6× LOSS `heic_stop_divisor`'s doc measured.
///   * **STILL COVERS the ask** — `closest` must be at least `target_long` on the long side, or the
///     scrub tier gets a frame softer than the one v0.8.100 served.
/// Plus the degenerate guard (`> 0`), because a zero dimension would divide the stride arithmetic.
///
/// FALSIFIER (L28): drop the `< native` term and a no-decode-at-scale codec's native-size answer is
/// "accepted", which is the arbitrary-size ask the round's own measurement condemns; drop the
/// `>= target_long` term and the coverage rows below fail with an undershooting stop.
#[inline]
pub fn heic_stop_is_acceptable(closest: (u32, u32), native: (u32, u32), target_long: u32) -> bool {
    let (sw, sh) = closest;
    sw > 0 && sh > 0 && sw.max(sh) < native.0.max(native.1) && sw.max(sh) >= target_long
}

#[inline]
pub fn heic_stop_divisor(long: u32, target: u32) -> u32 {
    if target == 0 {
        return 1;
    }
    // Coarsest first: the cheapest stop that still COVERS the ask, never one below it (undershooting
    // would hand the scrub tier a softer frame than v0.8.100 did — a fidelity regression wearing a
    // speed-up's clothes).
    for d in [8u32, 4, 2] {
        if d >= HEIC_MIN_STOP_DIV && long / d >= target {
            return d;
        }
    }
    1
}

/// v0.8.101 (S2): DECODE-AT-SCALE for HEIC — the ledger's N16, and the browse-lane lever that has
/// been a pure no-op for this format since the day the WIC arm landed.
///
/// The rungs, in order:
///   1. `IWICBitmapSourceTransform` asked for the power-of-two STOP that [`heic_stop_divisor`]
///      picks — never for an arbitrary size, for the measured reason documented there. The stop is
///      then Lanczos-trimmed to the exact target by the caller's chain.
///   2. `IWICBitmapScaler` asked for the SAME stop — the one rung 1 ACCEPTED
///      ([`heic_stop_is_acceptable`]), which is why the scaler does no interpolation of its own: it
///      is a different door to the same pixels, not a different resampler. Reachable only when rung
///      1 approved a stop and then could not drive `CopyPixels` (an exotic pixel format, say).
///      v0.8.102 (F4/F5) made that "only" true of the code and not just of this sentence.
///   3. The plain full decode + Lanczos: byte-for-byte v0.8.100. Taken when the frame exposes no
///      transform, when the codec has no real decode-at-scale (it answers `GetClosestSize` with the
///      native size), and — the common case on real iPhone files at the scrub tier — when NO stop
///      covers the ask. Not a fallback so much as the right answer for that shape.
///
/// The returned dims are exactly [`scaled_dims`] of the native size — the same numbers the old
/// full-decode-then-Lanczos path produced — so this is a cost change, not a layout change.
///
/// The bomb guard still runs on the NATIVE header dims, not the scaled ones. A source over the cap
/// keeps failing exactly as it did: decode-at-scale would make it survivable, but "this file now
/// opens where it used to be refused" is a behaviour change this round did not ask for.
#[cfg(windows)]
fn wic_decode_rgb24_scaled(
    path: &Path,
    target_long: u32,
    no_codec_hint: &str,
) -> Result<(Vec<u8>, u32, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat24bppRGB, IWICBitmapSource,
        IWICBitmapSourceTransform, IWICImagingFactory, IWICPixelFormatInfo,
        WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
        WICBitmapTransformRotate0, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .context("WIC imaging factory")?;
        let wpath: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let decoder = factory
            .CreateDecoderFromFilename(
                PCWSTR(wpath.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .context(no_codec_hint.to_string())?;
        let frame = decoder.GetFrame(0).context("WIC: frame 0")?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).context("WIC: GetSize")?;
        guard_source_dims(w, h, "WIC frame")?;
        // Nothing to shrink to: the source already fits the ask (every iPhone HEIC at the DETAIL
        // tier's 8192 cap, for instance). Fall through to the plain full decode below.
        let (nw, nh) = scaled_dims(w, h, target_long);
        // The stop the codec can decode CHEAPLY, or 1 for "none — full decode is the right answer
        // for this shape". See `heic_stop_divisor` for the measurements behind that.
        let div = heic_stop_divisor(w.max(h), target_long);
        let want_scale = w.max(h) > target_long;

        // ── rung 1: the codec's own decode-at-scale, at the STOP ──
        let mut source: Option<(IWICBitmapSource, u32, u32)> = None;
        // v0.8.102 (F4/F5): the stop rung 1 ACCEPTED, hoisted out as data. Set ONLY when
        // `heic_stop_is_acceptable` passed, and it carries the size `GetClosestSize` actually
        // answered — not a re-derivation of our own ask. Rung 2 is gated on this rather than on
        // "rung 1 declined for any reason at all", which is what used to route the two DELIBERATE
        // correctness declines (no decode-at-scale / the stop does not cover) into a Fant resample
        // at a size the transform had just refused.
        let mut approved_stop: Option<(u32, u32)> = None;
        let xform =
            if want_scale && div > 1 { frame.cast::<IWICBitmapSourceTransform>().ok() } else { None };
        if let Some(xf) = &xform {
            let (mut sw, mut sh) = ((w / div).max(1), (h / div).max(1));
            // Accept only an answer that is a REAL saving (smaller than native) and still COVERS
            // the ask (never softer than v0.8.100's frame). A codec whose ladder differs from ours
            // answers with its own nearest size, and these two tests are what make that safe.
            if xf.GetClosestSize(&mut sw, &mut sh).is_ok()
                && heic_stop_is_acceptable((sw, sh), (w, h), target_long)
            {
                approved_stop = Some((sw, sh));
                // Ask the transform which pixel format it can hand us; usually not RGB24, so wrap
                // whatever it gives in a WIC bitmap and run the SAME converter as the full path.
                let mut fmt = GUID_WICPixelFormat24bppRGB;
                let bpp = xf.GetClosestPixelFormat(&mut fmt).ok().and_then(|()| {
                    factory
                        .CreateComponentInfo(&fmt)
                        .ok()?
                        .cast::<IWICPixelFormatInfo>()
                        .ok()?
                        .GetBitsPerPixel()
                        .ok()
                });
                if let Some(bpp) = bpp.filter(|b| *b > 0 && b % 8 == 0) {
                    // v0.8.152 (R3-L8), rung 1's own transform buffer: same guard, wider multiplier
                    // — `bpp` comes from `GetBitsPerPixel` and can be up to 128, so this stride can
                    // overflow u32 at a quarter of the width the RGB24 doors need.
                    if (sw as u64) * (bpp as u64 / 8) > u32::MAX as u64 {
                        bail!("WIC row stride overflows u32 ({sw} px wide at {bpp} bpp)");
                    }
                    let stride = sw as usize * (bpp as usize / 8);
                    let mut raw = vec![0u8; stride * sh as usize];
                    if xf
                        .CopyPixels(
                            std::ptr::null(),
                            sw,
                            sh,
                            &fmt,
                            WICBitmapTransformRotate0,
                            stride as u32,
                            &mut raw,
                        )
                        .is_ok()
                    {
                        if let Some(s) = factory
                            .CreateBitmapFromMemory(sw, sh, &fmt, stride as u32, &raw)
                            .ok()
                            .and_then(|bmp| bmp.cast::<IWICBitmapSource>().ok())
                        {
                            note_once(
                                "s2-heic",
                                format!(
                                    "heic S2: WIC decode-at-scale engaged — {w}x{h} decoded at \
                                     the 1/{div} stop {sw}x{sh} for a {target_long} px ask, then \
                                     Lanczos to {nw}x{nh} (IWICBitmapSourceTransform)"
                                ),
                            );
                            source = Some((s, sw, sh));
                        }
                    }
                }
            }
        }

        // ── rung 2: WIC's own scaler, asked for the SAME stop ──
        // Reachable only when rung 1 APPROVED a stop but could not drive `CopyPixels` itself (an
        // exotic pixel format, a failed `CreateBitmapFromMemory`). v0.8.102 (F4/F5): the gate is
        // `approved_stop`, not `xform.is_some()` — the transform merely EXISTING says nothing about
        // whether the codec has a usable stop, and every other decline now falls to rung 3, which is
        // what this file's ladder doc and the `s2-heic-fallback` note have always promised.
        // Targeting the APPROVED stop (not `nw`×`nh`, and not a re-derived `w/div`) is what keeps the
        // claim below true: at a size the transform supports the scaler interpolates nothing, so this
        // is a second door to the same pixels rather than a second resampler quietly substituting for
        // our Lanczos.
        if let (None, Some((sw, sh))) = (&source, approved_stop) {
            if let Ok(scaler) = factory.CreateBitmapScaler() {
                if scaler.Initialize(&frame, sw, sh, WICBitmapInterpolationModeFant).is_ok() {
                    if let Ok(s) = scaler.cast::<IWICBitmapSource>() {
                        note_once(
                            "s2-heic-scaler",
                            format!(
                                "heic S2: the transform's direct CopyPixels was unavailable — \
                                 reaching the 1/{div} stop {sw}x{sh} through IWICBitmapScaler \
                                 instead"
                            ),
                        );
                        source = Some((s, sw, sh));
                    }
                }
            }
        }

        // ── rung 3: the v0.8.100 path, verbatim ──
        let (src, ow, oh) = match source {
            Some(s) => s,
            None => {
                if want_scale {
                    note_once(
                        "s2-heic-fallback",
                        format!(
                            "heic S2: no power-of-two stop covers a {target_long} px ask from a \
                             {w}x{h} master (or this codec has no decode-at-scale) — full decode \
                             + Lanczos, the pre-v0.8.101 path"
                        ),
                    );
                }
                (frame.cast::<IWICBitmapSource>()?, w, h)
            }
        };

        let converter = factory.CreateFormatConverter().context("WIC format converter")?;
        converter
            .Initialize(
                &src,
                &GUID_WICPixelFormat24bppRGB,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .context("WIC: convert to RGB24")?;
        if (ow as u64) * 3 > u32::MAX as u64 {
            bail!("WIC row stride overflows u32 ({ow} px wide)");
        }
        let stride = ow as usize * 3;
        let mut buf = vec![0u8; stride * oh as usize];
        converter
            .CopyPixels(std::ptr::null(), stride as u32, &mut buf)
            .context("WIC: CopyPixels")?;
        // Land on EXACTLY the dims the old path produced. Almost always a no-op (rung 2 asked for
        // them; rung 3 returns native and the caller's `finish_source` does the shrink); rung 1 can
        // land a few pixels off when the codec rounds its own way, and that is the one case this
        // trims — cheap, because it is trimming a target-sized buffer, not a 48 MP one. Trimming to
        // the EXACT pair (not just the long side) is what makes the frame-size equality total: a
        // long-side-only clamp would let a 2881×2161 answer settle at 2880×2159, one row short of
        // what v0.8.100 produced, and "the tile is one pixel different" is exactly the kind of
        // drift that turns into an unexplainable cache/layout bug three rounds later.
        if (ow, oh) != (w, h) {
            return resize_rgb_exact(buf, ow, oh, nw, nh);
        }
        Ok((buf, ow, oh))
    }
}

/// v0.8.103 (V7): **would rung 1 of [`wic_decode_rgb24_scaled`] actually run for this file at this
/// target?** — asked without decoding a pixel, so a test can tell "the ladder engaged" apart from
/// "the arithmetic picked a stop".
///
/// The distinction is not academic. [`heic_stop_divisor`] is pure arithmetic on the master's header
/// dims: it says a 1/4 stop COVERS a 1440 px ask from an 8064 px master, and it says that on every
/// machine on earth. Whether the installed codec can decode at that stop is a separate question, and
/// the v0.8.99 probe found a real configuration where the answer is no — the transform interface
/// exists but `GetClosestSize` answers with the NATIVE size, which [`heic_stop_is_acceptable`]
/// (rightly) refuses, sending the decode to rung 3, the plain full decode. On such a box a scaled
/// frame is bit-identical to a full decode for every row, and a test that asserted otherwise would be
/// reporting the absence of a codec capability as a Falcon defect — the exact thing `tests/heic.rs`'s
/// opening doctrine forbids.
///
/// Asks rung 1's own two questions through the SAME two pure predicates the ladder uses, so it cannot
/// drift into a second rule: is there a stop worth taking (`heic_stop_divisor > 1` on a frame bigger
/// than the ask), and does `GetClosestSize`'s answer pass `heic_stop_is_acceptable`. `false` for any
/// COM failure, any absent codec, and every non-Windows target.
///
/// A `true` here does NOT promise rung 1 specifically — rung 2 (the scaler at the same approved stop)
/// is the fallback when the transform's own `CopyPixels` cannot be driven, and it produces scaled
/// pixels too. Both differ from a full decode; only rung 3 is bit-identical. That is exactly the
/// distinction a caller wants.
#[cfg(windows)]
pub fn heic_decode_at_scale_engages(path: &Path, target_long: u32) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, IWICBitmapSourceTransform, IWICImagingFactory,
        WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory =
            match CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) {
                Ok(f) => f,
                Err(_) => return false,
            };
        let wpath: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let Ok(decoder) = factory.CreateDecoderFromFilename(
            PCWSTR(wpath.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        ) else {
            return false;
        };
        let Ok(frame) = decoder.GetFrame(0) else { return false };
        let (mut w, mut h) = (0u32, 0u32);
        if frame.GetSize(&mut w, &mut h).is_err() || w == 0 || h == 0 {
            return false;
        }
        let div = heic_stop_divisor(w.max(h), target_long);
        if !(w.max(h) > target_long && div > 1) {
            return false; // no stop to take — rung 3 is the CORRECT answer, not a decline
        }
        let Ok(xf) = frame.cast::<IWICBitmapSourceTransform>() else { return false };
        let (mut sw, mut sh) = ((w / div).max(1), (h / div).max(1));
        xf.GetClosestSize(&mut sw, &mut sh).is_ok()
            && heic_stop_is_acceptable((sw, sh), (w, h), target_long)
    }
}
#[cfg(not(windows))]
pub fn heic_decode_at_scale_engages(_path: &Path, _target_long: u32) -> bool {
    false // no WIC ladder off Windows; the Mac arm's scale-through lives in `decode_heic` itself
}

/// v0.8.71 (Round B assoc): is a WIC HEIF/HEIC decoder installed? READ-ONLY registry-of-codecs
/// probe — enumerates the installed WIC decoders (`CreateComponentEnumerator(WICDecoder)`) and looks
/// for one whose container format is `GUID_ContainerFormatHeif`. No file is opened, nothing is
/// decoded, so it is boot-safe and cheap (a handful of COM calls over the codec registry). Matching
/// on the CONTAINER GUID (not `CLSID_WICHeifDecoder`) covers any HEIF decoder implementation, not
/// just Microsoft's Store-delivered "HEVC/HEIF Image Extensions". Result is cached for the session
/// (`OnceLock`) — a codec installed mid-session enables on the next launch (accepted; the Settings
/// HEIC association row is the consumer). Any COM failure degrades to `false` (row disabled), never
/// a panic — the same posture as a genuinely absent codec.
///
/// THREADING (the RPC_E_CHANGED_MODE trap, caught by the v0.8.71 boot-verify): the probe runs on its
/// OWN throwaway thread. Its `CoInitializeEx(COINIT_MULTITHREADED)` is per-thread — issued on the
/// app's MAIN thread it poisons the apartment mode, and winit's later `OleInitialize` (STA, for
/// drag-drop) on that same thread panics with RPC_E_CHANGED_MODE. `wic_decode_rgb24` never hits this
/// because it only runs on decode-worker threads; this probe is called from boot (main thread), so
/// the thread isolation is mandatory, not hygiene. One-time cost (OnceLock), a few ms.
#[cfg(windows)]
pub fn wic_heif_codec_present() -> bool {
    use std::sync::OnceLock;
    static PRESENT: OnceLock<bool> = OnceLock::new();
    *PRESENT.get_or_init(|| {
        std::thread::spawn(|| unsafe { wic_heif_probe().unwrap_or(false) })
            .join()
            .unwrap_or(false)
    })
}
#[cfg(windows)]
unsafe fn wic_heif_probe() -> Option<bool> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_ContainerFormatHeif, IWICBitmapDecoderInfo,
        IWICImagingFactory, WICComponentEnumerateDefault, WICDecoder,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    // COM init: idempotent (S_FALSE if this thread is already initialised) — the wic_decode_rgb24 rule.
    // No matching CoUninitialize, by the same deliberate codebase-wide pattern documented at the
    // wic_decode_rgb24 site above (see ~line 3036): the process keeps its MTA alive for its whole
    // lifetime via the decode workers, and an explicit CoUninitialize here — even on this throwaway
    // probe thread — risks tearing down the last MTA reference and forcing an apartment teardown/
    // recreate cycle. Letting the thread simply exit drops its own init count without that risk.
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
    let enumerator = factory
        .CreateComponentEnumerator(WICDecoder.0 as u32, WICComponentEnumerateDefault.0 as u32)
        .ok()?;
    loop {
        let mut item: [Option<windows::core::IUnknown>; 1] = [None];
        let mut fetched = 0u32;
        // S_FALSE (fetched == 0) = end of the enumeration; a hard error also ends the walk.
        if enumerator.Next(&mut item, Some(&mut fetched)).is_err() || fetched == 0 {
            return Some(false);
        }
        let Some(unk) = item[0].take() else { return Some(false) };
        // Every WICDecoder item is an IWICBitmapDecoderInfo; a failed cast is skipped defensively.
        let Ok(info) = unk.cast::<IWICBitmapDecoderInfo>() else { continue };
        if info.GetContainerFormat() == Ok(GUID_ContainerFormatHeif) {
            return Some(true);
        }
    }
}
/// Non-Windows stub: HEIC availability is not WIC's concern there (macOS Image I/O decodes HEIC
/// natively — the caller's platform seam short-circuits before this).
#[cfg(not(windows))]
pub fn wic_heif_codec_present() -> bool {
    false
}

// ───────────────────── v0.8.99 (H1): the HEIC codec CAPABILITY probe ─────────────────────
// One-shot, read-only, decode-path-neutral. It answers the ONE question the HEIC strategy round
// turns on: can the installed HEIF codec give us a cheap small image, and by which door?
//
//   * `IWICBitmapSourceTransform::GetClosestSize(w/8, h/8)` — the codec's own answer to "what is the
//     smallest thing you can decode me directly?". A codec that supports decode-at-scale answers
//     with something NEAR w/8; one that does not answers with the full frame size (or does not
//     expose the interface at all). That decides strategy S2 (thread `scale_to` through the HEIC
//     arm — the ledger's N16) — the browse-lane lever that is currently a pure no-op for HEIC.
//   * `GetThumbnail()` — every iPhone HEIC carries an embedded preview, and Falcon has never read
//     one. A frame here decides strategy S1 (an embedded-preview fast lane, which would serve the
//     whole 256 px thumb tier for ~free and kill the frost-feeder multiplier).
//
// NOTHING here touches the decode path: it opens its own decoder, reads sizes, and returns data.
// Every failure mode lands in `err` and the caller still gets a line.

/// What the installed WIC HEIF codec can do — the payload of the one-shot H1 probe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeicProbe {
    /// `wic_heif_codec_present()` — is a HEIF container decoder registered at all?
    pub codec_present: bool,
    /// The frame's native size, for scale (`None` = the file would not open).
    pub frame: Option<(u32, u32)>,
    /// `GetClosestSize(w/8, h/8)` as the codec answered it. `None` = the frame does not expose
    /// `IWICBitmapSourceTransform`, or the call failed. **A `Some` that equals `frame` means the
    /// interface exists but the codec cannot actually decode at scale** — read the two together.
    pub scaled: Option<(u32, u32)>,
    /// The embedded thumbnail's size, if the codec surfaces one.
    pub thumb: Option<(u32, u32)>,
    /// Which door produced `thumb`: `"frame"` (IWICBitmapFrameDecode) or `"decoder"` (container).
    pub thumb_via: &'static str,
    /// The first thing that went wrong, if anything. Never a reason to skip the line.
    pub err: Option<String>,
}

/// Render the probe as ONE log line. Pure — platform-neutral, unit-tested on Windows, and the only
/// place the wire format lives (the strategy round reads this line verbatim).
pub fn heic_probe_line(p: &HeicProbe, file: &str) -> String {
    let st = match p.scaled {
        Some((w, h)) => format!("yes {w}x{h}"),
        None => "no".to_string(),
    };
    let th = match p.thumb {
        Some((w, h)) => format!("yes {w}x{h} via={}", p.thumb_via),
        None => "none".to_string(),
    };
    let fr = match p.frame {
        Some((w, h)) => format!("{w}x{h}"),
        None => "?".to_string(),
    };
    let err = match &p.err {
        Some(e) => format!(" err={e}"),
        None => String::new(),
    };
    format!(
        "heic probe: sourcetransform={st} thumbnail={th} codec={} frame={fr}{err} ({file})",
        if p.codec_present { "present" } else { "missing" }
    )
}

/// Open `path` with WIC and ask the two capability questions. Windows-only; fail-soft in every arm
/// (a COM error fills `err` and returns what it learned so far — never panics, never `?`s out
/// without a line, and never touches the shared decode path). Call it OFF the main thread: like
/// every other WIC entry point here it issues `CoInitializeEx(COINIT_MULTITHREADED)`, which would
/// poison winit's STA `OleInitialize` if run on the UI thread (the RPC_E_CHANGED_MODE trap).
#[cfg(windows)]
pub fn probe_heic_codec(path: &Path) -> HeicProbe {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, IWICBitmapSource, IWICBitmapSourceTransform, IWICImagingFactory,
        WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    let mut out = HeicProbe { codec_present: wic_heif_codec_present(), ..Default::default() };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory =
            match CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) {
                Ok(f) => f,
                Err(e) => {
                    out.err = Some(format!("factory {e:?}"));
                    return out;
                }
            };
        let wpath: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let decoder = match factory.CreateDecoderFromFilename(
            PCWSTR(wpath.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        ) {
            Ok(d) => d,
            Err(e) => {
                out.err = Some(format!("open {e:?}"));
                return out;
            }
        };
        let frame = match decoder.GetFrame(0) {
            Ok(f) => f,
            Err(e) => {
                out.err = Some(format!("frame0 {e:?}"));
                return out;
            }
        };
        let (mut w, mut h) = (0u32, 0u32);
        match frame.GetSize(&mut w, &mut h) {
            Ok(()) => out.frame = Some((w, h)),
            Err(e) => out.err = Some(format!("size {e:?}")),
        }
        // Q1 — decode-at-scale. `GetClosestSize` is an in/out pair: we ask for w/8 × h/8 and the
        // codec rewrites it to the nearest size it can actually produce.
        match frame.cast::<IWICBitmapSourceTransform>() {
            Ok(xf) => {
                let (mut sw, mut sh) = ((w / 8).max(1), (h / 8).max(1));
                match xf.GetClosestSize(&mut sw, &mut sh) {
                    Ok(()) => out.scaled = Some((sw, sh)),
                    Err(e) => {
                        out.err.get_or_insert(format!("closestsize {e:?}"));
                    }
                }
            }
            Err(_) => {} // no IWICBitmapSourceTransform at all — `scaled: None` says exactly that
        }
        // Q2 — the embedded preview. The per-FRAME thumbnail is the one an iPhone HEIC carries;
        // the container-level one is the fallback door some codecs use instead.
        let thumb: Option<(IWICBitmapSource, &'static str)> = frame
            .GetThumbnail()
            .ok()
            .map(|t| (t, "frame"))
            .or_else(|| decoder.GetThumbnail().ok().map(|t| (t, "decoder")));
        if let Some((t, via)) = thumb {
            let (mut tw, mut th) = (0u32, 0u32);
            if t.GetSize(&mut tw, &mut th).is_ok() {
                out.thumb = Some((tw, th));
                out.thumb_via = via;
            } else {
                out.err.get_or_insert("thumb size".to_string());
            }
        }
    }
    out
}

/// Non-Windows: the probe is about WIC's codec registry, which does not exist there. macOS reads
/// HEIC through Image I/O with no install gate, so there is nothing to discover.
#[cfg(not(windows))]
pub fn probe_heic_codec(_path: &Path) -> HeicProbe {
    HeicProbe { err: Some("not windows".to_string()), ..Default::default() }
}

#[cfg(test)]
mod heic_probe_tests {
    use super::{heic_probe_line, heic_scan_kind, is_heic_path, kind_tag, HeicProbe, SrcKind};
    use std::path::Path;

    /// v0.8.99 (H3a): the format tag every log line names a source by. `JPG` is BYTE-PINNED — the
    /// detail tier's `"JPG / CPU"` / `"JPG via nvJPEG"` strings, the testkit MANIFEST's documented
    /// boot expectations and every historical falcon.log read that exact token.
    ///
    /// FALSIFIER (L28): return `"JPEG"` for `SrcKind::Jpeg` and the pin fails (and the testkit's
    /// `(JPG / CPU)` boot assertion would silently stop matching); return the same tag for two
    /// different kinds and the distinctness row fails — decode-stats buckets would merge two
    /// formats into one number, which is precisely the confusion the tags exist to end.
    #[test]
    fn kind_tags_are_distinct_and_jpeg_is_byte_pinned() {
        assert_eq!(kind_tag(SrcKind::Jpeg), "JPG", "BYTE-PINNED: the historical detail-tier token");
        assert_eq!(kind_tag(SrcKind::Heic), "HEIC", "…and a HEIC finally says so");
        let all = [
            SrcKind::Jpeg,
            SrcKind::Png,
            SrcKind::Tiff,
            SrcKind::Webp,
            SrcKind::Heic,
            SrcKind::Jxl,
            SrcKind::Bmp,
            SrcKind::Gif,
            SrcKind::Unsupported,
        ];
        let mut tags: Vec<&str> = all.iter().map(|k| kind_tag(*k)).collect();
        let n = tags.len();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), n, "two SrcKinds share a tag — their decode-stats buckets would merge");
        assert!(tags.iter().all(|t| !t.is_empty() && t.len() <= 6), "tags stay short + non-empty");
    }

    /// v0.8.99 (H1/H3b): the two HEIC classification helpers.
    ///
    /// FALSIFIER (L28): drop the `to_ascii_lowercase` from `is_heic_path` and the SHOUTED-extension
    /// row fails — which is the common case, because that is exactly how a camera writes them
    /// (`IMG_2814.HEIC`), so the probe would never fire on real iPhone files. Invert
    /// `heic_scan_kind` and both of its rows fail.
    #[test]
    fn heic_is_recognised_by_extension_and_gated_by_the_codec() {
        assert!(is_heic_path(Path::new("C:/p/IMG_2814.HEIC")), "SHOUTED — how cameras write it");
        assert!(is_heic_path(Path::new("/p/img.heic")));
        assert!(is_heic_path(Path::new("/p/img.heif")));
        assert!(!is_heic_path(Path::new("/p/img.jpg")));
        assert!(!is_heic_path(Path::new("/p/heic")), "a bare stem is not an extension");
        assert!(!is_heic_path(Path::new("/p/noext")));

        assert_eq!(heic_scan_kind(true), SrcKind::Heic, "codec installed → the real decoder");
        assert_eq!(
            heic_scan_kind(false),
            SrcKind::Unsupported,
            "no codec → the HONEST unsupported card at SCAN time, not three tiers each failing a \
             decode and latching independently"
        );
    }

    /// v0.8.99 (H1): the probe LINE — the one artefact the S1-vs-S2 strategy decision is read off,
    /// so its wire format is pinned here and every state it can report has a row.
    ///
    /// FALSIFIER (L28): swap the `present`/`missing` words and the codec rows fail; render a
    /// `scaled: None` as `"yes"` (or omit the `no`) and the no-transform row fails; drop the `err=`
    /// suffix and the failure row fails — a COM error would then be indistinguishable from a codec
    /// that simply answered "no", which would send the strategy round the wrong way.
    #[test]
    fn the_probe_line_says_exactly_what_the_codec_answered() {
        // The S2-viable answer: the codec decodes at scale AND carries a preview.
        let both = HeicProbe {
            codec_present: true,
            frame: Some((8064, 6048)),
            scaled: Some((1008, 756)),
            thumb: Some((512, 384)),
            thumb_via: "frame",
            err: None,
        };
        assert_eq!(
            heic_probe_line(&both, "IMG_2814.HEIC"),
            "heic probe: sourcetransform=yes 1008x756 thumbnail=yes 512x384 via=frame \
             codec=present frame=8064x6048 (IMG_2814.HEIC)"
        );

        // The S1-only answer: no decode-at-scale interface, but a preview exists.
        let s1 = HeicProbe {
            codec_present: true,
            frame: Some((4032, 3024)),
            scaled: None,
            thumb: Some((320, 240)),
            thumb_via: "decoder",
            err: None,
        };
        assert_eq!(
            heic_probe_line(&s1, "a.heic"),
            "heic probe: sourcetransform=no thumbnail=yes 320x240 via=decoder codec=present \
             frame=4032x3024 (a.heic)"
        );

        // The bleak answer: neither door, though the codec is installed.
        let neither =
            HeicProbe { codec_present: true, frame: Some((4032, 3024)), ..Default::default() };
        assert_eq!(
            heic_probe_line(&neither, "a.heic"),
            "heic probe: sourcetransform=no thumbnail=none codec=present frame=4032x3024 (a.heic)"
        );

        // The codec-less machine, and the COM-failure arm: BOTH still produce a line.
        let missing = HeicProbe {
            codec_present: false,
            err: Some("open Error { code: 0x88982F50 }".to_string()),
            ..Default::default()
        };
        assert_eq!(
            heic_probe_line(&missing, "a.heic"),
            "heic probe: sourcetransform=no thumbnail=none codec=missing frame=? \
             err=open Error { code: 0x88982F50 } (a.heic)"
        );
        assert!(
            heic_probe_line(&missing, "a.heic").lines().count() == 1,
            "ONE line, always — the probe is grepped, not parsed"
        );
    }
}

/// P8: WIC fallback for TIFFs the pure-Rust `tiff` crate can't decode (CCITT G3/G4 fax compression,
/// 1-bit bilevel scans, old-JPEG-in-TIFF, YCbCr, exotic colour types) — the "Windows Photos /
/// QuickLook open it but we don't" class. Windows-only; other platforms keep the crate-only path.
#[cfg(windows)]
fn os_codec_decode_rgb(path: &Path) -> Result<(Vec<u8>, u32, u32)> {
    wic_decode_rgb24(path, "no OS codec for this file's compression/colour type")
}
// macOS (v0.9.9, MACOS_PORT §2): the exotic-TIFF fallback role WIC plays on Windows — CGImageSource
// also decodes the CCITT/YCbCr/CMYK/old-JPEG-in-TIFF cases the pure-Rust `tiff` crate rejects. Reached
// only when the crate path fails (see `decode_tiff_rgb`), so common TIFFs keep the crate decode.
#[cfg(target_os = "macos")]
fn os_codec_decode_rgb(path: &Path) -> Result<(Vec<u8>, u32, u32)> {
    imageio_decode_rgb(path, None)
}
#[cfg(all(not(windows), not(target_os = "macos")))]
fn os_codec_decode_rgb(_path: &Path) -> Result<(Vec<u8>, u32, u32)> {
    bail!("the OS-codec fallback requires WIC (Windows) or Image I/O (macOS)")
}

/// Cheap size probe via WIC (header read, no full decode) for the zoom/1:1 % readout — used for
/// HEIC and as the TIFF fallback (any WIC-openable file).
#[cfg(windows)]
fn wic_dimensions(path: &Path) -> Option<(u32, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, IWICImagingFactory, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let wpath: Vec<u16> =
            path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let decoder = factory
            .CreateDecoderFromFilename(
                PCWSTR(wpath.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 {
            None
        } else {
            Some((w, h))
        }
    }
}
// macOS (v0.9.9): the header-only size probe for HEIC + the exotic-TIFF fallback — CGImageSource's
// CopyPropertiesAtIndex (no pixel decode), the analogue of the Windows WIC GetSize probe.
#[cfg(target_os = "macos")]
fn wic_dimensions(path: &Path) -> Option<(u32, u32)> {
    imageio_dimensions(path)
}
#[cfg(all(not(windows), not(target_os = "macos")))]
fn wic_dimensions(_path: &Path) -> Option<(u32, u32)> {
    None
}

// ─────────────────────── macOS Image I/O decode (v0.9.9, MACOS_PORT §1/§2) ───────────────────────
// The Apple system image codec — the direct analogue of Windows WIC. `CGImageSource` hardware-decodes
// JPEG (Apple media engine), is the native HEIC decoder (macOS 10.13+), and covers the exotic-TIFF
// codec. Reached three ways, all sharing `imageio_decode_rgb`:
//   * the `ImageIODecoder` accel decoder (detail/ROI JPEG+HEIC, decode.rs) — scale-on-load,
//   * `decode_heic` — HEIC in every tier via CpuDecoder,
//   * `os_codec_decode_rgb` — the exotic-TIFF crate fallback (and, since v1.0.0-rc, R2's
//     truncated-JPEG second chance).
// Raw `#[link(framework)]` FFI (zero new Cargo deps), mirroring the v0.9.8 monitor-ICC arm's CF
// discipline: every Create/Copy result is released exactly once (a Get is borrowed, never released),
// every step fails closed (nil → Err/None, never a panic). CANNOT run on this Windows host — verified
// only by `cargo check --target aarch64-apple-darwin`; render behaviour is the CI-artifact tester's.

/// Pure ladder math for the Image I/O scale-on-load path (platform-neutral so it unit-tests on Windows
/// without Mac hardware). The `ImageDecoder` contract is "return ≥ `target_long`, the caller finishes
/// the downscale", but `kCGImageSourceThumbnailMaxPixelSize` yields ≤ its max — so from the probed
/// source long side pick the largest power-of-two subsample stop (`src / 2^n`) whose long side is still
/// ≥ `target_long`, and hand back `ceil(src / 2^n)` as the MaxPixelSize. That is the SMALLEST decode
/// still ≥ target (least work); the caller's Lanczos finish trims the residual overshoot to exact.
/// `None` ⇒ decode full-resolution (no thumbnail path): a native request (`target == 0`) or a target
/// already ≥ the source. Degenerate inputs (0 dims) also ⇒ `None`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn imageio_subsample_max_px(src_long: u32, target_long: u32) -> Option<u32> {
    if src_long == 0 || target_long == 0 || target_long >= src_long {
        return None;
    }
    // Grow the subsample factor (2^n) while the NEXT stop is still ≥ target — the largest factor / the
    // smallest decode that satisfies the ≥-target contract. The `n < 31` clamp keeps the shift total:
    // without it, `target == 1` with `src ≥ 2^31` would drive `n + 1` to 32, and `src >> 32` is a debug
    // panic / release wrong-result. Clamping only ever stops `n` growing EARLIER, i.e. returns a LARGER
    // MaxPixelSize — still ≥ target (the contract), and the caller's Lanczos finishes the downscale.
    // Unreachable via real callers (guard_source_dims caps ddim well under 2^31), but this fn must be total.
    let mut n = 0u32;
    while n < 31 && (src_long >> (n + 1)) >= target_long {
        n += 1;
    }
    Some(src_long.div_ceil(1u32 << n))
}

/// Which finished-image formats the macOS Image I/O accel decoder serves in the detail/ROI slot: JPEG
/// (Apple hardware decode) + HEIC (native). Everything else returns `Unsupported` so it falls through to
/// the CPU decoder — PNG/WebP/GIF/JXL/BMP keep their proven pure-Rust paths (identical cross-platform
/// colour), and TIFF stays on the crate→Image-I/O-fallback so every tier decodes it the SAME way (the
/// fast==detail parity invariant). Pure routing policy, split out so it unit-tests on Windows without
/// Mac hardware; `ImageIODecoder::serves` (decode.rs) is the sole caller.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn imageio_serves(kind: SrcKind) -> bool {
    matches!(kind, SrcKind::Jpeg | SrcKind::Heic)
}

#[cfg(target_os = "macos")]
use std::ffi::c_void;

#[cfg(target_os = "macos")]
#[repr(C)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[cfg(target_os = "macos")]
#[repr(C)]
struct CGSize {
    width: f64,
    height: f64,
}
#[cfg(target_os = "macos")]
#[repr(C)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

/// kCGImageAlphaPremultipliedLast (1) | kCGBitmapByteOrder32Big (4 << 12) → in-memory bytes [R,G,B,A].
#[cfg(target_os = "macos")]
const CG_BITMAP_RGBA8888: u32 = 1 | (4 << 12);
/// CFNumberType kCFNumberSInt64Type.
#[cfg(target_os = "macos")]
const CF_NUMBER_SINT64: i32 = 4;

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: *const c_void,
        buffer: *const u8,
        buf_len: isize,
        is_directory: u8,
    ) -> *const c_void; // CFURLRef (owned) or null
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void; // CFDictionaryRef (owned) or null
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void; // borrowed
    fn CFNumberCreate(allocator: *const c_void, the_type: i32, value_ptr: *const c_void)
        -> *const c_void; // CFNumberRef (owned) or null
    fn CFNumberGetValue(number: *const c_void, the_type: i32, value_ptr: *mut c_void) -> u8;
    static kCFBooleanTrue: *const c_void;
    static kCFBooleanFalse: *const c_void;
}

#[cfg(target_os = "macos")]
#[link(name = "ImageIO", kind = "framework")]
extern "C" {
    fn CGImageSourceCreateWithURL(url: *const c_void, options: *const c_void) -> *const c_void; // owned
    fn CGImageSourceCopyPropertiesAtIndex(
        src: *const c_void,
        index: isize,
        options: *const c_void,
    ) -> *const c_void; // CFDictionaryRef (owned) or null
    fn CGImageSourceCreateImageAtIndex(
        src: *const c_void,
        index: isize,
        options: *const c_void,
    ) -> *const c_void; // CGImageRef (owned) or null
    fn CGImageSourceCreateThumbnailAtIndex(
        src: *const c_void,
        index: isize,
        options: *const c_void,
    ) -> *const c_void; // CGImageRef (owned) or null
    static kCGImageSourceThumbnailMaxPixelSize: *const c_void;
    static kCGImageSourceCreateThumbnailFromImageAlways: *const c_void;
    /// v0.9.32 (S1): the OTHER thumbnail policy — return the file's EMBEDDED preview when it has
    /// one, and only synthesize from the master when it does not. `…Always` (above) is the
    /// opposite instruction and is what the v0.9.30 ladder wants; the two are never both set.
    static kCGImageSourceCreateThumbnailFromImageIfAbsent: *const c_void;
    static kCGImageSourceShouldCache: *const c_void;
    static kCGImageSourceCreateThumbnailWithTransform: *const c_void;
    static kCGImagePropertyPixelWidth: *const c_void;
    static kCGImagePropertyPixelHeight: *const c_void;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGImageGetWidth(image: *const c_void) -> usize;
    fn CGImageGetHeight(image: *const c_void) -> usize;
    fn CGImageGetColorSpace(image: *const c_void) -> *const c_void; // borrowed (Get) — never released
    fn CGColorSpaceCreateDeviceRGB() -> *const c_void; // owned or null
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *const c_void,
        bitmap_info: u32,
    ) -> *const c_void; // CGContextRef (owned) or null
    fn CGContextDrawImage(c: *const c_void, rect: CGRect, image: *const c_void);
}

/// Read a positive u32 from a CFDictionary CFNumber value (borrowed key + value; nothing released).
#[cfg(target_os = "macos")]
unsafe fn cf_dict_u32(dict: *const c_void, key: *const c_void) -> Option<u32> {
    let v = CFDictionaryGetValue(dict, key);
    if v.is_null() {
        return None;
    }
    let mut out: i64 = 0;
    let ok = CFNumberGetValue(v, CF_NUMBER_SINT64, &mut out as *mut i64 as *mut c_void);
    (ok != 0 && out > 0 && out <= u32::MAX as i64).then_some(out as u32)
}

/// Header-only pixel dims of a CGImageSource's frame 0 (no pixel decode). The source is borrowed.
#[cfg(target_os = "macos")]
unsafe fn imageio_source_dims(src: *const c_void) -> Option<(u32, u32)> {
    let props = CGImageSourceCopyPropertiesAtIndex(src, 0, std::ptr::null());
    if props.is_null() {
        return None;
    }
    let w = cf_dict_u32(props, kCGImagePropertyPixelWidth);
    let h = cf_dict_u32(props, kCGImagePropertyPixelHeight);
    CFRelease(props);
    match (w, h) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    }
}

/// Build a CFURL from a filesystem path (owned → the caller releases). `None` on an empty/oversized
/// path or allocation failure.
#[cfg(target_os = "macos")]
unsafe fn cfurl_for_path(path: &Path) -> Option<*const c_void> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() > isize::MAX as usize {
        return None;
    }
    let url =
        CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, 0);
    (!url.is_null()).then_some(url)
}

/// Header-only size probe via Image I/O (no pixel decode) — the macOS `wic_dimensions` analogue.
#[cfg(target_os = "macos")]
fn imageio_dimensions(path: &Path) -> Option<(u32, u32)> {
    unsafe {
        let url = cfurl_for_path(path)?;
        let src = CGImageSourceCreateWithURL(url, std::ptr::null());
        CFRelease(url);
        if src.is_null() {
            return None;
        }
        let dims = imageio_source_dims(src);
        CFRelease(src);
        dims.filter(|&(w, h)| w != 0 && h != 0)
    }
}

/// Decode a file via Image I/O to packed RGB8. `scale_to` requests scale-on-load (the ladder above
/// picks a subsample stop ≥ target; the caller finishes); `None` decodes full resolution.
///
/// Colour posture (the load-bearing point): the bitmap context is created in the CGImage's OWN colour
/// space, so drawing does NO colour conversion — the bytes handed back stay encoded in the source
/// profile, which the downstream `shot_source_gamut` reads from the FILE and Falcon's CM pass converts
/// (no double-conversion). A non-RGB source (CMYK/grey) has no RGBA context in its own space, so it
/// falls back to a device-RGB context — the one case where a conversion is unavoidable, matching WIC's
/// convert-to-RGB24 on Windows. Orientation stays with Falcon's EXIF pipeline: `WithTransform = false`,
/// and `CreateImageAtIndex` returns stored orientation, so nothing double-rotates.
#[cfg(target_os = "macos")]
fn imageio_decode_rgb(path: &Path, scale_to: Option<u32>) -> Result<(Vec<u8>, u32, u32)> {
    imageio_decode_rgb8_as(path, scale_to, false)
}

/// [`imageio_decode_rgb_as`] narrowed to packed RGB8 -- the shape every caller but round 35's ./export
/// PNG export wants, and the signature those callers had before the round.
#[cfg(target_os = "macos")]
fn imageio_decode_rgb8_as(
    path: &Path,
    scale_to: Option<u32>,
    embedded: bool,
) -> Result<(Vec<u8>, u32, u32)> {
    let (px, w, h) = imageio_decode_rgb_as(path, scale_to, embedded, Keep::NONE)?;
    Ok((px.into_rgb8()?, w, h))
}

/// [`imageio_decode_rgb`]'s ONE body, with the v0.9.32 (S1) policy switch.
///
/// `embedded` picks WHICH thumbnail Image I/O is being asked for, and it changes two things
/// together — they are not independent knobs:
///   * `false` (v0.9.30's H5 ladder) — `MaxPixelSize` = the largest power-of-two subsample stop
///     still ≥ the target, with `FromImageAlways`: never trust an embedded thumb's size, always
///     synthesize from the master. The caller's Lanczos then trims the stop to exact.
///   * `true` (S1) — `MaxPixelSize` = the target EXACTLY, with `FromImageIfAbsent`: hand back the
///     file's own preview (capped at the target) when it has one, and synthesize at that size when
///     it does not. Cheap either way; the caller decides whether what came back is good enough
///     (see `preview_is_usable`), because Image I/O will happily return a preview SMALLER than the
///     ask, which is the one outcome the thumb tier must refuse.
#[cfg(target_os = "macos")]
fn imageio_decode_rgb_as(
    path: &Path,
    scale_to: Option<u32>,
    embedded: bool,
    keep: Keep,
) -> Result<(Pixels, u32, u32)> {
    unsafe {
        let Some(url) = cfurl_for_path(path) else {
            bail!("ImageIO: could not build a CFURL for {}", path.display());
        };
        let src = CGImageSourceCreateWithURL(url, std::ptr::null());
        CFRelease(url);
        if src.is_null() {
            bail!("ImageIO: CGImageSourceCreateWithURL returned null for {}", path.display());
        }
        // Header dims first → bomb guard BEFORE any pixel-sized allocation (ImageIO has a live CVE
        // history), then the ladder decision.
        let (sw, sh) = match imageio_source_dims(src) {
            Some(d) => d,
            None => {
                CFRelease(src);
                bail!("ImageIO: no pixel dimensions in the source header for {}", path.display());
            }
        };
        if let Err(e) = guard_source_dims(sw, sh, "ImageIO source") {
            CFRelease(src);
            return Err(e);
        }
        let max_px = match (embedded, scale_to) {
            // S1: ask for exactly the tier target — the preview policy caps rather than ladders.
            (true, Some(t)) => (t > 0 && t < sw.max(sh)).then_some(t),
            (_, s) => s.and_then(|t| imageio_subsample_max_px(sw.max(sh), t)),
        };

        // Build the CGImage: a thumbnail (scale-on-load) when there is a MaxPixelSize, else a full
        // decode. Thumbnail options: the policy key chosen above, ShouldCache=false (Falcon owns
        // caching), WithTransform=false (orientation stays with Falcon's EXIF pipeline).
        let img = if let Some(mpx) = max_px {
            let mpx_val: i64 = mpx as i64;
            let num =
                CFNumberCreate(std::ptr::null(), CF_NUMBER_SINT64, &mpx_val as *const i64 as *const c_void);
            if num.is_null() {
                CFRelease(src);
                bail!("ImageIO: CFNumberCreate failed");
            }
            let keys: [*const c_void; 4] = [
                kCGImageSourceThumbnailMaxPixelSize,
                if embedded {
                    kCGImageSourceCreateThumbnailFromImageIfAbsent
                } else {
                    kCGImageSourceCreateThumbnailFromImageAlways
                },
                kCGImageSourceShouldCache,
                kCGImageSourceCreateThumbnailWithTransform,
            ];
            let values: [*const c_void; 4] = [num, kCFBooleanTrue, kCFBooleanFalse, kCFBooleanFalse];
            // NULL key/value callbacks: the dict stores raw (un-retained) pointers — safe because every
            // value outlives the single synchronous CGImageSource call below (the constants live forever;
            // `num` is released only after). This avoids needing the CFType callback statics.
            let opts = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                4,
                std::ptr::null(),
                std::ptr::null(),
            );
            let out = if opts.is_null() {
                std::ptr::null()
            } else {
                CGImageSourceCreateThumbnailAtIndex(src, 0, opts)
            };
            if !opts.is_null() {
                CFRelease(opts);
            }
            CFRelease(num);
            out
        } else {
            CGImageSourceCreateImageAtIndex(src, 0, std::ptr::null())
        };
        CFRelease(src);
        if img.is_null() {
            bail!("ImageIO: could not decode an image from {}", path.display());
        }

        // Actual decoded dims (a thumbnail is ≥ target by construction) — re-guard defensively.
        let w = CGImageGetWidth(img) as u32;
        let h = CGImageGetHeight(img) as u32;
        if let Err(e) = guard_source_dims(w, h, "ImageIO frame") {
            CFRelease(img);
            return Err(e);
        }
        // stride = w*4 explicit (deterministic, tight RGBA8888) — checked so a very wide strip can't
        // overflow usize before the alloc.
        let stride = match (w as usize).checked_mul(4) {
            Some(s) => s,
            None => {
                CFRelease(img);
                bail!("ImageIO: row stride overflow ({w} px wide)");
            }
        };
        let buf_len = match stride.checked_mul(h as usize) {
            Some(n) => n,
            None => {
                CFRelease(img);
                bail!("ImageIO: buffer size overflow ({w}x{h})");
            }
        };
        let mut buf = vec![0u8; buf_len];

        // Draw into the RGBA8 context. Prefer the image's own colour space (identity → no conversion);
        // fall back to device RGB only if that context can't be made (a non-RGB source).
        let img_space = CGImageGetColorSpace(img); // borrowed — never released
        let mut ctx = if img_space.is_null() {
            std::ptr::null()
        } else {
            CGBitmapContextCreate(
                buf.as_mut_ptr() as *mut c_void,
                w as usize,
                h as usize,
                8,
                stride,
                img_space,
                CG_BITMAP_RGBA8888,
            )
        };
        let mut device_space: *const c_void = std::ptr::null();
        if ctx.is_null() {
            device_space = CGColorSpaceCreateDeviceRGB();
            if !device_space.is_null() {
                ctx = CGBitmapContextCreate(
                    buf.as_mut_ptr() as *mut c_void,
                    w as usize,
                    h as usize,
                    8,
                    stride,
                    device_space,
                    CG_BITMAP_RGBA8888,
                );
            }
        }
        if ctx.is_null() {
            if !device_space.is_null() {
                CFRelease(device_space);
            }
            CFRelease(img);
            bail!("ImageIO: could not create an RGBA bitmap context ({w}x{h})");
        }
        let rect = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize { width: w as f64, height: h as f64 },
        };
        CGContextDrawImage(ctx, rect, img);
        CFRelease(ctx);
        if !device_space.is_null() {
            CFRelease(device_space);
        }
        CFRelease(img);

        // ROUND 35, the keep arm, taken BEFORE the flatten below (which is unchanged). The bitmap
        // context above is `kCGImageAlphaPremultipliedLast`, so STRAIGHT alpha is `c * 255 / a`,
        // and `a == 0` is fully transparent black rather than a division. Depth stays 8: the
        // 16-bpc context is stated residue, because nothing on the build host can execute it.
        if keep.alpha {
            let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
            for y in 0..h as usize {
                let row = &buf[y * stride..y * stride + w as usize * 4];
                for p in row.chunks_exact(4) {
                    let a = p[3];
                    if a == 0 {
                        rgba.extend_from_slice(&[0, 0, 0, 0]);
                    } else {
                        let un = |v: u8| ((v as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                        rgba.extend_from_slice(&[un(p[0]), un(p[1]), un(p[2]), a]);
                    }
                }
            }
            return Ok((Pixels::Rgba8(rgba), w, h));
        }
        // RGBA → packed RGB8, flattening any alpha over opaque white (premultiplied: straight = premult
        // + (255 - a); identity for opaque photos). Explicit stride so a padded row is still read right.
        let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
        for y in 0..h as usize {
            let row = &buf[y * stride..y * stride + w as usize * 4];
            for px in row.chunks_exact(4) {
                let inv = 255u16 - px[3] as u16;
                rgb.push((px[0] as u16 + inv).min(255) as u8);
                rgb.push((px[1] as u16 + inv).min(255) as u8);
                rgb.push((px[2] as u16 + inv).min(255) as u8);
            }
        }
        Ok((Pixels::Rgb8(rgb), w, h))
    }
}

/// v0.9.32 (S1, the Mac arm): the EMBEDDED-PREVIEW door, Image I/O edition — the structural twin of
/// Windows' [`wic_thumbnail_rgb24`], and the same three-guard contract.
///
/// The one platform difference worth stating: `CreateThumbnailFromImageIfAbsent` means the call
/// ALWAYS returns something usable — the file's own preview when it has one, a synthesized
/// at-target thumbnail when it does not. So unlike WIC there is no "no preview" decline; the
/// decline that matters here is a preview SMALLER than the tier target (Image I/O caps at
/// `MaxPixelSize` but will happily hand back less), and the aspect mismatch that would mean the
/// preview item carries a different `irot` from the master. Both are [`preview_is_usable`]'s job,
/// so the two platforms cannot drift.
///
/// Fail-soft in every arm (`None` ⇒ the caller decodes the main image), and one [`note_once`] line
/// per outcome so a Mac session's falcon.log says which door served its thumbs. v0.8.102 (F6/F10,
/// the Mac reading): "the main image" here means rung 3, `decode_heic` → `imageio_decode_rgb(path,
/// scale_to)` — which since v0.9.30 SCALE-THROUGHS on the subsample ladder rather than full-decoding,
/// so a declined Mac thumb is not the pre-v0.9.30 path either. Same picture, cheaper decode.
///
/// CANNOT run on the Windows dev box — verified by `cargo check --target aarch64-apple-darwin` and
/// by the shared pure guard's unit tests; the render behaviour is the CI-artifact tester's to
/// confirm (tester-doc item 45).
#[cfg(target_os = "macos")]
fn imageio_embedded_preview_rgb(path: &Path, min_long: u32) -> Option<(Vec<u8>, u32, u32)> {
    let (fw, fh) = imageio_dimensions(path)?;
    let (rgb, tw, th) = imageio_decode_rgb8_as(path, Some(min_long), true).ok()?;
    if !preview_is_usable(tw, th, fw, fh, min_long) {
        // v0.8.102 (F3, the Mac arm of the same fix): a decline is PER FILE — nothing latches, and
        // the next HEIC re-enters this door and can succeed. The old line said "this session",
        // which made one preview-less file read as S1 being off for the whole folder.
        // v0.8.103 (V11): keyed by WHICH rule refused, matching the Windows arm's key set exactly —
        // `s1-heic-decline-coverage` / `-aspect`, never a merged `-usable` that swallows the second
        // kind for the rest of the session.
        let reason = preview_decline_reason(tw, th, min_long);
        let why = if reason == "coverage" {
            format!("it is smaller than the {min_long} px tier")
        } else {
            format!("it does not match the master's {fw}x{fh} shape")
        };
        note_once(
            &format!("s1-heic-decline-{reason}"),
            format!(
                "heic thumb S1: Image I/O returned {tw}x{th} for a {min_long} px ask on a \
                 {fw}x{fh} master — {why}, so this file's tile came from the main-image decode \
                 instead. Per FILE, not a session-wide fallback: the next HEIC still tries the \
                 preview door."
            ),
        );
        return None;
    }
    note_once(
        "s1-heic",
        format!(
            "heic thumb S1: serving the {min_long} px tier from Image I/O's embedded-preview path \
             ({tw}x{th}, master {fw}x{fh}) — no full HEVC frame decoded"
        ),
    );
    Some((rgb, tw, th))
}

#[cfg(test)]
mod imageio_ladder_tests {
    use super::{
        imageio_serves, imageio_subsample_max_px, preview_decline_reason, preview_is_usable, SrcKind,
    };

    /// v0.8.101 (S1): the shared coverage + aspect guard, the one piece of the preview lane that
    /// runs identically on both platforms and can therefore be tested on either. It is the guard
    /// that makes "a 576 px preview never serves a 2880 px scrub frame" arithmetic rather than
    /// policy, and the one that catches a preview the codec handed back sideways.
    ///
    /// FALSIFIER (L28): drop the `tw.max(th) < min_long` term and the small-preview rows pass —
    /// the filmstrip would then show a 160 px tile where 256 was asked for. Drop the aspect term
    /// and the rotated row passes — a portrait shot's tile would arrive landscape, which the
    /// dims-only checks elsewhere cannot see.
    #[test]
    fn a_preview_must_cover_the_tier_and_match_the_master_shape() {
        // The real iPhone case: a 576×432 preview beside a 4032×3024 master, 256 px tier.
        assert!(preview_is_usable(576, 432, 4032, 3024, 256));
        // Portrait master, portrait preview.
        assert!(preview_is_usable(432, 576, 3024, 4032, 256));
        // Exactly the tier target still covers it.
        assert!(preview_is_usable(256, 192, 4032, 3024, 256));
        // …one pixel short does not.
        assert!(!preview_is_usable(255, 191, 4032, 3024, 256));
        // A camera that writes a tiny preview: refused, so the tile is decoded properly instead.
        assert!(!preview_is_usable(160, 120, 4032, 3024, 256));
        // The irot hazard: portrait master, LANDSCAPE preview. Ratio inverts → refused.
        assert!(!preview_is_usable(576, 432, 3024, 4032, 256));
        // A letterboxed 16:9 preview of a 4:3 master: refused.
        assert!(!preview_is_usable(576, 324, 4032, 3024, 256));
        // Rounding slack IS allowed — a 575×432 preview of a 4032×3024 master is the same picture.
        assert!(preview_is_usable(575, 432, 4032, 3024, 256));
        // Degenerate inputs never pass (no divide-by-zero, no "0×0 covers everything").
        assert!(!preview_is_usable(0, 0, 4032, 3024, 256));
        assert!(!preview_is_usable(576, 432, 0, 0, 256));
    }

    /// v0.8.103 (V11): the DECLINE KEY splits the shared predicate's two rules apart again, so a
    /// session that meets both kinds of unusable preview logs both lines instead of freezing on the
    /// first (`note_once` keys on the string and never clears it). The predicate itself is untouched
    /// — this only decides what a decline is CALLED, and it must agree with `preview_is_usable`
    /// about which rule actually fired.
    ///
    /// FALSIFIER (L28): swap the arms (report "aspect" when the preview is too small) and the
    /// degenerate `0x0` row below lands in "aspect", where its ratio is undefined — and a folder of
    /// tiny previews would log a shape complaint about files whose shape is fine. Merge them back
    /// into one key (the state this trunk was in) and the last two rows collapse to the same string,
    /// which is exactly the swallowed second reason.
    #[test]
    fn a_decline_is_named_for_the_rule_that_refused_it() {
        // Too small for the tier — coverage, whatever its shape.
        assert!(!preview_is_usable(160, 120, 4032, 3024, 256));
        assert_eq!(preview_decline_reason(160, 120, 256), "coverage");
        // Big enough, but the wrong SHAPE (the irot hazard: landscape preview, portrait master).
        assert!(!preview_is_usable(576, 432, 3024, 4032, 256));
        assert_eq!(preview_decline_reason(576, 432, 256), "aspect");
        // A letterboxed 16:9 preview of a 4:3 master covers the tier, so it is an aspect decline too.
        assert!(!preview_is_usable(576, 324, 4032, 3024, 256));
        assert_eq!(preview_decline_reason(576, 324, 256), "aspect");
        // Fails BOTH rules → coverage, matching the predicate's own test order.
        assert!(!preview_is_usable(160, 90, 4032, 3024, 256));
        assert_eq!(preview_decline_reason(160, 90, 256), "coverage");
        // Degenerate dims never reach the ratio arm.
        assert_eq!(preview_decline_reason(0, 0, 256), "coverage");
        // …and the two names are genuinely distinct, which is the whole point of the split.
        assert_ne!(preview_decline_reason(160, 120, 256), preview_decline_reason(576, 432, 256));
    }

    #[test]
    fn imageio_serves_only_jpeg_and_heic() {
        // The system codec serves the two formats where it is the win (JPEG hardware) or the only option
        // (HEIC); everything else must fall through to the CPU decoder by return value.
        assert!(imageio_serves(SrcKind::Jpeg));
        assert!(imageio_serves(SrcKind::Heic));
        for k in [
            SrcKind::Png,
            SrcKind::Tiff,
            SrcKind::Webp,
            SrcKind::Jxl,
            SrcKind::Bmp,
            SrcKind::Gif,
            SrcKind::Unsupported,
        ] {
            assert!(!imageio_serves(k), "{k:?} must fall through to CpuDecoder");
        }
    }

    #[test]
    fn native_and_upsize_requests_take_the_full_path() {
        // target 0 = native, and any target ≥ source → no thumbnail (decode full-resolution).
        assert_eq!(imageio_subsample_max_px(5472, 0), None);
        assert_eq!(imageio_subsample_max_px(5472, 5472), None);
        assert_eq!(imageio_subsample_max_px(5472, 9000), None);
        assert_eq!(imageio_subsample_max_px(0, 2048), None); // degenerate source
    }

    #[test]
    fn picks_the_smallest_stop_still_at_or_above_target() {
        // The spec's worked example: 5472 long, 2048 target → n=1 → MaxPixelSize 2736 (≥ 2048; the next
        // stop 1368 would drop below target).
        assert_eq!(imageio_subsample_max_px(5472, 2048), Some(2736));
        // 5472 / 4 = 1368 ≥ 1024, / 8 = 684 < 1024 → n=2 → 1368.
        assert_eq!(imageio_subsample_max_px(5472, 1024), Some(1368));
        // A target just under the source keeps n=0 (full via the thumbnail path is still ≥ target).
        assert_eq!(imageio_subsample_max_px(4000, 3999), Some(4000));
    }

    #[test]
    fn every_stop_is_within_one_subsample_of_target() {
        // Property: for assorted sources/targets, the chosen MaxPixelSize satisfies the contract (≥
        // target) without over-decoding — it lands in [target, 2·target], i.e. the SMALLEST subsample
        // stop still ≥ target (the next-finer stop would drop below). The upper bound is ≤ (not <) 2·tgt
        // because the `ceil` can round a `src/2` of x.5 up to exactly 2·tgt (e.g. src 65535, tgt 16384).
        for &src in &[513u32, 1000, 4000, 5472, 8192, 12000, 24568, 65535] {
            for &tgt in &[1u32, 100, 512, 1024, 2048, 4096, 8192, 16384] {
                if let Some(mpx) = imageio_subsample_max_px(src, tgt) {
                    assert!(mpx >= tgt, "ladder({src},{tgt}) = {mpx} must be ≥ target");
                    assert!(mpx <= src, "ladder({src},{tgt}) = {mpx} must be ≤ source");
                    assert!(mpx <= 2 * tgt, "ladder({src},{tgt}) = {mpx} over-decodes (> 2·target)");
                } else {
                    assert!(tgt == 0 || tgt >= src, "None only for native / target ≥ source");
                }
            }
        }
    }

    #[test]
    fn tiny_images_and_extreme_downscales() {
        assert_eq!(imageio_subsample_max_px(4, 1), Some(1)); // 4→2→1, n=2
        assert_eq!(imageio_subsample_max_px(2, 1), Some(1));
        assert_eq!(imageio_subsample_max_px(1, 1), None); // target ≥ source
        // A gigapixel-wide panorama down to a modest target still resolves without overflow.
        assert!(imageio_subsample_max_px(65535, 2048).unwrap() >= 2048);
    }

    #[test]
    fn extreme_source_target_one_stays_total() {
        // The shift-clamp pin: `target == 1` with a source ≥ 2^31 would, without the `n < 31` guard,
        // drive the loop's `src >> (n + 1)` to a `>> 32` — a debug panic / release wrong-result. The
        // function must be TOTAL for any u32 pair; the result must still satisfy the ≥-target contract.
        for &src in &[u32::MAX, u32::MAX - 1, 1u32 << 31, (1u32 << 31) + 1, (1u32 << 31) - 1] {
            let mpx = imageio_subsample_max_px(src, 1).expect("downscale to 1 has a stop");
            assert!(mpx >= 1, "ladder({src},1) = {mpx} must be ≥ target");
            assert!(mpx <= src, "ladder({src},1) = {mpx} must be ≤ source");
        }
        // A tiny target against a near-2^31 source resolves the same way (no overflow, contract holds).
        let mpx = imageio_subsample_max_px(u32::MAX, 2).expect("stop exists");
        // v1.0 MERGE: the upper half of this pair used to be spelled `mpx <= u32::MAX`, which is a
        // DENY-level clippy error (`absurd_extreme_comparisons`) and clippy is right — the source IS
        // `u32::MAX` on this line, so the ≤-source half of the contract is vacuous here and was
        // asserting nothing. The loop above pins it against five sources where it is not vacuous.
        // Pre-existing on `macos-prototype` and inherited whole by the merge; fixed here because it
        // fails `cargo clippy` outright, which would have left the v1.0 trunk unable to run the gate.
        assert!(mpx >= 2, "ladder(u32::MAX, 2) = {mpx} must be ≥ target");
    }
}

/// The colour-space description of a HEIC — the iPhone Display-P3 case (N2). The Windows HEIF codec
/// does NOT surface the profile through WIC's `GetColorContexts` (it returns 0), so read it straight
/// from the HEIF container's `colr` box: `nclx` → the colour primaries code (12/11 = Display P3, 9 =
/// Rec.2020, 1 = sRGB), or `prof`/`rICC` → an embedded ICC profile → its description. Scans a bounded
/// prefix (the `colr` box lives in the metadata before the media data); fails closed to `None` (→ sRGB).
/// The 'colr'+colour_type fourcc pair is specific enough that a false positive in real data is ~nil.
/// v0.8.140 (C2): the same box, plus the `prof`/`rICC` profile ITSELF when the answering box
/// carried one.
///
/// The WALK is byte-for-byte the one this door has always done, including which box answers: an
/// `nclx` box IS the file's answer even when its primaries code is unknown (a deliberate "the
/// container said something we don't model" → sRGB fallback), while an ICC box whose description
/// won't parse falls through to the next one. Widening that rule to "…or carried bytes" would let a
/// garbled `prof` box shadow a perfectly good `nclx` behind it — a regression, not an improvement.
///
/// v0.8.141 (R2) — THE ONE CASE THAT WIDENING WAS RIGHT ABOUT, AND ONLY THAT ONE. The unwidened
/// rule left this round's OWN defect class alive in this one door: a `prof` box carrying perfectly
/// good colorants under an absent or unreadable `desc` was discarded entirely, and the file fell to
/// sRGB — a wide-gamut HEIC rendered dull, silently, which is the whole round in miniature. But the
/// naive predicate — widening THIS rule to `… || colorants.is_some()` — is genuinely unsafe, and a
/// 20,000-trial fuzz over three real profiles measured why: 40-77% of profiles corrupted badly
/// enough to lose their `desc` still yield PARSEABLE colorants, so a garbled `prof` would answer,
/// miss τ, fall to sRGB and shadow a perfectly good `nclx` behind it. In the same 40,000 trials,
/// ZERO corrupted profiles landed WITHIN τ of a modeled gamut.
///
/// So the widening is STRICTLY ADDITIVE, in two passes, and it takes BOTH parts to be safe:
/// - THE STRUCTURE. Today's rule runs first and to COMPLETION, over every box the scan collected.
///   Every container that answered before gets the same box's answer — a descless `prof` followed
///   by an `nclx` still resolves to the `nclx`, including an `nclx` whose primaries code we do not
///   model, which has always been a deliberate answer of its own. Nothing in pass 2 can shadow an
///   `nclx`, because pass 2 only runs when pass 1 found nothing at all.
/// - THE PREDICATE. Within pass 2, a profile must actually PLACE, not merely parse. That is what
///   the fuzz's number is for: a container carrying a garbled descless `prof` AHEAD of a good
///   descless `prof` must resolve to the good one, and `colorants.is_some()` would hand the answer
///   to the garbled box and land on sRGB.
///
/// `heic_colr_scan`'s own `answered` flag is deliberately untouched: it governs the early exit, and
/// when nothing answers the scan already returns every `colr` box it found, which is what the second
/// pass needs.
fn heic_color_tag(path: &Path) -> ColorTag {
    let boxes = heic_colr_scan(path, true);
    for b in &boxes {
        if b.nclx || b.tag.desc.is_some() {
            return b.tag.clone();
        }
    }
    for b in &boxes {
        if colr_places_by_colorimetry(&b.tag) {
            return b.tag.clone();
        }
    }
    ColorTag::default()
}

/// v0.8.141 (R2): does this `colr` box carry a profile whose colorants actually PLACE within τ?
/// The strictly-additive widening's predicate, in one place so the master door and the preview
/// filter cannot drift apart. Deliberately NOT `icc.is_some()` and NOT `colorants.is_some()` — see
/// [`heic_color_tag`] for the fuzz measurement that rules both of those out.
fn colr_places_by_colorimetry(tag: &ColorTag) -> bool {
    tag.icc.as_deref().and_then(falcon_color::Gamut::from_icc_bytes).is_some()
}

/// One `colr` box as [`heic_colr_scan`] found it: whether it was the `nclx` form, and the colour
/// tag it yields (an all-`None` tag = present but not readable — an unknown primaries code, or an
/// ICC blob we could not parse).
struct ColrBox {
    nclx: bool,
    tag: ColorTag,
}

/// Walk a HEIF container's bounded prefix and collect its `colr` boxes IN FILE ORDER.
///
/// v0.8.105 (W5): the scan [`heic_color_tag`] has always done, split out so a second question can be
/// asked of the same bytes — "does this container declare ONE colour space, or several?" A HEIC holds
/// a master image AND (usually) an embedded preview item, and each can carry its own `colr`; the S1
/// preview door serves the preview's pixels while `shot_source_gamut` answers for the master, so the
/// two agreeing is a PRECONDITION of that door, not a detail. `stop_at_answer` reproduces the
/// original early-exit for the master-only question.
fn heic_colr_scan(path: &Path, stop_at_answer: bool) -> Vec<ColrBox> {
    use std::io::Read;
    let mut out: Vec<ColrBox> = Vec::new();
    let Ok(mut f) = std::fs::File::open(path) else { return out };
    let mut buf = vec![0u8; 256 * 1024];
    let Ok(n) = f.read(&mut buf) else { return out };
    buf.truncate(n);
    let mut i = 0usize;
    while i + 12 <= buf.len() {
        if &buf[i..i + 4] == b"colr" {
            let nclx = &buf[i + 4..i + 8] == b"nclx";
            let icc = matches!(&buf[i + 4..i + 8], b"prof" | b"rICC");
            if nclx || icc {
                // The body runs from the colour_type fourcc to the end of the box, whose size sits
                // in the 4 bytes BEFORE 'colr' (this is a byte SCAN, not a box walk).
                let size = if i >= 4 {
                    u32::from_be_bytes([buf[i - 4], buf[i - 3], buf[i - 2], buf[i - 1]]) as usize
                } else {
                    0
                };
                let end = (i.saturating_sub(4) + size).min(buf.len());
                let tag = if end > i + 4 { colr_tag(&buf[i + 4..end]) } else { ColorTag::default() };
                let answered = tag.desc.is_some();
                out.push(ColrBox { nclx, tag });
                if stop_at_answer && (nclx || answered) {
                    return out;
                }
            }
        }
        i += 1;
    }
    out
}

/// Every colour description a HEIF container declares, in file order — the master's and any
/// preview/thumbnail item's. Exported for the testkit row that pins them AGREEING on the real files
/// (v0.8.105 / W5); a file that ever returns two different strings is the file that would otherwise
/// have had its preview-sourced tile transformed with the wrong space.
pub fn heic_color_descs(path: &Path) -> Vec<String> {
    heic_colr_scan(path, false).into_iter().filter_map(|b| b.tag.desc).collect()
}

/// One `colr` box's BODY (everything after the 8-byte box header, i.e. starting at the colour_type
/// fourcc) → the colour tag it declares. An `nclx` box names its primaries by ENUMERATED CODE and
/// carries no profile, so it yields a name-only tag; a `prof`/`rICC` box yields the profile bytes
/// and whatever description they carry. An all-`None` tag = a form we don't read (an unknown `nclx`
/// code, or a box too short to hold anything).
fn colr_tag(body: &[u8]) -> ColorTag {
    if body.len() < 8 {
        return ColorTag::default();
    }
    match &body[0..4] {
        b"nclx" => {
            // colour_primaries(2 BE) transfer(2) matrix(2) full_range(1)
            match u16::from_be_bytes([body[4], body[5]]) {
                1 => ColorTag::named("sRGB"),
                9 => ColorTag::named("Rec. 2020"),
                11 | 12 => ColorTag::named("Display P3"),
                _ => ColorTag::default(), // unknown/absent → sRGB fallback in the caller
            }
        }
        b"prof" | b"rICC" => ColorTag::from_icc(body[4..].to_vec()),
        _ => ColorTag::default(),
    }
}

/// Children of an ISO-BMFF box: `(type, payload start, box end)` for each child in `[start, end)`.
/// Handles the 64-bit `largesize` and the "runs to the end" size-0 form; stops at the first
/// malformed header rather than guessing (every caller here is fail-soft).
///
/// POSTCONDITION every caller relies on: for each returned `(_, s, e)`, `start <= s <= e <= end`.
///
/// v0.8.106 (Round-A final check, L28): `size` is attacker-controlled — the 64-bit `largesize` form
/// lets a crafted file name any value up to `u64::MAX`, and the old bound test was `i + size > end`.
/// In DEBUG that addition PANICS on overflow; in RELEASE it WRAPS, and a wrapped sum ≤ `end` passed
/// the guard, after which `i += size` wrapped the cursor BACKWARDS (an unbounded re-walk, and with a
/// second crafted header a non-terminating one), `out` carried children whose `e` was less than their
/// `s` (every `e - s` length test at the call sites then underflow-panics, and `buf[s..e]` panics on
/// a reversed range), and `i + hdr` could index past the buffer. The worker's `catch_unwind` contains
/// the crash, but this function's own contract is fail-soft, and "contained by a panic handler" is
/// not fail-soft. Both bound tests are now overflow-free BY CONSTRUCTION — `size > end - i` never
/// adds (`i + 8 <= end` is the loop condition, so `end - i` cannot underflow), and `checked_add` is
/// the belt that also produces the cursor, so no arithmetic on `size` happens outside a checked form.
fn bmff_children(buf: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut i = start;
    while i + 8 <= end {
        let mut size = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        let typ = [buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]];
        let mut hdr = 8;
        if size == 1 {
            if i + 16 > end {
                break;
            }
            let mut v = [0u8; 8];
            v.copy_from_slice(&buf[i + 8..i + 16]);
            size = u64::from_be_bytes(v) as usize;
            hdr = 16;
        } else if size == 0 {
            size = end - i;
        }
        // `size > end - i` is the overflow-free spelling of the old `i + size > end`; `size < hdr`
        // keeps the payload start inside the box. Together they give the postcondition above.
        if size < hdr || size > end - i {
            break;
        }
        let Some(next) = i.checked_add(size) else { break }; // unreachable given the test above — kept as the belt
        out.push((typ, i + hdr, next));
        i = next;
    }
    out
}

/// An ISO-BMFF item ID at `at`: 32-bit when the enclosing FullBox's version says wide, else 16-bit.
/// Fail-soft — `None` when the buffer does not hold it.
///
/// v0.8.144 (E1): lifted verbatim out of [`heic_preview_colr`]'s local closure so the HEIF grid
/// parser reads item IDs through the same rule rather than a second one.
fn bmff_item_id(buf: &[u8], at: usize, wide: bool) -> Option<u32> {
    if wide {
        (at + 4 <= buf.len())
            .then(|| u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]))
    } else {
        (at + 2 <= buf.len()).then(|| u16::from_be_bytes([buf[at], buf[at + 1]]) as u32)
    }
}

/// One `ipma` box's entries: `(item_ID, the 1-based `ipco` property indices it associates)`, in
/// declaration order. `ps..pe` is the box's PAYLOAD span (its FullBox version/flags included), and
/// the caller has already checked it holds at least the 8 bytes of version/flags + entry_count.
///
/// v0.8.144 (E1) — THE EXTRACTION, and why it is not a refactor for tidiness. This code was written
/// inline in [`heic_preview_colr`] at v0.8.105, and E1 needs the identical reading for a different
/// question (which property is this tile's `hvcC`?). Two `ipma` readers thirty lines apart is the
/// shape a divergence bug grows in — one of them learns about version 1's 32-bit item IDs or the
/// flags bit-0 15-bit index form and the other does not, and the symptom is a photo whose colour
/// comes from one item and whose tile map comes from another. So there is one, and both call it.
///
/// The rules, unchanged from the v0.8.105 walk byte for byte: version ≥ 1 ⇒ 32-bit item IDs; flags
/// bit 0 ⇒ 15-bit property indices (2 bytes) rather than 7-bit (1 byte); the top bit of an index is
/// the ESSENTIAL flag and is masked off, because "must be understood to render" is a question about
/// the reader, not about which property is meant. Every bound test stops the walk rather than
/// guessing, so a truncated box yields the entries that were whole and nothing else.
fn bmff_ipma_entries(buf: &[u8], ps: usize, pe: usize) -> Vec<(u32, Vec<usize>)> {
    let mut out = Vec::new();
    if pe.saturating_sub(ps) < 8 || pe > buf.len() {
        return out;
    }
    let wide_id = buf[ps] >= 1; // version ≥ 1 ⇒ 32-bit item IDs
    let wide_ix = buf[ps + 3] & 1 == 1; // flags bit 0 ⇒ 15-bit property indices
    let n_entries = u32::from_be_bytes([buf[ps + 4], buf[ps + 5], buf[ps + 6], buf[ps + 7]]) as usize;
    let mut p = ps + 8;
    for _ in 0..n_entries {
        let Some(id) = bmff_item_id(buf, p, wide_id) else { break };
        p += if wide_id { 4 } else { 2 };
        if p >= pe {
            break;
        }
        let cnt = buf[p] as usize;
        p += 1;
        let step = if wide_ix { 2 } else { 1 };
        if p + cnt * step > pe {
            break;
        }
        let idxs: Vec<usize> = (0..cnt)
            .map(|k| {
                let at = p + k * step;
                if wide_ix {
                    (u16::from_be_bytes([buf[at], buf[at + 1]]) & 0x7fff) as usize
                } else {
                    (buf[at] & 0x7f) as usize
                }
            })
            .collect();
        p += cnt * step;
        out.push((id, idxs));
    }
    out
}

/// v0.8.105 (W5): the colour description the file's EMBEDDED PREVIEW declares FOR ITSELF.
///
/// `Lane::Thumb` is the only lane allowed to serve a tile from a HEIC's embedded preview, and since
/// v0.8.104 (C2) the thumb worker COLOUR-MANAGES that tile — with [`shot_source_gamut`], which reads
/// the MASTER's `colr`. A preview declaring a different space would then be converted with the wrong
/// source and ship visibly desaturated next to a correct stage, **on the shipped sRGB default**. The
/// served frame cannot be asked: `IWICBitmapFrameDecode::GetThumbnail` hands back a decoded bitmap,
/// not a taggable stream, so there is no APP2/colour context to read back off it. The CONTAINER can
/// be asked, and precisely — which is what this does.
///
/// The walk is the HEIF item model, no more of it than the question needs: `meta` → `pitm` (the
/// primary/master item) → `iref` `thmb` (the item that declares itself the primary's thumbnail) →
/// `iprp`/`ipma` (that item's property indices) → `iprp`/`ipco` (the ordered property boxes) → its
/// first `colr`. Fail-soft everywhere: a missing box, an unexpected version, an unreadable property
/// all yield `None`, and the caller then keeps the master's description exactly as before.
///
/// MEASURED on the 5-file iPhone testkit (2026-07-27): every file's `thmb` item associates the SAME
/// `colr` property index as its master (property 1, "Display P3"), while the container also carries
/// 2–4 OTHER `colr` boxes belonging to the HDR gain-map / tone-map auxiliary items ("sRGB
/// IEC61966-2.1 Linear", "Display P3 Linear", "Display P3 Primaries; PQ …"). So the assumption C2
/// rested on is true on these files — and "the first `colr` in the file" would have been a
/// coincidence, not a reading. `tests/heic.rs::the_embedded_preview_declares_the_masters_colour_space`
/// pins the agreement per file; `heic_preview_colour_comes_from_the_thumbnail_item` pins that this
/// really attributes (a synthetic container whose preview differs).
///
/// PREMISE, stated honestly: this assumes the platform decoder's preview door serves the `thmb`
/// item. The testkit's `thmb` sizes (576×432, 416×312) are exactly the preview sizes v0.8.99 measured
/// coming back from WIC, which is as close to a proof as a black-box API allows. Where the premise
/// could matter — a file whose `thmb` colr differs from its master's — the alternative is the
/// master's description, i.e. the pre-v0.8.105 behaviour, so this is never worse than not asking.
pub fn heic_preview_color_desc(path: &Path) -> Option<String> {
    heic_preview_color_tag(path).desc
}

/// [`heic_preview_color_desc`]'s bytes-carrying twin (v0.8.140 C2) — the preview item's own `colr`
/// box including a `prof` profile's bytes, so the preview door resolves by colorimetry too.
fn heic_preview_color_tag(path: &Path) -> ColorTag {
    heic_preview_colr(path).unwrap_or_default()
}

/// The preview item's `colr` walk itself — `Option`-shaped so every "the container doesn't say"
/// exit stays a single `?`, exactly as it was before the tag carried bytes.
fn heic_preview_colr(path: &Path) -> Option<ColorTag> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 256 * 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    // `meta` is a FullBox: 4 bytes of version+flags before its children.
    let meta = bmff_children(&buf, 0, buf.len())
        .into_iter()
        // v0.8.106 (L28): every child length test here is `saturating_sub`, so a reversed span can
        // only ever fail the test — never underflow-panic. `bmff_children` now guarantees `s <= e`;
        // this is the belt that keeps these call sites honest if that ever regresses.
        .find(|(t, s, e)| t == b"meta" && e.saturating_sub(*s) >= 4)
        .map(|(_, s, e)| (s + 4, e))?;
    let mut primary: Option<u32> = None;
    let mut ipco: Vec<([u8; 4], usize, usize)> = Vec::new();
    let mut ipma: Vec<(u32, Vec<usize>)> = Vec::new();
    let mut thmb: Vec<(u32, Vec<u32>)> = Vec::new();
    let read_id = |at: usize, wide: bool| -> Option<u32> { bmff_item_id(&buf, at, wide) };
    for (t, s, e) in bmff_children(&buf, meta.0, meta.1) {
        match &t {
            b"pitm" if e.saturating_sub(s) >= 6 => primary = read_id(s + 4, buf[s] >= 1),
            b"iref" if e.saturating_sub(s) >= 4 => {
                let wide = buf[s] >= 1;
                let w = if wide { 4 } else { 2 };
                for (rt, rs, re) in bmff_children(&buf, s + 4, e) {
                    if &rt != b"thmb" || re.saturating_sub(rs) < w + 2 {
                        continue;
                    }
                    let Some(from) = read_id(rs, wide) else { continue };
                    let count = u16::from_be_bytes([buf[rs + w], buf[rs + w + 1]]) as usize;
                    let to: Vec<u32> = (0..count)
                        .filter_map(|k| read_id(rs + w + 2 + k * w, wide))
                        .collect();
                    thmb.push((from, to));
                }
            }
            b"iprp" => {
                for (pt, ps, pe) in bmff_children(&buf, s, e) {
                    match &pt {
                        b"ipco" => ipco = bmff_children(&buf, ps, pe),
                        // v0.8.144 (E1): the walk that used to be spelled out here, now shared with
                        // the HEIF grid parser — see [`bmff_ipma_entries`] for why one reader.
                        b"ipma" if pe.saturating_sub(ps) >= 8 => {
                            ipma.extend(bmff_ipma_entries(&buf, ps, pe))
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let primary = primary?;
    // The preview is the item that declares ITSELF a thumbnail OF the primary.
    let item = thmb.iter().find(|(_, to)| to.contains(&primary)).map(|(f, _)| *f)?;
    let props = ipma.iter().find(|(id, _)| *id == item).map(|(_, p)| p)?;
    // Every `colr` property this item associates, in `ipma` order.
    let tags: Vec<ColorTag> = props
        .iter()
        .filter_map(|&ix| {
            let (t, s, e) = ipco.get(ix.checked_sub(1)?)?; // property indices are 1-based
            // v0.8.106 (L28): `*s..*e` PANICS on a reversed range, so the postcondition is checked
            // here rather than assumed — a malformed container must yield `None`, never unwind.
            (t == b"colr" && *e >= *s).then(|| colr_tag(&buf[*s..*e]))
        })
        .collect();
    // The `desc.is_some()` filter is the pre-v0.8.140 `colr_desc(..)?` rule kept intact: a property
    // we cannot name is not the preview's answer, and the master's stands instead. It runs FIRST and
    // over ALL of them, so every preview that answered before answers identically now.
    // v0.8.141 (R2): and only if none of them answers does a `prof` whose colorants PLACE get to —
    // the same strictly-additive widening, the same predicate, as the master door above.
    tags.iter()
        .find(|tag| tag.desc.is_some())
        .or_else(|| tags.iter().find(|tag| colr_places_by_colorimetry(tag)))
        .cloned()
}

/// v0.8.105 (W5): the source gamut to convert a SERVED browse frame FROM — the frame the tier is
/// actually holding, not the file's headline answer.
///
/// [`shot_source_gamut`] answers for the FILE (the master image), which is right for every lane but
/// one: `Lane::Thumb` may serve a HEIC tile from the container's embedded preview item, and that item
/// carries its own `colr` association. Preferring [`heic_preview_color_desc`] when the frame really
/// came through that door — and falling back to the master's description when the preview is
/// untagged, unreadable, or the format has no such door — is the whole rule.
pub fn frame_source_gamut(shot: &Shot, source: FrameSource) -> falcon_color::Gamut {
    if source == FrameSource::EmbeddedPreview && shot.kind == SrcKind::Heic {
        if let Some(path) = shot.jpg.as_deref() {
            // v0.8.140 (C2): the preview's own `colr` now resolves by COLORIMETRY too — its `prof`
            // box carries a profile like any other door. The fall-through rule is unchanged: only a
            // preview declaration that actually DECIDES overrides the master, so a preview that
            // says nothing (route `Fallback`) still defers, exactly as an unmatched name did.
            let tag = heic_preview_color_tag(path);
            let r = falcon_color::resolve_source_gamut(tag.icc.as_deref(), tag.desc.as_deref());
            if r.route != falcon_color::GamutRoute::Fallback {
                note_color_resolution(&tag, path, &r);
                return r.gamut;
            }
        }
    }
    shot_source_gamut(shot)
}

/// Decode a PNG. `EXPAND` promotes palette/low-bit-grey/tRNS to full channels and `STRIP_16`
/// reduces 16-bit to 8-bit, so under [`Keep::NONE`] the decoded buffer is always one of
/// Grey/GreyA/RGB/RGBA at 8-bit and the flattening match at the bottom is the shipped one, byte for
/// byte.
///
/// ROUND 35: `STRIP_16` is the ONE shipped statement this round makes conditional, because it is
/// set on the DECODER before the colour type is known and so cannot be branched around later. Under
/// `Keep::NONE` the value below is `EXPAND | STRIP_16` -- the same bits, spelled in two lines. The
/// keep arm then intercepts only a buffer that actually HOLDS something (16-bit samples, or an
/// alpha channel); an 8-bit RGB or grey PNG falls through to the shipped match with no copy,
/// because there is nothing in it to keep.
///
/// Palette + `tRNS` arrives here as `Rgba` at depth 8 through `EXPAND` (the `Indexed` arm stays
/// unreachable), so a transparent palette PNG keeps its transparency without a fifth arm.
fn decode_png_rgb(path: &Path, keep: Keep) -> Result<(Pixels, u32, u32)> {
    let file = std::fs::File::open(path)?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    let mut tr = png::Transformations::EXPAND;
    if !keep.depth {
        tr |= png::Transformations::STRIP_16;
    }
    dec.set_transformations(tr);
    let mut reader = dec.read_info()?;
    let (w0, h0) = {
        let i = reader.info();
        (i.width, i.height)
    };
    guard_source_dims(w0, h0, "PNG")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let deep = info.bit_depth == png::BitDepth::Sixteen;
    if keep.any() && (deep || matches!(info.color_type, png::ColorType::Rgba | png::ColorType::GrayscaleAlpha)) {
        // The WIDEST thing the file holds; `apply_keep` narrows it to what was actually asked for.
        let px = if deep {
            let d = be16_samples(&buf);
            match info.color_type {
                png::ColorType::Rgb => Pixels::Rgb16(d),
                png::ColorType::Rgba => Pixels::Rgba16(d),
                png::ColorType::Grayscale => Pixels::Rgb16(d.iter().flat_map(|&v| [v, v, v]).collect()),
                png::ColorType::GrayscaleAlpha => {
                    Pixels::Rgba16(d.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect())
                }
                png::ColorType::Indexed => bail!("indexed PNG not expanded"),
            }
        } else {
            match info.color_type {
                png::ColorType::Rgba => Pixels::Rgba8(buf),
                png::ColorType::GrayscaleAlpha => {
                    Pixels::Rgba8(buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect())
                }
                // The guard above admits only the two alpha types at 8 bits.
                other => bail!("internal: the PNG keep arm was reached for {other:?} at 8 bits"),
            }
        };
        return Ok((px, w, h));
    }
    let rgb = match info.color_type {
        png::ColorType::Rgb => buf,
        png::ColorType::Rgba => {
            buf.chunks_exact(4).flat_map(|p| [over_white(p[0], p[3]), over_white(p[1], p[3]), over_white(p[2], p[3])]).collect()
        }
        png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v]).collect(),
        png::ColorType::GrayscaleAlpha => {
            buf.chunks_exact(2).flat_map(|p| { let v = over_white(p[0], p[1]); [v, v, v] }).collect()
        }
        // EXPAND turns palette into RGB, so this arm is unreachable in practice.
        png::ColorType::Indexed => bail!("indexed PNG not expanded"),
    };
    Ok((Pixels::Rgb8(rgb), w, h))
}

/// Decode a WebP (packed RGB8 under `Keep::NONE`; RGBA8 kept for the PNG export since round 35) via the
/// pure-Rust `image-webp` crate — lossy VP8, lossless VP8L, or
/// the first frame of an animated WebP (`read_image` composites frame 0). Under `Keep::NONE` an RGBA image is flattened
/// over opaque white (photos are opaque; a stray transparent WebP stays legible), matching the
/// PNG/TIFF alpha handling. Guards the header dimensions against the adaptive source cap BEFORE
/// allocating the output buffer, so a crafted header can't force a huge alloc. (§48 #75.)
fn decode_webp_rgb(path: &Path, keep: Keep) -> Result<(Pixels, u32, u32)> {
    use image_webp::WebPDecoder;
    let file = std::fs::File::open(path)?;
    let mut dec = WebPDecoder::new(std::io::BufReader::new(file))
        .map_err(|e| anyhow::anyhow!("WebP header parse failed: {e}"))?;
    let (w, h) = dec.dimensions();
    guard_source_dims(w, h, "WebP")?;
    // output_buffer_size() = w*h*(3 or 4) per has_alpha() (fixed at header-parse time). None only on
    // usize overflow — impossible past the cap above, but handle it rather than unwrap.
    let size = dec.output_buffer_size().context("WebP dimensions overflow usize")?;
    let mut buf = vec![0u8; size];
    dec.read_image(&mut buf).map_err(|e| anyhow::anyhow!("WebP decode failed: {e}"))?;
    // ROUND 35: WebP is 8-bit by format, so the only thing there is to keep is the alpha channel,
    // and only when the file declared one at header-parse time.
    if keep.alpha && dec.has_alpha() {
        return Ok((Pixels::Rgba8(buf), w, h));
    }
    let rgb = if dec.has_alpha() {
        buf.chunks_exact(4)
            .flat_map(|p| [over_white(p[0], p[3]), over_white(p[1], p[3]), over_white(p[2], p[3])])
            .collect()
    } else {
        buf // already packed RGB8
    };
    Ok((Pixels::Rgb8(rgb), w, h))
}

/// The colour-space description of a WebP: its embedded ICCP profile (a Display P3 / Adobe RGB export
/// carries one) → its description, else `None` → the caller's sRGB fallback (WebP's assumed colour
/// space when no profile is present). Reads only the ICCP chunk — no pixel decode. (§48 #75, N2 family.)
/// v0.8.140 (C2): the `ICCP` chunk ITSELF, so the gamut is measured from it, not from its name.
fn webp_color_tag(path: &Path) -> ColorTag {
    webp_icc(path).map(ColorTag::from_icc).unwrap_or_default()
}

/// The `ICCP` chunk's bytes, under the same 4 MB DoS bound the description read has always used.
fn webp_icc(path: &Path) -> Option<Vec<u8>> {
    use image_webp::WebPDecoder;
    let file = std::fs::File::open(path).ok()?;
    let mut dec = WebPDecoder::new(std::io::BufReader::new(file)).ok()?;
    // DoS bound on the ICCP read: image-webp's default memory_limit is usize::MAX, and `icc_profile()`
    // allocates the ICCP chunk's DECLARED size — an unvalidated RIFF u32 that isn't clamped to the file
    // length — BEFORE reading it. So a ~70-byte crafted .webp claiming a 4 GB ICCP would force a multi-GB
    // alloc (a hard process abort on a memory-constrained machine) purely from gamut-probing during a
    // folder scan. Cap it at 4 MB, matching tiff_icc's ICC bound; any real display/working-space
    // profile is a few KB, so an over-cap profile fails closed to None → the sRGB fallback (graceful).
    // The pixel decode (decode_webp_rgb) is already dimension-guarded and streams its chunks, so this is
    // the only metadata path that needed bounding.
    dec.set_memory_limit(4_000_000);
    dec.icc_profile().ok().flatten()
}

/// Cheap WebP size probe (header parse via `image-webp`, no pixel decode) for the zoom/1:1 % readout.
fn webp_dimensions(path: &Path) -> Option<(u32, u32)> {
    use image_webp::WebPDecoder;
    let file = std::fs::File::open(path).ok()?;
    let (w, h) = WebPDecoder::new(std::io::BufReader::new(file)).ok()?.dimensions();
    (w != 0 && h != 0).then_some((w, h))
}

/// Decode a TIFF's first image -- packed RGB8 under `Keep::NONE`; its RGB(A)16 / RGBA8 / grey layouts are kept
/// for the PNG export since round 35. Fast path: the pure-Rust `tiff` crate (the common
/// photographer/scanner cases — 8/16-bit RGB(A), grey, naive CMYK). P8: anything the crate rejects
/// (CCITT G3/G4 fax, 1-bit bilevel scans, old-JPEG-in-TIFF, YCbCr, exotic depths) falls back to the
/// OS codec (WIC on Windows) instead of failing — the "Windows opens it but we didn't" class. A
/// genuinely corrupt TIFF fails both and surfaces as unsupported/corrupt (never a crash).
fn decode_tiff_rgb(path: &Path, keep: Keep) -> Result<(Pixels, u32, u32)> {
    match decode_tiff_rgb_crate(path, keep) {
        Ok(v) => Ok(v),
        // ROUND 35, stated residue (Q7 (ii)): the exotic fallback is the OS format converter, whose
        // target is 24bppRGB -- alpha and depth are gone before Falcon owns a byte. The pure-Rust
        // arm above covers every 8/16-bit RGB(A) TIFF, so what reaches here is bilevel/CMYK/odd,
        // which has neither to keep.
        Err(crate_err) => os_codec_decode_rgb(path)
            .map(|(rgb, w, h)| (Pixels::Rgb8(rgb), w, h))
            .map_err(|wic_err| {
                anyhow::anyhow!("TIFF unsupported by the Rust decoder ({crate_err}); OS codec also failed ({wic_err})")
            }),
    }
}

/// The pure-Rust `tiff`-crate decode (fast path for common TIFFs). Split out so `decode_tiff_rgb`
/// can fall back to the OS codec on any failure here.
fn decode_tiff_rgb_crate(path: &Path, keep: Keep) -> Result<(Pixels, u32, u32)> {
    use tiff::decoder::{Decoder, DecodingResult, Limits};
    use tiff::ColorType;
    let file = std::fs::File::open(path)?;
    // Our own guard_source_dims (below) replaces the crate's buffer cap so legitimately big
    // scans/panoramas decode, while a decompression-bomb header is still rejected up front.
    let mut dec = Decoder::new(std::io::BufReader::new(file))?.with_limits(Limits::unlimited());
    let (w, h) = dec.dimensions()?;
    guard_source_dims(w, h, "TIFF")?;
    let n = w as usize * h as usize;
    let ct = dec.colortype()?;
    let img = dec.read_image()?;
    // 16-bit samples → 8-bit by taking the high byte.
    let to8 = |v: &[u16]| -> Vec<u8> { v.iter().map(|&s| (s >> 8) as u8).collect() };
    // ROUND 35: the keep arm, taken BEFORE the flattening match below and only for the five colour
    // types that actually hold an alpha channel or more than 8 bits. RGB(8), Gray(8) and CMYK(8)
    // fall through to the shipped arms with no copy, because there is nothing in them to keep.
    if keep.any() {
        let kept = match (ct, &img) {
            (ColorType::RGB(16), DecodingResult::U16(b)) if b.len() >= n * 3 => {
                Some(Pixels::Rgb16(b[..n * 3].to_vec()))
            }
            (ColorType::RGBA(8), DecodingResult::U8(b)) if b.len() >= n * 4 => {
                Some(Pixels::Rgba8(b[..n * 4].to_vec()))
            }
            (ColorType::RGBA(16), DecodingResult::U16(b)) if b.len() >= n * 4 => {
                Some(Pixels::Rgba16(b[..n * 4].to_vec()))
            }
            (ColorType::Gray(16), DecodingResult::U16(b)) if b.len() >= n => {
                Some(Pixels::Rgb16(b[..n].iter().flat_map(|&v| [v, v, v]).collect()))
            }
            (ColorType::GrayA(8), DecodingResult::U8(b)) if b.len() >= n * 2 => Some(Pixels::Rgba8(
                b[..n * 2].chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
            )),
            _ => None,
        };
        if let Some(px) = kept {
            return Ok((px, w, h));
        }
    }
    // Each arm slices the decoded buffer to EXACTLY n pixels' worth first — a strip-padded buffer
    // (len > n·samples) would otherwise yield an over-long RGB vec that the resize/crop rejects.
    let rgb: Vec<u8> = match (ct, img) {
        (ColorType::RGB(8), DecodingResult::U8(mut b)) if b.len() >= n * 3 => { b.truncate(n * 3); b }
        (ColorType::RGB(16), DecodingResult::U16(b)) if b.len() >= n * 3 => to8(&b[..n * 3]),
        (ColorType::RGBA(8), DecodingResult::U8(b)) if b.len() >= n * 4 => {
            b[..n * 4].chunks_exact(4).flat_map(|p| [over_white(p[0], p[3]), over_white(p[1], p[3]), over_white(p[2], p[3])]).collect()
        }
        (ColorType::RGBA(16), DecodingResult::U16(b)) if b.len() >= n * 4 => {
            b[..n * 4].chunks_exact(4).flat_map(|p| { let a = (p[3] >> 8) as u8; [over_white((p[0] >> 8) as u8, a), over_white((p[1] >> 8) as u8, a), over_white((p[2] >> 8) as u8, a)] }).collect()
        }
        (ColorType::Gray(8), DecodingResult::U8(b)) if b.len() >= n => b[..n].iter().flat_map(|&v| [v, v, v]).collect(),
        (ColorType::Gray(16), DecodingResult::U16(b)) if b.len() >= n => to8(&b[..n]).iter().flat_map(|&v| [v, v, v]).collect(),
        (ColorType::GrayA(8), DecodingResult::U8(b)) if b.len() >= n * 2 => {
            b[..n * 2].chunks_exact(2).flat_map(|p| { let v = over_white(p[0], p[1]); [v, v, v] }).collect()
        }
        (ColorType::CMYK(8), DecodingResult::U8(b)) if b.len() >= n * 4 => {
            // Naive CMYK→RGB (no ICC): good enough to display a rare CMYK scan rather than fail it.
            b[..n * 4].chunks_exact(4).flat_map(|p| {
                let k = 255 - p[3] as u16;
                [((255 - p[0] as u16) * k / 255) as u8, ((255 - p[1] as u16) * k / 255) as u8, ((255 - p[2] as u16) * k / 255) as u8]
            }).collect()
        }
        (ct, _) => bail!("unsupported TIFF colour type/depth: {ct:?}"),
    };
    Ok((Pixels::Rgb8(rgb), w, h))
}

// ─────────────────────────────── JPEG XL (formats batch F1) ──────────────────────────────
// Pure-Rust decode via `jxl-oxide`. Two format-specific facts drive the wiring:
//  (a) ORIENTATION lives in the codestream header, not EXIF — jxl-oxide APPLIES it on render (its
//      `width()`/`height()` and the rendered stream are already oriented), so we DON'T route through
//      read_orientation/kamadak-exif (a `.jxl` carries no EXIF orientation to double-apply).
//  (b) the embedded ICC feeds the source-gamut path (see `jxl_color_tag`), mirroring `webp_color_tag`,
//      so a wide-gamut JXL renders correctly through the CM chain.
// HDR JXL (PQ/HLG transfer) is REFUSED with an honest reason — the HDR→SDR design round is deferred (as
// for AVIF); we never silently clip HDR to SDR.

/// Bounded whole-file read for a JXL decode. Generous enough for a lossless ~100 MP JXL (~200 MB) yet
/// bounds a crafted "huge" file (mirrors `jpeg_source`'s MAX_RAW_BYTES discipline).
const JXL_MAX_BYTES: u64 = 600_000_000;
/// Header-probe read for the cheap dims / colour-desc paths — the JXL image header (size + metadata +
/// embedded ICC) sits at the very start; 1 MB covers every real file. A larger header → probe returns
/// None (graceful; the full decode still reads the whole file).
const JXL_HEADER_PROBE_BYTES: usize = 1024 * 1024;

/// Parse a JXL image header from an in-memory prefix (or the whole file). Returns the initialized image
/// (header parsed, body possibly still to be fed) plus how many bytes the container parser consumed.
fn read_jxl_header(bytes: &[u8]) -> Result<(jxl_oxide::JxlImage, usize)> {
    use jxl_oxide::{InitializeResult, JxlImage};
    let mut uninit = JxlImage::builder().build_uninit();
    let consumed = uninit
        .feed_bytes(bytes)
        .map_err(|e| anyhow::anyhow!("JXL header read failed: {e}"))?;
    match uninit.try_init().map_err(|e| anyhow::anyhow!("JXL init failed: {e}"))? {
        InitializeResult::Initialized(img) => Ok((img, consumed)),
        InitializeResult::NeedMoreData(_) => bail!("JXL header incomplete or not a JPEG XL file"),
    }
}

/// ROUND 35, THE RULED TAIL (§R.S T1): **DOES THIS JXL'S OWN HEADER EARN A 16-BIT READ?**
///
/// The JXL keep arm's depth decision, factored out of the arm so that a row can reach it. The arm
/// itself cannot have one: there is no JXL encoder in this tree, so nothing here mints a `.jxl`
/// and no fixture reaches [`decode_jxl_rgb`]. A pure predicate over the crate's own public
/// `BitDepth` can be pinned at every depth the format admits, and it IS the whole decision.
///
/// The type is `jxl_image::BitDepth`, reached through jxl-oxide's re-export
/// (`jxl-oxide-0.12.6/src/lib.rs:166`, `pub use jxl_image::{self as image, ..}`); the datum is
/// `JxlImage::image_header()` (`jxl-oxide-0.12.6/src/lib.rs:520`) -> `.metadata.bit_depth`
/// (`jxl-image-0.13.0/src/lib.rs:142`, the enum at `:427`, `bits_per_sample()` at `:446`).
///
/// An INTEGER sample is kept only ABOVE eight bits: at eight the `u16` read would hand back
/// `v * 257` for every sample the file holds as `v`, and the deliverable would be a 16-bit PNG
/// twice the size of an 8-bit source -- the UP-conversion §0.3 forbids in as many words. A FLOAT
/// sample is always kept: jxl-oxide clamps and scales it into whichever sample type the buffer
/// asks for, so a `u8` read would throw away depth the file really carries, whatever
/// `bits_per_sample` says about the bitcast layout.
///
/// Pinned by `the_jxl_depth_term_keeps_only_what_the_file_holds`.
fn jxl_keeps_depth(bd: &jxl_oxide::image::BitDepth) -> bool {
    match bd {
        jxl_oxide::image::BitDepth::IntegerSample { bits_per_sample } => *bits_per_sample > 8,
        jxl_oxide::image::BitDepth::FloatSample { .. } => true,
    }
}

/// Decode a JPEG XL (orientation applied; packed RGB8 opaque over white under `Keep::NONE`, the file's own
/// layout and declared depth kept for the PNG export since round 35 -- see the arm's comment). Bomb-guards the header
/// (oriented) dims BEFORE loading any frame, and routes HDR (PQ/HLG) JXL to a clean error.
fn decode_jxl_rgb(path: &Path, keep: Keep) -> Result<(Pixels, u32, u32)> {
    use jxl_oxide::PixelFormat;
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(JXL_MAX_BYTES).read_to_end(&mut buf)?;
    let (mut image, consumed) = read_jxl_header(&buf)?;
    // (bomb guard) — oriented header dims, BEFORE any frame allocation.
    let (w, h) = (image.width(), image.height());
    guard_source_dims(w, h, "JPEG XL")?;
    // HDR (PQ/HLG) → honest refusal; the HDR→SDR handling is deferred (same as AVIF). No silent clip.
    if let Some(hdr) = image.hdr_type() {
        bail!("HDR JPEG XL ({hdr:?} transfer) is not supported yet — its HDR→SDR handling is deferred");
    }
    // Feed the remaining codestream (try_init already fed everything the container parser consumed) and
    // finalize so the first keyframe is fully loaded.
    if consumed < buf.len() {
        image
            .feed_bytes(&buf[consumed..])
            .map_err(|e| anyhow::anyhow!("JXL body read failed: {e}"))?;
    }
    image.finalize().map_err(|e| anyhow::anyhow!("JXL finalize failed: {e}"))?;
    if image.num_loaded_keyframes() == 0 {
        bail!("JXL has no decodable frame");
    }
    let render = image.render_frame(0).map_err(|e| anyhow::anyhow!("JXL render failed: {e}"))?;
    let pf = image.pixel_format();
    // `stream()` applies orientation and includes colour + black + alpha channels.
    let mut stream = render.stream();
    let (sw, sh, ch) = (stream.width(), stream.height(), stream.channels() as usize);
    // ROUND 35 + THE RULED TAIL (§R.S T1): the `::<u8>` turbofish is the depth reduction -- the
    // same call takes `u16`, and jxl-oxide clamps and scales a float sample into either. So the
    // keep arm is the identical stream read at whichever sample type THE FILE EARNS, and
    // `apply_keep` narrows further (and flattens) if the caller asked for less.
    //
    // WHAT THE FILE EARNS IS THE HEADER'S `bit_depth`, NEVER `pf`. `pf` is
    // `jxl_oxide::PixelFormat` -- a CHANNEL LAYOUT (`Rgb`, `Rgba`, `Gray`, `Graya`, `Cmyk`,
    // `Cmyka`) carrying no depth at all, so the guard below decides which CHANNELS survive and
    // `jxl_keeps_depth` decides how WIDE the samples are. Round 35 shipped without the second
    // half: every JXL at the PNG stop, an ordinary 8-bit one included, was read at `u16` and
    // written as a 16-bit PNG twice the size carrying `v * 257` -- the UP-conversion §0.3 forbids.
    //
    // UNVERIFIED BY FIXTURE (round 35 §C.7, Q7 (iii)): there is no JXL encoder in this tree, so no
    // row mints a `.jxl` -- this arm is proved by reading and by the shipped arm beside it, never
    // claimed tested. What a row CAN reach is the predicate, and
    // `the_jxl_depth_term_keeps_only_what_the_file_holds` pins it at every depth the format admits.
    if keep.any() && matches!(pf, PixelFormat::Rgb | PixelFormat::Rgba | PixelFormat::Gray | PixelFormat::Graya) {
        if keep.depth && jxl_keeps_depth(&image.image_header().metadata.bit_depth) {
            let mut deep = vec![0u16; sw as usize * sh as usize * ch];
            stream.write_to_buffer::<u16>(&mut deep);
            let px = match pf {
                PixelFormat::Rgb => Pixels::Rgb16(deep),
                PixelFormat::Rgba => Pixels::Rgba16(deep),
                PixelFormat::Gray => Pixels::Rgb16(deep.iter().flat_map(|&v| [v, v, v]).collect()),
                PixelFormat::Graya => Pixels::Rgba16(
                    deep.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
                ),
                // The guard above admits only the four colour formats.
                _ => bail!("internal: the JXL keep arm was reached for a CMYK pixel format"),
            };
            return Ok((px, sw, sh));
        }
        // The file is 8-bit integer (or `keep.depth` is false): the SHIPPED `::<u8>` read, handed
        // back in the layout the file carries. Alpha is not flattened here -- `apply_keep` is the
        // ONE place the two requests are applied (its own doc says so), so it drops the channel
        // when `keep.alpha` is false, exactly as it does for the 16-bit block above; and
        // `drop_opaque_alpha` still takes a channel that turns out to be full everywhere.
        let mut shallow = vec![0u8; sw as usize * sh as usize * ch];
        stream.write_to_buffer::<u8>(&mut shallow);
        let px = match pf {
            PixelFormat::Rgb => Pixels::Rgb8(shallow),
            PixelFormat::Rgba => Pixels::Rgba8(shallow),
            PixelFormat::Gray => Pixels::Rgb8(shallow.iter().flat_map(|&v| [v, v, v]).collect()),
            PixelFormat::Graya => Pixels::Rgba8(
                shallow.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
            ),
            // The guard above admits only the four colour formats.
            _ => bail!("internal: the JXL keep arm was reached for a CMYK pixel format"),
        };
        return Ok((px, sw, sh));
    }
    let mut samples = vec![0u8; sw as usize * sh as usize * ch];
    stream.write_to_buffer::<u8>(&mut samples);
    let rgb: Vec<u8> = match pf {
        PixelFormat::Rgb => samples, // ch == 3, already packed RGB8
        PixelFormat::Rgba => samples
            .chunks_exact(4)
            .flat_map(|p| [over_white(p[0], p[3]), over_white(p[1], p[3]), over_white(p[2], p[3])])
            .collect(),
        PixelFormat::Gray => samples.iter().flat_map(|&v| [v, v, v]).collect(),
        PixelFormat::Graya => samples
            .chunks_exact(2)
            .flat_map(|p| {
                let v = over_white(p[0], p[1]);
                [v, v, v]
            })
            .collect(),
        // CMYK JXL needs a colour-management transform (no CMS is compiled in — moxcms/lcms2 are off);
        // vanishingly rare for a photo culler, so refuse cleanly rather than pull a CMS + cmake surface.
        PixelFormat::Cmyk | PixelFormat::Cmyka => {
            bail!("CMYK JPEG XL is not supported (needs a colour-management transform)")
        }
    };
    Ok((Pixels::Rgb8(rgb), sw, sh))
}

/// Cheap JXL size probe (header only, oriented) for the zoom/1:1 % readout.
fn jxl_dimensions(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; JXL_HEADER_PROBE_BYTES];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let (image, _) = read_jxl_header(&buf).ok()?;
    let (w, h) = (image.width(), image.height());
    (w != 0 && h != 0).then_some((w, h))
}

/// The colour-space description of a JXL: its embedded ICC's description (a Display P3 / Adobe RGB export
/// carries one) — mirroring `webp_color_tag` — else the enum colour-encoding's CICP primaries (like the
/// HEIC nclx path: 1 → sRGB, 9 → Rec.2020, 11/12 → Display P3), else `None` → the caller's sRGB fallback.
/// v0.8.140 (C2): the original ICC ITSELF when the codestream embeds one. The CICP arm carries NO
/// bytes by construction — it is an enumerated primaries code, so it legitimately yields a name-only
/// tag, exactly like the HEIF `nclx` door.
fn jxl_color_tag(path: &Path) -> ColorTag {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return ColorTag::default() };
    let mut buf = vec![0u8; JXL_HEADER_PROBE_BYTES];
    let Ok(n) = f.read(&mut buf) else { return ColorTag::default() };
    buf.truncate(n);
    let Ok((image, _)) = read_jxl_header(&buf) else { return ColorTag::default() };
    jxl_tag_from(image.original_icc(), jxl_cicp_name(&image))
}

/// The JXL door's PRECEDENCE, as a pure function of what the codestream yielded (v0.8.141, R4).
///
/// Split out because it is the only part of this door a test can reach: `jxl-oxide` is decode-only
/// (there is no encoder in the tree or in the dep graph), so no fixture can be built that carries an
/// original ICC — the codestream's ICC is brotli-compressed under JXL's own prediction encoding, and
/// hand-rolling one to prove a one-line hop would be a fixture bigger than the door. The
/// `jxl_oxide → bytes` hop therefore stays unpinned and is REPORTED as such; the precedence and the
/// byte-threading below are pinned directly by `jxl_door_threads_bytes_and_never_calls_a_cicp_name_the_profiles`.
fn jxl_tag_from(icc: Option<&[u8]>, cicp: Option<String>) -> ColorTag {
    let Some(icc) = icc else {
        return match cicp {
            Some(d) => ColorTag::named(&d),
            None => ColorTag::default(),
        };
    };
    let tag = ColorTag::from_icc(icc.to_vec());
    if tag.desc.is_some() {
        return tag;
    }
    // A profile whose description will not parse: keep the BYTES (they still answer the gamut
    // question) and let the CICP code supply the name — which is a name the CONTAINER declared, not
    // one the profile carries, so `desc_from_profile` stays false (v0.8.141, R7).
    ColorTag { icc: tag.icc, desc: cicp, desc_from_profile: false }
}

/// The JXL enum colour-encoding's CICP primaries as a name (1 → sRGB, 9 → Rec.2020, 11/12 → P3),
/// mirroring the HEIC `nclx` mapping. `None` for a code we don't model → the caller's sRGB fallback.
fn jxl_cicp_name(image: &jxl_oxide::JxlImage) -> Option<String> {
    match image.rendered_cicp() {
        Some([prim, ..]) => match prim {
            1 => Some("sRGB".to_string()),
            9 => Some("Rec. 2020".to_string()),
            11 | 12 => Some("Display P3".to_string()),
            _ => None,
        },
        None => None,
    }
}

// ─────────────────────────────────── BMP (formats batch F3) ───────────────────────────────
// Pure-Rust decode via the `image` crate's `bmp` codec (already a transitive dep via rawler; only the
// `bmp` feature is newly enabled — no extra crates, no cmake). UNIVERSAL: works on the macOS port too.

/// Decode a BMP (packed RGB8 under `Keep::NONE`, opaque over white for the rare 32-bit alpha BMP; that alpha is
/// kept for the PNG export since round 35). Bomb-guards the
/// header dims BEFORE allocating the pixel buffer.
fn decode_bmp_rgb(path: &Path, keep: Keep) -> Result<(Pixels, u32, u32)> {
    use image::codecs::bmp::BmpDecoder;
    use image::{ColorType, ImageDecoder};
    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    let decoder = BmpDecoder::new(file).map_err(|e| anyhow::anyhow!("BMP header parse failed: {e}"))?;
    let (w, h) = decoder.dimensions();
    guard_source_dims(w, h, "BMP")?;
    let color = decoder.color_type();
    let mut buf = vec![0u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut buf).map_err(|e| anyhow::anyhow!("BMP decode failed: {e}"))?;
    // ROUND 35: this arm is 8-bit by construction, so the rare 32-bpp `BI_BITFIELDS` BMP's alpha is
    // the only thing there is to keep.
    if keep.alpha && matches!(color, ColorType::Rgba8) {
        return Ok((Pixels::Rgba8(buf), w, h));
    }
    let rgb = match color {
        ColorType::Rgb8 => buf,
        ColorType::Rgba8 => buf
            .chunks_exact(4)
            .flat_map(|p| [over_white(p[0], p[3]), over_white(p[1], p[3]), over_white(p[2], p[3])])
            .collect(),
        ColorType::L8 => buf.iter().flat_map(|&v| [v, v, v]).collect(),
        other => bail!("unexpected BMP colour type: {other:?}"),
    };
    Ok((Pixels::Rgb8(rgb), w, h))
}

/// Cheap BMP size probe (header parse) for the zoom/1:1 % readout.
fn bmp_dimensions(path: &Path) -> Option<(u32, u32)> {
    use image::codecs::bmp::BmpDecoder;
    use image::ImageDecoder;
    let file = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let (w, h) = BmpDecoder::new(file).ok()?.dimensions();
    (w != 0 && h != 0).then_some((w, h))
}

// ─────────────────────────────────── GIF (formats batch F2) ───────────────────────────────
// Pure-Rust decode via the `gif` crate (already a transitive dep via resvg). The gif crate hands us
// sub-rectangle RGBA frames; we COMPOSITE them with correct DISPOSAL handling into a full-canvas RGBA
// running image and carry per-frame delays, so the viewer can PLAY the animation (a static first frame
// isn't the value — owner). Still consumers (thumb / fast tier / scan / ROI source) get the composited
// FIRST frame via `decode_gif_first_frame_rgb`, routed through the normal `decode_source_rgb` arm.
//
// Memory policy: an animation whose precomposed RGBA (w·h·4·frames) fits `GIF_PRECOMPOSE_CAP_BYTES` is
// held entirely in RAM (`decode_gif_animation` → `InMemory`); above the cap it is STREAMED via
// `GifStream` (a running canvas that re-decodes per loop) so memory stays bounded for ANY GIF.

/// Precompose cap. 256 MB — at 4 B/px that's 64 M frame-pixels (e.g. a 480×270 clip of ~1200 frames, or
/// an 800×800 GIF of ~100 frames), which every real photographer's stray GIF sits well under. Above it we
/// stream, so the in-RAM frame store never exceeds this regardless of the GIF's frame count.
pub const GIF_PRECOMPOSE_CAP_BYTES: u64 = 256 * 1024 * 1024;
/// Hard bound on the frame-count probe walk (defence against a crafted many-frame header); a real GIF is
/// nowhere near this. A GIF at/over this many frames is streamed and its reported count saturates here.
const GIF_MAX_FRAMES: usize = 100_000;

/// One composited, display-ready animation frame: full-canvas RGBA8 (`width*height*4`, OPAQUE — GIF
/// transparency is flattened over white, matching the still PNG/WebP path) + how long it stays on screen.
/// Format-generic ("Anim", not "Gif") so an APNG / animated-AVIF path could reuse the playback engine.
#[derive(Clone)]
pub struct AnimFrame {
    /// Full logical-canvas RGBA8, opaque.
    pub rgba: Vec<u8>,
    /// How long this frame stays on screen, in milliseconds (already delay-clamped).
    pub delay_ms: u32,
}

/// A fully in-memory animation (every frame precomposed). Returned by [`decode_gif_animation`] when the
/// precomposed size fits [`GIF_PRECOMPOSE_CAP_BYTES`]; the playback engine cycles `frames`.
pub struct AnimFrames {
    pub width: u32,
    pub height: u32,
    pub frames: Vec<AnimFrame>,
}

/// The outcome of probing a GIF for animated playback.
pub enum GifPlayback {
    /// Small enough to hold every frame in RAM — cycle `frames`, honouring each `delay_ms`.
    InMemory(AnimFrames),
    /// Too large to precompose — drive it via [`GifStream`] instead (bounded memory). Carries the logical
    /// size + frame count for the panel readout / ring-buffer sizing.
    Streamed { width: u32, height: u32, frames: usize },
}

/// A decoded GIF frame's fields, captured owned so the `gif` decoder borrow is released before we
/// composite into our own canvas (sidesteps the decoder-vs-canvas borrow overlap).
struct GifFrameData {
    buffer: Vec<u8>, // sub-rect RGBA (width*height*4)
    dispose: gif::DisposalMethod,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    delay_cs: u32,
}

fn capture_gif_frame(f: &gif::Frame) -> GifFrameData {
    GifFrameData {
        buffer: f.buffer.to_vec(),
        dispose: f.dispose,
        left: f.left as u32,
        top: f.top as u32,
        width: f.width as u32,
        height: f.height as u32,
        delay_cs: f.delay as u32,
    }
}

/// Browser-conventional delay clamp: GIF delays are centiseconds; 0 or 1 cs (what many "as fast as
/// possible" encoders emit) clamp to 100 ms (10 cs), matching how browsers play them. Else `cs * 10` ms.
fn clamp_gif_delay(delay_cs: u32) -> u32 {
    if delay_cs <= 1 {
        100
    } else {
        delay_cs * 10
    }
}

/// Flatten an RGBA canvas to OPAQUE RGBA over white (alpha → 255) — keeps the animated + still paths
/// visually identical (both flatten transparency over white, the house convention for a photo viewer).
fn flatten_rgba_over_white(rgba: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; rgba.len()];
    for (o, p) in out.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
        o[0] = over_white(p[0], p[3]);
        o[1] = over_white(p[1], p[3]);
        o[2] = over_white(p[2], p[3]);
        o[3] = 255;
    }
    out
}

/// Clear a rect of an RGBA canvas to transparent (GIF "restore to background" for a viewer = transparent,
/// which flattens to white on display — we don't track the GIF's background-colour index).
fn clear_rect_rgba(canvas: &mut [u8], canvas_w: u32, l: u32, t: u32, w: u32, h: u32) {
    let cw = canvas_w as usize;
    let ch = if cw == 0 { 0 } else { canvas.len() / (cw * 4) };
    for row in 0..h as usize {
        let cy = t as usize + row;
        if cy >= ch {
            break;
        }
        for col in 0..w as usize {
            let cx = l as usize + col;
            if cx >= cw {
                continue;
            }
            let d = (cy * cw + cx) * 4;
            canvas[d..d + 4].copy_from_slice(&[0, 0, 0, 0]);
        }
    }
}

/// A running GIF canvas that composites frames with correct disposal. `push` returns the displayed frame
/// (full-canvas, flattened opaque over white) + its clamped delay. The internal canvas keeps TRUE alpha
/// (needed for disposal correctness); only the emitted frame is flattened.
struct GifCompositor {
    width: u32,
    height: u32,
    canvas: Vec<u8>, // w*h*4, running composite with true alpha
    // Disposal to apply from the PREVIOUS frame before drawing the next: (method, l, t, w, h, snapshot).
    pending: Option<(gif::DisposalMethod, u32, u32, u32, u32, Option<Vec<u8>>)>,
}

impl GifCompositor {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            canvas: vec![0u8; width as usize * height as usize * 4],
            pending: None,
        }
    }

    fn reset(&mut self) {
        for b in self.canvas.iter_mut() {
            *b = 0;
        }
        self.pending = None;
    }

    /// Steps 1 to 4 of the five [`GifCompositor::push`] used to run in one body: apply the previous
    /// frame's disposal, snapshot for the next one, composite this frame's sub-rect, and remember
    /// the disposal. The canvas is left holding TRUE alpha -- which is the state round 35's PNG
    /// export reads, and which the flatten in `push` is the only thing that ever took away.
    fn composite(&mut self, f: &GifFrameData) {
        use gif::DisposalMethod;
        let (cw, ch) = (self.width as usize, self.height as usize);
        // 1. Apply the PREVIOUS frame's disposal to prepare the canvas.
        if let Some((dispose, l, t, fw, fh, snap)) = self.pending.take() {
            match dispose {
                DisposalMethod::Background => clear_rect_rgba(&mut self.canvas, self.width, l, t, fw, fh),
                DisposalMethod::Previous => {
                    if let Some(s) = snap {
                        self.canvas.copy_from_slice(&s);
                    }
                }
                // 0 ("no disposal specified") and 1 ("do not dispose") both leave the canvas untouched.
                DisposalMethod::Any | DisposalMethod::Keep => {}
            }
        }
        // 2. Snapshot BEFORE compositing if THIS frame's disposal is "restore to previous".
        let snapshot = if f.dispose == DisposalMethod::Previous {
            Some(self.canvas.clone())
        } else {
            None
        };
        // 3. Composite this frame's sub-rect. GIF transparency is BINARY (alpha 0 → keep the canvas pixel,
        //    else overwrite) — the RGBA converter already set alpha 0 on the transparent-index pixels.
        for row in 0..f.height as usize {
            let cy = f.top as usize + row;
            if cy >= ch {
                break;
            }
            for col in 0..f.width as usize {
                let cx = f.left as usize + col;
                if cx >= cw {
                    continue;
                }
                let s = (row * f.width as usize + col) * 4;
                if s + 4 > f.buffer.len() {
                    continue;
                }
                if f.buffer[s + 3] != 0 {
                    let d = (cy * cw + cx) * 4;
                    self.canvas[d..d + 4].copy_from_slice(&f.buffer[s..s + 4]);
                }
            }
        }
        // 4. Remember this frame's disposal for the next push.
        self.pending = Some((f.dispose, f.left, f.top, f.width, f.height, snapshot));
    }

    /// Composite `f`, then emit step 5: the whole canvas as an OPAQUE display frame (over white)
    /// with its clamped delay. Every animation consumer takes this; only the ./export PNG export takes
    /// [`GifCompositor::composite`] and reads the canvas.
    fn push(&mut self, f: &GifFrameData) -> AnimFrame {
        self.composite(f);
        // 5. Emit an OPAQUE display frame (over white) + the clamped delay.
        AnimFrame {
            rgba: flatten_rgba_over_white(&self.canvas),
            delay_ms: clamp_gif_delay(f.delay_cs),
        }
    }
}

/// Open a GIF decoder with RGBA output + a bomb-guarded per-frame memory bound; returns it + logical size.
fn open_gif(path: &Path) -> Result<(gif::Decoder<std::io::BufReader<std::fs::File>>, u32, u32)> {
    use gif::{ColorOutput, DecodeOptions, MemoryLimit};
    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut opts = DecodeOptions::new();
    opts.set_color_output(ColorOutput::RGBA);
    // Bound the gif crate's per-frame alloc to the SAME bomb policy as guard_source_dims (its 50 MB
    // default would wrongly reject a large-but-valid frame). A guard-passing w·h·4 always fits here.
    let cap = (max_source_pixels().saturating_mul(4)).max(1);
    opts.set_memory_limit(MemoryLimit::Bytes(cap.try_into().expect("nonzero cap")));
    let dec = opts.read_info(file).map_err(|e| anyhow::anyhow!("GIF header parse failed: {e}"))?;
    let (w, h) = (dec.width() as u32, dec.height() as u32);
    guard_source_dims(w, h, "GIF")?;
    Ok((dec, w, h))
}

/// Cheap frame-count probe (skips LZW decode) for the memory decision + the "N frames" panel readout.
fn gif_frame_count(path: &Path) -> Result<usize> {
    use gif::{ColorOutput, DecodeOptions};
    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut opts = DecodeOptions::new();
    opts.set_color_output(ColorOutput::Indexed);
    opts.skip_frame_decoding(true);
    let mut dec = opts.read_info(file).map_err(|e| anyhow::anyhow!("GIF header parse failed: {e}"))?;
    let mut n = 0usize;
    while dec
        .next_frame_info()
        .map_err(|e| anyhow::anyhow!("GIF frame walk failed: {e}"))?
        .is_some()
    {
        n += 1;
        if n >= GIF_MAX_FRAMES {
            break;
        }
    }
    Ok(n)
}

/// v0.8.187 (X1) — **IS THIS GIF ANIMATED? The bounded, on-demand answer.**
///
/// The viewer refuses to rotate an animated GIF (its playback lane has no rotation code), and that
/// refusal needs a verdict for a shot the PLAYBACK LANE may never have touched — a context-menu
/// target three tiles away, or the current shot inside its own decode window. This is that verdict,
/// and it is deliberately the cheapest form of it:
///
///   * `skip_frame_decoding(true)` — the walk reads BLOCK STRUCTURE only. No LZW, no palette
///     expansion, no pixel buffer of any size is ever allocated.
///   * IT STOPS AT THE SECOND FRAME. The question is "are there two", not "how many", so the walk
///     is O(1) blocks rather than O(frames): a 500-frame clip costs the same as a 2-frame one. That
///     is what makes it safe to run SYNCHRONOUSLY on the UI thread, which is where a keypress and a
///     menu click both live. (Contrast [`gif_frame_count`], which must walk the whole file because
///     its callers print the number.)
///
/// `Err` on an unreadable header or a malformed block chain — the caller treats that as "cannot be
/// checked" and refuses conservatively rather than guessing.
pub fn gif_is_animated(path: &Path) -> Result<bool> {
    use gif::{ColorOutput, DecodeOptions};
    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut opts = DecodeOptions::new();
    opts.set_color_output(ColorOutput::Indexed);
    opts.skip_frame_decoding(true);
    let mut dec = opts.read_info(file).map_err(|e| anyhow::anyhow!("GIF header parse failed: {e}"))?;
    let mut n = 0usize;
    while dec
        .next_frame_info()
        .map_err(|e| anyhow::anyhow!("GIF frame walk failed: {e}"))?
        .is_some()
    {
        n += 1;
        if n >= 2 {
            return Ok(true); // the bound: two frames is the whole question
        }
    }
    Ok(false)
}

/// Cheap GIF size probe (logical screen descriptor, bomb-guarded) for the zoom/1:1 % readout.
fn gif_dimensions(path: &Path) -> Option<(u32, u32)> {
    let (_, w, h) = open_gif(path).ok()?;
    (w != 0 && h != 0).then_some((w, h))
}

/// Composite + return ONLY the first displayed frame -- packed RGB8 (opaque, over white) under `Keep::NONE`,
/// the canvas's true alpha kept for the PNG export since round 35 -- the still
/// every non-playback consumer uses (thumb, fast tier, scan, ROI source). Cheap: one frame decoded.
fn decode_gif_first_frame_rgb(path: &Path, keep: Keep) -> Result<(Pixels, u32, u32)> {
    let (mut dec, w, h) = open_gif(path)?;
    let mut comp = GifCompositor::new(w, h);
    let frame = dec
        .read_next_frame()
        .map_err(|e| anyhow::anyhow!("GIF decode failed: {e}"))?
        .context("GIF has no frames")?;
    let data = capture_gif_frame(frame);
    // ROUND 35: GIF transparency is a palette INDEX, so its alpha is binary (0 or 255) -- and the
    // compositor's canvas already holds it, one statement before the flatten. A Background-disposal
    // GIF's cleared rects therefore export TRANSPARENT, which is what the file says. 8-bit palette,
    // so depth has nothing to keep.
    if keep.alpha {
        comp.composite(&data);
        return Ok((Pixels::Rgba8(comp.canvas), w, h));
    }
    let anim = comp.push(&data); // full-canvas opaque RGBA
    let rgb: Vec<u8> = anim.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    Ok((Pixels::Rgb8(rgb), w, h))
}

/// The precompose-vs-stream decision (pure, so it's unit-testable without a giant GIF): hold all frames
/// in RAM only when the precomposed RGBA (`w·h·4·frames`) fits `cap` AND the count is under the walk
/// bound. Overflow-safe (`saturating_mul`), so a crafted huge `w·h·frames` decides "stream", never panics.
fn precompose_fits(w: u32, h: u32, frames: usize, cap: u64) -> bool {
    let per_frame = (w as u64).saturating_mul(h as u64).saturating_mul(4);
    let total = per_frame.saturating_mul(frames as u64);
    total <= cap && frames < GIF_MAX_FRAMES
}

/// Probe a GIF for animated playback: precompose every frame into RAM when the total fits the cap, else
/// signal a streamed decode. The logical size is bomb-guarded (via `open_gif`) before any allocation.
pub fn decode_gif_animation(path: &Path) -> Result<GifPlayback> {
    let (_, w, h) = open_gif(path)?; // guard + logical size
    let frames = gif_frame_count(path)?;
    if frames == 0 {
        bail!("GIF has no frames");
    }
    if precompose_fits(w, h, frames, GIF_PRECOMPOSE_CAP_BYTES) {
        let (mut dec, _, _) = open_gif(path)?;
        let mut comp = GifCompositor::new(w, h);
        let mut out = Vec::with_capacity(frames);
        while let Some(frame) = dec
            .read_next_frame()
            .map_err(|e| anyhow::anyhow!("GIF decode failed: {e}"))?
        {
            let data = capture_gif_frame(frame);
            out.push(comp.push(&data));
        }
        if out.is_empty() {
            bail!("GIF produced no frames");
        }
        Ok(GifPlayback::InMemory(AnimFrames { width: w, height: h, frames: out }))
    } else {
        Ok(GifPlayback::Streamed { width: w, height: h, frames })
    }
}

/// A bounded, streaming GIF player for animations too large to precompose. It holds one running canvas
/// and re-decodes frames on demand, LOOPING forever (re-opens the file at end-of-stream). Memory stays
/// ~2× the canvas (canvas + at most one snapshot) regardless of frame count.
pub struct GifStream {
    path: PathBuf,
    dec: gif::Decoder<std::io::BufReader<std::fs::File>>,
    comp: GifCompositor,
}

impl GifStream {
    /// Open a streaming player for `path` (bomb-guards the logical size).
    pub fn open(path: &Path) -> Result<Self> {
        let (dec, w, h) = open_gif(path)?;
        Ok(Self { path: path.to_path_buf(), dec, comp: GifCompositor::new(w, h) })
    }

    /// The logical screen size (== every emitted frame's dimensions).
    pub fn dimensions(&self) -> (u32, u32) {
        (self.comp.width, self.comp.height)
    }

    fn reopen(&mut self) -> Result<()> {
        let (dec, _, _) = open_gif(&self.path)?;
        self.dec = dec;
        self.comp.reset();
        Ok(())
    }

    /// Advance one frame and return the composited display frame (loops at end-of-stream).
    pub fn next_frame(&mut self) -> Result<AnimFrame> {
        if let Some(frame) = self
            .dec
            .read_next_frame()
            .map_err(|e| anyhow::anyhow!("GIF decode failed: {e}"))?
        {
            let data = capture_gif_frame(frame);
            return Ok(self.comp.push(&data));
        }
        // End of stream → loop back to the start.
        self.reopen()?;
        let frame = self
            .dec
            .read_next_frame()
            .map_err(|e| anyhow::anyhow!("GIF decode failed: {e}"))?
            .context("GIF has no frames on loop")?;
        let data = capture_gif_frame(frame);
        Ok(self.comp.push(&data))
    }
}

/// SIMD Lanczos3 downscale so the long side == `long` (no-op if already smaller). The packed-RGB8
/// entry, which is what every caller but round 35's ./export PNG export wants; the arithmetic, the
/// filter and the `Resizer` call are [`resize_pixels_to_long`]'s `Rgb8` arm.
fn resize_to_long(rgb: Vec<u8>, w: u32, h: u32, long: u32) -> Result<(Vec<u8>, u32, u32)> {
    let (px, nw, nh) = resize_pixels_to_long(Pixels::Rgb8(rgb), w, h, long)?;
    Ok((px.into_rgb8()?, nw, nh))
}

/// v1.0.0-rc PNG EXPORT (queue item 35): the same downscale at all four layouts.
///
/// **IT IS THE SAME CALL.** One `Resizer`, one `ResizeOptions::new()`, one `Lanczos3`; only the
/// [`PixelType`] moves, `U8x3 / U8x4 / U16x3 / U16x4`. That matters for the alpha arms in
/// particular: `ResizeOptions::new()` carries `mul_div_alpha: true`, so the crate PREMULTIPLIES by
/// alpha before the convolution and divides after -- which is the difference between a clean edge
/// and a dark halo where a hard-edged transparent region meets an opaque one. This crate's other
/// RGBA resize, `downscale_rgba`, falls SOFT on a resizer error and returns the source unresized;
/// on an export that would silently ship the wrong DIMENSIONS, so this one propagates, exactly as
/// the shipped `resize_to_long` always did.
///
/// The never-upscale early return is unchanged, and it is also the whole of the "Full" tier: the
/// sheet's Full stop is the `100000` sentinel, which no photograph exceeds, so a Full export takes
/// the `return` below and the buffer reaches the encoder without a second allocation.
///
/// The 16-bit arms borrow their `Vec<u16>` as bytes (`bytemuck`, a checked zero-copy cast -- the
/// crate is already in this workspace's lock) and write into a `Vec<u16>` this function owns, so a
/// 16-bit resize costs exactly ONE destination buffer, the same as an 8-bit one.
pub fn resize_pixels_to_long(px: Pixels, w: u32, h: u32, long: u32) -> Result<(Pixels, u32, u32)> {
    if w == 0 || h == 0 {
        bail!("cannot resize empty {w}x{h} image");
    }
    if w.max(h) <= long {
        return Ok((px, w, h));
    }
    let (nw, nh) = if w >= h {
        (long, ((long as u64 * h as u64) / w as u64).max(1) as u32)
    } else {
        (((long as u64 * w as u64) / h as u64).max(1) as u32, long)
    };
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    let n = nw as usize * nh as usize;
    let out = match px {
        Pixels::Rgb8(v) => {
            let src = Image::from_vec_u8(w, h, v, PixelType::U8x3)?;
            let mut dst = Image::new(nw, nh, PixelType::U8x3);
            Resizer::new().resize(&src, &mut dst, &opts)?;
            Pixels::Rgb8(dst.into_vec())
        }
        Pixels::Rgba8(v) => {
            let src = Image::from_vec_u8(w, h, v, PixelType::U8x4)?;
            let mut dst = Image::new(nw, nh, PixelType::U8x4);
            Resizer::new().resize(&src, &mut dst, &opts)?;
            Pixels::Rgba8(dst.into_vec())
        }
        Pixels::Rgb16(mut v) => {
            let mut o = vec![0u16; n * 3];
            {
                let src = Image::from_slice_u8(w, h, bytemuck::cast_slice_mut(&mut v), PixelType::U16x3)?;
                let mut dst =
                    Image::from_slice_u8(nw, nh, bytemuck::cast_slice_mut(&mut o), PixelType::U16x3)?;
                Resizer::new().resize(&src, &mut dst, &opts)?;
            }
            Pixels::Rgb16(o)
        }
        Pixels::Rgba16(mut v) => {
            let mut o = vec![0u16; n * 4];
            {
                let src = Image::from_slice_u8(w, h, bytemuck::cast_slice_mut(&mut v), PixelType::U16x4)?;
                let mut dst =
                    Image::from_slice_u8(nw, nh, bytemuck::cast_slice_mut(&mut o), PixelType::U16x4)?;
                Resizer::new().resize(&src, &mut dst, &opts)?;
            }
            Pixels::Rgba16(o)
        }
    };
    Ok((out, nw, nh))
}

/// v1.0.0-rc PNG EXPORT (queue item 35): the sRGB conversion at all four layouts. The RGB triples
/// go through the identical matrix and TRC pair whichever arm runs; the alpha plane is never read
/// and never written (both RGBA entries state that contract in falcon-color).
fn transform_pixels(px: &mut Pixels, src: falcon_color::Gamut, dst: falcon_color::Gamut) {
    match px {
        Pixels::Rgb8(v) => falcon_color::transform_rgb(v, src, dst),
        Pixels::Rgba8(v) => falcon_color::transform_rgba(v, src, dst),
        Pixels::Rgb16(v) => falcon_color::transform_rgb16(v, src, dst),
        Pixels::Rgba16(v) => falcon_color::transform_rgba16(v, src, dst),
    }
}

/// v0.8.101 (S2): Lanczos-resize a packed RGB8 buffer to EXACTLY `nw`×`nh`, or hand it back
/// untouched when it is already that size. [`resize_to_long`] can only express "fit this long
/// side", which is one rounding step short of what the scaled-decode arm needs — it has to land on
/// the SAME pair the old full-decode path produced, both dimensions.
#[cfg_attr(not(windows), allow(dead_code))] // `wic_decode_rgb24_scaled` is its only caller
fn resize_rgb_exact(rgb: Vec<u8>, w: u32, h: u32, nw: u32, nh: u32) -> Result<(Vec<u8>, u32, u32)> {
    if w == 0 || h == 0 || nw == 0 || nh == 0 {
        bail!("cannot resize {w}x{h} to {nw}x{nh}");
    }
    if (w, h) == (nw, nh) {
        return Ok((rgb, w, h));
    }
    let src = Image::from_vec_u8(w, h, rgb, PixelType::U8x3)?;
    let mut dst = Image::new(nw, nh, PixelType::U8x3);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    Resizer::new().resize(&src, &mut dst, &opts)?;
    Ok((dst.into_vec(), nw, nh))
}

/// `icc`: raw ICC profile bytes to embed as APP2 `ICC_PROFILE` segments (v0.8.100 / A1) — `None`
/// for every INTERNAL frame (scrub/thumb/reference/develop: transient in-process pixels that never
/// leave the app, so a tag would be pure overhead). Only the ./export DELIVERABLE tags itself.
fn encode_jpeg(rgb: &[u8], w: u32, h: u32, quality: u8, icc: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut enc = Encoder::new(&mut buf, quality);
    // 4:2:0 chroma subsampling — roughly halves both the file size and the encode
    // time vs the encoder's high-quality 4:4:4 default, with no visible loss for
    // on-screen viewing (the full-quality pixel-peek tier ships the original JPG).
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    if let Some(icc) = icc {
        // jpeg-encoder chunks this into spec-shaped APP2 `ICC_PROFILE\0` segments itself. A profile
        // too large to chunk (> ~16 MB) is not a reason to lose the whole export: log-free best
        // effort — drop the tag, still ship the (converted) pixels.
        let _ = enc.add_icc_profile(icc);
    }
    enc.encode(rgb, w as u16, h as u16, ColorType::Rgb)?;
    Ok(buf)
}

/// Encode packed RGB8 to JPEG. Public so the GPU develop path can encode its
/// read-back pixels through the same fast SIMD encoder.
pub fn encode_jpeg_rgb(rgb: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>> {
    encode_jpeg(rgb, w, h, quality, None)
}

/// v1.0.0-rc EXPORT FORMAT (F1): write a LOSSLESS PNG -- the second ./export deliverable format,
/// beside [`encode_jpeg`]. v1.0.0-rc PNG EXPORT (queue item 35): to a SINK, in whichever of the four
/// layouts the file's own pixels reached the encoder in.
///
/// `ColorType::{Rgb, Rgba}` x `BitDepth::{Eight, Sixteen}`, taken from the [`Pixels`] handed in --
/// which is what [`export_web_to`] produced: resized, converted to sRGB, watermark stamped, and
/// carrying whatever alpha and depth the SOURCE FILE held (round 35; before it, always RGB8, because
/// the decoders flattened both before the export was entered).
///
/// **16-BIT SAMPLES ARE WRITTEN BIG-ENDIAN.** PNG is network byte order (spec 7.1) and `png`
/// 0.17.16 passes the caller's SAMPLES through untouched -- `write_image_data` hands them to the
/// filter and the compressor as bytes, and the only two `to_be_bytes` in its `encoder.rs` write an
/// APNG `fcTL` sequence number (:780, :1253) -- so the swap below is not an optimisation, it is the
/// difference between a photograph and noise. It runs IN PLACE on a buffer this function owns, so
/// the widest deliverable still costs no second copy.
///
/// **THE SINK IS NOT STREAMING, AND THIS DOC ONCE CLAIMED IT WAS** (round 35's ruled tail, §R.S
/// MECHANISM R1). What the `W: Write` removes is the PARENT's SECOND copy: `export_web_image`
/// returned the finished file as a `Vec<u8>` that the caller then wrote, so the encoded bytes
/// were held twice; [`export_web_file`] hands this function a `BufWriter` on the run's `.part`
/// path instead and that returned `Vec` is gone. What it does NOT do is make `png` stream.
///
/// `png` 0.17.16's single-call `write_image_data` (`encoder.rs:661`), on the `Compression::Fast`
/// arm this function sets (`:698`), filters and compresses EVERY row into one
/// `fdeflate::Compressor` over a `Cursor<Vec<u8>>` (`:699`), takes the finished zlib stream as a
/// single `Vec` (`:717`), and only then chunks that buffer into the sink (`:763` ->
/// `write_zlib_encoded_idat`, `:814`). Its incompressible fallback (`:718-733`) builds a SECOND
/// file-sized `Vec` (`StoredOnlyCompressor`) while the first is still live. So the peak this
/// function costs is ONE PIXEL FRAME PLUS ONE COMPRESSED DELIVERABLE (two in that fallback) --
/// and the compressed term scales with the FILE, not with the frame, so a photograph's ratio,
/// not a synthetic ramp's, is what a budget has to be taken against.
///
/// Real streaming is the crate's `StreamWriter` (`:1328`, via `stream_writer_with_size` `:1010`),
/// which compresses through flate2's `ZlibEncoder` (`:1371`, `Compression::to_options()` at
/// `:1715`) -- a DIFFERENT deflate from the single-call arm's `fdeflate`. Switching would drop
/// the peak by the file's size and change every PNG's BYTES, re-taking the byte pin R0 holds, so
/// it is the owner's call (sheet 3.1m) and the default is as built. (The JPEG arm keeps its
/// `Vec` -- those files are small, and `jpeg-encoder` wants a buffer.)
///
/// **THE COMPRESSION LEVEL IS `Fast`, AND IT IS SET EXPLICITLY.** `png` 0.17.16's `Info::default()`
/// already chooses `Compression::Fast` (`common.rs:636-638`), so naming it changes no byte of any
/// deliverable -- it is named because the level is a DECISION about what the export loop costs, and
/// an inherited default is not a decision. (`Fast` is `fdeflate`, `encoder.rs:697-737`; every other
/// level routes through `ZlibEncoder`, which is why the step between them is so large. The round's
/// first doc called this "the flate2 default level" and was wrong on both halves.)
///
/// THE MEASURED TRADE, on the release build, this crate's own pipeline, a 45 MP photograph resized
/// to 4096 px: `Fast` 68 ms / 28.1 MB; `Default` 1 194 ms / 21.3 MB (-24 %); `Best` 1 715 ms /
/// 21.2 MB (-25 %). A quarter off the file for SEVENTEEN TIMES the encode -- about 1.1 s per
/// photograph, ~5 minutes on a 300-pick batch, against a deliverable that is 8-9x the JPG either
/// way. PHOTOGRAPHS ARE THE POPULATION here (this is a photo culler's ./export export), and at `Fast`
/// a PNG export is never slower than the JPG one -- 90/149/270 ms at 2048/4096/full against the
/// JPEG arm's 91/163/295, because the Lanczos resize dominates both. Flat graphics would prefer
/// `Default` (25 ms for -25 % on a screenshot), and that is the case this level is wrong for; it is
/// not the case this control exists to serve, and there is no user toggle.
///
/// **THE COLOUR TAG IS THE sRGB CHUNK, NOT AN iCCP.** The JPEG arm embeds
/// [`falcon_color::srgb_icc_for_export`] as an APP2 segment because JPEG has no other way to say
/// "sRGB"; PNG does. The `sRGB` chunk is one byte plus its header against the profile's ~2.4 KB,
/// every consumer understands it, and the PNG spec (11.3.2.5) has a decoder IGNORE `iCCP` when
/// `sRGB` is present -- so the profile would be dead bytes in every file. The rendering intent is
/// `Perceptual`, the one the spec names for photographs.
///
/// `set_source_srgb` is the LIVE spelling: `png` 0.17.16 deprecated `set_srgb` in favour of it
/// (`encoder.rs:273`, `#[deprecated(note = "use set_source_srgb")]`). The one behavioural
/// difference is that the deprecated helper ALSO wrote the spec's substitute `gAMA` + `cHRM`
/// fallbacks, for decoders that predate `sRGB`; those are set explicitly below -- the same values
/// as the crate's own `srgb::substitute_gamma` / `substitute_chromaticities`, which are
/// `pub(crate)` and so are spelled out here -- and the header writer emits them ONLY because they
/// match those substitutes exactly (`common.rs:767-778`).
fn write_png<W: std::io::Write>(px: Pixels, w: u32, h: u32, sink: W) -> Result<()> {
    if w == 0 || h == 0 {
        bail!("cannot encode an empty {w}x{h} PNG");
    }
    let want = w as u64 * h as u64 * px.bytes_per_px() as u64;
    if px.byte_len() as u64 != want {
        bail!(
            "PNG buffer is {} bytes, not the {want} a {} {w}x{h} needs",
            px.byte_len(),
            px.layout()
        );
    }
    let (color, depth) = match px {
        Pixels::Rgb8(_) => (png::ColorType::Rgb, png::BitDepth::Eight),
        Pixels::Rgba8(_) => (png::ColorType::Rgba, png::BitDepth::Eight),
        Pixels::Rgb16(_) => (png::ColorType::Rgb, png::BitDepth::Sixteen),
        Pixels::Rgba16(_) => (png::ColorType::Rgba, png::BitDepth::Sixteen),
    };
    {
        let mut enc = png::Encoder::new(sink, w, h);
        enc.set_color(color);
        enc.set_depth(depth);
        // v1.0.0-rc EXPORT FORMAT tail (A-O1): the level, STATED. Byte-identical to the crate
        // default it replaces -- that is the point: an inherited default is not a decision.
        enc.set_compression(png::Compression::Fast);
        enc.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        // The spec's sRGB substitutes (PNG 11.3.2.5), for decoders that do not read `sRGB`.
        enc.set_source_gamma(png::ScaledFloat::from_scaled(45455));
        enc.set_source_chromaticities(png::SourceChromaticities {
            white: (png::ScaledFloat::from_scaled(31270), png::ScaledFloat::from_scaled(32900)),
            red: (png::ScaledFloat::from_scaled(64000), png::ScaledFloat::from_scaled(33000)),
            green: (png::ScaledFloat::from_scaled(30000), png::ScaledFloat::from_scaled(60000)),
            blue: (png::ScaledFloat::from_scaled(15000), png::ScaledFloat::from_scaled(6000)),
        });
        let mut wtr = enc.write_header().context("PNG header")?;
        match px {
            Pixels::Rgb8(v) | Pixels::Rgba8(v) => {
                wtr.write_image_data(&v).context("PNG image data")?;
            }
            Pixels::Rgb16(mut v) | Pixels::Rgba16(mut v) => {
                wtr.write_image_data(be16_bytes_in_place(&mut v)).context("PNG image data")?;
            }
        }
        wtr.finish().context("PNG finish")?;
    }
    Ok(())
}

/// Rewrite a native-endian `[u16]` sample buffer as BIG-ENDIAN bytes, IN PLACE, and hand back the
/// byte view -- the one place in this crate that knows PNG is network byte order. On a big-endian
/// host every write below stores the bytes it read, so this is a no-op there rather than a `cfg`.
fn be16_bytes_in_place(v: &mut [u16]) -> &[u8] {
    let bytes: &mut [u8] = bytemuck::cast_slice_mut(v);
    for p in bytes.chunks_exact_mut(2) {
        let native = u16::from_ne_bytes([p[0], p[1]]);
        p.copy_from_slice(&native.to_be_bytes());
    }
    bytes
}

// ──────────────────────────── web export (resize + watermark) ────────────────────────────

/// A logo to stamp onto web exports (packed RGBA8). `pos_x`/`pos_y` are the logo CENTRE as a
/// fraction (0..1) of the image — precise placement (the 3×3 grid just picks 9 standard points);
/// the logo is clamped to stay fully on-image.
pub struct Watermark {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub scale: f32,   // logo long side as a fraction of the output long side (e.g. 0.18)
    pub opacity: f32, // 0..=1
    pub pos_x: f32,   // logo centre X as a fraction of the image width (0..1)
    pub pos_y: f32,   // logo centre Y as a fraction of the image height (0..1)
}

// ─────────────────── export colour policy (v0.8.100 / design-sweep O13 = A1) ───────────────────

/// Which export destination a pick is being written to. The two families differ in KIND, not in
/// degree: `Selected`/`Rejected` hand over the photographer's ORIGINAL FILES (a filesystem copy or
/// move — no decode, no re-encode, every original byte and its embedded profile preserved), while
/// `Web` MANUFACTURES a new deliverable (decode → resize → watermark → re-encode in the run's
/// [`WebFormat`] -- JPEG or PNG since v1.0.0-rc; this line said "JPEG" as a fact of the pipeline
/// until the tail swept it) and therefore owns its output colour space.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportTarget {
    /// `<dir>/Web` — the re-encoded social/web deliverable.
    Web,
    /// `<dir>/Selected` — a copy/move of the original picks.
    Selected,
    /// `<dir>/Rejected` — a copy/move of the original rejects.
    Rejected,
}

/// What the colour chain must do to a source in gamut `src` bound for `target`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportColorAction {
    /// Convert the pixels `src` → sRGB through the house transform, then tag the file sRGB.
    ConvertToSrgb,
    /// The source is ALREADY sRGB: no pixel work, but still tag the file so the deliverable states
    /// its own colour space instead of relying on the consumer's untagged-means-sRGB convention.
    TagSrgbOnly,
    /// Hand over the original file untouched — no decode, no transform, no re-tag.
    CopyOriginal,
}

/// The single source of truth for A1: the colour decision per export target.
///
/// `Web` is the one destination that universally assumes sRGB (browsers, Instagram, every consumer
/// pipeline), and it is also the one destination whose bytes Falcon manufactures — so a wide-gamut
/// source (in-camera Adobe RGB JPEG, iPhone Display-P3 HEIC, a P3 PNG/TIFF) must be converted DOWN
/// to sRGB and the result tagged. `Selected`/`Rejected` copy the originals: converting or re-tagging
/// them would DESTROY the photographer's wide-gamut masters, so they are always `CopyOriginal`,
/// whatever the source gamut is. Pure + platform-neutral: unit-tested against every gamut × target.
pub fn export_color_action(target: ExportTarget, src: falcon_color::Gamut) -> ExportColorAction {
    match target {
        ExportTarget::Selected | ExportTarget::Rejected => ExportColorAction::CopyOriginal,
        ExportTarget::Web => {
            if src == falcon_color::Gamut::Srgb {
                ExportColorAction::TagSrgbOnly
            } else {
                ExportColorAction::ConvertToSrgb
            }
        }
    }
}

/// v1.0.0-rc EXPORT FORMAT (F1): WHICH CONTAINER THE ./export DELIVERABLE IS WRITTEN IN.
///
/// The ./export export has always been a CONVERTER -- it decodes whatever the photographer shot (JPG,
/// PNG, TIFF, WebP, GIF, BMP, JXL, HEIC) and manufactures a new file -- and until this round the
/// new file was always a JPEG, with nothing on the sheet saying so. Two stops, because these are
/// the two a photographer actually hands over: a lossy JPEG for size, a lossless PNG for
/// screenshots, flat graphics and anything that will be re-edited. `Jpeg` is the DEFAULT, which is
/// what makes an old `settings.json` (and an old preset) load as the behaviour it was saved under.
///
/// Deliberately NOT here: WebP (the linked `image-webp` encoder is VP8L lossless-only, so a lossy
/// WebP would need libwebp, a C dependency), TIFF and GIF (neither is a "export for web" hand-over
/// format). Named for the owner in the round doc's veto list, not silently dropped.
///
/// `index()` / `from_index()` exist because Slint's segmented control speaks `int`; they are the
/// ONLY translation between the enum and the sheet, so a stop can never mean two things. Any
/// out-of-range int reads as `Jpeg` -- a settings file hand-edited to `7` gets the shipped format,
/// never a panic.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebFormat {
    /// Lossy JPEG at the sheet's quality tier, carrying the embedded sRGB ICC profile.
    #[default]
    Jpeg,
    /// Lossless PNG carrying an `sRGB` chunk. The quality tier is not an input.
    Png,
}

impl WebFormat {
    /// The deliverable's file extension, WITHOUT the dot -- the one producer of the suffix that
    /// [`WebFormat::noun`] names in prose.
    pub fn ext(self) -> &'static str {
        match self {
            WebFormat::Jpeg => "jpg",
            WebFormat::Png => "png",
        }
    }
    /// The format's name as a user reads it -- the sheet's value cell and the export log's noun.
    pub fn noun(self) -> &'static str {
        match self {
            WebFormat::Jpeg => "JPG",
            WebFormat::Png => "PNG",
        }
    }
    /// The sheet's Seg index: 0 = JPG, 1 = PNG.
    pub fn index(self) -> i32 {
        match self {
            WebFormat::Jpeg => 0,
            WebFormat::Png => 1,
        }
    }
    /// The inverse of [`WebFormat::index`]. Anything that is not the PNG stop is `Jpeg`, so a
    /// corrupt/hand-edited int degrades to the shipped format instead of failing.
    pub fn from_index(i: i32) -> Self {
        if i == 1 {
            WebFormat::Png
        } else {
            WebFormat::Jpeg
        }
    }
}

/// Resize packed RGB8 to `long` (long side, no upscale), convert `src` → sRGB, optionally stamp
/// `wm`, then encode the ./export deliverable in `fmt` — the "prepare a pick for social media" path.
///
/// v1.0.0-rc EXPORT FORMAT (F1): THE ORDER IS THE SAME FOR BOTH FORMATS, and that is the point of
/// one function rather than two. resize → sRGB → stamp → encode: the watermark raster is
/// sRGB-authored, so it must land in an already-converted image whichever container is written, and
/// `the_mark_never_rides_the_source_transform` runs over BOTH stops for exactly that reason.
/// `quality` is read only by the JPEG arm — PNG is lossless, so the tier is not an input to it,
/// which is what the sheet's dimmed Quality row says in words.
///
/// [`export_web_jpeg`] is the `Jpeg` delegate, kept because callers and rows already name it.
///
/// v0.8.100 (A1): `src` is the shot's SOURCE gamut ([`shot_source_gamut`]). The conversion happens
/// AFTER the downscale (a fraction of the pixels) and BEFORE the watermark stamp — the watermark
/// raster is sRGB-authored, so it must land in an sRGB image, not be dragged through a transform.
/// The deliverable describes itself instead of relying on the consumer's untagged-means-sRGB
/// convention. `src == Srgb` costs nothing (the transform early-outs).
///
/// v0.8.104 (Round-A C3): the embedded profile is [`falcon_color::srgb_icc_for_export`] — the v2
/// **sRGB IEC61966-2.1** profile — not the on-screen `icc_bytes_for_gamut` serializer, whose "Falcon
/// sRGB (exact)" description is right for a CAMetalLayer tag and wrong on a file handed to a client.
/// v0.8.104 (Round-A C4): the convert-BEFORE-stamp ordering below finally has a falsifier —
/// `the_mark_never_rides_the_source_transform` exports a NON-sRGB source WITH a coloured mark and
/// fails the moment the two statements swap.
/// v1.0.0-rc PNG EXPORT (queue item 35): **NOT THE PRODUCTION ENTRY.** The app writes its
/// deliverable through [`export_web_file`], which writes into the `.part`'s sink (the `png` encoder still
/// holds the compressed file whole -- see [`write_png`]); this one collects the same bytes into a `Vec`
/// and is kept because this crate's own rows drive it (nothing under `spikes` or `examples` does) -- and
/// because [`export_web_jpeg`] is its historical name. The buffer is taken BY VALUE (round 35): the
/// `rgb.to_vec()` that stood here was one of the export's three full-size copies.
#[allow(clippy::too_many_arguments)]
pub fn export_web_image(
    rgb: Vec<u8>,
    w: u32,
    h: u32,
    long: u32,
    quality: u8,
    wm: Option<&Watermark>,
    src: falcon_color::Gamut,
    fmt: WebFormat,
) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let spec = WebSpec { long, quality, wm, src, fmt };
    export_web_to(Pixels::Rgb8(rgb), w, h, &spec, &mut out)?;
    Ok(out)
}

/// v1.0.0-rc PNG EXPORT (queue item 35): **THE ./export DELIVERABLE'S RECIPE, AS ONE VALUE** --
/// everything about the OUTPUT that is not the pixels or their size.
///
/// It exists because the round's two new entries would otherwise carry nine arguments each, and a
/// nine-argument call is a place where two of them get swapped one day. `export_web_image` keeps
/// its flat argument list: the spikes, the examples and four of this crate's own colour rows call
/// it by that shape, and its own historical name [`export_web_jpeg`] does too.
pub struct WebSpec<'a> {
    /// The output's long side in pixels. Never upscales; the sheet's "Full" stop is the `100000`
    /// sentinel, which no photograph reaches.
    pub long: u32,
    /// The JPEG quality tier. Read by the `Jpeg` arm only -- PNG is lossless, so the tier is not an
    /// input to it, which is what the sheet's muted Quality row says in words.
    pub quality: u8,
    /// The mark to stamp, already loaded and already converted to sRGB.
    pub wm: Option<&'a Watermark>,
    /// The SHOT's source gamut, so the deliverable can be converted down to sRGB and tagged.
    pub src: falcon_color::Gamut,
    /// Which container the deliverable is written in -- and, through [`Keep::for_web`], whether the
    /// decode that produced these pixels was allowed to keep the file's alpha and depth.
    pub fmt: WebFormat,
}

/// v1.0.0-rc PNG EXPORT (queue item 35): **THE PRODUCTION ENTRY** -- the ./export deliverable, written
/// straight to `dst` (the run's instance-unique `.part` sibling, which it then renames onto the
/// final name; the atomic-replace contract is unchanged and lives in the caller).
///
/// Returns the deliverable's size in bytes, read from the file itself after the flush -- which is
/// the number the export log prints, and the number a photographer's disk agrees with.
///
/// This is where the memory pass lands (round 35, Q6): the pixels arrive BY VALUE and are never
/// copied again, and the PNG arm's encoder writes into a `BufWriter` on this path instead of a
/// `Vec<u8>` held whole. A Full-size 16-bit RGBA deliverable is ~250 MB of file; before this round
/// the shape held the source, a rotate copy, a resize copy AND the encoded bytes at once.
pub fn export_web_file(
    px: Pixels,
    w: u32,
    h: u32,
    spec: &WebSpec<'_>,
    dst: &std::path::Path,
) -> Result<u64> {
    use std::io::Write;
    let file = std::fs::File::create(dst)?;
    let mut sink = std::io::BufWriter::new(file);
    export_web_to(px, w, h, spec, &mut sink)?;
    sink.flush().context("flush the deliverable")?;
    let n = sink.get_ref().metadata().context("size the deliverable")?.len();
    Ok(n)
}

/// The ONE body of [`export_web_image`] and [`export_web_file`]: resize, convert, stamp, encode into
/// `sink`. Both entries exist so the production path can write into the file's sink and the rows can hold the bytes.
fn export_web_to<W: std::io::Write>(
    px: Pixels,
    w: u32,
    h: u32,
    spec: &WebSpec<'_>,
    sink: &mut W,
) -> Result<()> {
    let (mut out, ow, oh) = resize_pixels_to_long(px, w, h, spec.long.max(16))?;
    if export_color_action(ExportTarget::Web, spec.src) == ExportColorAction::ConvertToSrgb {
        transform_pixels(&mut out, spec.src, falcon_color::Gamut::Srgb);
    }
    if let Some(wm) = spec.wm {
        stamp_watermark_pixels(&mut out, ow, oh, wm)?;
    }
    match spec.fmt {
        // Built per call: ~2.4 KB of serialization against a multi-hundred-KB JPEG encode.
        WebFormat::Jpeg => {
            let icc = falcon_color::srgb_icc_for_export();
            // JPEG carries neither alpha nor depth, so the JPG stop asks for `Keep::NONE` and what
            // arrives here is always RGB8; a wider buffer is an internal error, not a silent flatten.
            let bytes =
                encode_jpeg(&out.into_rgb8()?, ow, oh, spec.quality.clamp(1, 100), Some(&icc))?;
            sink.write_all(&bytes).context("write the JPEG deliverable")?;
            Ok(())
        }
        // No ICC: PNG says sRGB with its own chunk (see [`write_png`]), and `quality` is not an
        // input to a lossless encoder.
        WebFormat::Png => write_png(out, ow, oh, sink),
    }
}

/// The [`WebFormat::Jpeg`] arm of [`export_web_image`], by its historical name. Kept rather than
/// renamed: the spikes, the examples and four of this crate's own colour rows call it, and every one
/// of them is asking about the JPEG pipeline specifically.
pub fn export_web_jpeg(
    rgb: &[u8],
    w: u32,
    h: u32,
    long: u32,
    quality: u8,
    wm: Option<&Watermark>,
    src: falcon_color::Gamut,
) -> Result<Vec<u8>> {
    export_web_image(rgb.to_vec(), w, h, long, quality, wm, src, WebFormat::Jpeg)
}

/// Composite a watermark onto a COPY of a packed-RGB8 image and return it — for the export
/// sheet's live preview (decode a small sample once, then recomposite cheaply on each change).
pub fn preview_with_watermark(rgb: &[u8], w: u32, h: u32, wm: Option<&Watermark>) -> Vec<u8> {
    let mut out = rgb.to_vec();
    if let Some(wm) = wm {
        let _ = stamp_watermark(&mut out, w, h, wm);
    }
    out
}

/// The composited watermark's pixel rect `(x, y, w, h)` on a `w`×`h` image — the sizing
/// (image long-side × scale, logo aspect preserved) and centre-then-clamp placement are the
/// SINGLE source of truth shared with `stamp_watermark`, exported so the export sheet's preview
/// can draw the bounding box without duplicating the math (W3a). Zero-sized logo → zero rect.
pub fn watermark_rect(w: u32, h: u32, wm: &Watermark) -> (i64, i64, u32, u32) {
    if wm.w == 0 || wm.h == 0 || w == 0 || h == 0 {
        return (0, 0, 0, 0);
    }
    let out_long = w.max(h) as f32;
    let target_long = (out_long * wm.scale.clamp(0.01, 1.0)).round().max(1.0) as u32;
    let (lw, lh) = if wm.w >= wm.h {
        (target_long, ((target_long as u64 * wm.h as u64) / wm.w as u64).max(1) as u32)
    } else {
        (((target_long as u64 * wm.w as u64) / wm.h as u64).max(1) as u32, target_long)
    };
    // Centre at (pos_x·w, pos_y·h), then clamp the top-left so the logo stays fully on-image.
    let cx = (wm.pos_x.clamp(0.0, 1.0) * w as f32).round() as i64;
    let cy = (wm.pos_y.clamp(0.0, 1.0) * h as f32).round() as i64;
    let ox = (cx - lw as i64 / 2).clamp(0, (w as i64 - lw as i64).max(0));
    let oy = (cy - lh as i64 / 2).clamp(0, (h as i64 - lh as i64).max(0));
    (ox, oy, lw, lh)
}

/// v1.0.0-rc PNG EXPORT (queue item 35): stamp the mark onto a [`Pixels`] of any layout.
///
/// The `Rgb8` arm is the SHIPPED [`stamp_watermark`], reached with its own bytes and not one
/// statement rewritten, because an opaque 8-bit destination is what every deliverable was before
/// this round and nothing about it should move. The other three go through
/// [`stamp_watermark_wide`], whose arithmetic is the real source-over: a destination that carries
/// alpha needs `a_out` written, and the shipped formula has no `a_out` to write.
///
/// `the_wide_stamp_is_the_shipped_arithmetic_over_rgb8` is what keeps the two from drifting: it
/// runs the wide body over an `Rgb8` buffer and asserts byte equality with this arm.
fn stamp_watermark_pixels(px: &mut Pixels, w: u32, h: u32, wm: &Watermark) -> Result<()> {
    match px {
        Pixels::Rgb8(v) => stamp_watermark(v, w, h, wm),
        other => stamp_watermark_wide(other, w, h, wm),
    }
}

/// One destination sample, at whichever depth the deliverable is -- so the source-over below is one
/// body rather than three near-copies.
trait StampSample: Copy {
    /// The value a fully-opaque / fully-bright sample takes.
    const FULL: f32;
    fn to_f32(self) -> f32;
    fn from_f32(v: f32) -> Self;
}

impl StampSample for u8 {
    const FULL: f32 = 255.0;
    #[inline]
    fn to_f32(self) -> f32 {
        self as f32
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        v.round().clamp(0.0, 255.0) as u8
    }
}

impl StampSample for u16 {
    const FULL: f32 = 65535.0;
    #[inline]
    fn to_f32(self) -> f32 {
        self as f32
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        v.round().clamp(0.0, 65535.0) as u16
    }
}

/// v1.0.0-rc PNG EXPORT (queue item 35): the watermark composite over a destination that may carry
/// ALPHA, may be 16-bit, or both.
///
/// **THE MARK IS VISIBLE OVER A TRANSPARENT REGION**, at its own opacity (round 35, Q5; the owner's
/// veto list names the alternative). Over an RGBA destination this is real straight-alpha
/// source-over -- `a_out = a_s + a_d(1 - a_s)`, `c_out = (c_s a_s + c_d a_d (1 - a_s)) / a_out`,
/// with `a_out == 0` meaning transparent black -- because a mark composited into a transparent hole
/// with the shipped formula would write colour under an alpha of zero, i.e. nothing. A watermark
/// says "mark here"; the file should say it too.
///
/// Over an RGB destination (`ch == 3`) the else-branch is the SHIPPED arithmetic written at the
/// destination's own scale, which at `FULL == 255` is the shipped expression exactly.
///
/// The mark's own raster stays RGBA8 -- it is an 8-bit overlay by design -- and its samples are
/// lifted to the destination scale by the `/ 255.0 * FULL` on `c_s`, i.e. x257 at 16 bits.
fn stamp_watermark_wide(px: &mut Pixels, w: u32, h: u32, wm: &Watermark) -> Result<()> {
    if wm.w == 0 || wm.h == 0 || (wm.rgba.len() as u64) < (wm.w as u64 * wm.h as u64 * 4) {
        return Ok(());
    }
    // The SAME sizing and centre-then-clamp placement `stamp_watermark` uses, from the one copy of
    // that math (`watermark_rect`), and the same Lanczos resize of the logo at `U8x4`.
    let (ox, oy, lw, lh) = watermark_rect(w, h, wm);
    let src = Image::from_vec_u8(wm.w, wm.h, wm.rgba.clone(), PixelType::U8x4)?;
    let mut dst = Image::new(lw, lh, PixelType::U8x4);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    Resizer::new().resize(&src, &mut dst, &opts)?;
    let logo = dst.into_vec();
    let op = wm.opacity.clamp(0.0, 1.0);
    let ch = px.channels();
    let r = StampRect { ox, oy, lw, lh };
    match px {
        Pixels::Rgb8(v) | Pixels::Rgba8(v) => stamp_over(v, w, h, ch, &logo, r, op),
        Pixels::Rgb16(v) | Pixels::Rgba16(v) => stamp_over(v, w, h, ch, &logo, r, op),
    }
    Ok(())
}

/// Where the resized mark sits on the destination, as [`watermark_rect`] answered it: the four
/// numbers travel together because they are one answer.
struct StampRect {
    ox: i64,
    oy: i64,
    lw: u32,
    lh: u32,
}

/// [`stamp_watermark_wide`]'s loop, over one destination sample type. `ch` is 3 (no destination
/// alpha: the shipped blend) or 4 (source-over, with `a_out` written).
fn stamp_over<T: StampSample>(
    dst: &mut [T],
    w: u32,
    h: u32,
    ch: usize,
    logo: &[u8],
    r: StampRect,
    op: f32,
) {
    let StampRect { ox, oy, lw, lh } = r;
    let full = T::FULL;
    for ly in 0..lh as i64 {
        let py = oy + ly;
        if py < 0 || py >= h as i64 {
            continue;
        }
        for lx in 0..lw as i64 {
            let px = ox + lx;
            if px < 0 || px >= w as i64 {
                continue;
            }
            let li = ((ly as usize * lw as usize) + lx as usize) * 4;
            let a_s = (logo[li + 3] as f32 / 255.0) * op;
            if a_s <= 0.0 {
                continue;
            }
            let di = ((py as usize * w as usize) + px as usize) * ch;
            if ch == 4 {
                let a_d = dst[di + 3].to_f32() / full;
                let a_out = a_s + a_d * (1.0 - a_s);
                if a_out <= 0.0 {
                    for c in 0..4 {
                        dst[di + c] = T::from_f32(0.0);
                    }
                    continue;
                }
                for c in 0..3 {
                    let c_s = (logo[li + c] as f32 / 255.0) * full;
                    let c_d = dst[di + c].to_f32();
                    dst[di + c] = T::from_f32((c_s * a_s + c_d * a_d * (1.0 - a_s)) / a_out);
                }
                dst[di + 3] = T::from_f32(a_out * full);
            } else {
                for c in 0..3 {
                    let c_s = (logo[li + c] as f32 / 255.0) * full;
                    let c_d = dst[di + c].to_f32();
                    dst[di + c] = T::from_f32(c_d * (1.0 - a_s) + c_s * a_s);
                }
            }
        }
    }
}

/// Alpha-composite the (resized) watermark onto a packed-RGB8 image at its precise position.
fn stamp_watermark(rgb: &mut [u8], w: u32, h: u32, wm: &Watermark) -> Result<()> {
    // u64 product: even bounded logos are large, and a future unbounded Watermark source shouldn't be
    // able to wrap this length check (u32 w*h*4 overflows past ~1 G-px). (falcon-security-backlog #1.)
    if wm.w == 0 || wm.h == 0 || (wm.rgba.len() as u64) < (wm.w as u64 * wm.h as u64 * 4) {
        return Ok(());
    }
    // Sizing + centre/clamp placement come from `watermark_rect` — the ONE copy of this math,
    // shared with the export sheet's bounding-box overlay (W3a).
    let (ox, oy, lw, lh) = watermark_rect(w, h, wm);
    let src = Image::from_vec_u8(wm.w, wm.h, wm.rgba.clone(), PixelType::U8x4)?;
    let mut dst = Image::new(lw, lh, PixelType::U8x4);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    Resizer::new().resize(&src, &mut dst, &opts)?;
    let logo = dst.into_vec();
    let op = wm.opacity.clamp(0.0, 1.0);
    for ly in 0..lh as i64 {
        let py = oy + ly;
        if py < 0 || py >= h as i64 {
            continue;
        }
        for lx in 0..lw as i64 {
            let px = ox + lx;
            if px < 0 || px >= w as i64 {
                continue;
            }
            let li = ((ly as usize * lw as usize) + lx as usize) * 4;
            let a = (logo[li + 3] as f32 / 255.0) * op;
            if a <= 0.0 {
                continue;
            }
            let di = ((py as usize * w as usize) + px as usize) * 3;
            for c in 0..3 {
                let s = rgb[di + c] as f32;
                let d = logo[li + c] as f32;
                rgb[di + c] = (s * (1.0 - a) + d * a).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Ok(())
}

/// Load a watermark logo PNG to packed RGBA8 **in sRGB**, plus the gamut the FILE declared.
///
/// v0.8.104 (Round-A C5): the mark's colour space used to be an unchecked assumption. v0.8.100 wrote
/// `Gamut::Srgb` into the preview transform as a literal and the export stamps `wm_cache` straight
/// into an sRGB file — but [`load_png_rgba`] never looked at the PNG's `iCCP`/`sRGB` chunks, and a
/// logo exported from any modern macOS design tool (Figma, Sketch, Affinity, Photoshop on a P3 Mac)
/// is routinely tagged **Display P3**. A brand red then came out visibly desaturated in every ./export
/// deliverable, with nothing in the log saying so.
///
/// The conversion happens ONCE, HERE, at load — so `wm_cache` stays the sRGB raster the export
/// stamps and the on-screen preview's `Srgb → output` hop keeps its meaning. Both surfaces are fixed
/// by the same line, and neither can drift from the other.
///
/// The returned gamut is what the FILE said (for the caller's log line), not what the pixels are
/// now: after this returns they are always sRGB. An untagged PNG keeps the sRGB assumption — that is
/// the web-wide convention and the only safe guess — and reports `Srgb`, i.e. "nothing to convert".
pub fn load_watermark_png_srgb(
    path: &std::path::Path,
) -> Result<(Vec<u8>, u32, u32, falcon_color::Gamut)> {
    let (mut rgba, w, h, tag) = load_png_rgba_tag(path)?;
    // v0.8.140 (C2): the logo's own PROFILE decides, not its profile's name. This door's doc comment
    // has worried since v0.8.104 that a mark exported from a macOS design tool is routinely tagged
    // Display P3 — and a mark exported through a CALIBRATED macOS setup carries a display-derived
    // profile whose name is not "Display P3" at all, which is the case the name match kept missing.
    let src = resolve_source_gamut_noting(&tag, path);
    // No-op when the file is already sRGB (or untagged) — `transform_rgba` early-outs on src == dst.
    falcon_color::transform_rgba(&mut rgba, src, falcon_color::Gamut::Srgb);
    Ok((rgba, w, h, src))
}

/// Load a PNG (e.g. the user's watermark logo) to packed RGBA8.
pub fn load_png_rgba(path: &std::path::Path) -> Result<(Vec<u8>, u32, u32)> {
    let (rgba, w, h, _) = load_png_rgba_tag(path)?;
    Ok((rgba, w, h))
}

/// [`load_png_rgba`] plus the file's own colour-space DESCRIPTION, read out of the same decoder pass
/// (`iCCP` first, then the bare `sRGB` chunk) — the identical precedence [`png_color_tag`] uses for
/// photos, so a logo and a photo in the same colour space are read the same way.
fn load_png_rgba_tag(path: &std::path::Path) -> Result<(Vec<u8>, u32, u32, ColorTag)> {
    let file = std::fs::File::open(path)?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    // v1.0.0-rc PNG EXPORT (queue item 35, Q9): the normalisation `decode_png_rgb` has always set,
    // which this door did not. Without it a 16-bit RGBA logo came back as `w*h*8` bytes LABELLED
    // RGBA8 -- and `stamp_watermark`'s guard (`len < w*h*4`) passes such a buffer, so the mark was
    // stamped from the wrong bytes at the wrong stride with nothing in the log. `STRIP_16` because
    // the mark is an 8-bit overlay by design (its raster is composited at `/ 255.0`), and `EXPAND`
    // because a palette logo should be expanded rather than refused.
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    // Bound the up-front allocation so a crafted/huge watermark PNG (a decompression bomb: tiny IDAT,
    // enormous declared dims) can't force a multi-GB `vec![0u8; output_buffer_size()]` → OOM. Mirrors
    // the same guard in decode_png_rgb; real logos are far under the cap. (falcon-security-backlog #1.)
    let (w0, h0, desc) = {
        let i = reader.info();
        // v0.8.104 (C5): the file's declared colour space, in the same precedence `png_color_tag`
        // applies to photo PNGs — an embedded profile's description first, else the bare `sRGB` chunk.
        // v0.8.140 (C2): and its BYTES, which are what actually decide the gamut.
        let mut t = i.icc_profile.as_ref().map(|icc| ColorTag::from_icc(icc.to_vec())).unwrap_or_default();
        if t.desc.is_none() {
            // v0.8.141 (R7): a container-declared name, not the profile's — `desc_from_profile`
            // stays false, exactly as in `png_color_tag`, whose precedence this mirrors.
            t.desc = i.srgb.map(|_| "sRGB".to_string());
        }
        (i.width, i.height, t)
    };
    guard_source_dims(w0, h0, "watermark PNG")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::GrayscaleAlpha => {
            buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect()
        }
        // `EXPAND` (above) turns palette into RGB/RGBA, so this arm is unreachable in practice --
        // the same standing `decode_png_rgb`'s `Indexed` arm has. Kept as an honest refusal rather
        // than an `unreachable!`.
        png::ColorType::Indexed => {
            bail!("indexed-palette PNG not supported for the watermark — re-export it as RGBA")
        }
    };
    Ok((rgba, w, h, desc))
}

/// Rasterise `text` into a trimmed packed-RGBA8 strip at glyph size `px` — the export-mode entry
/// of the ONE text rasteriser (v0.8.90) behind the 480 px export watermark
/// ([`text_watermark_rgba`], which wraps this). Trims to the inked bounding box on BOTH axes.
/// v0.8.91: the font-picker's typeface-preview strips moved to their own entry,
/// [`render_text_strip_picker`] (fixed-baseline canvas + width cap + right-edge fade); this
/// entry's output is byte-identical to pre-v0.8.91 (pinned by test). `color` is the glyph fill;
/// the soft dark drop-shadow (offset/pad scale with `px`) is baked in exactly as the export path
/// always did. The extra `all_notdef` return is true when EVERY non-space char had no glyph in
/// this font (symbol fonts — the picker keeps the plain name for those); it never affects the
/// raster itself. Error cases and their messages are the historical `text_watermark_rgba` ones
/// verbatim (the app's log lines surface them).
pub fn render_text_strip(
    font_bytes: &[u8],
    face_index: u32,
    text: &str,
    px: f32,
    color: [u8; 3],
) -> Result<(Vec<u8>, u32, u32, bool)> {
    render_text_strip_impl(font_bytes, face_index, text, px, color, None)
}

/// v0.8.91 (picker fix B): width of the right-edge alpha fade-out composited into a strip that
/// was CAPPED at the caller's row budget — the image-native equivalent of the plain rows' `…`
/// elide (no font-dependent ellipsis glyph, no mid-glyph hard cut). Raster px.
pub const STRIP_FADE_PX: u32 = 24;

/// v0.8.91 (picker fixes A+B): the font-picker's OWN strip entry — same rasteriser core as the
/// export watermark, but with two picker-only behaviours the export path never sees (the 480 px
/// export raster stays byte-identical through [`render_text_strip`], pinned by test):
///
/// A) FIXED-BASELINE canvas, font-INDEPENDENT (derived from `px` only), so every family's strip
///    shares ONE height and ONE baseline y and the picker's own-height centering can't wobble
///    row-to-row (descender names) or jump on the plain→preview swap:
///        baseline B = ceil(1.10·px) — clears Windows UI fonts' hhea ascents (Segoe UI: 1.079 em;
///                     rare diacritic outliers clip harmlessly at the canvas top),
///        descent box D = ceil(0.28·px) + shadow — real descents (~0.25 em) + the baked
///                     drop-shadow's downward offset,
///        canvas H = B + D; the strip is trimmed HORIZONTALLY only.
///    At the picker's px = 24 (2× strips displayed at half size in a 30 px row): B = 27, D = 8,
///    H = 35 → displayed baseline = (30 − 35/2)/2 + 27/2 = 19.75 logical px, vs the plain Text
///    rows' metric centering ≈ (30 − 1.33·12)/2 + 1.079·12 ≈ 19.97 (Segoe UI hhea at font-md
///    12) — a ~0.22 px residual: sub-pixel, visually the same line.
///
/// B) WIDTH CAP + honest truncation: a strip wider than `max_w` raster px is cut at `max_w`
///    (from the left/text start) and its last [`STRIP_FADE_PX`] columns get a linear alpha
///    fade-out. Uncapped strips are byte-unchanged. `max_w = 0` disables the cap.
pub fn render_text_strip_picker(
    font_bytes: &[u8],
    face_index: u32,
    text: &str,
    px: f32,
    color: [u8; 3],
    max_w: u32,
) -> Result<(Vec<u8>, u32, u32, bool)> {
    render_text_strip_impl(font_bytes, face_index, text, px, color, Some(max_w))
}

/// The shared rasteriser core. `picker_cap = None` is the HISTORICAL export path, expression-for-
/// expression identical to the pre-v0.8.91 body; its raster is byte-identical to that body and
/// pinned FOR REAL by `render_text_strip_matches_the_export_watermark_at_480px`, which diffs this
/// None-path against a FROZEN in-test copy of the pre-split rasteriser (an independent oracle, not
/// a forward back into this fn), so any future change to the None-path here breaks that test.
/// `Some(max_w)` switches on the picker's fixed-baseline canvas + width cap + fade (see
/// [`render_text_strip_picker`]). Every mode difference is an explicit branch below.
fn render_text_strip_impl(
    font_bytes: &[u8],
    face_index: u32,
    text: &str,
    px: f32,
    color: [u8; 3],
    picker_cap: Option<u32>,
) -> Result<(Vec<u8>, u32, u32, bool)> {
    use ab_glyph::{point, Font, FontRef, ScaleFont};

    // Bound work: trim, and cap length so a pathological paste can't allocate a huge buffer.
    let text: String = text.trim().chars().take(64).collect();
    if text.is_empty() {
        bail!("empty watermark text");
    }
    let font = FontRef::try_from_slice_and_index(font_bytes, face_index)
        .map_err(|e| anyhow::anyhow!("font parse: {e}"))?;
    let sf = font.as_scaled(px);
    let shadow = (px * 0.04).round().max(1.0) as i32; // drop-shadow offset, in render px
    let pad = (px * 0.10).ceil() as i32 + shadow;
    // Picker fixed-baseline geometry (fix A; see render_text_strip_picker for the derivation).
    let fixed = picker_cap.is_some();
    let ascent_box = (px * 1.10).ceil() as i32; // B: baseline y from the canvas top
    let descent_box = (px * 0.28).ceil() as i32 + shadow; // D: room below the baseline

    // Lay glyphs along the baseline, with kerning. Export mode: baseline at y = ascent (the
    // canvas then pads both axes). Picker mode: baseline at the FIXED y = B (vertical pad = 0 —
    // the canvas height is the constant B + D). The `notdef`/`total` counters ONLY feed the
    // `all_notdef` flag — the skip logic itself is the historical path, byte-identical.
    let baseline = if fixed { ascent_box as f32 } else { sf.ascent() };
    let mut pen = 0.0_f32;
    let mut glyphs = Vec::new();
    let mut prev = None;
    let mut nonspace_total = 0usize;
    let mut nonspace_notdef = 0usize;
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if ch != ' ' {
            nonspace_total += 1;
            if id.0 == 0 {
                nonspace_notdef += 1;
            }
        }
        // Skip glyphs the font can't render (id 0 = .notdef) — else a char with no glyph (e.g. an
        // emoji variation-selector U+FE0F pasted after "©") draws as a "tofu" rectangle box.
        if id.0 == 0 {
            continue;
        }
        if let Some(p) = prev {
            pen += sf.kern(p, id);
        }
        glyphs.push(id.with_scale_and_position(px, point(pen, baseline)));
        pen += sf.h_advance(id);
        prev = Some(id);
    }
    let all_notdef = nonspace_total > 0 && nonspace_notdef == nonspace_total;
    let text_w = pen.ceil().max(1.0) as i32;
    let text_h = (sf.ascent() - sf.descent()).ceil().max(1.0) as i32;
    let w = (text_w + 2 * pad) as usize;
    let h = if fixed { (ascent_box + descent_box) as usize } else { (text_h + 2 * pad) as usize };
    let y_off = if fixed { 0 } else { pad }; // picker: glyphs already positioned in canvas space

    // Accumulate glyph coverage (anti-aliased alpha) into a single plane. (The bounds guard
    // clips a rare over-tall ascender/diacritic at the picker canvas top — harmless.)
    let mut cov = vec![0f32; w * h];
    for g in glyphs {
        if let Some(og) = font.outline_glyph(g) {
            let bb = og.px_bounds();
            og.draw(|gx, gy, c| {
                let x = bb.min.x as i32 + gx as i32 + pad;
                let y = bb.min.y as i32 + gy as i32 + y_off;
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                    let idx = y as usize * w + x as usize;
                    cov[idx] = (cov[idx] + c).min(1.0);
                }
            });
        }
    }

    // Composite a dark drop-shadow (coverage sampled up-left), then the coloured text over it.
    let so = shadow as usize;
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let t = cov[y * w + x];
            let s = if x >= so && y >= so { cov[(y - so) * w + (x - so)] } else { 0.0 };
            let sa = s * 0.55; // shadow strength under the glyph
            // shadow is black; text is `color`, painted over the shadow with alpha = coverage.
            let r = color[0] as f32 * t;
            let gg = color[1] as f32 * t;
            let b = color[2] as f32 * t;
            let a = t + sa * (1.0 - t);
            let i = (y * w + x) * 4;
            rgba[i] = r.round().clamp(0.0, 255.0) as u8;
            rgba[i + 1] = gg.round().clamp(0.0, 255.0) as u8;
            rgba[i + 2] = b.round().clamp(0.0, 255.0) as u8;
            rgba[i + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    // Trim to the non-transparent bounds so size == the real text extent (not the padded box).
    // Picker mode trims HORIZONTALLY only — the fixed canvas height IS the contract (fix A).
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            if rgba[(y * w + x) * 4 + 3] > 0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x1 < x0 || y1 < y0 {
        bail!("watermark text produced no pixels");
    }
    if fixed {
        y0 = 0;
        y1 = h - 1;
    }
    let (mut cw, ch) = ((x1 - x0 + 1), (y1 - y0 + 1));
    // Fix B: cap the strip at the caller's row budget (keep the text START; fade the cut edge).
    let capped = match picker_cap {
        Some(maxw) if maxw > 0 && cw > maxw as usize => {
            cw = maxw as usize;
            true
        }
        _ => false,
    };
    let mut out = vec![0u8; cw * ch * 4];
    for row in 0..ch {
        let src = ((y0 + row) * w + x0) * 4;
        let dst = row * cw * 4;
        out[dst..dst + cw * 4].copy_from_slice(&rgba[src..src + cw * 4]);
    }
    if capped {
        // Linear right-edge alpha fade over the last STRIP_FADE_PX columns: factor runs 1 → 1/N
        // toward the cut edge (near-invisible at the edge). Alpha-only — the Slint side blends
        // straight-alpha, so scaling A fades the paint without shifting the glyph colour.
        let fade = (STRIP_FADE_PX as usize).min(cw);
        for row in 0..ch {
            for i in 0..fade {
                let col = cw - fade + i;
                let f = (fade - i) as f32 / fade as f32;
                let a = &mut out[(row * cw + col) * 4 + 3];
                *a = (*a as f32 * f).round() as u8;
            }
        }
    }
    Ok((out, cw as u32, ch as u32, all_notdef))
}

/// Rasterise `text` into a packed-RGBA8 "logo" the watermark stamp can place exactly like a
/// PNG — so a Text watermark reuses the whole Image pipeline (opacity / size / position / drag /
/// preview / export). `color` is the glyph fill; a soft dark drop-shadow is baked in so light
/// text stays legible over a bright photo. `font_bytes` is any TTF/OTF (the app passes a system
/// font). The result is trimmed to the inked bounding box, so the stamp's `scale` (fraction of
/// the output long edge) maps to the real text extent. `None`/error on empty text or a bad font.
/// v0.8.90: a thin wrapper over [`render_text_strip`] at the historical 480 px (render large so
/// the downstream resize sharpens rather than upscales for most exports) — output byte-identical.
pub fn text_watermark_rgba(text: &str, color: [u8; 3], font_bytes: &[u8], face_index: u32) -> Result<(Vec<u8>, u32, u32)> {
    let (rgba, w, h, _all_notdef) = render_text_strip(font_bytes, face_index, text, 480.0, color)?;
    Ok((rgba, w, h))
}

/// Offset of the largest embedded JPEG (by decoded area) in a raw container, found by probing SOI
/// markers. Hardened against a crafted-RAW O(n) header-parse DoS: it (a) SKIPS PAST each JPEG it
/// validates (to its EOI) so it never re-probes the inner `FF D8 FF` byte runs in entropy data, and
/// (b) caps how many candidate headers it parses. A real raw has a handful of previews near the
/// start, so both the real previews are still found and the worst case is bounded. (`spikes/real_bench`.)
fn largest_embedded_jpeg(buf: &[u8]) -> Option<usize> {
    const MAX_CANDIDATES: usize = 256; // far more than any real raw's preview count; bounds the DoS
    // A1/F8 (CODEBASE_REVIEW_2026-07 §5, defect #3): MAX_CANDIDATES bounds the parse COUNT, but a
    // crafted RAW with many valid SOI+SOF headers whose EOI marker is ABSENT makes each candidate's
    // `find_marker` scan run all the way to the buffer end — up to candidates × len bytes (~150 GB for
    // 256 × 600 MB) of pure scanning, minutes of worker stall. A real raw's previews sit near the start
    // and every one carries an EOI, so `find_marker` jumps to it and the successive scans are
    // NON-OVERLAPPING (cumulative ≤ len). Cap the cumulative EOI-scan bytes at a few whole-file passes:
    // a genuine file never hits it, a crafted file bails with the best preview found so far.
    let scan_budget = buf.len().saturating_mul(4).max(64 * 1024 * 1024);
    let mut scanned = 0usize;
    let mut best: Option<(usize, usize)> = None; // (offset, area)
    let mut i = 0usize;
    let mut probed = 0usize;
    while i + 3 < buf.len() && probed < MAX_CANDIDATES && scanned < scan_budget {
        if buf[i] == 0xFF && buf[i + 1] == 0xD8 && buf[i + 2] == 0xFF {
            probed += 1;
            let mut d = Decoder::new(Cursor::new(&buf[i..]));
            if d.read_info().is_ok() {
                if let Some(info) = d.info() {
                    let area = info.width as usize * info.height as usize;
                    if best.map_or(true, |(_, a)| area > a) {
                        best = Some((i, area));
                    }
                }
                // Skip past this JPEG to its EOI (FF D9): its compressed entropy data is full of
                // FF D8 FF-looking bytes, and re-probing each of them is the DoS. Jump over it. Charge
                // the scan distance (found OR to-end) against the cumulative budget above.
                match find_marker(buf, i + 3, 0xD9) {
                    Some(eoi) => {
                        scanned += eoi.saturating_sub(i + 3);
                        i = eoi + 2;
                        continue;
                    }
                    None => scanned += buf.len().saturating_sub(i + 3),
                }
            }
        }
        i += 1;
    }
    best.map(|(off, _)| off)
}

/// Exclusive end of one JPEG. Segment lengths protect embedded EXIF thumbnails and ICC
/// payloads from being mistaken for the outer EOI; entropy accepts stuffing, restart
/// markers and further progressive scans. An incomplete/invalid walk declines trimming.
fn embedded_jpeg_end(buf: &[u8], start: usize) -> Option<usize> {
    if buf.get(start..start.checked_add(2)?)? != [0xff, 0xd8] { return None; }
    let mut p = start + 2;
    let mut scan = false;
    loop {
        if scan {
            while *buf.get(p)? != 0xff { p += 1; }
        }
        if *buf.get(p)? != 0xff { return None; }
        while *buf.get(p)? == 0xff { p += 1; }
        let marker = *buf.get(p)?;
        p += 1;
        match marker {
            0xd9 => return Some(p),
            0x00 if scan => continue, // entropy byte stuffing
            0xd0..=0xd7 if scan => continue, // restart, still inside this scan
            0x01 => continue, // standalone TEM marker
            0x00 | 0xd8 | 0xd0..=0xd7 => return None,
            _ => {
                let size = u16::from_be_bytes([*buf.get(p)?, *buf.get(p + 1)?]) as usize;
                if size < 2 { return None; }
                p = p.checked_add(size)?;
                if p > buf.len() { return None; }
                // DNL may occur inside entropy without ending that scan.
                scan = marker == 0xda || (marker == 0xdc && scan);
            }
        }
    }
}

/// Find the next `FF <marker>` byte pair at/after `from`. Used to skip past a JPEG's EOI.
fn find_marker(buf: &[u8], from: usize, marker: u8) -> Option<usize> {
    let mut i = from;
    while i + 1 < buf.len() {
        if buf[i] == 0xFF && buf[i + 1] == marker {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `fin` = the finished-image format label ("JPG"/"PNG"/"TIFF"/"HEIC", from `Shot::finished_format`)
/// so a TIFF/PNG/HEIC shot's Files row reads "TIFF 93 KB", not a hardcoded "JPG".
///
/// v1.0.0-rc TAIL (skeptic B, O1 — OWNER RULING): `named` carries the extension the file WEARS when
/// its bytes say something else, and the row discloses it — `PNG 872 KB (named .JPG)`. This is the
/// one surface whose whole job is answering "what IS this file", so it is the one surface that
/// reconciles the contradiction the badge, the menu and the toast otherwise leave the reader
/// holding. `None` — which is every file whose name and bytes agree — renders exactly as before.
fn fmt_files(raw: Option<u64>, jpg: Option<u64>, fin: &str, named: Option<&str>) -> Option<String> {
    // Sub-MB files (small JPGs, web exports, scan TIFFs) would round to "0 MB"; show KB instead.
    let mb = |n: u64| {
        if n < 1_000_000 {
            format!("{} KB", (n as f64 / 1_000.0).round() as u64)
        } else {
            format!("{:.0} MB", n as f64 / 1_000_000.0)
        }
    };
    // The qualifier rides the FINISHED side only: it is a statement about the finished file's
    // format, and a RAW-only row has no finished file to qualify.
    let qual = named.map(|e| format!(" (named .{e})")).unwrap_or_default();
    match (raw, jpg) {
        (Some(r), Some(j)) => Some(format!("RAW {} · {} {}{qual}", mb(r), fin, mb(j))),
        (Some(r), None) => Some(format!("RAW {}", mb(r))),
        (None, Some(j)) => Some(format!("{} {}{qual}", fin, mb(j))),
        (None, None) => None,
    }
}

/// EXIF string values sometimes arrive wrapped in quotes; strip them.
fn trim_quotes(s: &str) -> String {
    s.trim().trim_matches('"').trim().to_string()
}

/// Round a positive number to `sig` significant digits, trimming trailing zeros.
fn round_sig(x: f64, sig: i32) -> String {
    if !x.is_finite() || x == 0.0 {
        return format!("{x}");
    }
    let int_digits = x.abs().log10().floor() as i32 + 1;
    let dec = (sig - int_digits).max(0) as usize;
    let s = format!("{x:.dec$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// Tidy a shutter-speed string. kamadak renders `ExposureTime` as `1/<denominator>`, and
/// phones often store an imprecise rational that yields a long fractional denominator
/// (e.g. `1/1000.4324342`). Round the denominator to 5 significant digits (`1/1000.4`);
/// plain integer shutters (`1/125`, `30`) pass through unchanged.
fn fmt_shutter(s: &str) -> String {
    if let Some((num, den)) = s.split_once('/') {
        if let Ok(d) = den.trim().parse::<f64>() {
            return format!("{}/{}", num.trim(), round_sig(d, 5));
        }
    } else if let Ok(v) = s.trim().parse::<f64>() {
        return round_sig(v, 5);
    }
    s.to_string()
}

/// Folded-grid display form of the Shutter ROW value (`"1/1000.4324342 s"` → `"1/1000.4 s"`).
/// The row itself keeps the original value — the expanded panel shows the unrounded truth.
pub fn brief_shutter(v: &str) -> String {
    match v.strip_suffix(" s") {
        Some(core) => format!("{} s", fmt_shutter(core)),
        None => fmt_shutter(v),
    }
}

/// Folded-grid display rounding for a decorated numeric EXIF value: the FIRST number in the
/// string is rounded to 3 significant digits, any prefix/suffix preserved — `"f/1.7799999"` →
/// `"f/1.78"`, `"6.764999999 mm"` → `"6.76 mm"`; clean values (`"f/2.8"`, `"400 mm"`) pass
/// through unchanged. Non-numeric input is returned as-is.
pub fn brief_round(v: &str) -> String {
    let start = match v.find(|c: char| c.is_ascii_digit()) {
        Some(i) => i,
        None => return v.to_string(),
    };
    let end = v[start..]
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map(|i| start + i)
        .unwrap_or(v.len());
    match v[start..end].parse::<f64>() {
        Ok(n) => format!("{}{}{}", &v[..start], round_sig(n, 3), &v[end..]),
        Err(_) => v.to_string(),
    }
}

/// Clean string value for a string-typed EXIF field, robust across camera brands.
///
/// kamadak-exif models an ASCII tag as a vector of NUL-separated components, so a
/// fixed-width field padded with NULs — common for LensModel / Artist / Software /
/// Make / Model on Fuji, Nikon, Panasonic… — renders through `display_value()` as
/// `"XF23mmF1.4 R LM WR", "", "", "", …` (the real value plus dozens of empty parts),
/// and a present-but-unset field (e.g. an empty in-camera Artist) renders as a string
/// of empty quoted parts. Both leaked into the panel. Instead we take the FIRST
/// non-empty component directly from the value; an all-empty field yields `None` (and
/// is dropped). Non-ASCII fields fall back to the trimmed `display_value()`.
fn exif_string(f: &exif::Field) -> Option<String> {
    if let exif::Value::Ascii(parts) = &f.value {
        for p in parts {
            let s = String::from_utf8_lossy(p);
            let s = s.trim().trim_matches('\0').trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
        None
    } else {
        let s = trim_quotes(&f.display_value().to_string());
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

#[cfg(test)]
mod tests {
    /// Restoring jpeg_source's buf[off..].to_vec() retains the multi-MB sensor tail.
    /// A naive FF D9 trim instead cuts inside the APP1 thumbnail and fails byte equality.
    #[test]
    fn raw_preview_source_excludes_sensor_tail_and_preserves_jpeg_segments() {
        for progressive in [false, true] {
            let tiny = super::encode_jpeg_rgb(&[70; 3 * 2 * 2], 2, 2, 90).unwrap();
            let mut jpeg = Vec::new();
            let mut enc = jpeg_encoder::Encoder::new(&mut jpeg, 90);
            enc.set_progressive(progressive);
            enc.encode(&[110; 3 * 24 * 16], 24, 16, jpeg_encoder::ColorType::Rgb).unwrap();
            // APP1 payloads may contain an entire nested JPEG (including its EOI).
            let mut tagged = vec![0xff, 0xd8, 0xff, 0xe1];
            tagged.extend_from_slice(&((tiny.len() + 2) as u16).to_be_bytes());
            tagged.extend_from_slice(&tiny);
            tagged.extend_from_slice(&jpeg[2..]);
            let mut raw = vec![0x55; 64];
            raw.extend_from_slice(&tiny);
            raw.extend_from_slice(&[0x55; 32]);
            raw.extend_from_slice(&tagged);
            raw.resize(raw.len() + 4 * 1024 * 1024, 0x55);
            let dir = std::env::temp_dir().join(format!("falcon_preview_tail_{}_{progressive}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("preview.cr3");
            std::fs::write(&path, raw).unwrap();
            let shot = Shot { id: 0, name: "preview".into(), raw: Some(path), jpg: None,
                kind: SrcKind::Jpeg, has_raw: true, has_jpg: false, cloud_placeholder: false, sniffed: None };
            let extracted = super::jpeg_source(&shot).unwrap();
            assert_eq!(extracted.len(), tagged.len(), "retain only the selected preview");
            assert_eq!(extracted, tagged, "metadata and every progressive scan stay intact");
            assert!(extracted.capacity() < 2 * tagged.len(), "do not retain the RAW allocation");
            let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(extracted));
            let pixels = dec.decode().unwrap();
            let mut expected = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg));
            assert_eq!(pixels, expected.decode().unwrap());
            std::fs::remove_file(shot.raw.unwrap()).unwrap();
            std::fs::remove_dir(dir).unwrap();
        }
    }

    use super::{
        brief_round, brief_shutter, choose_number_column, color_space_label, crop_region_rgba,
        crop_region_yuv, decode_jpeg, derive_fast_rgba, exif_rows, export_color_action,
        export_web_image, export_web_jpeg, export_web_to, fmt_shutter,
        frame_number, icc_description, near_stop_skip_resize, render_text_strip, scaled_dims,
        render_text_strip_picker, scan_folder, scan_fused_groups, text_watermark_rgba, tile_number_label,
        tile_number_label_planned, tile_number_plan, ExportColorAction, ExportTarget, Shot, SrcKind,
        Pixels, Watermark, WebFormat, WebSpec, STRIP_FADE_PX, TEX_SAFE_LONG,
    };
    use falcon_color::Gamut;

    /// v1.0.0-rc EXPORT FORMAT (F1): decode a PNG THESE ROWS PRODUCED, straight from bytes, and
    /// hand back the pixels beside the `sRGB` rendering intent the file declared.
    ///
    /// Deliberately NOT `decode_png_rgb`: that one takes a path and drops the chunk information,
    /// and a row that asserts a colour tag must read the tag itself rather than trust the
    /// production reader to have kept it.
    ///
    /// v1.0.0-rc PNG EXPORT (queue item 35): GENERALISED. It used to assert INSIDE ITSELF that the
    /// deliverable is `Rgb` at `Eight` -- true of every deliverable before this round and false of
    /// a transparent or 16-bit one, so the assert would have turned "the export kept what the file
    /// held" into a helper panic three call sites away from the row that meant it. The colour type
    /// and the depth are RETURNED now, and each caller states what it expects and why.
    /// Returns `(samples, w, h, srgb_intent, colour_type, bit_depth)`.
    fn png_pixels(
        bytes: &[u8],
    ) -> (Vec<u8>, u32, u32, Option<png::SrgbRenderingIntent>, png::ColorType, png::BitDepth) {
        let dec = png::Decoder::new(std::io::Cursor::new(bytes.to_vec()));
        let mut reader = dec.read_info().expect("the bytes must be a readable PNG");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("one PNG frame");
        buf.truncate(info.buffer_size());
        let srgb = reader.info().srgb;
        (buf, info.width, info.height, srgb, info.color_type, info.bit_depth)
    }

    /// [`super::write_png`] into a `Vec`, which is what the `encode_png` this round replaced did --
    /// the rows below want the bytes, the production path wants a sink, and there is one encoder.
    fn encode_png_rgb8(rgb: &[u8], w: u32, h: u32) -> anyhow::Result<Vec<u8>> {
        let mut buf: Vec<u8> = Vec::new();
        super::write_png(super::Pixels::Rgb8(rgb.to_vec()), w, h, &mut buf)?;
        Ok(buf)
    }

    /// B2: the near-stop resize-skip predicate. Skip up to max_dim × 1.125 (catches the measured
    /// maximized-4K want=3840 → stop 4096 = 1.067× case); resize beyond it (the next-octave 1.4–2.0×
    /// cases the DCT stops produce for smaller wants).
    #[test]
    fn near_stop_skip_resize_threshold() {
        // ── skip side (stop within max_dim × 1.125) ──
        assert!(near_stop_skip_resize(4096, 3840), "3840→4096 (1.067×) must skip"); // the target case
        assert!(near_stop_skip_resize(4096, 4096), "exact stop is a no-op → skip");
        assert!(near_stop_skip_resize(2048, 3840), "stop below window → resize is already a no-op");
        assert!(near_stop_skip_resize(3840 + 3840 / 8, 3840), "exact 1.125× boundary is inclusive");
        // ── resize side (stop beyond max_dim × 1.125) ──
        assert!(!near_stop_skip_resize(4096, 2880), "2880→4096 (1.42×) must resize");
        assert!(!near_stop_skip_resize(4096, 2049), "2049→4096 (~2.0×) must resize");
        assert!(!near_stop_skip_resize(3840 + 3840 / 8 + 1, 3840), "one past the boundary resizes");
        // ── texture-safe hard cap (huge-source detail fallback near the 16384 res-limit) ──
        assert!(near_stop_skip_resize(16384, TEX_SAFE_LONG), "an exact 16384 stop is texture-safe → skip");
        assert!(
            !near_stop_skip_resize(18000, TEX_SAFE_LONG),
            "an 18000 stop exceeds the max-texture bound → must resize (1.125× slack would else pass it)"
        );
    }

    /// v0.8.115 (DERIVE-DON'T-DECODE): [`derive_fast_rgba`] is the fast tier's own FINISH rule moved
    /// to the other side of the RGB→RGBA expansion. Given the SAME native pixels, the frame DERIVED
    /// from a decoded full-res buffer and the frame a fast-tier decode would have finished must be
    /// the same picture at the same size — otherwise "one decode serves both tiers" is quietly
    /// trading a decode for a fidelity change nobody signed off on.
    ///
    /// It is pinned against the REAL production path ([`browse_frame_rgba`] on a real file), not
    /// against a re-typed copy of the finish: a frozen oracle here would be exactly the kind of
    /// tautology that lets the shipping rule drift while the row stays green.
    ///
    /// THREE ROWS, and the third is the honest-difference one:
    ///   * NEAR-STOP — both paths KEEP the source size (the B2 skip), byte-identical.
    ///   * REDUCTION FROM THE SAME SOURCE — the ask is small enough to resize but too large for a
    ///     DCT stop to cover, so the decode is the native master and the two differ only by the
    ///     resampler's U8x3-vs-U8x4 SIMD kernel. The row PRINTS its worst byte and mean, so the
    ///     bound below is checkable rather than remembered.
    ///   * A COVERED STOP — the decoded frame comes from a 1/4 DCT decode while the derive comes
    ///     from the master. Same frame SIZE (that is the contract), different renderings of the same
    ///     picture, and the derived one is the higher-fidelity of the pair.
    ///
    /// FALSIFIER (L28): drop the near-stop branch from `derive_fast_rgba` (always downscale) and the
    /// NEAR-STOP row fails on dims. Swap `FilterType::Lanczos3` for `Bilinear` in `downscale_rgba`
    /// and the REDUCTION row fails on the byte bound by more than an order of magnitude.
    #[test]
    fn derive_fast_rgba_is_the_decoded_finish() {
        // Non-flat, non-separable content: a resampler difference cannot hide in a smooth ramp.
        let (w, h) = (840u32, 630u32);
        let mut rgb = vec![0u8; (w as usize) * (h as usize) * 3];
        for (i, p) in rgb.iter_mut().enumerate() {
            let (x, y) = ((i / 3) % w as usize, (i / 3) / w as usize);
            *p = ((x * 7 + y * 13 + (i % 3) * 61 + (x * y) / 5) & 0xff) as u8;
        }
        let bytes = super::encode_jpeg_rgb(&rgb, w, h, 95).expect("encode the probe jpeg");
        let dir = std::env::temp_dir()
            .join(format!("falcon_derive_finish_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("probe.jpg");
        std::fs::write(&path, &bytes).expect("write the probe jpeg");
        struct Tmp(std::path::PathBuf);
        impl Drop for Tmp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = Tmp(dir);
        let shot = Shot {
            id: 0,
            name: "probe".into(),
            has_raw: false,
            has_jpg: true,
            raw: None,
            jpg: Some(path),
            kind: SrcKind::Jpeg,
            cloud_placeholder: false,
            sniffed: None,
        };

        // The NATIVE master, exactly as the detail tier gets it (no reduction: 840 ≤ 4096 × 1.125).
        let native = super::browse_frame_rgba(&shot, 4096, true, super::Lane::Native)
            .expect("native decode");
        assert_eq!((native.w, native.h), (w, h), "the master decodes at full size");

        // The decoded fast frame and the derived one, compared byte-wise. Only valid when the two
        // land on the same frame size — the caller checks that first (rows 1 and 2); row 3 is where
        // they legitimately differ and it asserts the SIZE relationship instead.
        let both = |ask: u32, sup: bool| -> (super::BrowseFrame, (Vec<u8>, u32, u32)) {
            let dec = super::browse_frame_rgba(&shot, ask, sup, super::Lane::Fast)
                .expect("fast-lane decode");
            let der = derive_fast_rgba(&native.rgba, native.w, native.h, ask, sup);
            (dec, der)
        };
        let diff = |a: &[u8], b: &[u8]| -> (u8, f64) {
            assert_eq!(a.len(), b.len(), "same frame size ⇒ same buffer length");
            let (mut worst, mut sum, mut n) = (0u8, 0u64, 0u64);
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                if i % 4 == 3 {
                    assert_eq!((*x, *y), (255, 255), "both paths write opaque alpha");
                    continue;
                }
                worst = worst.max(x.abs_diff(*y));
                sum += x.abs_diff(*y) as u64;
                n += 1;
            }
            (worst, sum as f64 / n as f64)
        };

        // ── ROW 1: the NEAR-STOP skip (840 ≤ 800 × 1.125). Both KEEP the master's size. ──
        assert!(near_stop_skip_resize(w, 800), "the row is exercising the skip branch");
        let (dec, (der, rw, rh)) = both(800, true);
        assert_eq!((dec.w, dec.h), (w, h), "near-stop: the decoded finish keeps the source size");
        assert_eq!((rw, rh), (w, h), "near-stop: …and so does the derive");
        assert_eq!(diff(&dec.rgba, &der), (0, 0.0), "near-stop: byte-identical — neither resamples");

        // ── ROW 2: a REDUCTION from the same master (ask 500: 840/2 = 420 < 500, so no DCT stop
        // covers it and the fast lane decodes the master too). Only the resampler kernel differs. ──
        let (dec, (der, rw, rh)) = both(500, true);
        assert_eq!((dec.w, dec.h), scaled_dims(w, h, 500), "the decoded finish lands on scaled_dims");
        assert_eq!((rw, rh), (dec.w, dec.h), "…and the derive lands on the same frame size");
        let (worst, mean) = diff(&dec.rgba, &der);
        eprintln!("derive vs decoded, same master (840→500): worst byte {worst}, mean {mean:.4}");
        // U8x3 and U8x4 take different SIMD paths through fast_image_resize; anything beyond ±1 per
        // byte would be a different filter or a different geometry, not kernel rounding. (Measured
        // on this box: 0 and 0.0000 — the two kernels agree exactly.)
        assert!(worst <= 1, "derive differs from the decoded finish by {worst} > 1 in some byte");
        assert!(mean < 0.05, "…and typically not at all (mean {mean:.4})");

        // ── ROW 3: THE SUBSAMPLE TIER, which is where a derive written against `max_dim` alone
        // would have quietly produced FOUR TIMES the bytes per cached frame — and the prefetch
        // window is sized off the largest cached frame, so it would have quartered the browse's
        // runway with every test still green. `fast_decode_target` is shared with the decode path,
        // so the derive asks for HALF the want exactly as the decode does. ──
        let ask = super::fast_decode_target(500, false);
        assert_eq!(ask, 250, "the sub tier asks for half the want");
        let (dec, (_der, rw, rh)) = both(500, false);
        assert_eq!((rw, rh), scaled_dims(w, h, ask), "the sub tier's derive lands on the tier's ask");
        assert!(
            rw <= dec.w,
            "the derive is never the LARGER frame — that is the byte-budget guarantee ({rw} vs {})",
            dec.w
        );
        eprintln!("sub tier (want 500 → ask 250): decoded {}x{}, derived {rw}x{rh}", dec.w, dec.h);

        // ── ROW 4: THE SIZE DIVERGENCE, stated rather than discovered later. A decoder that can
        // scale hands back the nearest STOP at or above the ask (a JPEG's 1/2 DCT stop here, WIC's
        // exact-size transform on the HEIC lane the derive actually runs on), and the B2 near-stop
        // rule then KEEPS it whenever it sits within max_dim × 1.125. The derive always lands on
        // the ask itself. Both frames are filed under the same `dim` WANT bucket and both satisfy
        // it — the bucket is the want, never the pixel long side — so no consumer can tell them
        // apart, and the derived one is never the more expensive of the two. ──
        let (dec, (_der, rw, rh)) = both(200, true);
        assert_eq!((rw, rh), scaled_dims(w, h, 200), "the derive lands exactly on the ask");
        assert!(
            dec.w >= rw && dec.w <= 200 + 200 / 8,
            "the decoded frame keeps its covering stop, within the near-stop slack: {} vs {rw}",
            dec.w
        );
        eprintln!("covered stop (840→200): decoded {}x{}, derived {rw}x{rh}", dec.w, dec.h);
    }

    /// E4: the YUV region crop must snap OUTWARD to the chroma grid, return the snapped rect,
    /// and copy plane bytes that match direct index math on the full frame — for 4:2:0, 4:2:2
    /// AND odd (w-clamped) dims. Guards must reject undersized planes.
    #[test]
    fn crop_region_yuv_snaps_and_copies_exactly() {
        // Synthetic planes with position-coded bytes so any mis-indexed copy is caught.
        let mk = |w: u32, h: u32, salt: u8| -> Vec<u8> {
            (0..w as usize * h as usize).map(|i| (i as u8).wrapping_add(salt)).collect()
        };
        for (w, h, dx, dy, label) in [
            (64u32, 48u32, 2u32, 2u32, "4:2:0 even"),
            (64, 48, 2, 1, "4:2:2 even"),
            (63, 47, 2, 2, "4:2:0 odd (clamped edges)"),
            (64, 48, 1, 1, "4:4:4 (no snap)"),
        ] {
            let (cw, ch) = (w.div_ceil(dx), h.div_ceil(dy));
            let (y, cb, cr) = (mk(w, h, 0), mk(cw, ch, 11), mk(cw, ch, 23));
            // A deliberately grid-misaligned request (odd pixel origins for dx/dy=2).
            let (u0, v0, u1, v1) = (0.171, 0.313, 0.703, 0.851);
            let c = crop_region_yuv(&y, &cb, &cr, w, h, cw, ch, u0, v0, u1, v1)
                .unwrap_or_else(|| panic!("{label}: crop failed"));
            // Snapped rect: recover pixel coords and check grid alignment + outward cover.
            let x0 = (c.u0 * w as f32).round() as u32;
            let y0 = (c.v0 * h as f32).round() as u32;
            let x1 = (c.u1 * w as f32).round() as u32;
            let y1 = (c.v1 * h as f32).round() as u32;
            assert_eq!((x1 - x0, y1 - y0), (c.w, c.h), "{label}: rect/dims mismatch");
            assert_eq!(x0 % dx, 0, "{label}: x0 not snapped");
            assert_eq!(y0 % dy, 0, "{label}: y0 not snapped");
            assert!(x1 % dx == 0 || x1 == w, "{label}: x1 not snapped/clamped");
            assert!(y1 % dy == 0 || y1 == h, "{label}: y1 not snapped/clamped");
            let (sx, sy) = ((u0 * w as f32) as u32, (v0 * h as f32) as u32);
            let (ex, ey) = ((u1 * w as f32) as u32, (v1 * h as f32) as u32);
            assert!(x0 <= sx && y0 <= sy && x1 >= ex && y1 >= ey, "{label}: didn't cover request");
            // Chroma dims mirror the full-frame sizing rule over the snapped rect.
            assert_eq!(c.cw, (x1.div_ceil(dx) - x0 / dx).min(cw - x0 / dx), "{label}: tcw");
            assert_eq!(c.ch, (y1.div_ceil(dy) - y0 / dy).min(ch - y0 / dy), "{label}: tch");
            // Byte-exact copies (spot-check every 7th index of each plane).
            for i in (0..(c.w * c.h) as usize).step_by(7) {
                let (lx, ly) = (i as u32 % c.w, i as u32 / c.w);
                let full = ((y0 + ly) * w + x0 + lx) as usize;
                assert_eq!(c.y[i], y[full], "{label}: Y byte at {i}");
            }
            for i in (0..(c.cw * c.ch) as usize).step_by(7) {
                let (lx, ly) = (i as u32 % c.cw, i as u32 / c.cw);
                let full = ((y0 / dy + ly) * cw + x0 / dx + lx) as usize;
                assert_eq!(c.cb[i], cb[full], "{label}: Cb byte at {i}");
                assert_eq!(c.cr[i], cr[full], "{label}: Cr byte at {i}");
            }
        }
        // Guards: undersized planes and degenerate dims → None, never a panic.
        assert!(crop_region_yuv(&[0; 10], &[0; 4], &[0; 4], 8, 8, 4, 4, 0.0, 0.0, 1.0, 1.0).is_none());
        assert!(crop_region_yuv(&[0; 64], &[0; 3], &[0; 16], 8, 8, 4, 4, 0.0, 0.0, 1.0, 1.0).is_none());
        assert!(crop_region_yuv(&[], &[], &[], 0, 8, 4, 4, 0.0, 0.0, 1.0, 1.0).is_none());
        // Full-frame request returns the identity rect.
        let (y, cb, cr) = (vec![7u8; 64 * 48], vec![8u8; 32 * 24], vec![9u8; 32 * 24]);
        let c = crop_region_yuv(&y, &cb, &cr, 64, 48, 32, 24, 0.0, 0.0, 1.0, 1.0).unwrap();
        assert_eq!((c.w, c.h, c.cw, c.ch), (64, 48, 32, 24));
        assert_eq!((c.u0, c.v0, c.u1, c.v1), (0.0, 0.0, 1.0, 1.0));
    }

    /// Way-A pairing: extension classification, RAW-preview fallback, standalone-unsupported, and the
    /// RAW+JPG collapse. Two FINISHED siblings of the same stem (no RAW) now SPLIT into separate shots
    /// (only RAW+finished collapses); rank order decides a RAW's partner and the deterministic sort.
    /// Uses empty files — scan_folder classifies by extension only, so no valid image content is needed.
    #[test]
    fn scan_folder_classifies_and_pairs_formats() {
        use std::fs;
        let base = std::env::temp_dir().join(format!("falcon_scan_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let touch = |name: &str| fs::write(base.join(name), b"x").unwrap();
        touch("a_shot.png"); // standalone PNG
        touch("b_shot.tif"); // standalone TIFF
        touch("c_raw.cr3"); // RAW-only → decode via embedded JPEG preview
        touch("d_pair.cr3");
        touch("d_pair.JPG"); // RAW+JPG (mixed-case ext)
        touch("e_only.heic"); // standalone HEIC — decodable on Windows (WIC) + macOS (Image I/O), Unsupported elsewhere
        touch("f_both.png");
        touch("f_both.jpg"); // same stem, NO raw → two SEPARATE shots (ext-disambiguated names)
        touch("g_note.txt"); // non-image → ignored

        let shots = scan_folder(&base).unwrap();
        let by = |stem: &str| shots.iter().find(|s| s.name == stem).expect(stem);

        assert_eq!(by("a_shot").kind, SrcKind::Png);
        assert_eq!(by("b_shot").kind, SrcKind::Tiff);

        let c = by("c_raw");
        assert!(c.has_raw && !c.has_jpg && c.kind == SrcKind::Jpeg && c.jpg.is_none());

        let d = by("d_pair");
        assert!(d.has_raw && d.has_jpg && d.is_jpeg_source() && !d.is_unsupported());

        // HEIC keeps its path + isn't a RAW either way; it's decodable (SrcKind::Heic) on Windows (WIC)
        // and macOS (Image I/O, v0.9.9), and a badge-only Unsupported on platforms with no system HEVC
        // decoder (Way C §30.3).
        let e = by("e_only");
        assert!(e.jpg.is_some() && !e.has_raw);
        #[cfg(any(windows, target_os = "macos"))]
        assert!(
            e.kind == SrcKind::Heic
                && !e.is_unsupported()
                && e.finished_format().as_deref() == Some("HEIC")
        );
        #[cfg(all(not(windows), not(target_os = "macos")))]
        assert!(e.is_unsupported());

        // f_both.jpg + f_both.png (no RAW) SPLIT into two separate, ext-named shots — no collapse.
        assert!(!shots.iter().any(|s| s.name == "f_both"), "two finished siblings must not collapse");
        let fj = by("f_both.jpg");
        let fp = by("f_both.png");
        assert_eq!(fj.kind, SrcKind::Jpeg);
        assert_eq!(fp.kind, SrcKind::Png);
        assert!(!fj.has_raw && !fp.has_raw);

        assert!(!shots.iter().any(|s| s.name == "g_note"), "non-image file must be ignored");

        let _ = fs::remove_dir_all(&base);
    }

    /// v0.9.63 (B-R5-1, mac round-5): AN APPLEDOUBLE SIDECAR IS NEVER A SHOT.
    ///
    /// The tester's T7 (exFAT) folder carried `._IMG_7737.HEIC` beside `IMG_7737.HEIC`, and because
    /// `._` sorts before every letter it became shot #0 — the folder's LANDING photograph — with
    /// both its thumbnail and its fast decode failing loudly on 4 KB of AppleDouble metadata. The
    /// user's first sight of the folder was a broken frame for a file Finder does not even show
    /// them.
    ///
    /// The rows: the sidecar leaves no shot AND is counted; its real sibling is untouched and still
    /// lands at index 0 with the bare stem (so the fix cannot be mistaken for "the pair collapsed"
    /// or "the stem got disambiguated"); a `._` sidecar of a RAW goes too; and a `._` name with NO
    /// image extension — which the scan would have ignored anyway — still counts, because the
    /// predicate is about the FILE, not about whether this scan happened to want it.
    ///
    /// FALSIFIER (assert granularity): delete the `is_appledouble_sidecar` guard in the scan loop
    /// and the `"the sidecar is not a photograph"` assert reddens with a 3-shot folder; make the
    /// predicate require a `.` before the `_` (or match `_` alone) and the same row reddens;
    /// return the count as a constant 0 and the `"the skip is counted, not silent"` row reddens.
    #[test]
    fn scan_folder_skips_appledouble_sidecars() {
        use crate::{is_appledouble_sidecar, scan_folder_counted};
        use std::fs;
        let base = std::env::temp_dir().join(format!("falcon_appledouble_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let touch = |name: &str| fs::write(base.join(name), b"x").unwrap();
        touch("IMG_7737.HEIC"); // the photograph
        touch("._IMG_7737.HEIC"); // …and the metadata companion exFAT forced beside it
        touch("._x.heic"); // a sidecar whose original is not even here any more
        touch("._IMG_9000.CR3"); // the RAW-side flavour
        touch("._DS_Store_like"); // no image extension — ignored either way, still counted

        let (shots, appledouble) = scan_folder_counted(&base).unwrap();

        assert_eq!(
            shots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["IMG_7737"],
            "the sidecar is not a photograph — only the real HEIC becomes a shot, under its BARE stem"
        );
        assert_eq!(shots[0].id, 0, "…and it is shot #0, which is what the tester's landing frame was");
        assert!(shots[0].jpg.as_ref().map_or(false, |p| p.file_name().unwrap() == "IMG_7737.HEIC"));
        assert_eq!(appledouble, 4, "the skip is counted, not silent — all four `._` files");
        // The plain wrapper every other caller uses agrees with the counted one.
        assert_eq!(scan_folder(&base).unwrap().len(), 1);

        // The predicate itself, at its edges.
        assert!(is_appledouble_sidecar("._IMG_7737.HEIC"));
        assert!(is_appledouble_sidecar("._a"));
        assert!(!is_appledouble_sidecar("._"), "`._` alone is a companion to nothing");
        assert!(!is_appledouble_sidecar("."), "the current directory is not a sidecar");
        assert!(!is_appledouble_sidecar(".DS_Store"), "a dot-file is not an AppleDouble sidecar");
        assert!(!is_appledouble_sidecar("_IMG_7737.HEIC"), "a leading underscore is a legal photo name");
        assert!(!is_appledouble_sidecar("IMG_._7737.HEIC"), "the prefix is a PREFIX, not a substring");

        let _ = fs::remove_dir_all(&base);
    }

    /// v0.8.143 (A1): THE GPU-DEVELOP GATE IS THE SHADER'S OWN PRECONDITION, AND AN X-TRANS ARRAY
    /// MUST FAIL IT.
    ///
    /// The row that matters is RED-THEN-GREEN in the same assert: for every one of rawler 0.7.2's
    /// five 6×6 X-Trans layouts, the OLD predicate (`format!("{:?}", cfa).contains("RGGB")` —
    /// rawler's `Debug for CFA` prints the pattern string) answers TRUE, and the new
    /// [`cfa_is_rggb`] answers FALSE. That TRUE is the shipped bug: 35 Fuji bodies took a 2×2
    /// bilinear demosaic that cannot read a 6×6 array. CPU correctness is a separate property;
    /// the later X-Trans development round replaces its independently broken PPG fallback.
    ///
    /// The other arms keep the narrowing honest: a real RGGB stays TRUE (the whole GPU path must
    /// not be turned off by the fix), the other three 2×2 Bayer phases answer FALSE because the
    /// shader hard-codes the RGGB phase and nothing else, and `CFA::default()` (an empty,
    /// `is_valid() == false` array, which is what a decoder that never learned the pattern hands
    /// over) answers FALSE rather than indexing into a zero-filled table and claiming red.
    ///
    /// FALSIFIER: restore the substring predicate and every X-Trans row reddens on its second
    /// assert; drop the `width == 2 && height == 2` terms and X-TRANS-1/3/5 (whose (0,0) is R or
    /// whose (1,1) is B by coincidence of the repeat) sneak back through.
    #[test]
    fn the_rggb_predicate_is_the_shaders_own() {
        use crate::cfa_is_rggb;
        use rawler::CFA;
        // The old gate, verbatim, so the regression is demonstrated rather than remembered.
        let old = |cfa: &CFA| format!("{cfa:?}").contains("RGGB");

        // rawler 0.7.2 data/cameras/fuji/*.toml — every distinct `color_pattern` of length 36,
        // labelled with the bodies that carry it (12 + 14 + 4 + 1 + 4 = the 35 affected models).
        for (name, pat) in [
            ("X-Pro1/X-T3/X-T4/X100V (12)", "GGRGGBGGBGGRBRGRBGGGBGGRGGRGGBRBGBRG"),
            ("X-Pro2/X-T1/X-T2/X100F (14)", "RBGBRGGGRGGBGGBGGRBRGRBGGGBGGRGGRGGB"),
            ("X20/X30/XQ1/XQ2 (4)", "GBGGRGRGRBGBGBGGRGGRGGBGBGBRGRGRGGBG"),
            ("X-E5 (1)", "GRBGBRBGGRGGRGGBGGGBRGRBRGGBGGBGGRGG"),
            ("X-H2/X-H2S/X-T5/X-T50 (4)", "GGRGGBGGBGGRBRGRGBGGBGGRGGRGGBRBGBRG"),
        ] {
            let cfa = CFA::new(pat);
            assert_eq!((cfa.width, cfa.height), (6, 6), "{name} is a 6×6 X-Trans array");
            assert!(old(&cfa), "RED: the OLD substring gate sent {name} to the 2×2 shader");
            assert!(!cfa_is_rggb(&cfa), "GREEN: {name} must fall to the CPU develop");
        }

        // The one array the shader actually draws.
        let rggb = CFA::new("RGGB");
        assert!(old(&rggb) && cfa_is_rggb(&rggb), "a real RGGB sensor keeps the GPU develop");
        // The other three 2×2 phases: the shader hard-codes (0,0)=R and (1,1)=B, so they are not it.
        for pat in ["BGGR", "GRBG", "GBRG"] {
            assert!(!cfa_is_rggb(&CFA::new(pat)), "{pat} is a different Bayer phase, not RGGB");
        }
        // A decoder that never learned the pattern hands over an empty, invalid CFA.
        assert!(!cfa_is_rggb(&CFA::default()), "an empty CFA is not an RGGB sensor");
    }

    /// v0.8.36 (ITEM 4): two RAWs sharing a stem (CR3 + DNG) must SPLIT into two visible/deletable shots —
    /// the old single raw-slot silently dropped all but the last, so a delete-shot recycled only the tracked
    /// file and orphaned the invisible sibling. Also verifies the JPG pairs with exactly ONE of them and the
    /// bare-stem lone-RAW case is unchanged.
    #[test]
    fn scan_folder_splits_same_stem_raws() {
        use std::fs;
        let base = std::env::temp_dir().join(format!("falcon_stemraw_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let touch = |name: &str| fs::write(base.join(name), b"x").unwrap();
        touch("IMG_0001.CR3");
        touch("IMG_0001.DNG"); // same stem as the CR3 → must NOT shadow it
        touch("IMG_0001.JPG"); // pairs with exactly one RAW
        touch("IMG_0002.CR3"); // lone RAW → keeps the bare stem (no regression)

        let shots = scan_folder(&base).unwrap();

        // Key off the RAW file each shot tracks (the NAME reflects the finished/display side — the
        // CR3+JPG pair is named "IMG_0001.jpg" per the finished-multi precedent, the RAW-only DNG
        // "IMG_0001.dng"). Both same-stem RAWs must be present on DISTINCT shots — the bug was one
        // overwriting the other, leaving a file no shot tracked (an un-deletable orphan).
        let raw_ext = |s: &crate::Shot, e: &str| s.raw.as_ref().and_then(|p| p.extension()).map_or(false, |x| x.eq_ignore_ascii_case(e));
        let cr3 = shots.iter().find(|s| raw_ext(s, "cr3") && s.name.starts_with("IMG_0001")).expect("cr3 shot present");
        let dng = shots.iter().find(|s| raw_ext(s, "dng")).expect("dng shot present (was silently shadowed pre-fix)");
        assert_ne!(cr3.raw, dng.raw, "the two split shots must point at distinct RAW files");
        assert!(!shots.iter().any(|s| s.name == "IMG_0001"), "same-stem RAWs must not collapse to a bare stem");
        // The single JPG pairs with EXACTLY one of the two RAWs (the first by path order = the .CR3).
        let with_jpg = shots.iter().filter(|s| s.name.starts_with("IMG_0001") && s.has_jpg).count();
        assert_eq!(with_jpg, 1, "the lone JPG pairs with exactly one same-stem RAW");
        assert!(cr3.has_jpg && !dng.has_jpg, "the JPG pairs with the first RAW (CR3); the DNG is RAW-only");

        // The lone RAW (no stem collision) keeps the plain bare-stem name — no regression.
        let lone = shots.iter().find(|s| s.name == "IMG_0002").expect("lone cr3 keeps bare stem");
        assert!(lone.has_raw && !lone.has_jpg);

        let _ = fs::remove_dir_all(&base);
    }

    /// Verify colour-space detection against the real sample files (ICC-profile-based for
    /// Adobe RGB / Display P3, EXIF-tag fallback for Canon sRGB). Skips gracefully on a machine
    /// without the test folder so it never fails elsewhere; asserts only when the files exist.
    #[test]
    fn color_space_from_real_samples() {
        let dir = crate::fixture_paths::photos();
        let detect = |file: &str| -> Option<String> {
            let p = dir.join(file);
            if !p.exists() {
                return None;
            }
            let f = std::fs::File::open(&p).ok()?;
            let mut br = std::io::BufReader::new(f);
            let reader = exif::Reader::new().read_from_container(&mut br).ok()?;
            color_space_label(&p, &reader)
        };
        for (file, expect) in [
            ("AdobeRGB_test.jpg", "Adobe RGB"),
            ("Display_P3_test.jpg", "Display P3"),
            ("HWU_7781.JPG", "sRGB"), // Canon, no ICC → EXIF ColorSpace=1 fallback
        ] {
            let got = detect(file);
            println!("colour-space {file} → {got:?} (expect {expect})");
            if dir.join(file).exists() {
                assert_eq!(got.as_deref(), Some(expect), "wrong colour space for {file}");
            }
        }
    }

    #[test]
    fn export_web_caps_long_side_and_encodes_jpeg() {
        // 400×300 grey → cap long side to 200, expect a valid JPEG that decodes to ≤200.
        let rgb = vec![128u8; 400 * 300 * 3];
        let jpg = export_web_jpeg(&rgb, 400, 300, 200, 85, None, Gamut::Srgb).unwrap();
        assert_eq!(&jpg[..2], &[0xFF, 0xD8], "JPEG SOI marker");
        let (_d, w, h) = decode_jpeg(&jpg, None).unwrap();
        assert_eq!(w.max(h), 200, "long side capped");
        assert_eq!((w, h), (200, 150), "aspect preserved");
    }

    // ───────────── v0.8.100 / design-sweep O13 = A1: ./export export colour integrity ─────────────

    /// The POLICY table, exhaustively: every source gamut × every export target. This is the pure
    /// decision the two code paths consult — a change to it has to be a deliberate edit here.
    ///
    /// FALSIFIER: this fails if anyone makes the copy targets colour-manage (which would rewrite
    /// the photographer's wide-gamut MASTERS on their way into ./Selected), or if the Web path is
    /// ever made conditional on something other than "is the source already sRGB".
    #[test]
    fn export_color_policy_per_target() {
        let wide = [Gamut::AdobeRgb, Gamut::DisplayP3, Gamut::Rec2020, Gamut::DciP3, Gamut::Custom];
        for g in wide {
            assert_eq!(
                export_color_action(ExportTarget::Web, g),
                ExportColorAction::ConvertToSrgb,
                "a {g:?} source bound for ./export must be converted"
            );
        }
        assert_eq!(
            export_color_action(ExportTarget::Web, Gamut::Srgb),
            ExportColorAction::TagSrgbOnly,
            "an sRGB source needs no pixel work — but the deliverable still states its space"
        );
        // The copy targets NEVER touch colour, whatever the source is (incl. plain sRGB).
        for t in [ExportTarget::Selected, ExportTarget::Rejected] {
            for g in [Gamut::Srgb, Gamut::AdobeRgb, Gamut::DisplayP3, Gamut::Rec2020, Gamut::DciP3, Gamut::Custom] {
                assert_eq!(
                    export_color_action(t, g),
                    ExportColorAction::CopyOriginal,
                    "{t:?} hands over the ORIGINAL file — {g:?} must survive byte-identical"
                );
            }
        }
    }

    /// Find the ICC profile embedded in a JPEG's APP2 `ICC_PROFILE` segments and re-join the chunks.
    /// Deliberately re-implemented from the JPEG spec rather than borrowing the encoder's writer, so
    /// the assertion below is a genuine reader-side oracle and not an echo of the encoder.
    fn icc_from_jpeg(jpg: &[u8]) -> Option<Vec<u8>> {
        let mut i = 2usize; // skip SOI
        let mut out: Vec<u8> = Vec::new();
        while i + 4 <= jpg.len() {
            if jpg[i] != 0xFF {
                break;
            }
            let marker = jpg[i + 1];
            if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
                i += 2;
                continue;
            }
            if marker == 0xDA {
                break; // start of scan — no more headers
            }
            let len = ((jpg[i + 2] as usize) << 8) | jpg[i + 3] as usize;
            let body = jpg.get(i + 4..i + 2 + len)?;
            if marker == 0xE2 && body.starts_with(b"ICC_PROFILE\0") && body.len() > 14 {
                out.extend_from_slice(&body[14..]); // 12-byte id + seq + count
            }
            i += 2 + len;
        }
        (!out.is_empty()).then_some(out)
    }

    /// The ./export deliverable TAGS itself: the JPEG carries an APP2 ICC profile, and that profile
    /// really is sRGB — parsed back through falcon-color's own reader, which must recover the sRGB
    /// primaries. (Round-trip oracle: parse_display_icc has no idea which gamut wrote the bytes.)
    ///
    /// v0.8.105 (W3): …and it is the SHIPPING profile, not merely an sRGB one. The colorant matrix
    /// alone cannot tell `srgb_icc_for_export` from `icc_bytes_for_gamut(Srgb)` — both are exact
    /// sRGB, and C3 was about IDENTITY, not numbers — so pointing this one line back at the
    /// vendor-branded v4 serializer left the whole suite green. The three rows below are asserted
    /// against the APP2 bytes this test already extracts, which is what `SRGB_EXPORT_DESC` was
    /// exported for.
    ///
    /// FALSIFIER: fails if `add_icc_profile` is dropped, if the segment is written malformed, or if
    /// some future edit embeds the SOURCE profile (Adobe RGB primaries) instead of the destination's;
    /// point `export_web_jpeg` back at `icc_bytes_for_gamut(Gamut::Srgb)` and the version / desc /
    /// no-branding rows fail on 0x04 and "Falcon sRGB (exact)".
    #[test]
    fn export_web_embeds_an_srgb_icc_profile() {
        let rgb = vec![128u8; 64 * 64 * 3];
        for src in [Gamut::Srgb, Gamut::AdobeRgb] {
            let jpg = export_web_jpeg(&rgb, 64, 64, 64, 90, None, src).unwrap();
            let icc = icc_from_jpeg(&jpg)
                .unwrap_or_else(|| panic!("no APP2 ICC segment in the {src:?}-sourced export"));
            let prof = falcon_color::parse_display_icc(&icc, "embedded")
                .unwrap_or_else(|| panic!("the embedded profile must parse as a display profile"));
            // FROZEN ORACLE: the published sRGB (IEC 61966-2-1) D65 colorant matrix, written out
            // here rather than read back from falcon-color, so this cannot become a tautology.
            // Adobe RGB's first colorant is 0.5767 — nowhere near sRGB's 0.4124, so embedding the
            // SOURCE profile by mistake fails loudly on the very first cell.
            const SRGB_D65: [[f32; 3]; 3] = [
                [0.412_390_8, 0.357_584_3, 0.180_480_8],
                [0.212_639, 0.715_168_7, 0.072_192_3],
                [0.019_330_8, 0.119_194_8, 0.950_532_2],
            ];
            for (r, (got, want)) in prof.rgb_to_xyz.iter().zip(SRGB_D65.iter()).enumerate() {
                for (c, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                    assert!(
                        (g - w).abs() < 1e-3,
                        "embedded profile must be sRGB, not {src:?}: [{r}][{c}] {g} vs {w}"
                    );
                }
            }
            // ── v0.8.105 (W3): the IDENTITY of the embedded profile, at the deliverable ──
            let desc = icc_description(&icc).expect("the embedded profile must carry a description");
            assert_eq!(icc[8], 0x02, "a web deliverable carries a v2 profile, got major {}", icc[8]);
            assert_eq!(
                desc,
                falcon_color::SRGB_EXPORT_DESC,
                "the shipped file must name the STANDARD colour space"
            );
            assert!(!desc.contains("Falcon"), "no vendor branding on a client-facing file: {desc}");
        }
    }

    /// The BYTE-LEVEL arm: identical input pixels exported as an Adobe RGB source must come out
    /// with DIFFERENT numbers than the same pixels exported as an sRGB source — and the sRGB export
    /// must leave them (within JPEG round-trip noise) alone.
    ///
    /// The oracle is frozen independently of the export code: the expected converted value is
    /// computed straight from `falcon_color::transform_rgb8`, the same published entry point the UI
    /// chrome uses — not by calling anything inside `export_web_jpeg`.
    ///
    /// FALSIFIERS: (a) delete the transform → the two exports become equal and the first assert
    /// fires; (b) convert UNCONDITIONALLY (ignoring `src`) → the sRGB export moves and the second
    /// assert fires; (c) convert with the arguments swapped (sRGB→AdobeRGB) → the third assert,
    /// which pins the DIRECTION against the independent oracle, fires.
    #[test]
    fn export_web_converts_wide_gamut_pixels_to_srgb() {
        // A saturated, NON-primary colour: primaries clip at the cube corners and would hide the
        // transform. This one lands well inside sRGB's cube after conversion.
        const PX: [u8; 3] = [40, 200, 110];
        let rgb: Vec<u8> = PX.iter().copied().cycle().take(64 * 64 * 3).collect();
        let sample = |jpg: &[u8]| -> [u8; 3] {
            let (px, w, _h) = decode_jpeg(jpg, None).unwrap();
            let mid = ((32 * w as usize) + 32) * 3; // dead centre, away from block edges
            [px[mid], px[mid + 1], px[mid + 2]]
        };
        let as_srgb = sample(&export_web_jpeg(&rgb, 64, 64, 64, 98, None, Gamut::Srgb).unwrap());
        let as_adobe = sample(&export_web_jpeg(&rgb, 64, 64, 64, 98, None, Gamut::AdobeRgb).unwrap());

        // (a) the wide-gamut export must have MOVED — far beyond JPEG noise.
        let moved = (0..3).map(|i| (as_adobe[i] as i32 - as_srgb[i] as i32).abs()).max().unwrap();
        assert!(moved > 12, "Adobe RGB source must be converted; moved only {moved} ({as_srgb:?} → {as_adobe:?})");

        // (b) the sRGB source must NOT have moved (src == dst ⇒ the transform is a no-op).
        let drift = (0..3).map(|i| (as_srgb[i] as i32 - PX[i] as i32).abs()).max().unwrap();
        assert!(drift <= 3, "an sRGB source must ride through untouched; drifted {drift} to {as_srgb:?}");

        // (c) DIRECTION, against the independent oracle.
        let want = falcon_color::transform_rgb8(PX, Gamut::AdobeRgb, Gamut::Srgb);
        let err = (0..3).map(|i| (as_adobe[i] as i32 - want[i] as i32).abs()).max().unwrap();
        assert!(err <= 3, "converted pixel {as_adobe:?} must match the house transform's {want:?}");
    }

    /// v1.0.0-rc EXPORT FORMAT (F1) — **THE PNG ENCODE IS LOSSLESS AND SAYS sRGB THE PNG WAY.**
    ///
    /// v1.0.0-rc tail (§R-B B-R1): the row was called `..._the_png_deliverable_is_lossless_...` and
    /// its first claim was written as if it were about the FILE. It is not, and the sheet no longer
    /// says it is: under `Keep::NONE` `decode_png_rgb` composites alpha onto white and strips 16-bit
    /// before this function is reached; since round 35 the PNG stop asks for `Keep::ALL`, so a
    /// transparent or deep SOURCE now reaches the encoder whole. What is lossless is the ENCODE — which is what this row measures, and
    /// all it measures.
    ///
    /// Three claims, each with its own falsifier:
    /// 1. `write_png` round-trips PIXEL-EXACT. The pixels are compared against the buffer that
    ///    went in, not against a tolerance — an encoder that loses nothing is the whole reason this
    ///    stop exists, and "no compression loss" is what the sheet's muted Quality row says.
    /// 2. The file declares sRGB through its own `sRGB` chunk (intent `Perceptual`), NOT through an
    ///    embedded profile: the spec has a decoder ignore `iCCP` when `sRGB` is present, so a
    ///    profile beside it would be ~2.4 KB of dead bytes in every deliverable.
    /// 3. A NON-neutral, non-primary pixel is used, so a channel swap or an off-by-one row stride
    ///    cannot pass by symmetry.
    ///
    /// FALSIFIERS (L28): route the `Png` arm through `encode_jpeg` and row 1 fails on the first
    /// differing byte; drop `set_source_srgb` and row 2 fails with `left: None`; keep the `iCCP`
    /// (there is nowhere to put one, which is the point) and row 2's `icc_profile` assert fails.
    #[test]
    fn the_png_encode_is_lossless_and_carries_an_srgb_chunk() {
        // A gradient with all three channels distinct per pixel: symmetry would hide a swap.
        let (w, h) = (37u32, 23u32); // deliberately odd + non-square: stride bugs surface
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                rgb.push((x * 7 % 256) as u8);
                rgb.push((y * 11 % 256) as u8);
                rgb.push(((x * 3 + y * 5) % 256) as u8);
            }
        }
        let bytes = encode_png_rgb8(&rgb, w, h).expect("encode a PNG");
        let (back, bw, bh, srgb, color, depth) = png_pixels(&bytes);
        assert_eq!(
            (color, depth),
            (png::ColorType::Rgb, png::BitDepth::Eight),
            "this row's fixture is opaque 8-bit RGB, so the deliverable is too"
        );
        assert_eq!((bw, bh), (w, h), "the PNG keeps the dimensions it was handed");
        assert_eq!(back, rgb, "PNG is LOSSLESS: every byte must survive the round trip");
        assert_eq!(
            srgb,
            Some(png::SrgbRenderingIntent::Perceptual),
            "the deliverable declares sRGB with the PNG spec's own chunk, at the photographic intent"
        );
        let dec = png::Decoder::new(std::io::Cursor::new(bytes.clone()));
        let reader = dec.read_info().expect("re-read the header");
        assert!(
            reader.info().icc_profile.is_none(),
            "no iCCP beside the sRGB chunk — the spec has decoders ignore it, so it would be dead bytes"
        );
        // The spec's substitute gAMA/cHRM ride along for decoders that predate `sRGB`.
        assert!(reader.info().source_gamma.is_some(), "the sRGB substitute gAMA is written too");
        assert!(reader.info().source_chromaticities.is_some(), "…and the substitute cHRM");
        // A degenerate size is refused rather than encoded into a corrupt file.
        assert!(encode_png_rgb8(&[], 0, 0).is_err(), "an empty image is not a deliverable");
        assert!(encode_png_rgb8(&rgb[..10], w, h).is_err(), "a short buffer is refused, not padded");
    }

    /// v1.0.0-rc EXPORT FORMAT tail (§R-A A-O1) — **THE COMPRESSION LEVEL IS THE ONE THE DOC NAMES.**
    ///
    /// The round shipped no `set_compression` call at all and a doc claiming
    /// `png::Compression::Default` ("the flate2 default level"); the bytes were the crate default,
    /// which is `Fast` (fdeflate). Nobody could have caught that by reading, because a level that is
    /// never named leaves nothing to disagree with the prose — so the level is now SET, and this row
    /// is what makes the doc's sentence falsifiable: it rebuilds the identical chunk set at each of
    /// the two candidate levels and pins which one the shipped encoder produces.
    ///
    /// The second assert is the ANTI-VACUITY guard: on this fixture the two levels really do differ,
    /// so "shipped == Fast" is a measurement and not an accident of a fixture too small to compress.
    ///
    /// FALSIFIER (L28): change the live `enc.set_compression(png::Compression::Fast)` call in
    /// [`super::write_png`] to `png::Compression::Default` (or delete it, which is what shipped
    /// before the level was named) and the first assert reddens on the byte lengths.
    #[test]
    fn the_png_compression_level_is_the_one_the_doc_names() {
        // A PHOTOGRAPH-SHAPED fixture: smooth ramps carrying a little grain, so the two levels really
        // do produce different files. Pure noise is incompressible (the levels land within a few bytes
        // of each other) and a flat fill compresses identically at every level — either would let the
        // anti-vacuity assert below pass for the wrong reason.
        let (w, h) = (160u32, 120u32);
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        let mut s: u32 = 0x1234_5678;
        for y in 0..h {
            for x in 0..w {
                let grain = |s: &mut u32| {
                    *s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    ((*s >> 28) as i32) - 8
                };
                let base = [(x * 255 / w) as i32, (y * 255 / h) as i32, ((x + y) * 255 / (w + h)) as i32];
                for b in base {
                    rgb.push((b + grain(&mut s)).clamp(0, 255) as u8);
                }
            }
        }
        // The same chunk set `write_png` writes, at an explicitly chosen level.
        let at = |level: png::Compression| -> Vec<u8> {
            let mut buf: Vec<u8> = Vec::new();
            {
                let mut enc = png::Encoder::new(&mut buf, w, h);
                enc.set_color(png::ColorType::Rgb);
                enc.set_depth(png::BitDepth::Eight);
                enc.set_compression(level);
                enc.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
                enc.set_source_gamma(png::ScaledFloat::from_scaled(45455));
                enc.set_source_chromaticities(png::SourceChromaticities {
                    white: (png::ScaledFloat::from_scaled(31270), png::ScaledFloat::from_scaled(32900)),
                    red: (png::ScaledFloat::from_scaled(64000), png::ScaledFloat::from_scaled(33000)),
                    green: (png::ScaledFloat::from_scaled(30000), png::ScaledFloat::from_scaled(60000)),
                    blue: (png::ScaledFloat::from_scaled(15000), png::ScaledFloat::from_scaled(6000)),
                });
                let mut wtr = enc.write_header().unwrap();
                wtr.write_image_data(&rgb).unwrap();
                wtr.finish().unwrap();
            }
            buf
        };
        let shipped = encode_png_rgb8(&rgb, w, h).expect("encode a PNG");
        let fast = at(png::Compression::Fast);
        let deflt = at(png::Compression::Default);
        // ANTI-VACUITY: on this fixture the levels really do produce different files.
        assert!(
            deflt.len() * 20 < fast.len() * 19,
            "the two levels must differ MATERIALLY here, or the row proves nothing: Fast {} / Default {}",
            fast.len(),
            deflt.len()
        );
        // Length first: a byte-vector mismatch would dump ~60 KB into the failure message.
        assert_eq!(
            shipped.len(),
            fast.len(),
            "the deliverable is written at the level the doc names: Fast ({} bytes), not Default ({} bytes)",
            fast.len(),
            deflt.len()
        );
        assert!(shipped == fast, "…and byte for byte, not merely at the same size");
    }

    /// v1.0.0-rc EXPORT FORMAT (F1) — **QUALITY IS NOT AN INPUT TO THE PNG ARM; `long` IS.**
    ///
    /// The sheet leaves the Quality row MOUNTED while PNG is selected (a user may pick a JPEG
    /// quality before switching back, and the value he chose has to still be there), muted and
    /// INERT since the tail, under the Quality row's PNG caption — `falcon-native`'s
    /// `support::PNG_QUALITY_CAPTION`, which is where that sentence lives and where its width is
    /// measured. NAMED rather than re-quoted (queue 34 rider R1): the string was reworded twice and
    /// this doc still carried the first spelling, which is the D-V3 class in a second crate.
    /// This row is what makes that caption true rather than merely written: the same export at
    /// q1 and at q100 must be BYTE-IDENTICAL. And the other half — the long-edge cap, which the
    /// format does NOT excuse — must still be honoured, or "PNG ignores quality" would have quietly
    /// become "PNG ignores the output settings".
    ///
    /// FALSIFIERS (L28): pass `quality` into the `Png` arm in any form and row 1 fails on a length
    /// or byte difference; drop the `resize_to_long` call and row 2 fails with 200x150 on the left.
    #[test]
    fn a_png_export_ignores_quality_and_still_honours_the_long_edge() {
        let rgb: Vec<u8> = [40u8, 200, 110].iter().copied().cycle().take(200 * 150 * 3).collect();
        let at_q =
            |q: u8| export_web_image(rgb.clone(), 200, 150, 64, q, None, Gamut::Srgb, WebFormat::Png).unwrap();
        let (lo, hi) = (at_q(1), at_q(100));
        assert_eq!(lo, hi, "PNG is lossless: the quality tier cannot reach these bytes");
        let (_, pw, ph, _, color, depth) = png_pixels(&lo);
        assert_eq!(
            (color, depth),
            (png::ColorType::Rgb, png::BitDepth::Eight),
            "an opaque 8-bit source is delivered as RGB8, quality tier or not"
        );
        assert_eq!((pw, ph), (64, 48), "the long edge is still the long edge: 200x150 fits into 64");
        // …and the JPEG arm, at the same call, DOES answer differently — without which row 1
        // could pass on an export path that ignores `quality` for every format.
        let j_lo = export_web_image(rgb.clone(), 200, 150, 64, 20, None, Gamut::Srgb, WebFormat::Jpeg).unwrap();
        let j_hi = export_web_image(rgb.clone(), 200, 150, 64, 95, None, Gamut::Srgb, WebFormat::Jpeg).unwrap();
        assert_ne!(j_lo, j_hi, "ANTI-VACUITY: the JPEG arm really is quality-driven");
        assert!(lo.starts_with(&[0x89, b'P', b'N', b'G']), "the PNG stop writes a PNG signature");
        assert!(j_lo.starts_with(&[0xFF, 0xD8]), "…and the JPG stop writes an SOI marker");
        // ROUND 35: the SAME two facts over a TRANSPARENT source, which is a deliverable this row
        // could not previously produce. `quality` still cannot reach the bytes, the long edge is
        // still honoured, and the alpha channel is now part of what "lossless" delivers.
        let rgba: Vec<u8> = [40u8, 200, 110, 0, 40, 200, 110, 255]
            .iter()
            .copied()
            .cycle()
            .take(200 * 150 * 4)
            .collect();
        let at_q_rgba = |q: u8| -> Vec<u8> {
            let mut b: Vec<u8> = Vec::new();
            let spec =
                WebSpec { long: 64, quality: q, wm: None, src: Gamut::Srgb, fmt: WebFormat::Png };
            export_web_to(Pixels::Rgba8(rgba.clone()), 200, 150, &spec, &mut b).unwrap();
            b
        };
        let (t_lo, t_hi) = (at_q_rgba(1), at_q_rgba(100));
        assert_eq!(t_lo, t_hi, "PNG is lossless whatever the tier, transparency included");
        let (_, tw, th, _, tcolor, tdepth) = png_pixels(&t_lo);
        assert_eq!(
            (tcolor, tdepth),
            (png::ColorType::Rgba, png::BitDepth::Eight),
            "a transparent source is delivered with its channel"
        );
        assert_eq!((tw, th), (64, 48), "…and the long edge still applies to it");
    }

    /// v1.0.0-rc EXPORT FORMAT (F1): the enum's own table — the extension, the noun, the Seg index
    /// round trip, and `Default = Jpeg`, which is what makes an old settings file (and an old
    /// preset) load as the format it was written under.
    ///
    /// The SERDE spelling is pinned next door in `falcon/native` (`the_export_format_persists_and_an_old_file_reads_as_jpg`),
    /// where the settings/preset structs that actually carry it live and where `serde_json` is
    /// already a dependency — this crate has no JSON in its dependency tree, and adding one for a
    /// two-line assertion would be a dependency taken for a test.
    ///
    /// FALSIFIERS (L28): swap either arm of `ext`/`noun` and the first block fails; drop
    /// `#[default]` and the default row fails with `Png` on the left; make `from_index` saturate or
    /// wrap on an out-of-range int and the last row fails.
    #[test]
    fn the_web_format_enum_is_a_table_not_a_convention() {
        assert_eq!(WebFormat::Jpeg.ext(), "jpg");
        assert_eq!(WebFormat::Png.ext(), "png");
        assert_eq!(WebFormat::Jpeg.noun(), "JPG");
        assert_eq!(WebFormat::Png.noun(), "PNG");
        assert_eq!(WebFormat::default(), WebFormat::Jpeg, "an unstated format is the shipped one");
        for f in [WebFormat::Jpeg, WebFormat::Png] {
            assert_eq!(WebFormat::from_index(f.index()), f, "the Seg index round-trips");
            assert_eq!(f.ext(), f.noun().to_lowercase(), "the noun and the extension name one thing");
        }
        assert_eq!((WebFormat::Jpeg.index(), WebFormat::Png.index()), (0, 1), "JPG is stop 0, PNG stop 1");
        // An int from outside the sheet's two stops degrades to the shipped format, never a panic.
        for bad in [-1, 2, 7, i32::MIN, i32::MAX] {
            assert_eq!(WebFormat::from_index(bad), WebFormat::Jpeg, "{bad} is not a stop");
        }
    }

    #[test]
    fn watermark_brightens_bottom_right_only() {
        // dark image + an opaque white logo stamped flush in the corner (margin 0).
        let rgb = vec![10u8; 120 * 120 * 3];
        let logo = vec![255u8; 24 * 24 * 4];
        let wm = Watermark { rgba: logo, w: 24, h: 24, scale: 0.25, opacity: 1.0, pos_x: 0.9, pos_y: 0.9 };
        let jpg = export_web_jpeg(&rgb, 120, 120, 120, 95, Some(&wm), Gamut::Srgb).unwrap();
        let (px, w, h) = decode_jpeg(&jpg, None).unwrap();
        let br = ((h as usize - 1) * w as usize + (w as usize - 1)) * 3; // bottom-right
        assert!(px[br] > 150, "watermark should brighten the bottom-right (got {})", px[br]);
        assert!(px[0] < 80, "top-left should stay dark (got {})", px[0]);
    }

    /// v0.8.104 (Round-A C4 / L28): **the falsifier for `export_web_jpeg`'s convert-BEFORE-stamp
    /// ordering** — the one behaviour the function's doc says the shape exists to protect, and the
    /// one cell its four existing tests never covered. Every one of them is either `src == Srgb`
    /// (the transform no-ops) or `wm == None`, so swapping the two statements left all four green.
    ///
    /// The trap this closes is quiet by construction: the default WHITE text mark maps to white
    /// through any gamut transform, and the Grey preset moves by ONE level — so a "do the pixel work
    /// last" refactor would survive review and then shift a photographer's brand colour in every
    /// delivered file. Hence the deliberately saturated, non-neutral mark.
    ///
    /// FALSIFIER: swap the `transform_rgb` and `stamp_watermark` statements in `export_web_image`
    /// and the Adobe RGB run's mark lands on `transform_rgb8([200,90,40], AdobeRgb, Srgb)` — the
    /// last assertion names that value explicitly, so the failure message points straight at the
    /// cause. Row 2 is the anti-vacuity guard: it proves this run really did convert something.
    ///
    /// v1.0.0-rc EXPORT FORMAT (F1): PARAMETRISED OVER BOTH DELIVERABLE FORMATS. The convert-then-
    /// stamp ORDER is the shared spine of `export_web_image` — only the last statement differs —
    /// so a future edit that gave the PNG arm its own copy of the pipeline would have to break this
    /// row to do it. On the PNG stop every number below is EXACT (nothing is lossy), so the ±3
    /// tolerances are JPEG's alone and the PNG pass is the stricter of the two.
    /// The transparent-source cell of [`the_mark_never_rides_the_source_transform`] (round 35). A
    /// 120x120 RGBA8 photo whose right-hand ten columns are fully transparent -- both probe points
    /// stay opaque, so the two claims the parent row makes are the two claims measured here, over
    /// the four-channel path.
    ///
    /// FALSIFIER (L28): move `transform_pixels` after `stamp_watermark_pixels` in `export_web_to`
    /// and the "mark moved" assert reddens; the anti-vacuity assert below is what keeps it honest.
    fn transparent_source_cell() {
        const PHOTO: [u8; 3] = [40, 200, 110];
        const MARK: [u8; 3] = [200, 90, 40];
        let mut rgba: Vec<u8> = Vec::with_capacity(120 * 120 * 4);
        for _ in 0..120 {
            for x in 0..120u32 {
                let a = if x >= 110 { 0 } else { 255 };
                rgba.extend_from_slice(&[PHOTO[0], PHOTO[1], PHOTO[2], a]);
            }
        }
        let logo: Vec<u8> =
            [MARK[0], MARK[1], MARK[2], 255].iter().copied().cycle().take(24 * 24 * 4).collect();
        let wm = Watermark { rgba: logo, w: 24, h: 24, scale: 0.5, opacity: 1.0, pos_x: 0.5, pos_y: 0.5 };
        let run = |src: Gamut| -> Vec<u8> {
            let mut b: Vec<u8> = Vec::new();
            let spec =
                WebSpec { long: 120, quality: 98, wm: Some(&wm), src, fmt: WebFormat::Png };
            export_web_to(Pixels::Rgba8(rgba.clone()), 120, 120, &spec, &mut b).unwrap();
            b
        };
        let at = |bytes: &[u8], x: usize, y: usize| -> [u8; 4] {
            let (p, w, _h, _srgb, color, depth) = png_pixels(bytes);
            assert_eq!(
                (color, depth),
                (png::ColorType::Rgba, png::BitDepth::Eight),
                "the transparent source keeps its channel through the stamp"
            );
            let i = (y * w as usize + x) * 4;
            [p[i], p[i + 1], p[i + 2], p[i + 3]]
        };
        let (srgb_run, adobe_run) = (run(Gamut::Srgb), run(Gamut::AdobeRgb));
        let (mark_srgb, mark_adobe) = (at(&srgb_run, 60, 60), at(&adobe_run, 60, 60));
        let (photo_srgb, photo_adobe) = (at(&srgb_run, 5, 5), at(&adobe_run, 5, 5));
        let mark_moved = (0..3).map(|i| (mark_adobe[i] as i32 - mark_srgb[i] as i32).abs()).max().unwrap();
        assert!(
            mark_moved <= 3,
            "[RGBA8] the mark must be identical whatever the source gamut, but it moved {mark_moved} \
             ({mark_srgb:?} to {mark_adobe:?}) — the stamp is running BEFORE the conversion"
        );
        let photo_moved =
            (0..3).map(|i| (photo_adobe[i] as i32 - photo_srgb[i] as i32).abs()).max().unwrap();
        assert!(photo_moved >= 4, "ANTI-VACUITY: the photo underneath really did convert");
        assert_eq!(mark_srgb[3], 255, "the mark over an opaque region is opaque");
        assert_eq!(at(&srgb_run, 115, 5)[3], 0, "…and the transparent columns stayed transparent");
    }

    #[test]
    fn the_mark_never_rides_the_source_transform() {
        // ROUND 35: the same claim over a TRANSPARENT source. The deliverable is RGBA8 and the
        // mark composites through the round's real source-over rather than the shipped
        // opaque-destination blend -- so the "authored in sRGB, lands in sRGB" contract has to hold
        // through a second arithmetic, which is exactly the kind of parallel path that drifts.
        transparent_source_cell();
        // Saturated + non-neutral on BOTH the photo and the mark: a neutral either would hide the bug.
        const PHOTO: [u8; 3] = [40, 200, 110];
        const MARK: [u8; 3] = [200, 90, 40];
        let rgb: Vec<u8> = PHOTO.iter().copied().cycle().take(120 * 120 * 3).collect();
        let logo: Vec<u8> =
            [MARK[0], MARK[1], MARK[2], 255].iter().copied().cycle().take(24 * 24 * 4).collect();
        // scale 0.5 on a 120px image ⇒ a 60×60 mark centred at (60,60), i.e. spanning 30..90.
        let wm = Watermark { rgba: logo, w: 24, h: 24, scale: 0.5, opacity: 1.0, pos_x: 0.5, pos_y: 0.5 };
        let at = |bytes: &[u8], fmt: WebFormat, x: usize, y: usize| -> [u8; 3] {
            let (px, w) = match fmt {
                WebFormat::Jpeg => {
                    let (p, w, _h) = decode_jpeg(bytes, None).unwrap();
                    (p, w)
                }
                WebFormat::Png => {
                    let (p, w, _h, _srgb, color, _depth) = png_pixels(bytes);
                    assert_eq!(color, png::ColorType::Rgb, "this row's photo is opaque: RGB8 out");
                    (p, w)
                }
            };
            let i = (y * w as usize + x) * 3;
            [px[i], px[i + 1], px[i + 2]]
        };
        for fmt in [WebFormat::Jpeg, WebFormat::Png] {
            let noun = fmt.noun();
            let run =
                |src| export_web_image(rgb.clone(), 120, 120, 120, 98, Some(&wm), src, fmt).unwrap();
            let (srgb_run, adobe_run) = (run(Gamut::Srgb), run(Gamut::AdobeRgb));
            let (mark_srgb, mark_adobe) = (at(&srgb_run, fmt, 60, 60), at(&adobe_run, fmt, 60, 60));
            let (photo_srgb, photo_adobe) = (at(&srgb_run, fmt, 5, 5), at(&adobe_run, fmt, 5, 5));

            // (1) THE ASSERTION: the mark is the SAME in both exports. It is authored in sRGB and
            //     lands in an sRGB image, so the source gamut of the PHOTO can never reach it.
            let mark_moved =
                (0..3).map(|i| (mark_adobe[i] as i32 - mark_srgb[i] as i32).abs()).max().unwrap();
            assert!(
                mark_moved <= 3,
                "[{noun}] the mark must be identical whatever the source gamut, but it moved \
                 {mark_moved} ({mark_srgb:?} → {mark_adobe:?}) — the stamp is running BEFORE the \
                 conversion"
            );
            // (2) ANTI-VACUITY: the photo underneath DID convert, so row 1 is a real distinction
            //     and not a test of two identical exports.
            let photo_moved =
                (0..3).map(|i| (photo_adobe[i] as i32 - photo_srgb[i] as i32).abs()).max().unwrap();
            assert!(
                photo_moved > 12,
                "[{noun}] the PHOTO must convert (moved only {photo_moved}) — otherwise row 1 \
                 proves nothing"
            );
            // (3) …and it is genuinely the mark's own colour, not the transformed one. Named
            //     explicitly so a reordering failure reads as what it is rather than as "some
            //     numbers differ".
            let dragged = falcon_color::transform_rgb8(MARK, Gamut::AdobeRgb, Gamut::Srgb);
            let to_mark =
                (0..3).map(|i| (mark_adobe[i] as i32 - MARK[i] as i32).abs()).max().unwrap();
            let to_dragged =
                (0..3).map(|i| (mark_adobe[i] as i32 - dragged[i] as i32).abs()).max().unwrap();
            assert!(
                to_mark < to_dragged,
                "[{noun}] the stamped mark {mark_adobe:?} is nearer the DRAGGED value {dragged:?} \
                 than its own {MARK:?} — the sRGB-authored raster was pulled through the source \
                 transform"
            );
        }
    }

    /// v0.8.104 (Round-A C5): a watermark logo carries its OWN colour space, and `load_watermark_png_srgb`
    /// is where it is honoured. A Display-P3 PNG (what Figma / Sketch / Affinity / Photoshop-on-a-Mac
    /// write by default) used to be treated as sRGB by both the preview and the export, so a brand
    /// red shipped visibly desaturated in every ./export file.
    ///
    /// FALSIFIERS (L28): drop the `transform_rgba` and row 1 fails (the P3 bytes ride through raw);
    /// transform with the arguments swapped and row 1 fails against the independent `transform_rgb8`
    /// oracle; assume P3 for an untagged file and row 2 fails; drop the alpha exclusion anywhere in
    /// the chain and row 3 fails.
    #[test]
    fn a_wide_gamut_watermark_png_is_converted_to_srgb_at_load() {
        use crate::load_watermark_png_srgb;
        const PX: [u8; 4] = [200, 90, 40, 137]; // saturated, non-neutral, semi-transparent
        let raw: Vec<u8> = PX.iter().copied().cycle().take(4 * 4 * 4).collect();
        let write_png = |name: &str, icc: Option<Vec<u8>>| -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!("falcon_wm_c5_{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let path = dir.join(name);
            let mut info = png::Info::with_size(4, 4);
            info.color_type = png::ColorType::Rgba;
            info.bit_depth = png::BitDepth::Eight;
            info.icc_profile = icc.map(std::borrow::Cow::Owned);
            let file = std::fs::File::create(&path).expect("create png");
            let enc = png::Encoder::with_info(std::io::BufWriter::new(file), info).expect("encoder");
            enc.write_header().expect("header").write_image_data(&raw).expect("idat");
            path
        };
        // A REAL profile whose description a consumer would read — falcon-color's own P3
        // serialization, i.e. the same reader path `shot_source_gamut` uses for photos.
        let p3 = falcon_color::icc_bytes_for_gamut(Gamut::DisplayP3).expect("P3 serializes");
        let tagged = write_png("p3_logo.png", Some(p3));
        let untagged = write_png("plain_logo.png", None);

        // (1) the tagged logo is CONVERTED at load, and reports the gamut the file declared.
        let (rgba, w, h, src) = load_watermark_png_srgb(&tagged).expect("load the P3 logo");
        assert_eq!((w, h), (4, 4));
        assert_eq!(src, Gamut::DisplayP3, "the file's own iCCP must be read, not assumed");
        let want = falcon_color::transform_rgb8([PX[0], PX[1], PX[2]], Gamut::DisplayP3, Gamut::Srgb);
        assert_eq!(
            [rgba[0], rgba[1], rgba[2]],
            want,
            "a P3 logo must reach `wm_cache` in sRGB — that is the raster the export stamps"
        );
        assert_ne!([rgba[0], rgba[1], rgba[2]], [PX[0], PX[1], PX[2]], "…visibly, not by rounding");
        // (3) alpha is not colour — it must ride through the transform untouched.
        assert_eq!(rgba[3], PX[3], "the alpha byte must be preserved exactly");

        // (2) an UNTAGGED PNG keeps the sRGB assumption: the web-wide convention, and a no-op.
        let (plain, _, _, psrc) = load_watermark_png_srgb(&untagged).expect("load the plain logo");
        assert_eq!(psrc, Gamut::Srgb, "an untagged PNG is assumed sRGB, never guessed wide");
        assert_eq!(&plain[..4], &PX[..], "…so its bytes must ride through byte-identical");
        let _ = std::fs::remove_file(&tagged);
        let _ = std::fs::remove_file(&untagged);
    }

    /// v0.8.105 (W18): the FIFTH CELL of the ./export byte-pin — the one where the wave really does
    /// change delivered pixels.
    ///
    /// v0.8.104's evidence varied the PHOTO's source gamut × the mark's presence (4 cells) and
    /// concluded "pixels identical, only the ICC block moves". True for C1 (preview-only) and C3
    /// (metadata-only) — but C5 converts the MARK at load, so a Display-P3-tagged logo legitimately
    /// ships different pixels. Nothing varied the logo's own profile, so the pin could not tell "C5
    /// is inert here" from "C5 is broken". The bound is therefore restated and MEASURED here:
    /// **identical for an sRGB/untagged mark; a tagged wide-gamut mark changes by design.**
    ///
    /// Two PNGs with byte-identical IDATs, one tagged Display P3, exported through the real
    /// `export_web_jpeg` at the same quality:
    ///   * away from the mark the deliverables are identical (the bound still holds where C5 cannot
    ///     reach),
    ///   * under the mark they differ, and the stamped colour is the INDEPENDENT oracle
    ///     `transform_rgb8(MARK, DisplayP3, Srgb)` rather than the raw file bytes.
    /// The measured delta is printed, so the evidence is re-takeable rather than remembered.
    ///
    /// FALSIFIERS (L28): drop `load_watermark_png_srgb`'s `transform_rgba` (or route a `wm_cache`
    /// fill through the bare `load_png_rgba` again) and the tagged run stops moving — row 2 fails;
    /// convert with the arguments swapped and row 3 fails against the oracle; convert the UNTAGGED
    /// logo too and row 1's "identical away from the mark" survives but row 4 (the untagged run is
    /// byte-for-byte the raw-mark run) fails.
    #[test]
    fn a_tagged_wide_gamut_mark_changes_the_web_deliverable_by_design() {
        use crate::load_watermark_png_srgb;
        const PHOTO: [u8; 3] = [40, 200, 110];
        const MARK: [u8; 4] = [200, 90, 40, 255]; // saturated + non-neutral: a grey would hide it
        let raw: Vec<u8> = MARK.iter().copied().cycle().take(24 * 24 * 4).collect();
        let dir = std::env::temp_dir().join(format!("falcon_wm_w18_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let write_png = |name: &str, icc: Option<Vec<u8>>| -> std::path::PathBuf {
            let path = dir.join(name);
            let mut info = png::Info::with_size(24, 24);
            info.color_type = png::ColorType::Rgba;
            info.bit_depth = png::BitDepth::Eight;
            info.icc_profile = icc.map(std::borrow::Cow::Owned);
            let file = std::fs::File::create(&path).expect("create png");
            let enc = png::Encoder::with_info(std::io::BufWriter::new(file), info).expect("encoder");
            enc.write_header().expect("header").write_image_data(&raw).expect("idat");
            path
        };
        let p3 = falcon_color::icc_bytes_for_gamut(Gamut::DisplayP3).expect("P3 serializes");
        let tagged = write_png("p3_logo.png", Some(p3));
        let untagged = write_png("plain_logo.png", None);

        // The two marks, loaded exactly as `load_wm_logo` loads them for `wm_cache`.
        let mk = |p: &std::path::Path| -> Watermark {
            let (rgba, w, h, _src) = load_watermark_png_srgb(p).expect("load the logo");
            Watermark { rgba, w, h, scale: 0.5, opacity: 1.0, pos_x: 0.5, pos_y: 0.5 }
        };
        let rgb: Vec<u8> = PHOTO.iter().copied().cycle().take(120 * 120 * 3).collect();
        let run = |wm: Option<&Watermark>| -> Vec<u8> {
            export_web_jpeg(&rgb, 120, 120, 120, 98, wm, Gamut::Srgb).expect("export")
        };
        let at = |jpg: &[u8], x: usize, y: usize| -> [u8; 3] {
            let (px, w, _h) = decode_jpeg(jpg, None).unwrap();
            let i = (y * w as usize + x) * 3;
            [px[i], px[i + 1], px[i + 2]]
        };
        let (t_run, u_run) = (run(Some(&mk(&tagged))), run(Some(&mk(&untagged))));

        // (1) AWAY from the mark the bound still holds: same photo pixels, to the byte.
        assert_eq!(at(&t_run, 5, 5), at(&u_run, 5, 5), "outside the mark the deliverables agree");
        // (2) UNDER the mark they differ — by design, and by a visible amount.
        let (t_mark, u_mark) = (at(&t_run, 60, 60), at(&u_run, 60, 60));
        let delta = (0..3).map(|i| (t_mark[i] as i32 - u_mark[i] as i32).abs()).max().unwrap();
        eprintln!("W18 fifth cell: tagged-P3 mark {t_mark:?} vs untagged {u_mark:?} → max delta {delta}");
        assert!(
            delta > 8,
            "a Display-P3-tagged mark must land DIFFERENT in the deliverable (moved only {delta})"
        );
        // (3) …and it lands on the INDEPENDENT oracle, not on some other number.
        let want = falcon_color::transform_rgb8([MARK[0], MARK[1], MARK[2]], Gamut::DisplayP3, Gamut::Srgb);
        let off = (0..3).map(|i| (t_mark[i] as i32 - want[i] as i32).abs()).max().unwrap();
        assert!(off <= 3, "the stamped P3 mark {t_mark:?} must be {want:?} converted to sRGB");
        // (4) …while the untagged mark ships the file's own bytes (JPEG rounding aside).
        let off_raw = (0..3).map(|i| (u_mark[i] as i32 - MARK[i] as i32).abs()).max().unwrap();
        assert!(off_raw <= 3, "an untagged mark must ship as authored: {u_mark:?} vs {MARK:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v0.8.140 (C3) — THE LABEL HOLE. `normalize_cs` hands an unrecognised description straight
    /// through, so a screen capture's profile put the literal word "Display" in the info panel while
    /// the render pipeline (correctly, after C1/C2) converted its pixels FROM Display P3. A panel
    /// that contradicts the render is worse than one that says nothing.
    ///
    /// FALSIFIERS: drop the `r.gamut.label() != named` guard and row 1 fails (every ordinary file
    /// would grow a redundant "— profile '…'" tail); resolve from the DESCRIPTION instead of the
    /// bytes and row 2 fails; drop `quoted_profile_name`'s cap and row 5 fails; drop the `(assumed)`
    /// hedge and row 4 fails; drop the `profile_desc` gate and row 7 fails.
    ///
    /// v0.8.141 (R8/R10/B5): the four modeled gamuts each get an agreeing AND a disagreeing row —
    /// v0.8.140's rows only ever exercised sRGB and Display P3, so the Adobe RGB and Rec. 2020 arms
    /// of the agreement guard (which depend on `Gamut::label()` being spelled exactly the way
    /// `normalize_cs` spells it — pinned separately below) were never run at all.
    #[test]
    fn the_panel_names_the_resolved_space_when_the_profile_name_does_not() {
        use crate::{color_space_label_for, ColorTag};
        let bytes = |g: Gamut| falcon_color::icc_bytes_for_gamut(g).expect("a modeled gamut serializes");
        let (p3, srgb) = (bytes(Gamut::DisplayP3), bytes(Gamut::Srgb));
        let tag = |icc: &[u8], desc: &str| ColorTag {
            icc: Some(icc.to_vec()),
            desc: Some(desc.into()),
            desc_from_profile: true,
        };
        let label = |t: &ColorTag, desc: &str| {
            let r = falcon_color::resolve_source_gamut(t.icc.as_deref(), t.desc.as_deref());
            color_space_label_for(t, desc, &r)
        };

        // (1) name and colorimetry agree → EXACTLY what the panel said before this round. All four
        //     modeled gamuts, so the Adobe RGB / Rec. 2020 guards are really run.
        for (g, desc, want) in [
            (Gamut::DisplayP3, "Display P3", "Display P3"),
            (Gamut::Srgb, "sRGB IEC61966-2.1", "sRGB"),
            (Gamut::Srgb, "sRGB", "sRGB"),
            (Gamut::AdobeRgb, "Adobe RGB (1998)", "Adobe RGB"),
            (Gamut::AdobeRgb, "AdobeRGB1998", "Adobe RGB"),
            (Gamut::Rec2020, "Rec. 2020", "Rec. 2020"),
            (Gamut::Rec2020, "ITU-R BT.2020", "Rec. 2020"),
        ] {
            let icc = bytes(g);
            assert_eq!(label(&tag(&icc, desc), desc), want, "{desc:?} must be unchanged");
        }
        // (2) THE ROUND'S CASE: the name says nothing, the bytes say Display P3 — MEASURED, so no
        //     hedge. All four gamuts, each under a name that names nothing.
        for (g, want) in [
            (Gamut::Srgb, "sRGB — profile 'Display'"),
            (Gamut::DisplayP3, "Display P3 — profile 'Display'"),
            (Gamut::AdobeRgb, "Adobe RGB — profile 'Display'"),
            (Gamut::Rec2020, "Rec. 2020 — profile 'Display'"),
        ] {
            let icc = bytes(g);
            assert_eq!(label(&tag(&icc, "Display"), "Display"), want, "{}: measured, no hedge", g.label());
        }
        // (3) a MISLABELED profile: the panel follows the bytes too, and shows what the file claimed.
        assert_eq!(label(&tag(&srgb, "Display P3"), "Display P3"), "sRGB — profile 'Display P3'");
        // (4) v0.8.141 (R8/A7) — THE HEDGE. A profile whose colorants we could measure but NOT place
        //     (a measured panel, ProPhoto, true DCI) falls through to the name or to the sRGB floor,
        //     and the panel must not present that guess in the same voice as a measurement. Here: a
        //     real DCI-P3 profile named after the panel it came off, which resolves sRGB by the
        //     FLOOR — 0.08 from anything we model.
        let dci = bytes(Gamut::DciP3);
        let panel = "DELL U2723QE Cinema, calibrated";
        let got = label(&tag(&dci, panel), panel);
        eprintln!("C3/R8 unplaceable-profile label: {got}");
        assert_eq!(got, format!("sRGB (assumed) — profile '{panel}'"), "an unmeasured answer must say so");
        // …and the same profile under a name the matcher DOES know is hedged too (route Description).
        let named_dci = "DCI-P3 theatrical";
        assert_eq!(
            label(&tag(&dci, named_dci), named_dci),
            format!("Display P3 (assumed) — profile '{named_dci}'"),
            "the v0.8.67 name mapping answered, not the colorants — the panel says so"
        );
        // (5) a long real-world description is bounded so the panel row cannot be pushed off-panel.
        let long = "Dell S2725QS Native, D6500, gamma 2.2, MHC2 calibrated 2026-01-14";
        let got = label(&tag(&p3, long), long);
        eprintln!("C3 long-name label: {got}");
        assert!(got.starts_with("Display P3 — profile 'Dell S2725QS Native, D6500, gam"));
        assert!(got.ends_with("…'") && got.chars().count() < 60, "the raw name must be capped: {got}");
        // (6) a tag with no profile at all keeps normalize_cs's answer, annotation and all.
        let bytes_less = ColorTag { icc: None, desc: Some("ProPhoto RGB".into()), desc_from_profile: false };
        assert_eq!(label(&bytes_less, "ProPhoto RGB"), "ProPhoto RGB");
        // (7) v0.8.141 (R7) — A NAME THE PROFILE DOES NOT HAVE IS NEVER QUOTED AS THE PROFILE'S. A
        //     PNG carrying P3 bytes whose `desc` will not read, plus a bare `sRGB` chunk: the panel
        //     must name the resolved space and must NOT print `profile 'sRGB'` over P3 colorants.
        let synth = ColorTag { icc: Some(p3.clone()), desc: Some("sRGB".into()), desc_from_profile: false };
        let got = label(&synth, "sRGB");
        eprintln!("C3/R7 synthesized-name label: {got}");
        assert_eq!(got, "sRGB", "a container-declared name is not the profile's to quote");
        assert!(!got.contains("profile"), "…and must not be attributed to the profile: {got}");
    }

    /// v0.8.141 (R10/B5) — the agreement guard in `color_space_label_for` compares `Gamut::label()`
    /// against `normalize_cs`'s output. That only works if every modeled gamut's own label is a
    /// FIXED POINT of `normalize_cs`; if a future round renames one ("Rec.2020", say, or "P3") the
    /// guard silently stops matching and every ordinary file of that gamut grows a redundant
    /// "— profile '…'" tail. Two spellings, one line, pinned.
    #[test]
    fn every_modeled_gamut_label_is_a_fixed_point_of_normalize_cs() {
        for g in [Gamut::Srgb, Gamut::DisplayP3, Gamut::AdobeRgb, Gamut::Rec2020] {
            assert_eq!(
                crate::normalize_cs(g.label()),
                g.label(),
                "{}: the panel's agreement guard needs label() == normalize_cs(label())",
                g.label()
            );
        }
    }

    /// v0.8.141 (R11/B8) — the panel row is a fixed-width cell and an ICC description is untrusted
    /// free text. Three ways it could break out, all closed: control characters (a newline in a
    /// panel cell, and the same helper guards the LOG line, where a macOS file name carrying one
    /// would forge a second line), bidi overrides (which reorder everything after them), and a
    /// wide-script name that fits 32 `char`s but occupies ~64 cells.
    #[test]
    fn an_untrusted_profile_name_cannot_break_the_panel_row() {
        use crate::{quoted_profile_name, sanitize_one_line};
        let nasty = "Dell\u{202E}QS\nNative\u{7}";
        let got = quoted_profile_name(nasty);
        eprintln!("R11 sanitized: {got}");
        assert_eq!(got, "'DellQSNative'", "controls and bidi overrides are dropped, text survives");
        // A CJK name: 32 chars would be ~64 cells, so the cap must bite at 16 of them.
        let wide = "彩".repeat(40);
        let got = quoted_profile_name(&wide);
        let kept = got.chars().filter(|c| *c == '彩').count();
        eprintln!("R11 wide-name label: {got} ({kept} kept)");
        assert_eq!(kept, 16, "a width-2 script must be capped at 16 characters, not 32");
        assert!(got.ends_with("…'"), "…and marked as clipped: {got}");
        // The log-line half: a newline in a FILE NAME cannot forge a second line.
        assert_eq!(sanitize_one_line("IMG_1\n2026 colour: forged"), "IMG_12026 colour: forged");
    }

    /// v0.8.105 (W5): the preview's colour description really is ATTRIBUTED to the thumbnail item —
    /// it is not "the first `colr` in the file" wearing a longer name.
    ///
    /// Driven on a SYNTHETIC HEIF container (no codec, no testkit, runs on every machine) built so
    /// the two answers differ by construction: the master item declares Display P3 and the preview
    /// item declares sRGB, with the MASTER's box first in both `ipco` order and file order. A reader
    /// that takes the first box, or the last, or any box at all without walking
    /// `pitm` → `iref thmb` → `ipma` → `ipco`, gets Display P3 and fails.
    ///
    /// FALSIFIERS (L28): return the first `colr` and row 2 fails; look the properties up on the
    /// PRIMARY item instead of on the thumbnail item and row 2 fails; treat the 1-based property
    /// index as 0-based and row 2 fails (it reads the master's box); drop the `iref thmb`
    /// to-list check (so any `thmb` reference matches) and row 3 — a container whose thumbnail
    /// points at a DIFFERENT item — fails.
    #[test]
    fn heic_preview_colour_comes_from_the_thumbnail_item() {
        use crate::{heic_color_descs, heic_preview_color_desc};
        // ── the little box builders (size + fourcc + body; FullBoxes carry version+flags) ──
        let bx = |t: &[u8; 4], body: &[u8]| -> Vec<u8> {
            let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(t);
            v.extend_from_slice(body);
            v
        };
        let full = |t: &[u8; 4], ver: u8, flags: u32, body: &[u8]| -> Vec<u8> {
            let mut b = vec![ver, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
            b.extend_from_slice(body);
            bx(t, &b)
        };
        // `colr` nclx: colour_type + primaries + transfer + matrix + full-range byte.
        let colr = |prim: u16| -> Vec<u8> {
            let mut b = b"nclx".to_vec();
            b.extend_from_slice(&prim.to_be_bytes());
            b.extend_from_slice(&13u16.to_be_bytes()); // transfer
            b.extend_from_slice(&6u16.to_be_bytes()); // matrix
            b.push(0x80);
            bx(b"colr", &b)
        };
        // item 1 = master (P3, property 1), item 2 = its thumbnail (sRGB, property 2).
        let mut ipco = colr(12); // property 1 — Display P3, FIRST in the file
        ipco.extend_from_slice(&colr(1)); // property 2 — sRGB
        let ipco = bx(b"ipco", &ipco);
        let ipma = full(b"ipma", 0, 0, &[
            0, 0, 0, 2, // entry_count
            0, 1, 1, 1, // item 1 → [property 1]
            0, 2, 1, 2, // item 2 → [property 2]
        ]);
        let iprp = bx(b"iprp", &[ipco.clone(), ipma].concat());
        let pitm = full(b"pitm", 0, 0, &[0, 1]);
        let thmb_ref = |from: u16, to: u16| bx(b"thmb", &[from.to_be_bytes(), 1u16.to_be_bytes(), to.to_be_bytes()].concat());
        let write = |name: &str, iref_body: &[u8]| -> std::path::PathBuf {
            let meta = full(b"meta", 0, 0, &[pitm.clone(), full(b"iref", 0, 0, iref_body), iprp.clone()].concat());
            let dir = std::env::temp_dir().join(format!("falcon_heif_w5_{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let p = dir.join(name);
            let mut bytes = bx(b"ftyp", b"heic\0\0\0\0heic");
            bytes.extend_from_slice(&meta);
            std::fs::write(&p, &bytes).expect("write the synthetic container");
            p
        };

        // (1) the container really does carry BOTH declarations, master first.
        let good = write("attributed.heic", &thmb_ref(2, 1));
        assert_eq!(
            heic_color_descs(&good),
            vec!["Display P3".to_string(), "sRGB".to_string()],
            "the fixture must declare two different spaces, the master's first"
        );
        // (2) …and the preview reader picks the THUMBNAIL item's, not the first one.
        assert_eq!(
            heic_preview_color_desc(&good).as_deref(),
            Some("sRGB"),
            "the preview's colour must come from the item that declares itself the primary's thumbnail"
        );
        // (3) a `thmb` pointing at some OTHER item is not this master's preview → no answer, and the
        //     caller keeps the master's description.
        let other = write("unrelated_thmb.heic", &thmb_ref(2, 9));
        assert_eq!(heic_preview_color_desc(&other), None, "a thumbnail OF ANOTHER ITEM must not answer");
        let _ = std::fs::remove_dir_all(good.parent().unwrap());
    }

    /// v0.8.106 (Round-A final check, L28): a CRAFTED 64-bit `largesize` cannot walk the BMFF reader
    /// out of its buffer. The box size is attacker-controlled — any HEIC/AVIF a user opens, and the
    /// v0.8.105 (W5) preview probe now runs this walker on every HEIC thumbnail — and the old bound
    /// test was `i + size > end`: in DEBUG that addition PANICS on overflow, and in RELEASE it WRAPS,
    /// so a size chosen to wrap the sum back below `end` PASSED the guard. The cursor then moved
    /// BACKWARDS, and the emitted child had `e < s`, which underflow-panics every `e - s` length test
    /// at the call sites and panics outright on `buf[s..e]`. The fast/thumb worker's `catch_unwind`
    /// contains the crash, but the function's own contract is fail-soft.
    ///
    /// Row 1 drives `bmff_children` directly and asserts the POSTCONDITION (`start <= s <= e <= end`
    /// for every child) plus that the walk stops AT the bad header rather than before it. Row 2 is
    /// the end-to-end shape: a container whose `meta` box is the crafted one must yield `None`.
    ///
    /// The size is picked so that BOTH failure modes fire on the old code: at `i == 16`,
    /// `size = usize::MAX - 8` makes `i + size` wrap to 7 — a debug overflow panic, and in release a
    /// backward cursor plus the reversed span `(32, 7)`.
    ///
    /// FALSIFIERS (L28): restore `i + size > end` and this test panics with 'attempt to add with
    /// overflow' in the debug gate; drop the `size < hdr` half and a `largesize` of 8 emits a child
    /// whose payload start is past its end; make the call sites use plain `e - s` again and row 2
    /// panics instead of returning `None` the moment the postcondition is weakened.
    #[test]
    fn a_crafted_largesize_box_cannot_overflow_the_bmff_walk() {
        use crate::{bmff_children, heic_preview_color_desc};
        // A well-formed 16-byte `ftyp`, then a `largesize` header whose 64-bit size is chosen to
        // wrap `i + size`. Total 32 bytes, so the `i + 16 <= end` largesize-header test passes and
        // the size itself is the only thing standing between the walker and the buffer.
        let mut buf = Vec::new();
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"ftyp");
        buf.extend_from_slice(b"heic\0\0\0\0"); // 16 bytes so far
        buf.extend_from_slice(&1u32.to_be_bytes()); // size == 1 ⇒ 64-bit largesize follows
        buf.extend_from_slice(b"meta");
        buf.extend_from_slice(&(u64::MAX - 8).to_be_bytes()); // 16 + (2^64 − 9) ≡ 7 (mod 2^64)
        assert_eq!(buf.len(), 32);

        let kids = bmff_children(&buf, 0, buf.len());
        for (t, s, e) in &kids {
            assert!(
                *s <= *e && *e <= buf.len(),
                "postcondition: child {:?} spans ({s}, {e}) inside a {}-byte buffer",
                std::str::from_utf8(t).unwrap_or("????"),
                buf.len()
            );
        }
        assert_eq!(kids.len(), 1, "the walk keeps the good box and STOPS at the crafted one: {kids:?}");
        assert_eq!(&kids[0].0, b"ftyp");
        assert_eq!((kids[0].1, kids[0].2), (8, 16));

        // The same bytes as a file: fail-soft all the way out, no panic, no answer.
        let dir = std::env::temp_dir().join(format!("falcon_heif_l28_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let p = dir.join("crafted_largesize.heic");
        std::fs::write(&p, &buf).expect("write the crafted container");
        assert_eq!(
            heic_preview_color_desc(&p),
            None,
            "a container whose meta box carries a crafted largesize must answer None, softly"
        );
        // …and the degenerate largesize at i == 0 (no wrap, just absurdly large) is refused too.
        let mut huge = Vec::new();
        huge.extend_from_slice(&1u32.to_be_bytes());
        huge.extend_from_slice(b"meta");
        huge.extend_from_slice(&u64::MAX.to_be_bytes());
        assert!(bmff_children(&huge, 0, huge.len()).is_empty(), "a largesize of u64::MAX yields nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crop_region_clamps_and_outputs_rgba() {
        // 4×4 RGB; crop the inner 2×2 at (1,1), no downscale (out_long ≥ 2).
        let rgb = vec![7u8; 4 * 4 * 3];
        let (rgba, ow, oh, sx, sy, sw, sh) = crop_region_rgba(&rgb, 4, 4, 1, 1, 2, 2, 64).unwrap();
        assert_eq!((ow, oh), (2, 2));
        assert_eq!((sx, sy, sw, sh), (1, 1, 2, 2));
        assert_eq!(rgba.len(), 2 * 2 * 4);
        assert_eq!(rgba[3], 255); // opaque alpha
                                   // an out-of-bounds rect is clamped, not a panic
        assert!(crop_region_rgba(&rgb, 4, 4, 3, 3, 99, 99, 64).is_some());
    }

    #[test]
    fn fmt_shutter_rounds_long_denominators() {
        // phone imprecise rationals → 5 significant digits in the denominator
        assert_eq!(fmt_shutter("1/1000.4324342"), "1/1000.4");
        assert_eq!(fmt_shutter("1/33.333333"), "1/33.333");
        // clean camera values pass through unchanged
        assert_eq!(fmt_shutter("1/125"), "1/125");
        assert_eq!(fmt_shutter("1/8000"), "1/8000");
        assert_eq!(fmt_shutter("30"), "30");
        assert_eq!(fmt_shutter("0.5"), "0.5");
    }

    #[test]
    fn brief_formatters_round_for_the_folded_grid_only() {
        // the ROW value (with its " s" suffix) rounds like fmt_shutter
        assert_eq!(brief_shutter("1/1000.4324342 s"), "1/1000.4 s");
        assert_eq!(brief_shutter("1/125 s"), "1/125 s");
        assert_eq!(brief_shutter("30 s"), "30 s");
        // phone long decimals → 3 significant digits, prefix/suffix intact
        assert_eq!(brief_round("f/1.7799999"), "f/1.78");
        assert_eq!(brief_round("6.764999999 mm"), "6.76 mm");
        // clean camera values pass through unchanged
        assert_eq!(brief_round("f/2.8"), "f/2.8");
        assert_eq!(brief_round("f/11"), "f/11");
        assert_eq!(brief_round("400 mm"), "400 mm");
        assert_eq!(brief_round("135.0 mm"), "135 mm"); // trailing-zero tidy-up is welcome
        // non-numeric input is untouched, empty stays empty
        assert_eq!(brief_round(""), "");
        assert_eq!(brief_round("n/a"), "n/a");
    }

    #[test]
    fn text_watermark_rasterizes_with_a_system_font() {
        // Best-effort: exercise the rasteriser with a Windows system font when present; on a
        // box without it (CI/Linux) the test skips cleanly rather than bundling a font here.
        let candidates = [r"C:\Windows\Fonts\arial.ttf", r"C:\Windows\Fonts\segoeui.ttf"];
        let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
            return;
        };
        let (rgba, w, h) = text_watermark_rgba("Falcon", [255, 255, 255], &bytes, 0).unwrap();
        assert!(w > 0 && h > 0, "non-empty raster");
        assert_eq!(rgba.len(), (w * h * 4) as usize, "packed RGBA");
        // at least one near-opaque white glyph pixel exists
        assert!(
            rgba.chunks_exact(4).any(|p| p[3] > 200 && p[0] > 200 && p[1] > 200),
            "white text pixels present"
        );
        // empty text is rejected, not a panic
        assert!(text_watermark_rgba("   ", [255, 255, 255], &bytes, 0).is_err());
    }

    /// FROZEN REFERENCE ORACLE for the export watermark raster — an independent, byte-for-byte
    /// copy of the PRE-split v0.8.90 export rasteriser (the body of `render_text_strip` at commit
    /// 8869188, i.e. the historical `text_watermark_rgba` core, BEFORE the v0.8.91
    /// `render_text_strip_impl` split introduced the picker's fixed-baseline/width-cap/fade
    /// branches). It is deliberately DUPLICATED and must never be refactored to call the live
    /// code: it is the frozen yardstick that makes the export pin real. Any future edit to the
    /// shared impl's None-path that changes the exported raster breaks the assertion in
    /// `render_text_strip_matches_the_export_watermark_at_480px`. Machine-independent: it
    /// re-derives the raster from the SAME system font the live path loads, so nothing here golden-
    /// hashes a font whose glyphs vary by box or version. `all_notdef` never affects the raster,
    /// so the oracle drops its counters and returns just `(rgba, w, h)` — the export contract.
    fn export_watermark_oracle_pre_v0_8_91(
        font_bytes: &[u8],
        face_index: u32,
        text: &str,
        px: f32,
        color: [u8; 3],
    ) -> anyhow::Result<(Vec<u8>, u32, u32)> {
        use ab_glyph::{point, Font, FontRef, ScaleFont};

        let text: String = text.trim().chars().take(64).collect();
        if text.is_empty() {
            anyhow::bail!("empty watermark text");
        }
        let font = FontRef::try_from_slice_and_index(font_bytes, face_index)
            .map_err(|e| anyhow::anyhow!("font parse: {e}"))?;
        let sf = font.as_scaled(px);
        let shadow = (px * 0.04).round().max(1.0) as i32; // drop-shadow offset, in render px
        let pad = (px * 0.10).ceil() as i32 + shadow;

        // Lay glyphs along the baseline (y = ascent), with kerning; skip .notdef (id 0).
        let mut pen = 0.0_f32;
        let mut glyphs = Vec::new();
        let mut prev = None;
        for ch in text.chars() {
            let id = font.glyph_id(ch);
            if id.0 == 0 {
                continue;
            }
            if let Some(p) = prev {
                pen += sf.kern(p, id);
            }
            glyphs.push(id.with_scale_and_position(px, point(pen, sf.ascent())));
            pen += sf.h_advance(id);
            prev = Some(id);
        }
        let text_w = pen.ceil().max(1.0) as i32;
        let text_h = (sf.ascent() - sf.descent()).ceil().max(1.0) as i32;
        let w = (text_w + 2 * pad) as usize;
        let h = (text_h + 2 * pad) as usize;

        // Accumulate glyph coverage (anti-aliased alpha) into a single plane.
        let mut cov = vec![0f32; w * h];
        for g in glyphs {
            if let Some(og) = font.outline_glyph(g) {
                let bb = og.px_bounds();
                og.draw(|gx, gy, c| {
                    let x = bb.min.x as i32 + gx as i32 + pad;
                    let y = bb.min.y as i32 + gy as i32 + pad;
                    if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                        let idx = y as usize * w + x as usize;
                        cov[idx] = (cov[idx] + c).min(1.0);
                    }
                });
            }
        }

        // Composite a dark drop-shadow (coverage sampled up-left), then the coloured text over it.
        let so = shadow as usize;
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let t = cov[y * w + x];
                let s = if x >= so && y >= so { cov[(y - so) * w + (x - so)] } else { 0.0 };
                let sa = s * 0.55; // shadow strength under the glyph
                let r = color[0] as f32 * t;
                let gg = color[1] as f32 * t;
                let b = color[2] as f32 * t;
                let a = t + sa * (1.0 - t);
                let i = (y * w + x) * 4;
                rgba[i] = r.round().clamp(0.0, 255.0) as u8;
                rgba[i + 1] = gg.round().clamp(0.0, 255.0) as u8;
                rgba[i + 2] = b.round().clamp(0.0, 255.0) as u8;
                rgba[i + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }

        // Trim to the non-transparent bounds on BOTH axes.
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
        for y in 0..h {
            for x in 0..w {
                if rgba[(y * w + x) * 4 + 3] > 0 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        if x1 < x0 || y1 < y0 {
            anyhow::bail!("watermark text produced no pixels");
        }
        let (cw, ch) = ((x1 - x0 + 1), (y1 - y0 + 1));
        let mut out = vec![0u8; cw * ch * 4];
        for row in 0..ch {
            let src = ((y0 + row) * w + x0) * 4;
            let dst = row * cw * 4;
            out[dst..dst + cw * 4].copy_from_slice(&rgba[src..src + cw * 4]);
        }
        Ok((out, cw as u32, ch as u32))
    }

    /// v0.8.91: the export path (`text_watermark_rgba` → `render_text_strip` → `…_impl(None)`)
    /// must stay BYTE-IDENTICAL to the pre-split v0.8.90 export rasteriser. Two independent pins:
    ///   1. REAL pin — the live export entry equals the FROZEN oracle above (a duplicated copy of
    ///      the old body, NOT a forward into the new impl) over inputs that exercise the whole
    ///      None-path. Because the oracle is frozen, a future regression of the shared impl's
    ///      None-path fails HERE honestly — the earlier `text_watermark_rgba == render_text_strip`
    ///      check alone was a tautology (both call the same code) and could not.
    ///   2. Wrapper pin — the original `text_watermark_rgba == render_text_strip(…, 480.0, …)`
    ///      assertion, kept to pin the wrapper's forwarded args (px = 480, same font/colour).
    #[test]
    fn render_text_strip_matches_the_export_watermark_at_480px() {
        let candidates = [r"C:\Windows\Fonts\arial.ttf", r"C:\Windows\Fonts\segoeui.ttf"];
        let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
            return; // best-effort on a box with no system font (CI/Linux)
        };

        // 1. REAL pin: live export raster == frozen oracle, byte-for-byte, over inputs that drive
        //    the whole None-path — caps + digits + the © symbol; glyphs with heavy descenders
        //    (drives the vertical trim); and a thin pair whose baked drop-shadow defines the
        //    trimmed down-right edge. The oracle is frozen, so a None-path change fails right here.
        for text in ["© Falcon 2026", "jumpy frog qty", "i."] {
            let (live, lw, lh) = text_watermark_rgba(text, [20, 20, 20], &bytes, 0).unwrap();
            let (oracle, ow, oh) =
                export_watermark_oracle_pre_v0_8_91(&bytes, 0, text, 480.0, [20, 20, 20]).unwrap();
            assert_eq!((lw, lh), (ow, oh), "export dims diverged from the frozen oracle for {text:?}");
            assert_eq!(live, oracle, "export raster diverged from the frozen oracle for {text:?}");
        }

        // 2. Wrapper pin (original assertion): `text_watermark_rgba` forwards to
        //    `render_text_strip(…, 480.0, …)` with the historical args.
        let (wm, ww, wh) = text_watermark_rgba("© Falcon 2026", [20, 20, 20], &bytes, 0).unwrap();
        let (st, sw, sh, all_notdef) =
            render_text_strip(&bytes, 0, "© Falcon 2026", 480.0, [20, 20, 20]).unwrap();
        assert_eq!((ww, wh), (sw, sh), "identical trimmed dims");
        assert_eq!(wm, st, "wrapper forwards to render_text_strip unchanged");
        assert!(!all_notdef, "a real text in a real font is never all-.notdef");
    }

    /// v0.8.90 (font-picker previews): the strip rasterises at small picker sizes, and the
    /// no-glyph funnel (symbol fonts / PUA junk) errors cleanly instead of panicking or
    /// producing an empty-but-Ok strip — the picker keeps the plain name on ANY non-Ok result.
    #[test]
    fn render_text_strip_picker_size_and_notdef_funnel() {
        let candidates = [r"C:\Windows\Fonts\arial.ttf", r"C:\Windows\Fonts\segoeui.ttf"];
        let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
            return;
        };
        let (rgba, w, h, all_notdef) =
            render_text_strip(&bytes, 0, "Arial", 24.0, [247, 247, 247]).unwrap();
        assert!(w > 0 && h > 0, "non-empty picker strip");
        assert_eq!(rgba.len(), (w * h * 4) as usize, "packed RGBA");
        assert!(!all_notdef);
        assert!(h < 48, "a 24px-glyph strip stays comfortably inside the 30px picker row at 2x (h={h})");
        // PUA-only text: every char is .notdef → no pixels → a clean Err (never a panic); the
        // producer treats Err exactly like all_notdef — the row keeps its plain name.
        assert!(render_text_strip(&bytes, 0, "\u{E011}\u{E012}", 24.0, [247, 247, 247]).is_err());
    }

    /// v0.8.91 (picker fix A): every picker strip shares ONE font-independent canvas height —
    /// H = ceil(1.10·px) + ceil(0.28·px) + shadow — regardless of whether the name has
    /// descenders, so the Slint side's own-height centering yields a constant baseline (no
    /// row-to-row wobble, no plain→preview jump). The EXPORT entry keeps its inked-bbox trim
    /// (heights DIFFER with content there) — pinning that the picker mode didn't leak in.
    #[test]
    fn picker_strips_share_one_fixed_height_and_the_export_trim_is_untouched() {
        let candidates = [r"C:\Windows\Fonts\segoeui.ttf", r"C:\Windows\Fonts\arial.ttf"];
        let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
            return; // best-effort on a box with no system font (CI/Linux)
        };
        let px = 24.0f32;
        let expect_h = {
            let shadow = (px * 0.04).round().max(1.0) as u32; // = 1 at 24px
            (px * 1.10).ceil() as u32 + (px * 0.28).ceil() as u32 + shadow // 27 + 7 + 1 = 35
        };
        // "ACE" — caps only, no descender; "gypsy" — descenders on every glyph.
        let (_, _, h1, _) =
            render_text_strip_picker(&bytes, 0, "ACE", px, [247, 247, 247], 0).unwrap();
        let (_, _, h2, _) =
            render_text_strip_picker(&bytes, 0, "gypsy", px, [247, 247, 247], 0).unwrap();
        assert_eq!(h1, expect_h, "picker canvas height derives from px only");
        assert_eq!(h1, h2, "descender and no-descender names share one strip height");
        // Export mode still trims to the inked bbox: caps-plus-descenders ("Ag" spans the cap
        // height AND the descender) is strictly taller than caps-only ("ACE").
        let (_, _, e1, _) = render_text_strip(&bytes, 0, "ACE", px, [247, 247, 247]).unwrap();
        let (_, _, e2, _) = render_text_strip(&bytes, 0, "Ag", px, [247, 247, 247]).unwrap();
        assert!(e2 > e1, "export trim still content-dependent (ACE {e1} vs Ag {e2})");
        assert!(e1 < expect_h, "export trim is tighter than the fixed picker canvas");
    }

    /// v0.8.91 (picker fix B): a strip wider than `max_w` is capped at `max_w` with a linear
    /// right-edge alpha fade over the last STRIP_FADE_PX columns; the columns BEFORE the fade
    /// are byte-identical to the uncapped render, and an uncapped strip has no fade at all.
    #[test]
    fn picker_strip_fades_only_when_capped() {
        let candidates = [r"C:\Windows\Fonts\segoeui.ttf", r"C:\Windows\Fonts\arial.ttf"];
        let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
            return;
        };
        let px = 24.0f32;
        let long = "A Very Long Font Family Name Indeed";
        let (full, fw, fh, _) =
            render_text_strip_picker(&bytes, 0, long, px, [247, 247, 247], 0).unwrap();
        let cap = fw / 2; // force a cap well inside the inked extent
        assert!(cap > STRIP_FADE_PX, "test geometry: cap wider than the fade band");
        let (capped, cw, ch, _) =
            render_text_strip_picker(&bytes, 0, long, px, [247, 247, 247], cap).unwrap();
        assert_eq!(cw, cap, "capped exactly at max_w");
        assert_eq!(ch, fh, "the cap never changes the fixed height");
        // Pre-fade columns are byte-identical to the uncapped render (same trim origin).
        let pre = (cap - STRIP_FADE_PX) as usize;
        for row in 0..ch as usize {
            let a = &capped[row * cw as usize * 4..(row * cw as usize + pre) * 4];
            let b = &full[row * fw as usize * 4..(row * fw as usize + pre) * 4];
            assert_eq!(a, b, "fade touches only the last STRIP_FADE_PX columns (row {row})");
        }
        // The capped strip's final column is (near-)transparent; the uncapped strip's final
        // column is inked by trim-tightness. Compare column max-alpha.
        let col_max_a = |buf: &[u8], w: u32, h: u32, col: u32| -> u8 {
            (0..h).map(|r| buf[((r * w + col) * 4 + 3) as usize]).max().unwrap()
        };
        let full_last = col_max_a(&full, fw, fh, fw - 1);
        let capped_last = col_max_a(&capped, cw, ch, cw - 1);
        assert!(full_last > 0, "uncapped strip: trim guarantees an inked last column");
        assert!(
            capped_last <= 32,
            "capped strip fades to near-transparent at the cut edge (max alpha {capped_last})"
        );
    }

    /// v0.8.90 (ITEM 2, closes the v0.8.89 audit candidate): a READABLE file with no EXIF container
    /// (PNG without an eXIf chunk) yields its EXIF-independent rows IMMEDIATELY — Dimensions (with
    /// the oriented swap) + Files — so the v0.8.89 empty-parse retry no longer burns 5 attempts
    /// (~11 s of blank panel) on it. A 0-byte/mid-copy file still yields EMPTY → stays on the retry
    /// path (self-heal preserved for the case it was built for).
    #[test]
    fn exif_rows_for_exifless_readable_files() {
        let dir = std::env::temp_dir().join(format!("falcon_exifless_rows_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // a tiny 3×2 RGB PNG — the png crate writes no EXIF chunk
        let p = dir.join("plain.png");
        {
            let f = std::fs::File::create(&p).unwrap();
            let mut enc = png::Encoder::new(std::io::BufWriter::new(f), 3, 2);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&[128u8; 18]).unwrap();
        }
        let shot = |path: std::path::PathBuf| Shot {
            id: 0,
            name: "plain".to_string(),
            has_raw: false,
            has_jpg: true,
            raw: None,
            jpg: Some(path),
            kind: SrcKind::Png,
            cloud_placeholder: false,
            sniffed: None,
        };
        let rows = exif_rows(&shot(p.clone()), 0);
        assert!(
            rows.iter().any(|(k, v)| k == "Dimensions" && v == "3 × 2"),
            "EXIF-less PNG emits Dimensions immediately (rows: {rows:?})"
        );
        assert!(
            rows.iter().any(|(k, v)| k == "Files" && v.contains("PNG")),
            "EXIF-less PNG emits the Files row (rows: {rows:?})"
        );
        // odd display turns swap the oriented Dimensions exactly like the EXIF-present fallback
        let rows90 = exif_rows(&shot(p), 1);
        assert!(rows90.iter().any(|(k, v)| k == "Dimensions" && v == "2 × 3"), "oriented swap");
        // the mid-copy case: a 0-byte ".png" has no parsable header → EMPTY → the retry path
        let z = dir.join("zero.png");
        std::fs::write(&z, b"").unwrap();
        let mut zs = shot(z);
        zs.name = "zero".to_string();
        assert!(exif_rows(&zs, 0).is_empty(), "unreadable file still routes to the v0.8.89 retry");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frame_number_extracts_trailing_digits() {
        assert_eq!(frame_number("HWU_7841"), "7841"); // Canon-style prefix
        assert_eq!(frame_number("DSC_0123"), "0123"); // Nikon/Sony, keep leading zero
        assert_eq!(frame_number("IMG_5678"), "5678");
        assert_eq!(frame_number("P1000123"), "1000123"); // Panasonic, no separator
        assert_eq!(frame_number("100_3021"), "3021"); // folder-style, only trailing run
        assert_eq!(frame_number("plain"), ""); // no trailing digits
    }

    /// v0.8.57 (owner rider) + v0.8.61 (owner bug — `HWU_0544E` fell to the fallback): the
    /// number-CHIP label never renders empty; the last ≥2-digit run anywhere is the counter,
    /// a trailing single digit still counts, a lone mid-name digit is noise (text fallback).
    #[test]
    fn tile_number_label_never_empty() {
        // trailing runs — byte-identical to the pre-v0.8.61 behaviour
        assert_eq!(tile_number_label("HWU_7841"), "7841");
        assert_eq!(tile_number_label("DSC_0123"), "0123"); // leading zero preserved
        assert_eq!(tile_number_label("P1000123"), "1000123"); // no separator, whole run
        assert_eq!(tile_number_label("shot_1"), "1"); // trailing single digit still shown
        // v0.8.61: non-trailing counters now found (the owner's screenshot case first)
        assert_eq!(tile_number_label("HWU_0544E"), "0544"); // letter-suffixed counter
        assert_eq!(tile_number_label("7O7A1389"), "1389"); // last ≥2 group wins, not the lone 7s
        assert_eq!(tile_number_label("DSC00042"), "00042"); // leading zeros as written
        assert_eq!(tile_number_label("IMG_1234 (2)"), "1234", "the (2) copy-suffix is a 1-digit group — the counter wins");
        // v0.8.61 owner amendment: BOTH date/time screenshot shapes yield the TIME field — the
        // undotted run as written, the dotted one fused across single '.'/'-'/':' separators.
        assert_eq!(tile_number_label("Screenshot 2026-07-16 125538"), "125538");
        assert_eq!(tile_number_label("截屏2025-12-13 21.24.40"), "212440"); // 21.24.40 = ONE fused time
        assert_eq!(tile_number_label("IMG 2026-07-16"), "20260716"); // date-only name → the fused date
        assert_eq!(tile_number_label("a1b2"), "2"); // singles only (text breaks groups) → trailing digit
        // digitless / noise fallbacks — unchanged from v0.8.57
        assert_eq!(tile_number_label("sunset-milan"), "sunse…"); // digitless → first 5 + …
        assert_eq!(tile_number_label("abc"), "abc"); // short digitless → whole name, no ellipsis
        assert_eq!(tile_number_label("plain"), "plain"); // exactly 5 chars → no ellipsis
        assert_eq!(tile_number_label("vacation2milan"), "vacat…"); // lone MID-name digit = noise, not a counter
    }

    // ── v0.8.85: the FOLDER-AWARE number chip (tile_number_plan + tile_number_label_planned) ──

    #[test]
    fn scan_fused_groups_tokenizes_and_skeletonizes() {
        // v0.8.86: each group's span collapses to a NUL (`'\0'`) marker, NOT '#', so a LITERAL '#'
        // in a filename can never masquerade as a group boundary (see scan_fused_groups' doc). The
        // GROUP vectors below are byte-identical to v0.8.85 — only the skeleton's marker char moved.
        // the owner's shape: three fused groups, the skeleton keeps the literal glue verbatim
        let (g, sk) = scan_fused_groups("10001_v1_2048");
        assert_eq!(g, vec!["10001".to_string(), "1".to_string(), "2048".to_string()]);
        assert_eq!(sk, "\0_v\0_\0");
        // a fused time across single separators is ONE group; a non-ASCII prefix rides the skeleton
        let (g2, sk2) = scan_fused_groups("截屏2025-12-13 21.24.40");
        assert_eq!(g2, vec!["20251213".to_string(), "212440".to_string()]);
        assert_eq!(sk2, "截屏\0 \0");
        // the copy suffix (2) is its OWN 1-digit group; the base is column 0
        let (g3, sk3) = scan_fused_groups("IMG_1234 (2)");
        assert_eq!(g3, vec!["1234".to_string(), "2".to_string()]);
        assert_eq!(sk3, "IMG_\0 (\0)");
        // digitless → no groups, the skeleton is the whole stem
        let (g4, sk4) = scan_fused_groups("sunset-milan");
        assert!(g4.is_empty());
        assert_eq!(sk4, "sunset-milan");
    }

    #[test]
    fn tile_number_plan_owner_example() {
        // [10001_v1_2048, 10002_v1_2048, 10003_v2_2048]: the last ≥2-digit group (2048) is CONSTANT
        // across the folder, so the folder-blind chip would repeat "2048" on every tile. The plan
        // picks the unique 5-digit head (P1) so each tile shows its own id.
        let stems = ["10001_v1_2048", "10002_v1_2048", "10003_v2_2048"];
        let plan = tile_number_plan(&stems);
        assert_eq!(tile_number_label_planned("10001_v1_2048", &plan), "10001");
        assert_eq!(tile_number_label_planned("10002_v1_2048", &plan), "10002");
        assert_eq!(tile_number_label_planned("10003_v2_2048", &plan), "10003");
        // proof the plan is doing the work: the folder-blind label is the useless constant
        assert_eq!(tile_number_label("10001_v1_2048"), "2048");
    }

    #[test]
    fn tile_number_plan_no_op_on_uniform_folders() {
        // IMG_#### folders: the single fused group is always-unique + ≥2 digits → P1 picks it, which
        // is exactly the value tile_number_label already returns. The amendment is a NO-OP here.
        let stems = ["IMG_1230", "IMG_1231", "IMG_1232"];
        let plan = tile_number_plan(&stems);
        for s in stems {
            assert_eq!(tile_number_label_planned(s, &plan), tile_number_label(s));
        }
        assert_eq!(tile_number_label_planned("IMG_1231", &plan), "1231");
        // inverse: FIRST column constant, LAST unique → rightmost P1 = the last group, same as today
        let inv = ["v1_0001", "v1_0002"];
        let planv = tile_number_plan(&inv);
        assert_eq!(tile_number_label_planned("v1_0001", &planv), "0001");
        assert_eq!(tile_number_label("v1_0001"), "0001"); // byte-identity
        assert_eq!(tile_number_label_planned("v1_0002", &planv), "0002");
    }

    #[test]
    fn tile_number_plan_rightmost_tiebreak() {
        // date + time columns both pairwise-unique → RIGHTMOST (the time), the owner tie-break.
        let stems = ["IMG_20260720_123456", "IMG_20260721_123457"];
        let plan = tile_number_plan(&stems);
        assert_eq!(tile_number_label_planned("IMG_20260720_123456", &plan), "123456");
        assert_eq!(tile_number_label_planned("IMG_20260721_123457", &plan), "123457");
    }

    #[test]
    fn tile_number_plan_copy_suffix_filled_folder_p2() {
        // a folder FILLED with same-base copies: the base is constant, the (n) suffix varies → P2
        // (strict majority — the whole folder is this skeleton) shows 1 / 2 / 3.
        let stems = ["IMG_1234 (1)", "IMG_1234 (2)", "IMG_1234 (3)"];
        let plan = tile_number_plan(&stems);
        assert_eq!(tile_number_label_planned("IMG_1234 (1)", &plan), "1");
        assert_eq!(tile_number_label_planned("IMG_1234 (2)", &plan), "2");
        assert_eq!(tile_number_label_planned("IMG_1234 (3)", &plan), "3");
    }

    #[test]
    fn tile_number_plan_copy_suffix_outliers_p3() {
        // a majority-plain IMG_#### folder with a FEW same-base (n) outliers: the outliers' skeleton
        // is NOT a strict majority → P2 gated off → they fall to P3 and show the BASE (1234); the
        // plain files resolve on P1 to their own ids.
        let stems = ["IMG_1230", "IMG_1231", "IMG_1232", "IMG_1234 (1)", "IMG_1234 (2)"];
        let plan = tile_number_plan(&stems);
        assert_eq!(tile_number_label_planned("IMG_1230", &plan), "1230");
        assert_eq!(tile_number_label_planned("IMG_1234 (1)", &plan), "1234");
        assert_eq!(tile_number_label_planned("IMG_1234 (2)", &plan), "1234");
    }

    #[test]
    fn tile_number_plan_copy_suffix_5050_gated_off() {
        // 2 plain + 2 suffixed: the suffixed skeleton has group_n*2 == folder_n (NOT a strict
        // majority) → P2 gated off → both suffixed tiles show the base 1234.
        let stems = ["IMG_1230", "IMG_1231", "IMG_1234 (1)", "IMG_1234 (2)"];
        let plan = tile_number_plan(&stems);
        assert_eq!(tile_number_label_planned("IMG_1234 (1)", &plan), "1234");
        assert_eq!(tile_number_label_planned("IMG_1234 (2)", &plan), "1234");
    }

    #[test]
    fn tile_number_plan_copy_suffix_different_bases_p1() {
        // two same-suffix copies of DIFFERENT bases → the base column is always-unique + ≥2 digits →
        // P1 fires (no majority gate on P1), so they show 1234 / 1235, not the constant "2".
        let stems = ["IMG_1234 (2)", "IMG_1235 (2)"];
        let plan = tile_number_plan(&stems);
        assert_eq!(tile_number_label_planned("IMG_1234 (2)", &plan), "1234");
        assert_eq!(tile_number_label_planned("IMG_1235 (2)", &plan), "1235");
        // even embedded in a plain-majority folder, P1 still fires for the pair (n ≥ 2 suffices)
        let big = ["IMG_9990", "IMG_9991", "IMG_9992", "IMG_1234 (2)", "IMG_1235 (2)"];
        let planb = tile_number_plan(&big);
        assert_eq!(tile_number_label_planned("IMG_1234 (2)", &planb), "1234");
        assert_eq!(tile_number_label_planned("IMG_1235 (2)", &planb), "1235");
    }

    #[test]
    fn tile_number_plan_singletons_and_fallthrough() {
        // mixed folder: a lone (n=1) skeleton has no plan and falls back byte-identically.
        let stems = ["IMG_1230", "IMG_1231", "DSC_9999"];
        let plan = tile_number_plan(&stems);
        assert_eq!(tile_number_label_planned("IMG_1230", &plan), "1230");
        assert_eq!(tile_number_label_planned("DSC_9999", &plan), tile_number_label("DSC_9999"));
        assert_eq!(tile_number_label_planned("DSC_9999", &plan), "9999");
        // empty + one-file folders: no plan; application falls through
        assert!(tile_number_plan(&[]).is_empty());
        let one = tile_number_plan(&["IMG_1234"]);
        assert!(one.is_empty());
        assert_eq!(tile_number_label_planned("IMG_1234", &one), "1234");
    }

    #[test]
    fn tile_number_plan_leading_zeros_preserved() {
        // the chosen column renders its digits AS WRITTEN — leading zeros survive.
        let stems = ["0001_2048", "0002_2048", "0003_2048"];
        let plan = tile_number_plan(&stems);
        assert_eq!(tile_number_label_planned("0001_2048", &plan), "0001");
        assert_eq!(tile_number_label_planned("0002_2048", &plan), "0002");
    }

    #[test]
    fn tile_number_plan_mixed_skeletons_independent() {
        // two skeletons in one folder, planned on their own: A picks its unique head over the
        // constant tail; B (SNAP_####) shows its own id.
        let stems = [
            "10001_v1_2048", "10002_v1_2048", "10003_v1_2048",
            "SNAP_5000", "SNAP_5001", "SNAP_5002",
        ];
        let plan = tile_number_plan(&stems);
        assert_eq!(tile_number_label_planned("10002_v1_2048", &plan), "10002");
        assert_eq!(tile_number_label_planned("SNAP_5001", &plan), "5001");
    }

    // ── v0.8.86: '#'-in-filename robustness (the ragged-skeleton crash) + defence-in-depth guard ──

    #[test]
    fn tile_number_plan_hash_collision_no_panic() {
        // THE AUDIT REPRO. Under the old '#' skeleton marker these two stems BOTH skeletonized to
        // "###" while carrying DIFFERENT group counts ("1#2" → ["1","2"], "##3" → ["3"]), so
        // choose_number_column's `files[0].len()` + unguarded `f[j]` indexed out of bounds and
        // panicked on the UI thread at scan time. With the NUL marker their skeletons now DIFFER
        // ("\0#\0" vs "##\0"), so each is a singleton skeleton → no plan → per-file fallback.
        let stems = ["1#2", "##3"];
        let plan = tile_number_plan(&stems); // must NOT panic
        // no skeleton has ≥2 files → nothing planned
        assert!(plan.is_empty());
        // both files degrade gracefully to the folder-blind label (no crash, sane output)
        assert_eq!(tile_number_label_planned("1#2", &plan), tile_number_label("1#2"));
        assert_eq!(tile_number_label_planned("##3", &plan), tile_number_label("##3"));
    }

    #[test]
    fn scan_fused_groups_literal_hash_is_distinct_from_marker() {
        // the natural collision pair: a digit vs a literal '#' in the SAME textual slot. The digit
        // becomes a group (marker in the skeleton); the '#' rides the skeleton verbatim as data.
        // Distinct skeletons now — the whole point of moving the marker off the data alphabet.
        let (g1, sk1) = scan_fused_groups("a1b");
        let (g2, sk2) = scan_fused_groups("a#b");
        assert_eq!(g1, vec!["1".to_string()]);
        assert_eq!(sk1, "a\0b");
        assert!(g2.is_empty());
        assert_eq!(sk2, "a#b");
        assert_ne!(sk1, sk2, "a literal '#' must never share a skeleton with a digit group");
        // and the planner over the pair does not panic (two distinct singleton skeletons)
        let _ = tile_number_plan(&["a1b", "a#b"]);
    }

    #[test]
    fn choose_number_column_ragged_returns_none_not_panic() {
        // BELT test: feed the ragged condition DIRECTLY (the NUL marker prevents it arising upstream,
        // but a future tokenizer change must degrade to the per-file fallback, never index-panic).
        // files[0] has 2 columns, files[1] has 1 → the old unguarded `f[1]` would panic.
        let ragged = vec![
            vec!["1".to_string(), "2".to_string()],
            vec!["3".to_string()],
        ];
        assert_eq!(choose_number_column(&ragged, 2), None); // no panic, graceful None
        // reversed lengths (shorter first) must also be safe
        let ragged_rev = vec![
            vec!["3".to_string()],
            vec!["1".to_string(), "2".to_string()],
        ];
        assert_eq!(choose_number_column(&ragged_rev, 2), None);
        // sanity: the guard does NOT over-trigger — a well-formed equal-length pair still resolves.
        let ok = vec![
            vec!["10".to_string(), "2048".to_string()],
            vec!["11".to_string(), "2048".to_string()],
        ];
        assert_eq!(choose_number_column(&ok, 2), Some(0)); // col 0 is unique + ≥2 digits (P1)
    }

    #[test]
    fn tile_number_plan_hash_heavy_names_still_correct() {
        // '#'-heavy but WELL-FORMED names: the literal '#' rides the skeleton as data, so both files
        // share "shot#\0_\0" and P1 picks the unique ≥2-digit id column (col 1). Correct chips, no crash.
        let stems = ["shot#1_10001", "shot#2_10002"];
        let plan = tile_number_plan(&stems);
        assert!(!plan.is_empty());
        assert_eq!(tile_number_label_planned("shot#1_10001", &plan), "10001");
        assert_eq!(tile_number_label_planned("shot#2_10002", &plan), "10002");
    }

    #[test]
    fn tile_number_plan_fuzz_smoke_never_panics() {
        // A BOUNDED, DETERMINISTIC fuzz: a hand-rolled LCG (literal seed — no Date/time entropy) over
        // an alphabet that INCLUDES '#', driving a few thousand small random folders through the whole
        // plan+apply path. The assertion is implicit: any index panic (the class this round fixes)
        // fails the test. We also checksum the output so the calls can't be optimized away.
        const ALPHABET: &[u8] = b"0123456789#._-: ()abIMGDSC";
        let mut state: u64 = 0x0123_4567_89ab_cdef; // literal seed — reproducible run to run
        let mut next = || {
            // Knuth MMIX LCG constants; take high bits (the low bits of an LCG are weakly random).
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state >> 33
        };
        let mut checksum: u64 = 0;
        for _ in 0..4000 {
            let n_files = 2 + (next() as usize % 5); // 2..=6 files per folder
            let mut stems: Vec<String> = Vec::with_capacity(n_files);
            for _ in 0..n_files {
                let len = 1 + (next() as usize % 8); // 1..=8 chars
                let mut s = String::with_capacity(len);
                for _ in 0..len {
                    let idx = next() as usize % ALPHABET.len();
                    s.push(ALPHABET[idx] as char);
                }
                stems.push(s);
            }
            let refs: Vec<&str> = stems.iter().map(String::as_str).collect();
            let plan = tile_number_plan(&refs); // MUST NOT PANIC
            for s in &refs {
                // apply path must be panic-free too; fold the label length into the checksum
                checksum = checksum.wrapping_add(tile_number_label_planned(s, &plan).len() as u64);
            }
        }
        // the folders are non-empty, so at least some labels were produced — proves the loop ran.
        assert!(checksum > 0, "fuzz smoke produced no labels — the loop did not execute");
    }
}

/// v1.0.0-rc — BYTES OVER NAMES (`FIX_2026-09-02_bytes_over_names.md`), the falsifier family.
///
/// Every row here was RED on `0597b0c`. The reddening edit is named in each row's own doc (L28),
/// and the two campaign builds that recorded the red text are listed in the round record's §C.
#[cfg(test)]
mod bytes_over_names {

    use super::{
        bmff_image_kind, decode_full_rgb, jpeg_sof_dims, placeholder_naming_line,
        read_head, scan_classify, scan_folder, sniff_disagreement_line, sniff_kind, source_dimensions,
        browse_frame_or_superseded, browse_frame_rgba, shot_source_gamut, Lane, RotApplyPlan, Shot,
        SrcKind, SNIFF_HEAD_BYTES,
    };
    use std::path::{Path, PathBuf};

    /// The unknown-container, disagreement, and note-cap rows drain only their own prefixes;
    /// taking the process-wide channel would discard notes a parallel row is about to assert.
    fn drain_notes_with_prefix(prefix: &str) -> Vec<String> {
        let mut notes = super::DECODE_NOTES.lock().unwrap_or_else(|e| e.into_inner());
        let Some((_, out)) = notes.as_mut() else { return Vec::new() };
        let (mine, others): (Vec<_>, Vec<_>) = std::mem::take(out).into_iter()
            .partition(|line| line.starts_with(prefix));
        *out = others;
        mine
    }

    /// The committed fixture family (`tests/fixtures/mismatch/`, regenerated by the
    /// `make_mismatch_fixtures.py` beside them).
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("mismatch").join(name)
    }

    /// A private scratch folder for one row. Named per row so the whole family can run in parallel
    /// and so the note keys (which are PATHS) never collide between rows.
    fn fresh_dir(row: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("falcon_bon_{row}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Copy a fixture into a scratch folder UNDER A DIFFERENT NAME — which is the whole subject of
    /// this round.
    fn place(dir: &Path, fixture_name: &str, as_name: &str) -> PathBuf {
        let p = dir.join(as_name);
        std::fs::copy(fixture(fixture_name), &p).unwrap();
        p
    }

    /// v1.0.0-rc TAIL: the CMYK route is a PROCESS-WIDE lever (an `AtomicBool` read per decode),
    /// so the two rows that drive it must not run concurrently — `cargo test` is threaded, and a
    /// neighbour flipping it mid-decode is exactly the flake a global setting invites. Every row
    /// that reads or writes it holds this first.
    static CMYK_LEVER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn only_shot(dir: &Path) -> Shot {
        let mut shots = scan_folder(dir).unwrap();
        assert_eq!(shots.len(), 1, "the scratch folder must hold exactly one shot");
        shots.remove(0)
    }

    // ══════════════════════════ the mechanism, pure ══════════════════════════

    /// The magic table itself: every signature [`sniff_kind`] claims, plus the three shapes that
    /// must answer `None` — an empty head, a head shorter than the signature it starts, and junk.
    ///
    /// L28 — REDDENED BY: deleting any single arm of `sniff_kind` (each magic is asserted here by
    /// its own line), or by widening one (drop the DIB-header qualifier and `"BMxx"` junk starts
    /// answering `Bmp`; drop the `WEBP` fourcc check and a WAV answers `Webp`).
    #[test]
    fn sniff_kind_reads_every_magic_and_nothing_else() {
        let head = |bytes: &[u8]| {
            let mut v = bytes.to_vec();
            v.resize(SNIFF_HEAD_BYTES.max(bytes.len()), 0);
            v
        };
        // ── the magics ──
        assert_eq!(sniff_kind(&head(b"\x89PNG\r\n\x1a\n")), Some(SrcKind::Png));
        assert_eq!(sniff_kind(&head(&[0xFF, 0xD8, 0xFF, 0xE0])), Some(SrcKind::Jpeg));
        assert_eq!(sniff_kind(&head(b"II\x2a\x00")), Some(SrcKind::Tiff), "classic little-endian TIFF");
        assert_eq!(sniff_kind(&head(b"MM\x00\x2a")), Some(SrcKind::Tiff), "classic big-endian TIFF");
        // BigTIFF (version 43) is INCLUDED because `tiff` 0.9.1 reads it (decoder/mod.rs matches 43),
        // and where the crate still declines one the TIFF arm falls back to the OS codec.
        assert_eq!(sniff_kind(&head(b"II\x2b\x00")), Some(SrcKind::Tiff), "BigTIFF little-endian");
        assert_eq!(sniff_kind(&head(b"MM\x00\x2b")), Some(SrcKind::Tiff), "BigTIFF big-endian");
        assert_eq!(sniff_kind(&head(b"RIFF\x00\x00\x00\x00WEBPVP8 ")), Some(SrcKind::Webp));
        assert_eq!(sniff_kind(&head(b"GIF87a")), Some(SrcKind::Gif));
        assert_eq!(sniff_kind(&head(b"GIF89a")), Some(SrcKind::Gif));
        assert_eq!(sniff_kind(&head(&[0xFF, 0x0A])), Some(SrcKind::Jxl), "naked JXL codestream");
        assert_eq!(
            sniff_kind(&head(&[0, 0, 0, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A])),
            Some(SrcKind::Jxl),
            "the JXL ISO-BMFF signature box"
        );
        // BMP: `BM` + a DIB header size the format defines (40 = BITMAPINFOHEADER).
        let mut bmp = vec![0u8; SNIFF_HEAD_BYTES];
        bmp[0] = b'B';
        bmp[1] = b'M';
        bmp[14] = 40;
        assert_eq!(sniff_kind(&bmp), Some(SrcKind::Bmp));

        // ── the ISO-BMFF brands ──
        let ftyp = |major: &[u8; 4], compat: &[&[u8; 4]]| {
            let mut v = vec![0u8, 0, 0, 32];
            v.extend_from_slice(b"ftyp");
            v.extend_from_slice(major);
            v.extend_from_slice(&[0, 0, 0, 0]); // minor version
            for c in compat {
                v.extend_from_slice(*c);
            }
            v.resize(SNIFF_HEAD_BYTES, 0);
            v
        };
        assert_eq!(sniff_kind(&ftyp(b"heic", &[b"mif1"])), Some(SrcKind::Heic));
        assert_eq!(sniff_kind(&ftyp(b"heix", &[])), Some(SrcKind::Heic));
        assert_eq!(sniff_kind(&ftyp(b"mif1", &[b"heic"])), Some(SrcKind::Heic), "decided by the compatible brand");
        assert_eq!(sniff_kind(&ftyp(b"mif1", &[])), Some(SrcKind::Heic), "the generic HEIF brand alone");
        // AVIF must NOT read as HEIC on the strength of the `mif1`/`miaf` it legally carries.
        assert_eq!(sniff_kind(&ftyp(b"avif", &[b"mif1", b"miaf"])), Some(SrcKind::Unsupported));
        assert_eq!(sniff_kind(&ftyp(b"mif1", &[b"avif", b"miaf"])), Some(SrcKind::Unsupported));
        // A video / a CR3 wearing an image extension degrades to the NAME, never to a HEIC decode.
        assert_eq!(sniff_kind(&ftyp(b"isom", &[b"mp41", b"avc1"])), None);
        assert_eq!(sniff_kind(&ftyp(b"crx ", &[b"isom"])), None, "a CR3 is not a HEIC");
        assert_eq!(bmff_image_kind(&ftyp(b"qt  ", &[])), None);

        // ── the three silences ──
        assert_eq!(sniff_kind(&[]), None, "a 0-byte head");
        assert_eq!(sniff_kind(b"\x89PNG"), None, "a head shorter than the signature it starts");
        assert_eq!(sniff_kind(&[0xFF, 0xD8]), None, "SOI alone is not enough for the JPEG magic");
        assert_eq!(sniff_kind(b"RIFF\x00\x00\x00\x00WAVEfmt "), None, "RIFF alone is not WebP");
        assert_eq!(sniff_kind(b"GIF8"), None, "the 4-byte GIF prefix is not a signature");
        assert_eq!(sniff_kind(&head(b"BMnot-a-bitmap-at-all")), None, "`BM` without a legal DIB header");
        assert_eq!(sniff_kind(&head(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3")), None, "a PDF");
        assert_eq!(sniff_kind(&head(b"the quick brown fox jumps over")), None, "prose");

        // ── the real fixtures agree with the table ──
        assert_eq!(sniff_kind(&read_head(&fixture("png_named_jpg.jpg")).unwrap()), Some(SrcKind::Png));
        assert_eq!(sniff_kind(&read_head(&fixture("jpeg_named_png.png")).unwrap()), Some(SrcKind::Jpeg));
        assert_eq!(sniff_kind(&read_head(&fixture("junk_named_jpg.jpg")).unwrap()), None);
    }

    /// Row 4 — **the sniff never touches a cloud placeholder.** Rule (1): a dehydrated OneDrive file
    /// is present in the listing and absent from the disk, and OPENING it downloads it. The scan is
    /// built around that constraint (`entry.file_type()`, `entry.metadata()`, no data read anywhere),
    /// so the head reader is a CLOSURE and this row proves the placeholder arm never calls it.
    ///
    /// L28 — REDDENED BY: deleting the `if cloud_placeholder { return … }` guard at the top of
    /// `scan_classify` (the closure then runs and the file is reclassified from bytes that a scan is
    /// forbidden to read).
    #[test]
    fn the_sniff_never_touches_a_cloud_placeholder() {
        let png_head = || Some(b"\x89PNG\r\n\x1a\n".to_vec());
        let read = std::cell::Cell::new(0usize);
        let heic = std::cell::Cell::new(0usize);

        let (kind, disagreed) = scan_classify(
            SrcKind::Jpeg,
            true, // a placeholder
            || {
                read.set(read.get() + 1);
                png_head()
            },
            || {
                heic.set(heic.get() + 1);
                SrcKind::Heic
            },
        );
        assert_eq!(kind, SrcKind::Jpeg, "a placeholder keeps its EXTENSION kind");
        assert_eq!(disagreed, None, "and says nothing — nothing was read, so nothing disagreed");
        assert_eq!(read.get(), 0, "a placeholder must cost ZERO reads — a read here is a download");
        assert_eq!(heic.get(), 0, "and no codec probe either");

        // The mirror, so this row is about the GATE and not about a dead sniff: the SAME head with
        // the placeholder flag cleared does reclassify, and reads exactly once.
        let (kind, disagreed) = scan_classify(
            SrcKind::Jpeg,
            false,
            || {
                read.set(read.get() + 1);
                png_head()
            },
            || SrcKind::Heic,
        );
        assert_eq!((kind, disagreed), (SrcKind::Png, Some(SrcKind::Png)));
        assert_eq!(read.get(), 1, "exactly one head read per candidate — never two");
    }

    /// The other two rules of the chokepoint, at the same pure seam: **the bytes win** (2) and
    /// **silence degrades to the name** (3) — including the one case where the stamped kind is
    /// NEITHER the name's nor the bytes', because this machine has no codec for what the bytes are.
    ///
    /// L28 — REDDENED BY: returning `sniffed` unconditionally from `scan_classify` (the HEIC
    /// capability gate then vanishes and a HEIC named `.jpg` is stamped `Heic` on a box that cannot
    /// decode one), or by returning `ext_kind` unconditionally (rule (2) dies).
    #[test]
    fn the_bytes_win_and_silence_keeps_the_name() {
        let jpeg = || Some(vec![0xFF, 0xD8, 0xFF, 0xE0]);
        // (2) the bytes win
        assert_eq!(
            scan_classify(SrcKind::Png, false, jpeg, || SrcKind::Heic),
            (SrcKind::Jpeg, Some(SrcKind::Jpeg))
        );
        // (3) silence — unreadable, short, and unrecognised all keep the name and say nothing
        assert_eq!(scan_classify(SrcKind::Jpeg, false, || None, || SrcKind::Heic), (SrcKind::Jpeg, None));
        assert_eq!(scan_classify(SrcKind::Jpeg, false, || Some(vec![]), || SrcKind::Heic), (SrcKind::Jpeg, None));
        assert_eq!(
            scan_classify(SrcKind::Tiff, false, || Some(b"%PDF-1.7".to_vec()), || SrcKind::Heic),
            (SrcKind::Tiff, None),
            "an unknown magic must not be able to reclassify ANY existing file"
        );
        // an agreeing sniff is silent too
        assert_eq!(scan_classify(SrcKind::Jpeg, false, jpeg, || SrcKind::Heic), (SrcKind::Jpeg, None));
        // the HEIC capability gate, both ways round
        let heic_head = || {
            let mut v = vec![0u8, 0, 0, 32];
            v.extend_from_slice(b"ftypheic");
            v.extend_from_slice(&[0; 20]);
            Some(v)
        };
        assert_eq!(
            scan_classify(SrcKind::Jpeg, false, heic_head, || SrcKind::Heic),
            (SrcKind::Heic, Some(SrcKind::Heic)),
            "a codec-bearing machine decodes it"
        );
        assert_eq!(
            scan_classify(SrcKind::Jpeg, false, heic_head, || SrcKind::Unsupported),
            (SrcKind::Unsupported, Some(SrcKind::Heic)),
            "a codec-less machine stamps Unsupported — and the line still gets to say HEIC"
        );
    }

    /// The honesty SENTENCE, pinned as text. The owner's veto list names this line explicitly, so it
    /// lives in one pure composer with one row rather than inside a `format!` at a call site.
    ///
    /// v1.0.0-rc TAIL (skeptic B, R1): **it says a ROUTING, not an OUTCOME.** The shipped wording
    /// was "decoded as PNG", composed inside the SCAN, where nothing has been decoded — and false on
    /// a reachable path (a PNG with a broken IHDR CRC named `.JPG` sniffs cleanly and never
    /// decodes). The decode's outcome is already reported by the tier that has it.
    ///
    /// L28 — REDDENED BY: any wording change in `sniff_disagreement_line`, by composing the line
    /// from `finished_format()`/an extension string instead of from `kind_tag`, or by putting the
    /// past tense back.
    #[test]
    fn the_disagreement_line_says_what_the_scan_knows() {
        assert_eq!(
            sniff_disagreement_line("53d879f01a3f481c.JPG", SrcKind::Png, SrcKind::Png, SrcKind::Jpeg),
            "53d879f01a3f481c.JPG: PNG by its bytes — routing to the PNG decoder (the name says JPG)"
        );
        assert_eq!(
            sniff_disagreement_line("scan.png", SrcKind::Jpeg, SrcKind::Jpeg, SrcKind::Png),
            "scan.png: JPG by its bytes — routing to the JPG decoder (the name says PNG)"
        );
        // The tense is the point, so it is asserted as a fact about the string and not only as a
        // literal: nothing this composer produces may claim an outcome the scan cannot have.
        for line in [
            sniff_disagreement_line("a.JPG", SrcKind::Png, SrcKind::Png, SrcKind::Jpeg),
            sniff_disagreement_line("b.jpg", SrcKind::Heic, SrcKind::Unsupported, SrcKind::Jpeg),
            sniff_disagreement_line("c.jpg", SrcKind::Unsupported, SrcKind::Unsupported, SrcKind::Jpeg),
        ] {
            assert!(!line.contains("decoded"), "the scan has decoded nothing: {line}");
        }
        assert_eq!(
            sniff_disagreement_line("photo.jpg", SrcKind::Heic, SrcKind::Unsupported, SrcKind::Jpeg),
            "photo.jpg: HEIC by its bytes — not decodable on this machine (the name says JPG)",
            "the machine's own limit is not the file's fault, and the line must not call it a decode"
        );
        assert_eq!(
            sniff_disagreement_line("photo.jpg", SrcKind::Unsupported, SrcKind::Unsupported, SrcKind::Jpeg),
            "photo.jpg: its bytes are a container this build does not decode (the name says JPG)",
            "`UNSUP` is a log column token, never a format noun in a sentence"
        );
    }

    // ══════════════════════════ the ten rows of the report's §4.3 ══════════════════════════

    /// Row 1 — **a PNG named `.jpg` opens at every tier.** The field case, reduced: the scan kind,
    /// the header probe and all four decode entry points, on bytes whose name lies about them.
    /// The pixel assertions are exact because PNG is lossless — a flat or garbage buffer cannot
    /// pass them.
    ///
    /// L28 — REDDENED BY: reverting the chokepoint in `scan_folder_counted` to
    /// `slot.1.push((p, ext_kind))` (the shot is stamped `Jpeg` and `jpeg-decoder` refuses the PNG
    /// signature at every tier: "invalid JPEG format: first two bytes are not an SOI marker").
    #[test]
    fn a_png_named_jpg_opens_at_every_tier() {
        let dir = fresh_dir("row1");
        place(&dir, "png_named_jpg.jpg", "53d879f01a3f481c.JPG");
        let shot = only_shot(&dir);

        assert_eq!(shot.kind, SrcKind::Png, "the scan classifies by the BYTES");
        assert_eq!(source_dimensions(&shot), Some((16, 16)), "the header probe follows the kind");

        for lane in [Lane::Fast, Lane::Thumb] {
            let f = browse_frame_or_superseded(&shot, 256, false, lane)
                .unwrap_or_else(|e| panic!("{lane:?} tier: {e}"))
                .expect("a non-HEIC shot can never be superseded");
            assert!(f.w > 0 && f.h > 0, "{lane:?} tier produced an empty frame");
        }
        let native = browse_frame_rgba(&shot, 8192, false, Lane::Native).expect("detail tier");
        assert_eq!((native.w, native.h), (16, 16));

        let (rgb, w, h) = decode_full_rgb(&shot).expect("full-res / ROI source");
        assert_eq!((w, h), (16, 16));
        assert_eq!(rgb.len(), 16 * 16 * 3);
        // The generator's own pattern: px(x, y) = (x*16, y*16, (x*y) % 256).
        assert_eq!(&rgb[..3], &[0, 0, 0], "pixel (0,0)");
        assert_eq!(&rgb[15 * 3..15 * 3 + 3], &[240, 0, 0], "pixel (15,0)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Row 2 — **the mirror: a JPEG named `.png` admits the GPU lane.** `is_jpeg_source()` is the
    /// literal admission predicate of every nvJPEG entry point (`falcon-nvjpeg/src/lib.rs` 742, 766,
    /// 788, 801) and of the ROI YUV route (`main.rs:8334`), so pinning it here pins them. Before this
    /// round a real JPEG that happened to be named `.png` could never reach the hardware decoder —
    /// the silent half of the same bug, and the reason the fix is "the bytes decide", not "PNG wins".
    ///
    /// L28 — REDDENED BY: the same chokepoint revert as row 1 (the shot is stamped `Png`,
    /// `is_jpeg_source()` is false, and the GPU lane declines a file it can decode).
    #[test]
    fn a_jpeg_named_png_admits_the_gpu_lane() {
        let dir = fresh_dir("row2");
        place(&dir, "jpeg_named_png.png", "export.png");
        let shot = only_shot(&dir);
        assert_eq!(shot.kind, SrcKind::Jpeg);
        assert!(shot.is_jpeg_source(), "the nvJPEG / ROI-YUV admission predicate reads the BYTES");
        assert_eq!(source_dimensions(&shot), Some((16, 16)));
        let (_, w, h) = decode_full_rgb(&shot).expect("and the CPU spine takes it as a JPEG");
        assert_eq!((w, h), (16, 16));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Row 3 — **an unknown container named `.jpg` fails HONESTLY.** The one guarantee rule (3) owes
    /// the user: a file whose magic we do not know keeps its extension's answer, fails exactly as it
    /// did before this round, says NOTHING about its bytes (silence, not a guess), and does not
    /// disturb the neighbour sitting next to it in the folder.
    ///
    /// L28 — REDDENED BY: making `sniff_kind` return `Some(SrcKind::Unsupported)` for an
    /// unrecognised magic instead of `None` — the file would then badge as unsupported rather than
    /// failing the way every pre-round corpus expects, and rule (c) would be gone.
    #[test]
    fn an_unknown_container_named_jpg_fails_honestly() {
        let drain_decode_notes = || drain_notes_with_prefix("not_really");
        let dir = fresh_dir("row3");
        place(&dir, "junk_named_jpg.jpg", "not_really.jpg");
        place(&dir, "png_named_jpg.jpg", "neighbour.JPG");
        let shots = scan_folder(&dir).unwrap();
        let junk = shots.iter().find(|s| s.name == "not_really").expect("the junk shot");
        let neighbour = shots.iter().find(|s| s.name == "neighbour").expect("the neighbour");

        assert_eq!(junk.kind, SrcKind::Jpeg, "an unknown magic keeps the NAME's answer");
        assert!(!junk.is_unsupported(), "and is not silently re-badged as unsupported");
        assert_eq!(source_dimensions(junk), None);
        assert!(decode_full_rgb(junk).is_err(), "it still fails — honestly");
        // v1.0.0-rc TAIL (skeptic A, Y2): "exactly once" was in this row's prose and in none of its
        // asserts. Decode it TWICE and count what the note channel actually holds for it: an
        // unrecognised magic says nothing at all (rule (3) is silence, not a line), and the OS-codec
        // second chance must not start narrating a file it also refused.
        let _ = drain_decode_notes();
        assert!(decode_full_rgb(junk).is_err());
        assert!(decode_full_rgb(junk).is_err());
        let about_junk: Vec<String> =
            drain_decode_notes().into_iter().filter(|l| l.starts_with("not_really")).collect();
        assert!(about_junk.is_empty(), "an unknown container is silent, not chatty: {about_junk:?}");

        // The neighbour is untouched: no shared latch, no shared classification, nothing borrowed.
        assert_eq!(neighbour.kind, SrcKind::Png);
        assert_eq!(decode_full_rgb(neighbour).map(|(_, w, h)| (w, h)).unwrap(), (16, 16));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Row 5 — **pairing ranks the bytes, not the name.** `finished_rank` puts `Jpeg` at 8, the
    /// highest of any finished format, so before this round a PNG wearing a `.JPG` extension won a
    /// RAW's partner slot over a GENUINE JPEG sibling — the RAW paired with an impostor and the real
    /// deliverable was demoted to a split shot. The stem rule itself is unchanged (pairing is by
    /// stem, which is a user/camera convention and not a format claim); only the RANK moves.
    ///
    /// L28 — REDDENED BY: the chokepoint revert (both finished siblings then rank 8, the tie breaks
    /// on path order, and `IMG_1.JPG` sorts before `IMG_1.jpeg` — so the impostor takes the slot).
    #[test]
    fn pairing_ranks_the_bytes_not_the_name() {
        let dir = fresh_dir("row5");
        std::fs::write(dir.join("IMG_1.CR3"), b"not a real raw, and never sniffed").unwrap();
        place(&dir, "png_named_jpg.jpg", "IMG_1.JPG"); // the impostor
        place(&dir, "jpeg_named_png.png", "IMG_1.jpeg"); // the genuine JPEG
        let shots = scan_folder(&dir).unwrap();

        let paired = shots.iter().find(|s| s.has_raw).expect("the RAW's shot");
        assert_eq!(
            paired.jpg.as_ref().and_then(|p| p.file_name()).and_then(|s| s.to_str()),
            Some("IMG_1.jpeg"),
            "the RAW's partner is the genuine JPEG, not the PNG that merely spells .JPG"
        );
        assert_eq!(paired.kind, SrcKind::Jpeg);
        let impostor = shots.iter().find(|s| !s.has_raw).expect("the impostor's own split shot");
        assert_eq!(impostor.kind, SrcKind::Png, "and it is still visible, correctly labelled");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Row 6 — **the badge and the panel name the bytes.** The tile badge and the info panel's
    /// Format row both come from `Shot::finished_format`, which read the EXTENSION. The filename in
    /// the header is untouched — the user keeps seeing `53d879f01a3f481c.JPG`, which is what the file
    /// is actually called — while the FORMAT noun tells the truth.
    ///
    /// L28 — REDDENED BY: reverting `Shot::finished_format` to its extension table (the badge reads
    /// `JPG` on a PNG again). It is also reddened by the chokepoint revert, because the label is now
    /// derived from the kind that revert would get wrong.
    #[test]
    fn badge_and_panel_name_the_bytes() {
        let dir = fresh_dir("row6");
        place(&dir, "png_named_jpg.jpg", "53d879f01a3f481c.JPG");
        let shot = only_shot(&dir);
        assert_eq!(shot.finished_format().as_deref(), Some("PNG"), "the info panel's Format row");
        assert_eq!(shot.badge_label(), "PNG", "the tile badge");
        assert_eq!(
            shot.jpg.as_ref().unwrap().file_name().unwrap().to_str(),
            Some("53d879f01a3f481c.JPG"),
            "the FILE keeps its name — only the format noun changed"
        );

        // The mirror, and the guard on the one arm that still reads the extension: an `Unsupported`
        // shot has no format noun of its own, so it must keep naming itself from its name.
        let dir2 = fresh_dir("row6b");
        std::fs::write(dir2.join("holiday.avif"), b"").unwrap();
        let avif = only_shot(&dir2);
        assert_eq!(avif.kind, SrcKind::Unsupported);
        assert_eq!(avif.finished_format().as_deref(), Some("AVIF"), "an unsupported file still names itself");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// Row 7 — **the disagreement speaks once per file, and an agreeing file says nothing.** The line
    /// rides the existing once-per-session decode-note channel (`note_once`), keyed by PATH: a
    /// 500-file folder logs 500 facts and not 500 × 18 workers, and a rescan of the same folder does
    /// not repeat itself.
    ///
    /// L28 — REDDENED BY: deleting the `note_once(…)` call at the chokepoint (nothing is said at all),
    /// or by keying it on something other than the path — a per-format key would report the first
    /// mismatched file in a folder and stay silent about the rest.
    #[test]
    fn the_disagreement_speaks_once_per_file() {
        let drain_decode_notes = || drain_notes_with_prefix("bon_row7_");
        let dir = fresh_dir("row7");
        place(&dir, "png_named_jpg.jpg", "bon_row7_liar.JPG");
        place(&dir, "jpeg_named_png.png", "bon_row7_honest.jpg");
        let _ = drain_decode_notes(); // start from a known state

        let shots = scan_folder(&dir).unwrap();
        assert_eq!(shots.len(), 2);
        let mine: Vec<String> =
            drain_decode_notes().into_iter().filter(|l| l.starts_with("bon_row7_")).collect();
        assert_eq!(
            mine,
            vec!["bon_row7_liar.JPG: PNG by its bytes — routing to the PNG decoder (the name says JPG)".to_string()],
            "exactly one line, for the one file whose bytes and name disagree"
        );

        // A second scan of the same folder repeats nothing.
        let _ = scan_folder(&dir).unwrap();
        let again: Vec<String> =
            drain_decode_notes().into_iter().filter(|l| l.starts_with("bon_row7_")).collect();
        assert!(again.is_empty(), "once per file per session — a rescan is silent: {again:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Row 8 — **the existing corpus is not reclassified.** The regression guard on rule (3): every
    /// file in the standard testkit must come out of the sniffing scan wearing exactly the format
    /// noun the pre-round EXTENSION table gave it. Skipped (not failed) where the testkit is absent —
    /// it is the owner's local corpus, not a committed one.
    ///
    /// L28 — REDDENED BY: making `sniff_kind` answer on a partial signature (e.g. matching `GIF8` or
    /// a bare `BM`), which is how a sniff starts reclassifying files nobody asked it to touch.
    #[test]
    fn the_existing_corpus_is_not_reclassified() {
        let kit = crate::fixture_paths::standard();
        if !kit.is_dir() {
            eprintln!("testkit/standard absent — row skipped");
            return;
        }
        let shots = scan_folder(kit).unwrap();
        assert!(shots.len() >= 18, "the standard testkit should hold at least its 18 shots");
        for s in &shots {
            let Some(p) = s.jpg.as_ref() else { continue };
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
            // The pre-round table, re-typed here on purpose: this row's whole job is to compare the
            // new answer against the OLD one.
            let was = match ext.as_str() {
                "jpg" | "jpeg" => "JPG".to_string(),
                "tif" | "tiff" => "TIFF".to_string(),
                "heic" | "heif" => "HEIC".to_string(),
                "webp" => "WEBP".to_string(),
                other => other.to_ascii_uppercase(),
            };
            assert_eq!(
                s.finished_format().as_deref(),
                Some(was.as_str()),
                "{} was reclassified by the sniff",
                p.display()
            );
        }
    }

    /// Row 9 — **the colour door follows the sniff.** `file_color_tag(path, kind)` dispatches by
    /// KIND: a `.jpg` name sent a PNG through `extract_icc_from_jpeg`, which checks SOI, bails, and
    /// answers sRGB — so a Display-P3 PNG named `.jpg` was silently colour-managed as sRGB. That is
    /// exactly the v0.8.140 C2 bug this door was built to close, re-opened by a filename.
    ///
    /// L28 — REDDENED BY: the chokepoint revert (the kind is `Jpeg`, the JPEG ICC door bails on the
    /// missing SOI, and the gamut falls back to sRGB).
    #[test]
    fn the_colour_door_follows_the_sniff() {
        let dir = fresh_dir("row9");
        place(&dir, "p3_png_named_jpg.jpg", "wide_gamut.jpg");
        let shot = only_shot(&dir);
        assert_eq!(shot.kind, SrcKind::Png);
        assert_eq!(
            shot_source_gamut(&shot),
            falcon_color::Gamut::DisplayP3,
            "the PNG iCCP door read the profile the JPEG door could never reach"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Row 10 — **rotate-apply never opens a non-JPEG for write.** `RotApplyPlan::finished_is_jpeg`
    /// (built at `main.rs:16851` from `shot.is_jpeg_source()` and nowhere else) is the ONLY gate on
    /// the in-place arm at `apply.rs:998`, which opens the user's file `read+write` and patches its
    /// EXIF. On a PNG named `.JPG` that arm was entered and was saved only by an INCIDENTAL SOI check
    /// inside `locate_jpeg_orientation` — a write handle on a foreign file, opened before anything
    /// looked at the bytes.
    ///
    /// L28 — REDDENED BY: the chokepoint revert (`is_jpeg_source()` becomes true and the plan routes
    /// the PNG into `patch_jpeg_orientation`).
    #[test]
    fn rotate_apply_never_opens_a_non_jpeg_for_write() {
        let dir = fresh_dir("row10");
        place(&dir, "png_named_jpg.jpg", "53d879f01a3f481c.JPG");
        let shot = only_shot(&dir);
        let plan = RotApplyPlan {
            finished: shot.jpg.clone(),
            finished_is_jpeg: shot.is_jpeg_source(), // the mirror of main.rs:16851
            raw: shot.raw.clone(),
            base_turns: 0,
            delta: 1,
        };
        assert!(
            !plan.finished_is_jpeg,
            "the in-place EXIF patch (apply.rs:998) must not be reachable for PNG bytes"
        );
        // And the mirror: a genuine JPEG still takes the in-place route it has always taken.
        let dir2 = fresh_dir("row10b");
        place(&dir2, "jpeg_named_png.png", "real.jpg");
        assert!(only_shot(&dir2).is_jpeg_source());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    // ══════════════════════════ the riders (the report's §5) ══════════════════════════

    /// R1 — **4-component JPEGs (CMYK / YCCK, Adobe APP14) open at every tier.** `to_rgb` handled
    /// `RGB24` and `L8` and nothing else, so the classic Acrobat / Photoshop / print-RIP export
    /// failed at EVERY tier with `unsupported JPEG pixel format: CMYK32`. `jpeg-decoder` had already
    /// done the hard half (it applies the YCCK colour transform itself); only the 4→3 channel
    /// conversion was missing.
    ///
    /// THE ORACLE IS THE PICTURE. All four fixtures encode the SAME 16×16 source: one as an RGB
    /// JPEG, three as 4-component JPEGs (Adobe CMYK, genuine Adobe YCCK, and 4 components with no
    /// APP14 at all). A conversion that is right reproduces the RGB twin to within JPEG
    /// quantisation; one that inverts a channel, or applies the black plate the wrong way round,
    /// cannot come close. That is a stronger test than re-typed arithmetic — and a stronger one than
    /// the OS codec, which is NOT used as the oracle here and the round record says why: WIC
    /// colour-manages an untagged CMYK JPEG through a default print profile (measured: a 100 % CMY
    /// black arrives as `[65, 50, 40]`, 96 away from the source picture), which is a defensible
    /// answer this tree cannot reproduce without a CMYK ICC engine — see §C, flagged for the owner.
    ///
    /// L28 — REDDENED BY: deleting the `PixelFormat::CMYK32` arm of `to_rgb` (back to the `bail!` —
    /// the exact pre-round failure), or by dropping either half of its arithmetic (drop the
    /// `255 - ink` and every fixture comes out photographically negative; drop the `× (255 - K)` and
    /// a file with any black plate comes out washed out).
    #[test]
    fn a_cmyk_jpeg_decodes_to_the_same_picture_as_its_rgb_twin() {
        // This row is about FALCON'S OWN arithmetic, so it pins the lever off for its duration.
        let _guard = CMYK_LEVER.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(windows)]
        let restore = super::cmyk_os_route();
        #[cfg(windows)]
        super::set_cmyk_os_route(false);
        let twin_dir = fresh_dir("r1_twin");
        place(&twin_dir, "jpeg_named_png.png", "twin.jpg");
        let (twin, tw, th) = decode_full_rgb(&only_shot(&twin_dir)).expect("the RGB JPEG twin");

        for name in ["cmyk_adobe.jpg", "cmyk_ycck.jpg", "cmyk_no_app14.jpg"] {
            let dir = fresh_dir(&format!("r1_{}", name.trim_end_matches(".jpg")));
            place(&dir, name, name);
            let shot = only_shot(&dir);
            assert_eq!(shot.kind, SrcKind::Jpeg);
            let (ours, w, h) = decode_full_rgb(&shot).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((w, h, ours.len()), (tw, th, twin.len()), "{name}: geometry");
            let worst = ours.iter().zip(&twin).map(|(a, b)| u32::from(a.abs_diff(*b))).max().unwrap_or(0);
            // 12 is comfortably above the MEASURED spread — 5 on all three flavours, which is two
            // lossy encodes of one source (plus, for YCCK, a YCbCr round trip) — and far below any
            // arithmetic error: the mildest of those is an inverted black plate, which moves a pixel
            // by hundreds. For the record, WIC's colour-managed answer sits 96 from this same twin.
            assert!(worst <= 12, "{name}: worst channel delta vs the RGB twin is {worst}");
            let _ = std::fs::remove_dir_all(&dir);
        }
        let _ = std::fs::remove_dir_all(&twin_dir);
        #[cfg(windows)]
        super::set_cmyk_os_route(restore);
    }

    /// R1's platform half: the OS codec opens the same four files, so the geometry the tiers get is
    /// the geometry every other viewer on this machine gets. The COLOUR is deliberately not asserted
    /// against WIC — see the row above and §C.
    ///
    /// L28 — REDDENED BY: nothing in this round; it is the standing measurement that keeps the §C
    /// deviation honest, and it fails the day WIC stops opening a 4-component JPEG.
    #[cfg(windows)]
    #[test]
    fn the_os_codec_also_opens_the_four_component_family() {
        for name in ["cmyk_adobe.jpg", "cmyk_ycck.jpg", "cmyk_no_app14.jpg"] {
            let (_, w, h) = super::wic_decode_rgb24(&fixture(name), "no codec")
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((w, h), (16, 16), "{name}");
        }
    }

    /// R2 — **a JPEG that ends mid-scan reaches the OS codec.** A missing 2-byte EOI, or a file cut
    /// at 85 %, is refused outright by the pure-Rust decoder (`failed to fill whole buffer`) and
    /// rendered by every other viewer on the machine. Genuine corruption is NOT routed there: the
    /// junk fixture still fails, so the second chance is scoped to the one error class that earns it.
    ///
    /// L28 — REDDENED BY: deleting the `Err(e) if jpeg_err_is_truncation(&e)` arm of
    /// `decode_source_rgb`'s `SrcKind::Jpeg` case, or by widening `jpeg_err_is_truncation` to any
    /// `jpeg_decoder::Error` (the junk assertion at the end of this row then fails, because a PDF
    /// would be handed to the OS codec too).
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn a_truncated_jpeg_falls_through_to_the_os_codec() {
        for name in ["no_eoi.jpg", "truncated.jpg"] {
            let dir = fresh_dir(&format!("r2_{}", name.trim_end_matches(".jpg")));
            place(&dir, name, name);
            let shot = only_shot(&dir);
            let (rgb, w, h) = decode_full_rgb(&shot).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((w, h), (128, 128), "{name}");
            assert_eq!(rgb.len(), 128 * 128 * 3, "{name}");
            let _ = std::fs::remove_dir_all(&dir);
        }
        // The scope: genuine corruption is still a hard failure, not an OS-codec round trip.
        let dir = fresh_dir("r2_junk");
        place(&dir, "junk_named_jpg.jpg", "not_really.jpg");
        assert!(decode_full_rgb(&only_shot(&dir)).is_err(), "a PDF named .jpg is not a truncated JPEG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R3 — **`source_dimensions` finds a SOF that sits past 256 KB.** The probe read a fixed 256 KB
    /// prefix, so a JPEG with a big chunked ICC or a fat EXIF thumbnail ahead of its frame header
    /// returned `None` — and `None` reads downstream as "no size available", which silently costs the
    /// ROI hi-res tile, the focus readout and the zoom decision on a file that decodes perfectly.
    ///
    /// L28 — REDDENED BY: restoring the 256 KB-prefix + `Decoder::read_info()` body of
    /// `source_dimensions`' `SrcKind::Jpeg` arm.
    #[test]
    fn a_jpeg_whose_sof_sits_past_256_kb_reports_its_dims() {
        let p = fixture("big_app2.jpg");
        assert!(std::fs::metadata(&p).unwrap().len() > 256 * 1024, "the fixture must clear the old window");
        let dir = fresh_dir("r3");
        place(&dir, "big_app2.jpg", "scan_with_a_fat_icc.jpg");
        let shot = only_shot(&dir);
        assert_eq!(source_dimensions(&shot), Some((16, 16)));
        // and it still decodes, so the probe is not answering about a file the tiers cannot open
        assert_eq!(decode_full_rgb(&shot).map(|(_, w, h)| (w, h)).unwrap(), (16, 16));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R3's walker, as a table — the shapes a marker walk has to survive that no real fixture in
    /// this family exercises.
    ///
    /// L28 — REDDENED BY: dropping the standalone-marker arm (`0x01 | 0xD0..=0xD8`) so an RST is read
    /// as a length-bearing segment; dropping the `h > 0` guard so a DNL frame reports a zero height;
    /// or removing the `MAX_SEGMENTS` bound, which the self-referential-length case below relies on
    /// terminating.
    #[test]
    fn the_sof_walk_survives_the_shapes_a_jpeg_can_take() {
        let dims = |b: &[u8]| {
            jpeg_sof_dims(&mut std::io::Cursor::new(b.to_vec()))
                .filter(|f| f.w > 0 && f.h > 0)
                .map(|f| (f.w, f.h))
        };
        let sof = |w: u16, h: u16| {
            let mut v = vec![0xFF, 0xC0, 0x00, 0x11, 0x08];
            v.extend_from_slice(&h.to_be_bytes());
            v.extend_from_slice(&w.to_be_bytes());
            v.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
            v
        };
        let mut plain = vec![0xFF, 0xD8];
        plain.extend_from_slice(&sof(640, 480));
        assert_eq!(dims(&plain), Some((640, 480)));

        // A fill byte before the marker, and a standalone marker in the chain.
        let mut padded = vec![0xFF, 0xD8, 0xFF, 0xFF, 0xFF, 0x01];
        padded.extend_from_slice(&sof(1649, 2337));
        assert_eq!(dims(&padded), Some((1649, 2337)), "fill bytes and TEM are not segments");

        // A long segment ahead of the SOF is SKIPPED BY ITS OWN LENGTH, not by a byte cap.
        let mut fat = vec![0xFF, 0xD8, 0xFF, 0xE2, 0xFF, 0xFF];
        fat.extend_from_slice(&vec![0u8; 0xFFFF - 2]);
        fat.extend_from_slice(&sof(4000, 3000));
        assert_eq!(dims(&fat), Some((4000, 3000)));

        assert_eq!(dims(b"\x89PNG\r\n\x1a\n"), None, "not a JPEG");
        assert_eq!(dims(&[0xFF, 0xD8, 0xFF, 0xD9]), None, "EOI before any frame header");
        assert_eq!(dims(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2]), None, "SOS before any frame header");
        let mut dnl = vec![0xFF, 0xD8];
        dnl.extend_from_slice(&sof(640, 0));
        assert_eq!(dims(&dnl), None, "a DNL frame's height is not known from the header");
        assert_eq!(dims(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), None, "a segment shorter than its length field");
        assert_eq!(dims(&[0xFF, 0xD8, 0xFF, 0xE0]), None, "a truncated segment header");
    }

    // ═══════════ v1.0.0-rc TAIL — the skeptic rounds' own rows ═══════════

    /// **B-R2 + A-Y1 — a sniffed-`Unsupported` file never defames a format the app supports.**
    ///
    /// `finished_format`'s `Unsupported` arm was the one place still reading the extension, and it
    /// is read by FIVE surfaces: the full-stage placeholder note (`tick.rs` `step_stage` →
    /// `format!("{fmt} is currently unsupported")`), the tile badge, the context-menu Copy row
    /// (`support::ctx_populate_fields`), the copy toast (`tick::copy_result_text`) and the info
    /// panel's Files row. So an AVIF named `holiday.jpg` told a photographer running a JPEG-centric
    /// culler that **"JPG is currently unsupported"** — a materially worse false statement than the
    /// "may be corrupt" card this whole round exists to delete, because it defames a format the app
    /// fully supports. The fix carries the sniffed container on the `Shot` and this row walks all
    /// five, because gating a shared answer owes the mirror on every invoker (L26).
    ///
    /// L28 — REDDENED BY: deleting `Shot::sniffed` and letting the `Unsupported` arm fall back to
    /// the extension (every assert below reads `JPG`), or by having it answer `kind_tag(kind)`,
    /// which is the log token `UNSUP` and not a format noun.
    #[test]
    fn an_unsupported_container_never_says_jpg() {
        let dir = fresh_dir("tail_r2");
        // A legal 4-byte-size `ftyp` box declaring AVIF — the shape `bmff_image_kind` answers
        // `Unsupported` for, under a name that says JPEG.
        let mut avif = vec![0u8, 0, 0, 24];
        avif.extend_from_slice(b"ftypavif");
        avif.extend_from_slice(&[0, 0, 0, 0]);
        avif.extend_from_slice(b"mif1miaf");
        std::fs::write(dir.join("holiday.jpg"), &avif).unwrap();
        let shot = only_shot(&dir);

        assert_eq!(shot.kind, SrcKind::Unsupported, "the bytes are a container we cannot decode");
        assert_eq!(shot.sniffed, Some(SrcKind::Unsupported), "and the scan remembered that it READ them");
        // (1) the info panel / (2) the stage note / (3) the badge / (4) the menu row / (5) the toast.
        // All five compose from `finished_format`, so `None` is what makes every one of them fall to
        // the generic form it already has — "This format is currently unsupported", `IMG`,
        // "Copy image", "<name> image copied".
        assert_eq!(shot.finished_format(), None, "there is no honest noun, so none is offered");
        assert_eq!(shot.badge_label(), "IMG");
        for surface in [
            format!("{} is currently unsupported", shot.finished_format().unwrap_or_default()),
            format!("Copy {}", shot.finished_format().unwrap_or_else(|| "image".into())),
            format!("{} {} copied to clipboard", shot.name, shot.finished_format().unwrap_or_else(|| "image".into())),
        ] {
            assert!(!surface.contains("JPG"), "a surface still says JPG: {surface}");
        }

        // A-Y1's own case, which is the one with a RIGHT answer rather than merely a safe one: a
        // HEIC on a machine with no HEVC codec is stamped `Unsupported`, and the panel must still
        // say HEIC — that is the fact that tells the user what to install.
        let mut heic = vec![0u8, 0, 0, 24];
        heic.extend_from_slice(b"ftypheic");
        heic.extend_from_slice(&[0, 0, 0, 0]);
        heic.extend_from_slice(b"mif1heic");
        let codecless = Shot {
            id: 0,
            name: "IMG_9".into(),
            has_raw: false,
            has_jpg: true,
            raw: None,
            jpg: Some(dir.join("IMG_9.jpg")),
            kind: SrcKind::Unsupported,
            cloud_placeholder: false,
            sniffed: Some(SrcKind::Heic),
        };
        assert_eq!(codecless.finished_format().as_deref(), Some("HEIC"));
        assert_eq!(codecless.badge_label(), "HEIC");

        // And a GENUINE `.avif` — where the name was never overridden — still names itself, which is
        // the arm §C ruled and which this tail must not break.
        let dir2 = fresh_dir("tail_r2b");
        std::fs::write(dir2.join("real.avif"), b"").unwrap();
        let real = only_shot(&dir2);
        assert_eq!(real.sniffed, None, "an empty file is a short read — rule (3), the name stands");
        assert_eq!(real.finished_format().as_deref(), Some("AVIF"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// **B-O1 (owner ruling) — the info panel reconciles the name and the bytes; nothing else does.**
    ///
    /// Five surfaces now say `PNG` about a file the header calls `.JPG`, and until this row the only
    /// place the contradiction was explained was a log line no user reads. The owner ruled the
    /// qualifier onto the FILES row alone: it is the one surface whose whole job is answering "what
    /// IS this file", and the tile badge — scanned at a glance across a filmstrip — stays a single
    /// clean noun.
    ///
    /// L28 — REDDENED BY: dropping the `named` argument from `fmt_files`, or by returning `Some`
    /// from `named_ext_when_bytes_disagree` for a file whose name and bytes agree (the second half
    /// of this row, which is what keeps the qualifier off 99.9 % of photographs).
    #[test]
    fn the_files_row_says_what_the_file_is_called() {
        let dir = fresh_dir("tail_o1");
        place(&dir, "png_named_jpg.jpg", "53d879f01a3f481c.JPG");
        let shot = only_shot(&dir);
        assert_eq!(shot.named_ext_when_bytes_disagree().as_deref(), Some("JPG"));
        let files = super::fmt_files(None, Some(892_913), "PNG", shot.named_ext_when_bytes_disagree().as_deref());
        assert_eq!(files.as_deref(), Some("PNG 893 KB (named .JPG)"));
        // The badge is NOT qualified — that is the ruled division.
        assert_eq!(shot.badge_label(), "PNG");

        // An agreeing file is untouched, on both the RAW-paired and the lone shapes.
        let dir2 = fresh_dir("tail_o1b");
        place(&dir2, "jpeg_named_png.png", "ok.jpg");
        let plain = only_shot(&dir2);
        assert_eq!(plain.named_ext_when_bytes_disagree(), None);
        assert_eq!(
            super::fmt_files(None, Some(1_500_000), "JPG", None).as_deref(),
            Some("JPG 2 MB")
        );
        assert_eq!(
            super::fmt_files(Some(45_000_000), Some(1_500_000), "JPG", None).as_deref(),
            Some("RAW 45 MB · JPG 2 MB")
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// **B-Y1 + A-O6 — the per-path note families are capped, and the cap says how much it hid.**
    ///
    /// `sniff:<path>` was the note channel's first always-on, user-data-derived key family, in a
    /// process whose key set is never cleared: A measured 300 keys from one folder drained on a
    /// single ~1.5 s report tick and resident for the session; B measured four such folders produce
    /// 1 200 lines with no cap at all. The cap bounds OUTPUT and RESIDENCY (no key is inserted once
    /// it is full), and the scan posts one trailer naming the remainder — because a cap that goes
    /// quiet without saying how much it swallowed cannot be told from a folder that was clean.
    ///
    /// L28 — REDDENED BY: removing the `counted.load() >= cap` early return (the count below runs
    /// away), by inserting the key before the cap check (the residency half), or by deleting
    /// `note_scan_suppressed` (the trailer assert).
    #[test]
    fn the_note_families_are_capped_and_say_so() {
        let drain_decode_notes = || drain_notes_with_prefix("scan: ");
        // The mechanism, driven directly so the row costs no I/O and no 300-file folder: the same
        // counter, the same cap, the same three verdicts the callers branch on.
        static COUNTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        const CAP: usize = 4;
        let mut logged = 0usize;
        let mut suppressed = 0usize;
        let mut composed = 0usize;
        for i in 0..20 {
            let v = super::note_path_capped(
                &format!("tail-cap-row:{i}"),
                &COUNTED,
                CAP,
                || {
                    composed += 1;
                    format!("line {i}")
                },
            );
            match v {
                super::NoteVerdict::Logged => logged += 1,
                super::NoteVerdict::Suppressed => suppressed += 1,
                super::NoteVerdict::Repeat => panic!("every key here is distinct"),
            }
        }
        assert_eq!(logged, CAP, "output is bounded by the cap");
        assert_eq!(suppressed, 20 - CAP);
        assert_eq!(composed, CAP, "a suppressed line is never even BUILT");
        // Residency: the keys of the suppressed calls were never inserted, so asking again after the
        // counter is reset logs them — which is only possible if they are absent from the set.
        COUNTED.store(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            super::note_path_capped("tail-cap-row:19", &COUNTED, CAP, || "again".into()),
            super::NoteVerdict::Logged,
            "a suppressed key must not have been inserted — that is the residency half"
        );
        assert_eq!(
            super::note_path_capped("tail-cap-row:0", &COUNTED, CAP, || "again".into()),
            super::NoteVerdict::Repeat,
            "…while a LOGGED key is still resident and is not logged twice"
        );
        let _ = drain_decode_notes();

        // And the trailer names the remainder, keyed on the folder so a second folder can say it too.
        super::note_scan_suppressed(Path::new("C:/tail/rowfolder"), 100);
        let lines: Vec<String> =
            drain_decode_notes().into_iter().filter(|l| l.starts_with("scan: ")).collect();
        assert_eq!(
            lines,
            vec!["scan: …and 100 more like it in this folder — per-file name/bytes lines are capped at 200 for this session".to_string()]
        );
        super::note_scan_suppressed(Path::new("C:/tail/rowfolder"), 100);
        assert!(
            drain_decode_notes().iter().all(|l| !l.starts_with("scan: ")),
            "one trailer per folder, not one per call"
        );
        super::note_scan_suppressed(Path::new("C:/tail/rowfolder"), 0);
        assert!(drain_decode_notes().iter().all(|l| !l.starts_with("scan: ")), "nothing hidden, nothing said");
    }

    /// **B-Y2 — a not-yet-downloaded file says so, and is re-asked the moment it arrives.**
    ///
    /// Rule (1) is right and untouched: a placeholder is not read, because opening it IS the
    /// download. What was wrong is that the arm was also SILENT — a dehydrated OneDrive PNG named
    /// `.JPG` got the exact pre-round experience with no diagnostic anywhere — and that
    /// `step_cloud_retry` cleared the latches every 30 s so all three tiers could re-fail as a JPEG
    /// for ever, until the user thought to leave the folder and come back.
    ///
    /// The hydration half is driven through `reclassify_hydrated`, which is the function the sweep
    /// calls; the sweep's own wiring (`tick.rs` `step_cloud_retry`, which now takes the shot list's
    /// owner and re-stamps copy-on-write) is what carries it to the app.
    ///
    /// L28 — REDDENED BY: deleting `placeholder_naming_line`'s call at the chokepoint (the first
    /// assert), or making `reclassify_hydrated` a no-op / dropping its `shot.kind` write (the
    /// second) — which is exactly the state this round shipped and §C recorded as residue.
    #[test]
    fn a_placeholder_says_it_was_named_and_is_re_asked_when_it_lands() {
        // The naming line is pure, so the placeholder half needs no cloud filesystem.
        assert_eq!(
            placeholder_naming_line("53d879f01a3f481c.JPG", SrcKind::Jpeg),
            "53d879f01a3f481c.JPG: not downloaded yet — classified by its NAME as JPG until it arrives"
        );

        // The hydration half, on the real function the sweep calls: a shot that the scan could only
        // name, whose file is now on disk and is a PNG.
        let dir = fresh_dir("tail_y2");
        let p = place(&dir, "png_named_jpg.jpg", "arrived.JPG");
        let mut shot = Shot {
            id: 0,
            name: "arrived".into(),
            has_raw: false,
            has_jpg: true,
            raw: None,
            jpg: Some(p),
            kind: SrcKind::Jpeg, // what the NAME said, which is all the scan was allowed to read
            cloud_placeholder: true,
            sniffed: None,
        };
        assert!(super::reclassify_hydrated(&mut shot), "the answer changed once the bytes arrived");
        assert_eq!(shot.kind, SrcKind::Png);
        assert_eq!(shot.sniffed, Some(SrcKind::Png));
        assert_eq!(shot.finished_format().as_deref(), Some("PNG"));
        assert_eq!(decode_full_rgb(&shot).map(|(_, w, h)| (w, h)).unwrap(), (16, 16));
        // Idempotent: a second sweep over an already-corrected shot reports nothing changed, so the
        // 30 s cadence cannot turn into a 30 s log line.
        assert!(!super::reclassify_hydrated(&mut shot), "already right — nothing to say");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A-R1 — the macOS half of "never read a cloud placeholder".**
    ///
    /// Rule (1) was a WINDOWS-ONLY guarantee: `cloud_paths` was populated under `#[cfg(windows)]`
    /// alone, so on a Mac `scan_classify` sniffed everything and `read_head` opened every candidate.
    /// On an iCloud Drive folder under "Optimise Mac Storage" — or OneDrive/Dropbox on macOS 12.3+,
    /// which both moved onto the same File Provider eviction — that would MATERIALISE every image in
    /// the folder at folder open. The macOS marker is `SF_DATALESS` in `st_flags`, and like its
    /// Windows twin it is metadata-only.
    ///
    /// STATED LIMIT: this row pins the PREDICATE, which is pure and runs on any host. It cannot
    /// reach the caller — `std::os::macos::fs::MetadataExt` compiles only on macOS — so the
    /// caller-side guarantee is first EXECUTED by the mac CI, and that run is its falsifier.
    ///
    /// L28 — REDDENED BY: changing the constant (it is `<sys/stat.h>`'s `SF_DATALESS`, 0x4000_0000),
    /// or by testing `!= 0` on the whole word instead of masking the one bit — which is what makes
    /// the `SF_IMMUTABLE`-alone assert below the real falsifier.
    #[test]
    fn a_dataless_file_is_a_placeholder_on_macos() {
        assert_eq!(super::SF_DATALESS, 0x4000_0000, "<sys/stat.h>");
        assert!(super::is_dataless(super::SF_DATALESS));
        assert!(super::is_dataless(super::SF_DATALESS | 0x0000_0002), "beside UF_NODUMP");
        assert!(!super::is_dataless(0), "an ordinary local file");
        assert!(!super::is_dataless(0x0002_0000), "SF_IMMUTABLE alone is not datalessness");
        assert!(!super::is_dataless(0x0000_0002 | 0x0002_0000), "…nor any other flag combination");
    }

    /// **A-O1 — the SOF walk is bounded by BYTES, not by hope.**
    ///
    /// The doc claimed "a crafted file cannot make it read forever", and the two FF-search loops
    /// ended only at EOF, one `read_exact` of one byte at a time: A measured `FF D8` + 32 MB of
    /// `0x00` at 36.9 ms (~1.15 ms/MB). `source_dimensions` runs on the UI thread and rule (2) makes
    /// any `FF D8 FF` file a `Jpeg` by content, so a 2 GB one was a ~2.3 s stall. The walk now
    /// block-scans and charges every byte it reads or seeks to a 16 MiB budget.
    ///
    /// It is pinned by BYTES CONSUMED and never by time — a timing assert on a shared CI box is a
    /// flake generator, and the budget is the thing that actually holds.
    ///
    /// L28 — REDDENED BY: removing either `checked_sub` (the walk then runs to the end of the
    /// ≈16.03 MiB fixture — `JPEG_HEADER_BUDGET + 4 × 8192` — and the position assert reddens with
    /// `the walk consumed 16809986 bytes against a 16777216-byte budget (+ one 8192 buffer)`), or
    /// by going back to the byte-at-a-time
    /// `read_exact` search (the budget still holds, but this row's own timing on its
    /// ≈ 16.03 MiB fixture — `JPEG_HEADER_BUDGET + 4 × 8192` — goes from ~1 ms to ~25 ms,
    /// which is why the fixture is built in memory here). TAIL 4, re-verification Y3: this
    /// clause said "20 MiB", a size nothing in this row builds — the FIRST clause was
    /// corrected in tail 3 and this one was missed.
    #[test]
    fn the_sof_walk_is_bounded_by_a_byte_budget() {
        use std::io::{BufReader, Cursor, Seek};
        const BUF: usize = 8192;
        let budget = super::JPEG_HEADER_BUDGET;

        // (1) FF D8 then a fill run LONGER than the budget: None, and the walk stopped inside it.
        let mut fill = vec![0xFFu8, 0xD8];
        fill.extend(std::iter::repeat_n(0x00, budget + 4 * BUF));
        let mut r = BufReader::with_capacity(BUF, Cursor::new(fill));
        assert_eq!(jpeg_sof_dims(&mut r).map(|f| (f.w, f.h)), None, "past the budget the answer is None");
        let read = r.stream_position().unwrap() as usize;
        assert!(
            read <= budget + BUF + 2,
            "the walk consumed {read} bytes against a {budget}-byte budget (+ one {BUF} buffer)"
        );

        // (2) …and a file whose SOF sits 1 MiB in — comfortably inside the budget — still answers.
        let mut ok = vec![0xFFu8, 0xD8];
        ok.extend(std::iter::repeat_n(0x00, 1024 * 1024));
        ok.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        ok.extend_from_slice(&2337u16.to_be_bytes());
        ok.extend_from_slice(&1649u16.to_be_bytes());
        ok.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        let mut r = BufReader::with_capacity(BUF, Cursor::new(ok));
        let f = jpeg_sof_dims(&mut r).expect("a SOF inside the budget is found");
        assert_eq!((f.w, f.h, f.components), (1649, 2337, 3));
    }

    /// **A-O3 — the `ftyp` box's own size bounds the brand list.**
    ///
    /// The brand walk read five fixed slots with no reference to the box's declared length, so bytes
    /// belonging to the NEXT box could be read as brands. A measured it: a legal 16-byte
    /// `ftyp mif1` followed by an `mdat` whose payload begins `avif` answered `Some(Unsupported)`
    /// from four bytes that are not a brand at all.
    ///
    /// L28 — REDDENED BY: dropping the `end` clamp (the first pair below stops agreeing), or by
    /// clamping to the declared size WITHOUT `.min(head.len())` (a box declaring 4 GB then indexes
    /// past a 32-byte head).
    #[test]
    fn the_ftyp_box_size_bounds_the_brand_walk() {
        // A 16-byte ftyp: size(4) + "ftyp"(4) + major(4) + minor(4). No compatible brands at all.
        let ftyp16 = |trailer: &[u8]| {
            let mut v = vec![0u8, 0, 0, 16];
            v.extend_from_slice(b"ftypmif1");
            v.extend_from_slice(&[0, 0, 0, 0]);
            v.extend_from_slice(trailer);
            v.resize(SNIFF_HEAD_BYTES, 0);
            v
        };
        let alone = sniff_kind(&ftyp16(&[]));
        let with_mdat = sniff_kind(&ftyp16(b"\x00\x00\x10\x00mdatavif"));
        assert_eq!(
            alone, with_mdat,
            "bytes outside the ftyp box must not change its answer (they are another box's payload)"
        );
        assert_eq!(alone, Some(SrcKind::Heic), "…and `mif1` alone is still the generic HEIF brand");

        // size 0 (to EOF) and size 1 (64-bit largesize) have no usable length, so the head is the
        // only bound there is — and the brands inside it are still read.
        for size in [0u32, 1] {
            let mut v = size.to_be_bytes().to_vec();
            v.extend_from_slice(b"ftypavif");
            v.extend_from_slice(&[0, 0, 0, 0]);
            v.extend_from_slice(b"mif1miaf");
            v.resize(SNIFF_HEAD_BYTES, 0);
            assert_eq!(sniff_kind(&v), Some(SrcKind::Unsupported), "size {size} = 'no length here'");
        }
        // A box that declares more than the head holds must not index past it.
        let mut huge = 0xFFFF_FFFFu32.to_be_bytes().to_vec();
        huge.extend_from_slice(b"ftypheic");
        huge.resize(SNIFF_HEAD_BYTES, 0);
        assert_eq!(sniff_kind(&huge), Some(SrcKind::Heic), "clamped to the head, not to the claim");
    }


    /// **V-Y1 — ONE mint, and its four arms.** The passenger rule is a table, and the round shipped
    /// that table twice: once in the scan and once, wrongly, inside `reclassify_hydrated`, which
    /// re-stamped `kind` from the sniff alone. `mint_finished` is now the only copy.
    ///
    /// L28 — REDDENED BY: swapping the `has_raw` guard on the second arm (the passenger and the
    /// sole-source rows trade answers), or by returning `true` for `has_jpg` on any undecodable arm
    /// — which is the state the hydration row below proves the scanner cannot produce.
    #[test]
    fn the_mint_has_four_arms_and_only_one_of_them_is_a_passenger() {
        use super::mint_finished;
        // decodable → it IS the picture, with or without a RAW beside it
        assert_eq!(mint_finished(Some(SrcKind::Png), false), (SrcKind::Png, true));
        assert_eq!(mint_finished(Some(SrcKind::Png), true), (SrcKind::Png, true));
        assert_eq!(mint_finished(Some(SrcKind::Heic), true), (SrcKind::Heic, true));
        // undecodable WITH a RAW → a passenger: the RAW-preview kind, and not the picture
        assert_eq!(mint_finished(Some(SrcKind::Unsupported), true), (SrcKind::Jpeg, false));
        // undecodable ALONE → the shot IS the file, and the card is honest
        assert_eq!(mint_finished(Some(SrcKind::Unsupported), false), (SrcKind::Unsupported, false));
        // no finished candidate at all → the RAW, exactly as before this round existed
        assert_eq!(mint_finished(None, true), (SrcKind::Jpeg, false));
        assert_eq!(mint_finished(None, false), (SrcKind::Jpeg, false));
        // THE INVARIANT THE WHOLE TAIL RESTS ON: `Unsupported` and `has_jpg` never travel together,
        // for any input. That pairing is what puts the codec card in front of a RAW's photograph.
        for sniffed in [None, Some(SrcKind::Jpeg), Some(SrcKind::Heic), Some(SrcKind::Unsupported)] {
            for has_raw in [false, true] {
                let (k, has_jpg) = mint_finished(sniffed, has_raw);
                assert!(
                    !(k == SrcKind::Unsupported && has_jpg),
                    "unmintable state for {sniffed:?}/{has_raw}: Unsupported with has_jpg"
                );
            }
        }
    }

    /// **V-Y1 — the hydration mint obeys the same table.** A RAW whose same-stem sibling was a cloud
    /// PLACEHOLDER at scan takes rule (1): unread, so the name's kind stands and `has_jpg` is true.
    /// When that file lands and turns out to be an AVIF, `reclassify_hydrated` used to stamp
    /// `kind = Unsupported` while leaving `has_jpg` true — a state the scanner cannot produce — and
    /// the RAW lost its picture to the codec card until the next folder swap.
    ///
    /// L28 — REDDENED BY: going back to `shot.kind = kind` straight from `scan_classify` (the
    /// `has_jpg` assert reddens with `left true right false`, and the kind assert with
    /// `left Unsupported right Jpeg`).
    #[test]
    fn a_placeholder_that_lands_as_an_undecodable_file_becomes_a_passenger() {
        let dir = fresh_dir("t3_hydrate_mint");
        let mut avif = vec![0u8, 0, 0, 24];
        avif.extend_from_slice(b"ftypavif");
        avif.extend_from_slice(&[0, 0, 0, 0]);
        avif.extend_from_slice(b"mif1miaf");
        let p = dir.join("IMG_9.jpg");
        std::fs::write(&p, &avif).unwrap();
        let mut shot = Shot {
            id: 0,
            name: "IMG_9".into(),
            has_raw: true,
            has_jpg: true, // rule (1): unread at scan, so the NAME's kind stood
            raw: Some(dir.join("IMG_9.CR3")),
            jpg: Some(p),
            kind: SrcKind::Jpeg,
            cloud_placeholder: true,
            sniffed: None,
        };
        assert!(super::reclassify_hydrated(&mut shot), "the state changed when the bytes arrived");
        assert_eq!(shot.kind, SrcKind::Jpeg, "the RAW is the picture");
        assert!(!shot.has_jpg, "…and the arrival is a passenger, not the picture");
        assert!(!shot.is_unsupported(), "the codec card must not reach a shot that has a RAW");
        assert_eq!(shot.jpg.as_ref().map(|p| p.file_name().unwrap()), Some("IMG_9.jpg".as_ref()));
        // Idempotent: a second sweep over the corrected shot changes nothing and says nothing.
        assert!(!super::reclassify_hydrated(&mut shot));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **V-R1 + V-R2 + V-O1 + V-Y2 — THE FALSIFIER: a passenger shot and a RAW-only shot are the
    /// same photograph, and every picture surface must say so.**
    ///
    /// The A-Y3b row asserted that the passenger decodes, badges and cards correctly. What it never
    /// asked is how the picture is ORIENTED, what the panel says about it, or whether the zoom chip
    /// may speak for it — and the verifier found all three wrong: a portrait RAW with a passenger
    /// displayed and web-exported SIDEWAYS (`read_orientation` took the finished arm and got
    /// `None`), the panel described the passenger's EXIF, and the chip lit "Focus 1:1" over the
    /// RAW's ~1600 px embedded preview.
    ///
    /// So the row builds the SAME RAW twice — alone, and with an undecodable same-stem sibling —
    /// and asserts the two shots AGREE on every picture surface with a pure seam, while DIFFERING
    /// on the file surfaces. That shape is the rule itself, executable: *a passenger is a RAW-only
    /// shot as far as the picture goes.*
    ///
    /// L28 — REDDENED BY: dropping `.filter(|_| shot.has_jpg)` from `read_orientation`,
    /// `exif_rows` or `read_date_taken`. At `e0dc974` the orientation pair reads
    /// `the two shots are the same photograph, so they are oriented alike: left None right Some(1)`
    /// (and `Some(6)` on the owner's portrait RAW, in `tests/real.rs`); at `29562f6` the panel pair
    /// reads `the panel must describe the RAW on screen, not the passenger beside it:
    /// left Some("Apple iPhone 17 Pro") right None`.
    ///
    /// TAIL 4 (re-verification O1) — THE ROW NAMES PRODUCTION FUNCTIONS ONLY. It used to assert on
    /// `read_exif` and `decoder_consumed_turns`; both are twins the app never calls (`read_exif` has
    /// zero references under `native/`, `decoder_consumed_turns` none either), so the assertions
    /// were green against code that does not ship. The panel is `exif_rows` and nothing else.
    #[test]
    fn a_passenger_shot_is_a_raw_only_shot_for_every_picture_surface() {
        let kit = crate::fixture_paths::standard_raw();
        if !kit.is_file() {
            eprintln!("testkit RAW absent — row skipped");
            return;
        }
        let mut avif = vec![0u8, 0, 0, 24];
        avif.extend_from_slice(b"ftypavif");
        avif.extend_from_slice(&[0, 0, 0, 0]);
        avif.extend_from_slice(b"mif1miaf");

        // The SAME RAW, twice: alone, and carrying an undecodable same-stem sibling.
        let alone_dir = fresh_dir("t3_alone");
        std::fs::copy(kit, alone_dir.join("IMG_1.CR3")).unwrap();
        let with_dir = fresh_dir("t3_with");
        std::fs::copy(kit, with_dir.join("IMG_1.CR3")).unwrap();
        std::fs::write(with_dir.join("IMG_1.jpg"), &avif).unwrap();
        let alone = only_shot(&alone_dir);
        let with = only_shot(&with_dir);

        // ── THE PICTURE SURFACES: every one of these must agree. ──
        assert_eq!(with.has_jpg, alone.has_jpg, "neither shot's picture is a finished file");
        assert_eq!(with.kind, alone.kind, "both decode through the RAW-preview arm");
        assert_eq!(
            super::read_orientation(&with, false),
            super::read_orientation(&alone, false),
            "the two shots are the same photograph, so they are oriented alike"
        );
        assert_eq!(
            super::read_orientation(&with, true),
            super::read_orientation(&alone, true),
            "…and alike in RAW mode too"
        );
        assert_eq!(source_dimensions(&with), source_dimensions(&alone));
        assert_eq!(super::decoder_consumed_turns(&with), super::decoder_consumed_turns(&alone));
        // THE PANEL IS `exif_rows`, AND ONLY `exif_rows`. TAIL 4 (re-verification O1): tail 3 put
        // this assertion on `read_exif`, which has ZERO callers under `native/` — so the fix shipped
        // green while the shipping panel still read the passenger. The row now drives the function
        // `main.rs`'s EXIF worker actually calls.
        let camera = |sh: &Shot| {
            super::exif_rows(sh, 0).into_iter().find(|(k, _)| k == "Camera").map(|(_, v)| v)
        };
        assert_eq!(camera(&with), camera(&alone), "the panel describes the picture on screen");
        assert_eq!(super::read_date_taken(&with), super::read_date_taken(&alone));

        // V-Y2 NEEDS A PASSENGER WHOSE FILE ACTUALLY CARRIES EXIF, or the pair above agrees for the
        // wrong reason (a CR3 answers `None` on both sides, so removing the `has_jpg` term would not
        // redden it). A real HEIC does carry EXIF kamadak reads, and on a codec-less box a HEIC
        // beside a RAW is exactly a passenger — a state this box cannot SCAN into, because it HAS
        // the codec, so the shot is built directly. The mirror below is what makes the assertion
        // non-vacuous: the same file, marked as the picture, IS read.
        let heic = crate::fixture_paths::sample_heic();
        if heic.is_file() {
            let passenger = Shot { has_jpg: false, jpg: Some(heic.to_path_buf()), ..alone.clone() };
            let as_picture = Shot { has_jpg: true, ..passenger.clone() };
            // The guard first: the fixture must actually carry EXIF, or the pair below agrees
            // emptily — which is exactly how tail 3's CR3-only version passed against a twin.
            assert!(
                camera(&as_picture).is_some(),
                "the fixture must carry EXIF, or this half proves nothing"
            );
            assert_eq!(
                camera(&passenger),
                camera(&alone),
                "the panel must describe the RAW on screen, not the passenger beside it"
            );
            assert_eq!(super::read_date_taken(&passenger), super::read_date_taken(&alone));
        }
        assert_eq!(with.finished_format(), alone.finished_format(), "the badge names the RAW");
        assert_eq!(with.badge_label(), alone.badge_label());
        let (dw, da) = (decode_full_rgb(&with).unwrap(), decode_full_rgb(&alone).unwrap());
        assert_eq!((dw.1, dw.2), (da.1, da.2), "the same pixels, at the same size");
        assert!(da.1 > 0 && da.2 > 0, "…and it is a real picture, not an empty agreement");

        // ── THE FILE SURFACES: these must DIFFER, or the file would be invisible again. ──
        assert!(alone.jpg.is_none() && with.jpg.is_some(), "the passenger is on the shot");
        assert_eq!(with.finished_file_format(), None, "an AVIF has no noun this build can print");
        assert_eq!(alone.finished_file_format(), None);
        // Delete's own collector shape: `[shot.jpg, shot.raw]`, so the passenger costs one file.
        let files = |s: &Shot| s.jpg.iter().chain(s.raw.iter()).count();
        assert_eq!((files(&alone), files(&with)), (1, 2), "delete takes both files for the passenger");
        let _ = std::fs::remove_dir_all(&alone_dir);
        let _ = std::fs::remove_dir_all(&with_dir);
    }
    /// **A-Y3 + A-Y3b — the undecodable sibling stays on the shot, and the RAW keeps its picture.**
    ///
    /// A-Y3 (the tail): a RAW paired with a finished sibling this round newly re-stamps
    /// `Unsupported` fell to `_ => (None, SrcKind::Jpeg)` and the finished PATH was thrown away, so
    /// the file became invisible and undeletable in a culler whose whole job is deciding what to
    /// keep. A-Y3b (the micro-tail, my own flag upheld): the tail's fix then stamped the SHOT
    /// `Unsupported`, which traded the RAW's photograph for an "install the codec" card — reachable
    /// for `X.CR3` + `X.HEIC` on any Windows box without the HEVC Image Extension, which is most of
    /// them. The ruling was about visibility, never about which picture the shot shows.
    ///
    /// So the file rides along and the RAW is the picture. `has_jpg` is where the two questions
    /// part — its doc has said "False for a RAW-only or unsupported shot" since it was written —
    /// and every consumer that asks "what do I decode / probe / colour-read / patch / badge?" now
    /// reads it, while everything that asks "what files are here?" still reads `jpg` and finds
    /// both. **DELETE therefore takes both files with no change at all:** `main.rs`'s delete
    /// collector pushes `shot.raw` and `shot.jpg` (plus each one's sidecar), and the passenger is
    /// in `shot.jpg` — which is the entire reason the path is kept there rather than in a new
    /// field that every file operation in the tree would have had to learn about.
    ///
    /// L28 — REDDENED BY: putting the `if !has_raw` guard back on the middle arm of the `(jpg,
    /// kind, has_jpg)` resolution (the path is dropped and the first assert reddens — at `a51da1d`
    /// it read `the sibling must stay on the shot: left None right Some("IMG_1.jpg")`); or by
    /// stamping the passenger `SrcKind::Unsupported`, which is what `0a3e11a` shipped — the decode
    /// assert then reddens with `the RAW must still be the picture: Err("unsupported source")` and
    /// the card assert with `left true right false`.
    #[test]
    fn a_raw_with_an_undecodable_sibling_keeps_its_picture_and_its_file() {
        // A real RAW, because the claim is that its EMBEDDED PREVIEW is what gets decoded. The
        // testkit is the owner's local corpus, so the row skips rather than fails without it —
        // the same contract `the_existing_corpus_is_not_reclassified` uses.
        let kit = crate::fixture_paths::standard_raw();
        if !kit.is_file() {
            eprintln!("testkit RAW absent — row skipped");
            return;
        }
        let mut avif = vec![0u8, 0, 0, 24];
        avif.extend_from_slice(b"ftypavif");
        avif.extend_from_slice(&[0, 0, 0, 0]);
        avif.extend_from_slice(b"mif1miaf");

        let dir = fresh_dir("mt_ay3b");
        std::fs::copy(kit, dir.join("IMG_1.CR3")).unwrap();
        std::fs::write(dir.join("IMG_1.jpg"), &avif).unwrap();
        let shots = scan_folder(&dir).unwrap();
        let paired = shots.iter().find(|s| s.has_raw).expect("the RAW's shot");

        // (1) THE FILE IS KEPT — this is A-Y3, and it is what makes the shot's delete take both.
        assert_eq!(
            paired.jpg.as_ref().and_then(|p| p.file_name()).and_then(|s| s.to_str()),
            Some("IMG_1.jpg"),
            "the sibling must stay on the shot"
        );
        // (2) THE PICTURE IS THE RAW'S — this is A-Y3b.
        assert!(!paired.has_jpg, "the sibling is a passenger, not the picture");
        assert_eq!(paired.kind, SrcKind::Jpeg, "the RAW-preview kind, as before the round");
        assert!(!paired.is_unsupported(), "no 'install the codec' card on a shot that has a picture");
        let (_, w, h) = decode_full_rgb(paired).expect("the RAW must still be the picture");
        assert!(w > 0 && h > 0, "the RAW's embedded preview decoded: {w}×{h}");
        // (3) THE SURFACES NAME WHAT IS ON SCREEN, and the panel carries the passenger.
        assert_eq!(paired.finished_format(), None, "the badge/menu/toast describe the RAW");
        assert_eq!(paired.badge_label(), "RAW");
        assert_eq!(source_dimensions(paired), None, "a passenger has no dimensions to report");

        // (4) THE SOLE-SOURCE CASE STILL REACHES THE CARD — the case it was written for.
        let dir2 = fresh_dir("mt_ay3b_sole");
        std::fs::write(dir2.join("holiday.jpg"), &avif).unwrap();
        let sole = only_shot(&dir2);
        assert!(sole.is_unsupported(), "with no RAW behind it, the file IS the shot");
        assert!(sole.jpg.is_some(), "…and it is still visible and deletable");
        assert_eq!(sole.finished_format(), None, "…and still never says JPG (B-R2)");

        // (5) A GENUINE codec-less HEIC passenger is listed on the panel BY ITS CONTAINER. The
        // capability gate is driven the way the HEIC rows drive it — through the pure
        // `heic_scan_kind` — because this box HAS the HEVC codec and cannot produce the state by
        // scanning. `finished_file_format` is the panel's question; `finished_format` is the
        // badge's, and they now differ, which is the whole of A-Y3b in one pair of asserts.
        assert_eq!(super::heic_scan_kind(false), SrcKind::Unsupported, "the codec-less verdict");
        let codecless_passenger = Shot {
            id: 0,
            name: "IMG_2".into(),
            has_raw: true,
            has_jpg: false,
            raw: Some(dir.join("IMG_1.CR3")),
            jpg: Some(dir.join("IMG_2.HEIC")),
            kind: SrcKind::Jpeg,
            cloud_placeholder: false,
            sniffed: None,
        };
        assert_eq!(codecless_passenger.finished_file_format().as_deref(), Some("HEIC"));
        assert_eq!(codecless_passenger.finished_format(), None);
        assert_eq!(codecless_passenger.badge_label(), "RAW");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// **OWNER RULING (CMYK) — both routes produce a picture, and the lever is live per decode.**
    ///
    /// The option exists because the owner's colour ground truth is Photoshop, which — like Windows
    /// — assigns a default working CMYK profile to an untagged 4-component JPEG, while Falcon does
    /// the straight ink arithmetic. Neither reads anything from the file; both are assumptions. What
    /// this row pins is that the lever is real, that it is read per decode, and that NEITHER answer
    /// is a failure: the option can change the colour, never whether the photograph opens.
    ///
    /// L28 — REDDENED BY: memoising `cmyk_os_route` (a `OnceLock`, the shape three other levers in
    /// this tree once had), or by letting the OS route REPLACE the pure arm instead of preceding it
    /// — the restore-and-decode at the end would then still be on the OS route.
    #[cfg(windows)]
    #[test]
    fn the_cmyk_route_is_a_live_lever_and_both_routes_open_the_file() {
        let _guard = CMYK_LEVER.lock().unwrap_or_else(|e| e.into_inner());
        let restore = super::cmyk_os_route();
        let dir = fresh_dir("tail_cmyk");
        place(&dir, "cmyk_adobe.jpg", "press.jpg");
        let shot = only_shot(&dir);
        assert_eq!(super::jpeg_component_count(&std::fs::read(dir.join("press.jpg")).unwrap()), Some(4));

        super::set_cmyk_os_route(false);
        assert!(!super::cmyk_os_route());
        let (ours, w, h) = decode_full_rgb(&shot).expect("Falcon's own conversion opens it");
        assert_eq!((w, h), (16, 16));

        super::set_cmyk_os_route(true);
        assert!(super::cmyk_os_route(), "read fresh, never memoised");
        let (theirs, w2, h2) = decode_full_rgb(&shot).expect("Windows' codec opens it too");
        assert_eq!((w2, h2), (16, 16), "same picture, same geometry — only the colour policy differs");
        assert_eq!(ours.len(), theirs.len());
        assert_ne!(ours, theirs, "…and they DO differ, or the option would be theatre");

        // Back to the default, and the default is what the flip returns to — per decode, no memo.
        super::set_cmyk_os_route(false);
        assert_eq!(decode_full_rgb(&shot).unwrap().0, ours, "the next photograph takes the new setting");
        super::set_cmyk_os_route(restore);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod r35_golden {
    //! ROUND 35 (queue item 35, sheet 2.2c) — **R0 GOLDEN.** The four deliverables the round is not
    //! allowed to move: the two testkit files the export can reach, each written at the JPG stop and
    //! at the PNG stop. Measured at the PARENT commit (`4a34ad4`, source tree `0461c8e`) by this row
    //! itself and asserted afterwards — the round keeps transparency and depth for sources that
    //! HAVE them, and neither of these two has either (`IMG_TEST.tif.png` is RGBA with every alpha
    //! 255, which is exactly the 0.1 (b) opaque-alpha drop; `AdobeRGB_test.jpg` is an 8-bit JPEG),
    //! so every byte of all four files must survive the change.
    //!
    //! The four hashes are SHA-256 of the FILE, written to a temp directory through
    //! `std::fs::write` and read back, so the row measures a deliverable rather than a Vec.
    use super::*;
    use std::path::PathBuf;

    /// The curated fixtures the app boots against (TESTING.md §8). Only present on the dev box; the
    /// row no-ops elsewhere, the same skip `cpu_decoder_is_byte_identical_to_decode_source_rgb` uses.
    fn testkit_std() -> Option<PathBuf> {
        let d = std::env::var_os("LOCALAPPDATA")
            .map(|p| PathBuf::from(p).join("Falcon").join("testkit").join("standard"))?;
        d.is_dir().then_some(d)
    }

    fn shot_for(path: &std::path::Path, kind: SrcKind) -> Shot {
        Shot {
            id: 0,
            name: path.file_name().and_then(|s| s.to_str()).unwrap_or("t").to_string(),
            has_raw: false,
            has_jpg: true,
            raw: None,
            jpg: Some(path.to_path_buf()),
            kind,
            cloud_placeholder: false,
            sniffed: None,
        }
    }

    const K256: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    /// SHA-256 (FIPS 180-4), written here because this crate has no digest dependency and a golden
    /// is worth nothing if its hash function is not the one everyone else's tools compute. The
    /// `abc` vector below is the anti-vacuity guard: it is the standard's own published digest.
    fn sha256_hex(data: &[u8]) -> String {
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut msg = data.to_vec();
        let bits = (data.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bits.to_be_bytes());
        for block in msg.chunks_exact(64) {
            let mut w = [0u32; 64];
            for (slot, four) in w.iter_mut().zip(block.chunks_exact(4)) {
                *slot = u32::from_be_bytes([four[0], four[1], four[2], four[3]]);
            }
            for i in 16..64 {
                let a = w[i - 15];
                let b = w[i - 2];
                let s0 = a.rotate_right(7) ^ a.rotate_right(18) ^ (a >> 3);
                let s1 = b.rotate_right(17) ^ b.rotate_right(19) ^ (b >> 10);
                w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut z] = h;
            for (k, wi) in K256.iter().zip(w.iter()) {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let t1 = z.wrapping_add(s1).wrapping_add(ch).wrapping_add(*k).wrapping_add(*wi);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                z = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, z]) {
                *slot = slot.wrapping_add(v);
            }
        }
        h.iter().map(|v| format!("{v:08x}")).collect()
    }

    /// ROUND 35 R0 — **THE GOLDEN.** Every stop of the shipped export, on the two testkit files the
    /// round's `Keep` request can never fire for, must produce the identical FILE afterwards.
    ///
    /// FALSIFIER (L28), NAMED FOR THESE TWO FILES rather than for the path in general: both are
    /// OPAQUE and one of them is smaller than the long edge, so the statements that actually
    /// decide their bytes are the resize (the AdobeRGB cells), the sRGB transform, the PNG
    /// encoder's compression level and chunk set (the PNG cells) and the JPEG encoder's sampling
    /// factor. Change any of those and the assert below prints the moved hash beside the one
    /// measured at the parent. The flatten itself (`over_white`) and `rotate_packed`'s turns-0 arm
    /// CANNOT be reddened by these two files -- an opaque source and a zero turn -- which is what
    /// R4 and `the_full_tier_hands_the_same_allocation_through` are for.
    #[test]
    fn the_two_testkit_goldens_export_byte_for_byte_as_they_did() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "ANTI-VACUITY: the row's SHA-256 is the standard's, or its four goldens mean nothing"
        );
        let Some(dir) = testkit_std() else {
            eprintln!("testkit/standard absent — R0 skipped");
            return;
        };
        let out = std::env::temp_dir().join("falcon_r35_golden");
        let _ = std::fs::create_dir_all(&out);
        let cases: [(&str, SrcKind, WebFormat); 4] = [
            ("IMG_TEST.tif.png", SrcKind::Png, WebFormat::Jpeg),
            ("IMG_TEST.tif.png", SrcKind::Png, WebFormat::Png),
            ("AdobeRGB_test.jpg", SrcKind::Jpeg, WebFormat::Jpeg),
            ("AdobeRGB_test.jpg", SrcKind::Jpeg, WebFormat::Png),
        ];
        let mut got: Vec<String> = Vec::new();
        for (name, kind, fmt) in cases {
            let src = dir.join(name);
            if !src.is_file() {
                eprintln!("R0: {name} absent — skipped");
                return;
            }
            let shot = shot_for(&src, kind);
            let gamut = shot_source_gamut(&shot);
            let (rgb, w, h) = decode_full_rgb(&shot).expect("the golden source decodes");
            // The production walk's own shape: decode, bake rotation (0 turns here — the two
            // goldens are upright), then the one export entry at the sheet's 2048 / q80.
            let (rgb, w, h) = rotate_rgb(&rgb, w, h, 0);
            let bytes = export_web_image(rgb, w, h, 2048, 80, None, gamut, fmt)
                .expect("the golden exports");
            let file = out.join(format!("{name}.{}", fmt.ext()));
            std::fs::write(&file, &bytes).expect("write the deliverable");
            let read_back = std::fs::read(&file).expect("read the deliverable back");
            got.push(format!("{name} {} {} {}", fmt.noun(), read_back.len(), sha256_hex(&read_back)));
        }
        let _ = std::fs::remove_dir_all(&out);
        // MEASURED 2026-09-08 at the parent commit `4a34ad4` (source tree `0461c8e`) by this
        // row itself, on this machine's testkit. Each cell is `<file> <stop> <bytes> <sha256>`,
        // the SHA-256 of the deliverable as written to disk and read back.
        let want = [
            "IMG_TEST.tif.png JPG 14591 66801f684fc9c8e11de5f98a8bedc8600c67ca7fe33ef77634d59b01acb07833".to_string(),
            "IMG_TEST.tif.png PNG 180420 6cb116e954fc4641d7f0cc6dee6212ff85d84de200016dfa337d8b757dca2086".to_string(),
            "AdobeRGB_test.jpg JPG 838146 81cf5f2018ccecb3b12d5d54d25b3cec4f14fec5d6aba834f012f742cb32a576".to_string(),
            "AdobeRGB_test.jpg PNG 6687561 d98475706863222f7a70fcaffc06d0178b881b594eb1e1b7faf3c7c46337e575".to_string(),
        ];
        assert_eq!(got, want, "R0 GOLDEN: the four deliverables measured at the parent commit");
    }
}

#[cfg(test)]
mod r35 {
    //! ROUND 35 (queue item 35, sheet 2.2c) -- **THE PNG EXPORT KEEPS TRANSPARENCY AND 16-BIT
    //! DEPTH.** Every fixture below is MINTED IN PROCESS, into a temp directory, at test time
    //! (Q8): nothing lands in `testkit/`, nothing is committed as a binary, and every row can say
    //! exactly what its input holds because it wrote it.
    use super::*;
    use falcon_color::Gamut;
    use std::path::{Path, PathBuf};

    // ─────────────────────────────── the fixture mint ───────────────────────────────

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("falcon_r35_{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    fn shot_for(path: &Path, kind: SrcKind) -> Shot {
        Shot {
            id: 0,
            name: path.file_name().and_then(|s| s.to_str()).unwrap_or("t").to_string(),
            has_raw: false,
            has_jpg: true,
            raw: None,
            jpg: Some(path.to_path_buf()),
            kind,
            cloud_placeholder: false,
            sniffed: None,
        }
    }

    /// Mint a PNG at an exact colour type and depth. `data` is the raw sample stream the PNG spec
    /// wants -- for 16-bit that means BIG-ENDIAN pairs, which is also what the production encoder
    /// writes, so a fixture built here and a deliverable written there are the same bytes.
    fn mint_png(path: &Path, w: u32, h: u32, color: png::ColorType, depth: png::BitDepth, data: &[u8]) {
        let file = std::fs::File::create(path).expect("create the fixture");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(color);
        enc.set_depth(depth);
        let mut wtr = enc.write_header().expect("fixture header");
        wtr.write_image_data(data).expect("fixture data");
        wtr.finish().expect("fixture finish");
    }

    /// Native `u16` samples as the big-endian byte stream a PNG carries.
    fn be(v: &[u16]) -> Vec<u8> {
        v.iter().flat_map(|s| s.to_be_bytes()).collect()
    }

    /// Read a PNG with NO transformations -- the colour type, the depth and the raw sample stream
    /// exactly as the file carries them (16-bit stays big-endian, which is what R8 measures).
    fn read_png_raw(bytes: &[u8]) -> (png::ColorType, png::BitDepth, u32, u32, Vec<u8>) {
        let dec = png::Decoder::new(std::io::Cursor::new(bytes.to_vec()));
        let mut r = dec.read_info().expect("the bytes must be a readable PNG");
        let mut buf = vec![0u8; r.output_buffer_size()];
        let info = r.next_frame(&mut buf).expect("one PNG frame");
        buf.truncate(info.buffer_size());
        (info.color_type, info.bit_depth, info.width, info.height, buf)
    }

    /// One pixel of a deliverable, as four samples AT THE FILE'S OWN DEPTH -- an 8-bit file hands
    /// back its bytes widened, a 16-bit one its big-endian pairs assembled. The caller has already
    /// asserted the colour type, so this reader refuses anything but RGBA rather than guessing a
    /// stride. (Round 35's ruled tail, §R.S T3: the row that needed it read the decode only.)
    fn deliverable_rgba_at(bytes: &[u8], i: usize) -> [u16; 4] {
        let (color, depth, _, _, back) = read_png_raw(bytes);
        assert_eq!(color, png::ColorType::Rgba, "this reader wants an RGBA deliverable");
        match depth {
            png::BitDepth::Eight => {
                let o = i * 4;
                [back[o] as u16, back[o + 1] as u16, back[o + 2] as u16, back[o + 3] as u16]
            }
            png::BitDepth::Sixteen => {
                let o = i * 8;
                let s = |k: usize| u16::from_be_bytes([back[o + 2 * k], back[o + 2 * k + 1]]);
                [s(0), s(1), s(2), s(3)]
            }
            other => panic!("a deliverable at depth {other:?} is not one this round can write"),
        }
    }

    /// The ./export deliverable's bytes for a MINTED SOURCE FILE, through the production entry:
    /// `Keep::for_web` -> `decode_full_pixels` -> `rotate_pixels` -> `export_web_file`.
    fn export_file(src: &Path, kind: SrcKind, fmt: WebFormat, long: u32, gamut: Gamut) -> Vec<u8> {
        let shot = shot_for(src, kind);
        let (px, w, h) = decode_full_pixels(&shot, Keep::for_web(fmt)).expect("the fixture decodes");
        let (px, w, h) = rotate_pixels(px, w, h, 0);
        let out = src.with_extension(format!("out.{}", fmt.ext()));
        let spec = WebSpec { long, quality: 80, wm: None, src: gamut, fmt };
        export_web_file(px, w, h, &spec, &out).expect("the fixture exports");
        std::fs::read(&out).expect("read the deliverable")
    }

    /// THE FRINGING WITNESS (Q8): 64x48 RGBA8, an opaque saturated-red block on a fully
    /// transparent field whose RGB is BLACK. A downscale that forgets to premultiply drags that
    /// black into every partly-covered edge pixel; one that premultiplies recovers pure red.
    fn fringing_witness() -> Vec<u8> {
        let (w, h) = (64usize, 48usize);
        let mut v = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let inside = (16..48).contains(&x) && (12..36).contains(&y);
                if inside {
                    v.extend_from_slice(&[255, 0, 0, 255]);
                } else {
                    v.extend_from_slice(&[0, 0, 0, 0]);
                }
            }
        }
        v
    }

    /// The display path's flatten, WRITTEN OUT HERE rather than borrowed from the production
    /// helpers -- R4 is worth nothing if it compares a function with itself. The order is the
    /// shipped decoders': narrow to 8 bits FIRST (`STRIP_16` / `to8`), composite over white second.
    fn flatten_like_the_display_path(px: &Pixels) -> Vec<u8> {
        fn ow(v: u8, a: u8) -> u8 {
            ((v as u16 * a as u16 + 255 * (255 - a as u16)) / 255) as u8
        }
        match px {
            Pixels::Rgb8(v) => v.clone(),
            Pixels::Rgba8(v) => v
                .chunks_exact(4)
                .flat_map(|p| [ow(p[0], p[3]), ow(p[1], p[3]), ow(p[2], p[3])])
                .collect(),
            Pixels::Rgb16(v) => v.iter().map(|&x| (x >> 8) as u8).collect(),
            Pixels::Rgba16(v) => v
                .chunks_exact(4)
                .flat_map(|p| {
                    let a = (p[3] >> 8) as u8;
                    [ow((p[0] >> 8) as u8, a), ow((p[1] >> 8) as u8, a), ow((p[2] >> 8) as u8, a)]
                })
                .collect(),
        }
    }

    // ─────────────────────────────── R1 ───────────────────────────────

    /// **R1 -- AN RGBA8 PNG SURVIVES THE EXPORT PIXEL FOR PIXEL, ALPHA INCLUDED.** The fringing
    /// witness at its own size under the PNG stop: colour type 6, depth 8, and every one of the
    /// 12 288 bytes identical to the file that went in.
    ///
    /// FALSIFIER (L28): make `Keep::for_web` answer `Keep::NONE` for `WebFormat::Png` and the
    /// colour-type assert reddens with `Rgb` on the left; delete the PNG decoder's keep arm and it
    /// reddens the same way.
    #[test]
    fn an_rgba8_png_exports_with_its_transparency_intact() {
        let dir = scratch("r1");
        let src = dir.join("witness.png");
        let data = fringing_witness();
        mint_png(&src, 64, 48, png::ColorType::Rgba, png::BitDepth::Eight, &data);
        let out = export_file(&src, SrcKind::Png, WebFormat::Png, 100_000, Gamut::Srgb);
        let (color, depth, w, h, back) = read_png_raw(&out);
        assert_eq!(color, png::ColorType::Rgba, "the deliverable keeps the alpha channel");
        assert_eq!(depth, png::BitDepth::Eight, "an 8-bit source stays 8-bit -- nothing is up-converted");
        assert_eq!((w, h), (64, 48));
        assert_eq!(back, data, "every byte of the source survives, transparent pixels included");
        // …and the hole really is a hole, so the equality above is not an equality of opacity.
        assert_eq!(back[0..4], [0, 0, 0, 0], "the corner pixel is fully transparent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─────────────────────────────── R2 ───────────────────────────────

    /// **R2 -- AN ALPHA CHANNEL THAT IS FULL EVERYWHERE IS NOT TRANSPARENCY** (sheet 2.2c, 0.1 (b)).
    /// An opaque-RGBA source exports as `ColorType::Rgb`: a third smaller, pixel-identical. This is
    /// the case the testkit's two PNGs are, which is why the R0 golden can hold across this round.
    ///
    /// FALSIFIER (L28): delete the `drop_opaque_alpha` call in `decode_full_pixels` (or make its
    /// scan `any` instead of `all`) and the first assert reddens with `Rgba` on the left.
    #[test]
    fn an_opaque_alpha_channel_is_dropped_and_a_real_one_is_not() {
        let dir = scratch("r2");
        let opaque = dir.join("opaque.png");
        let holed = dir.join("holed.png");
        let mut o = Vec::new();
        let mut t = Vec::new();
        for i in 0..(8 * 8) {
            o.extend_from_slice(&[(i * 3) as u8, (i * 5) as u8, (i * 7) as u8, 255]);
            t.extend_from_slice(&[(i * 3) as u8, (i * 5) as u8, (i * 7) as u8, if i == 9 { 0 } else { 255 }]);
        }
        mint_png(&opaque, 8, 8, png::ColorType::Rgba, png::BitDepth::Eight, &o);
        mint_png(&holed, 8, 8, png::ColorType::Rgba, png::BitDepth::Eight, &t);
        let a = export_file(&opaque, SrcKind::Png, WebFormat::Png, 100_000, Gamut::Srgb);
        let b = export_file(&holed, SrcKind::Png, WebFormat::Png, 100_000, Gamut::Srgb);
        assert_eq!(read_png_raw(&a).0, png::ColorType::Rgb, "no pixel is transparent: the channel goes");
        assert_eq!(read_png_raw(&b).0, png::ColorType::Rgba, "ONE transparent pixel keeps it");
        // The dropped channel really was dropped, not merely relabelled.
        assert_eq!(read_png_raw(&a).4.len(), 8 * 8 * 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─────────────────────────────── R3 ───────────────────────────────

    /// **R3 -- EVERY FORMAT THAT HOLDS ALPHA HANDS IT OVER.** Seven minted sources, one per arm the
    /// round touched that this host can build a fixture for: PNG palette + `tRNS`, PNG grey+alpha,
    /// TIFF RGBA8, TIFF RGBA16, WebP `VP8X`+`ALPH`, GIF with a transparent index, BMP 32-bpp. Each
    /// is decoded at `Keep::ALL` and asked for the alpha of a pixel the row itself made transparent.
    ///
    /// FALSIFIER (L28): remove any one format's keep arm and that format's cell reddens with the
    /// flattened `255` on the left; the assert message names the format. And for the DELIVERABLE
    /// half (round 35's ruled tail, §R.S T3): force every alpha sample opaque in
    /// [`super::write_png`] just after its `(color, depth)` match and ALL SEVEN are named in one
    /// failure -- which is the reach the shipped row did not have, since it read the colour TYPE
    /// of the deliverable and the alpha VALUE of the decode only.
    #[test]
    fn every_format_that_can_carry_alpha_keeps_it() {
        let dir = scratch("r3");
        // The fourth element is THE RGB THE FIXTURE WROTE UNDER ITS TRANSPARENT PIXEL, at the
        // depth that pixel is carried at (16-bit only for the RGBA16 TIFF). It is a literal, not
        // a read of the decode, so the deliverable is compared against the MINT and not against
        // another run of the same pipeline. Every one of them differs from pixel 0's colour, so a
        // stage that wrote the wrong pixel's RGB into the hole cannot pass by symmetry.
        let mut cases: Vec<(&str, PathBuf, SrcKind, [u16; 3])> = Vec::new();

        // (a) PNG palette + tRNS: index 1 is transparent. EXPAND turns it into RGBA8.
        let pal = dir.join("pal.png");
        {
            let file = std::fs::File::create(&pal).unwrap();
            let mut enc = png::Encoder::new(std::io::BufWriter::new(file), 2, 1);
            enc.set_color(png::ColorType::Indexed);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_palette(vec![255, 0, 0, 0, 255, 0]);
            enc.set_trns(vec![255, 0]);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[0, 1]).unwrap();
            w.finish().unwrap();
        }
        cases.push(("PNG palette+tRNS", pal, SrcKind::Png, [0, 255, 0]));

        // (b) PNG grey+alpha.
        let ga = dir.join("ga.png");
        mint_png(&ga, 2, 1, png::ColorType::GrayscaleAlpha, png::BitDepth::Eight, &[200, 255, 111, 0]);
        cases.push(("PNG grey+alpha", ga, SrcKind::Png, [111, 111, 111]));

        // (c) + (d) TIFF RGBA8 and RGBA16, through the pure-Rust `tiff` encoder.
        let t8 = dir.join("rgba8.tiff");
        {
            let f = std::fs::File::create(&t8).unwrap();
            let mut e = tiff::encoder::TiffEncoder::new(f).unwrap();
            e.write_image::<tiff::encoder::colortype::RGBA8>(2, 1, &[255u8, 0, 0, 255, 9, 9, 9, 0]).unwrap();
        }
        cases.push(("TIFF RGBA8", t8, SrcKind::Tiff, [9, 9, 9]));
        let t16 = dir.join("rgba16.tiff");
        {
            let f = std::fs::File::create(&t16).unwrap();
            let mut e = tiff::encoder::TiffEncoder::new(f).unwrap();
            e.write_image::<tiff::encoder::colortype::RGBA16>(
                2,
                1,
                &[65535u16, 0, 0, 65535, 999, 999, 999, 0],
            )
            .unwrap();
        }
        cases.push(("TIFF RGBA16", t16, SrcKind::Tiff, [999, 999, 999]));

        // (e) WebP VP8X + ALPH, through the lossless encoder in the tree.
        let wp = dir.join("alpha.webp");
        {
            let f = std::fs::File::create(&wp).unwrap();
            image_webp::WebPEncoder::new(std::io::BufWriter::new(f))
                .encode(&[255u8, 0, 0, 255, 9, 9, 9, 0], 2, 1, image_webp::ColorType::Rgba8)
                .unwrap();
        }
        cases.push(("WebP ALPH", wp, SrcKind::Webp, [9, 9, 9]));

        // (f) GIF with a transparent palette index.
        let gf = dir.join("t.gif");
        {
            let mut f = std::fs::File::create(&gf).unwrap();
            let palette = [255u8, 0, 0, 0, 0, 0];
            let mut e = gif::Encoder::new(&mut f, 2, 1, &palette).unwrap();
            let mut fr = gif::Frame { width: 2, height: 1, ..Default::default() };
            fr.buffer = std::borrow::Cow::Owned(vec![0u8, 1]);
            fr.transparent = Some(1);
            e.write_frame(&fr).unwrap();
        }
        cases.push(("GIF transparent index", gf, SrcKind::Gif, [0, 0, 0]));

        // (g) BMP 32-bpp.
        let bm = dir.join("a.bmp");
        {
            let mut buf: Vec<u8> = Vec::new();
            image::codecs::bmp::BmpEncoder::new(&mut buf)
                .encode(&[255u8, 0, 0, 255, 9, 9, 9, 0], 2, 1, image::ExtendedColorType::Rgba8)
                .unwrap();
            std::fs::write(&bm, &buf).unwrap();
        }
        cases.push(("BMP 32-bpp", bm, SrcKind::Bmp, [9, 9, 9]));

        // The two deliverable asserts COLLECT rather than panic in place: a `Vec` failure names
        // every format that broke, where an assert inside the loop names only the first. The
        // falsifier below turns all seven, and the row is worth the difference.
        let mut wrong: Vec<String> = Vec::new();
        for (what, path, kind, under) in &cases {
            let shot = shot_for(path, *kind);
            let (px, _, _) = decode_full_pixels(&shot, Keep::ALL).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert!(px.has_alpha(), "{what}: the keep arm must hand back an alpha channel");
            let a = match &px {
                Pixels::Rgba8(v) => v[7] as u32,
                Pixels::Rgba16(v) => v[7] as u32,
                other => panic!("{what}: expected an alpha layout, got {}", other.layout()),
            };
            assert_eq!(a, 0, "{what}: the pixel this row made transparent is still transparent");
            // …and the deliverable really carries it. The colour type is the CHANNEL's survival;
            // the two asserts under it are the VALUE's, which is what §3 promised ("read back with
            // alpha intact at the witness pixel") and what the shipped row never read for any
            // format but PNG RGBA8 (round 35's ruled tail, §R.S PIXELS Y2).
            let out = export_file(path, *kind, WebFormat::Png, 100_000, Gamut::Srgb);
            assert_eq!(read_png_raw(&out).0, png::ColorType::Rgba, "{what}: the PNG keeps the channel");
            let px1 = deliverable_rgba_at(&out, 1);
            if px1[3] != 0 || px1[0..3] != under[..] {
                wrong.push(format!(
                    "{what}: the deliverable's witness pixel is {px1:?}, not [{}, {}, {}, 0]",
                    under[0], under[1], under[2]
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of the seven deliverables lost the alpha VALUE or the colour under it:\n  {}",
            wrong.len(),
            wrong.join("\n  ")
        );
        assert_eq!(cases.len(), 7, "seven formats, and the count is re-counted against the list above");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─────────────────────────────── R4 ───────────────────────────────

    /// **R4 -- THE DISPLAY PATH IS THE FLATTENED KEEP PATH, ON EVERY FIXTURE.** For each minted
    /// source, `decode_full_rgb` (what the stage, the thumbnails, the fast tier and the JPG export
    /// all take) must equal this row's OWN flatten of `decode_full_pixels(.., Keep::ALL)` -- an
    /// arithmetic written out above, not a call into the production helpers.
    ///
    /// This is the row-shaped half of Q1; the byte-shaped half is R0's four hashes.
    ///
    /// FALSIFIER (L28): change any decoder's flattening statement -- `over_white`'s constant, the
    /// `>> 8` in `to8`, the PNG `STRIP_16` transformation -- and the cell for that format reddens
    /// on the first differing byte.
    #[test]
    fn the_display_path_is_the_kept_pixels_flattened() {
        let dir = scratch("r4");
        let mut cases: Vec<(&str, PathBuf, SrcKind)> = Vec::new();

        let p8 = dir.join("rgba8.png");
        mint_png(&p8, 64, 48, png::ColorType::Rgba, png::BitDepth::Eight, &fringing_witness());
        cases.push(("PNG RGBA8", p8, SrcKind::Png));

        let p16 = dir.join("rgb16.png");
        let ramp: Vec<u16> = (0..(8 * 8)).flat_map(|i| [i * 900, 65535 - i * 900, 12345]).collect();
        mint_png(&p16, 8, 8, png::ColorType::Rgb, png::BitDepth::Sixteen, &be(&ramp));
        cases.push(("PNG RGB16", p16, SrcKind::Png));

        let pa16 = dir.join("rgba16.png");
        let deep: Vec<u16> =
            (0..(8 * 8)).flat_map(|i| [i * 900, 65535 - i * 900, 12345, if i % 3 == 0 { 0 } else { 65535 }]).collect();
        mint_png(&pa16, 8, 8, png::ColorType::Rgba, png::BitDepth::Sixteen, &be(&deep));
        cases.push(("PNG RGBA16", pa16, SrcKind::Png));

        let ga = dir.join("ga.png");
        mint_png(&ga, 2, 1, png::ColorType::GrayscaleAlpha, png::BitDepth::Eight, &[200, 255, 200, 0]);
        cases.push(("PNG grey+alpha", ga, SrcKind::Png));

        let t16 = dir.join("gray16.tiff");
        {
            let f = std::fs::File::create(&t16).unwrap();
            let mut e = tiff::encoder::TiffEncoder::new(f).unwrap();
            e.write_image::<tiff::encoder::colortype::Gray16>(2, 1, &[65535u16, 300]).unwrap();
        }
        cases.push(("TIFF Gray16", t16, SrcKind::Tiff));

        let ta8 = dir.join("rgba8.tiff");
        {
            let f = std::fs::File::create(&ta8).unwrap();
            let mut e = tiff::encoder::TiffEncoder::new(f).unwrap();
            e.write_image::<tiff::encoder::colortype::RGBA8>(2, 1, &[255u8, 0, 0, 128, 9, 9, 9, 0]).unwrap();
        }
        cases.push(("TIFF RGBA8", ta8, SrcKind::Tiff));

        let wp = dir.join("a.webp");
        {
            let f = std::fs::File::create(&wp).unwrap();
            image_webp::WebPEncoder::new(std::io::BufWriter::new(f))
                .encode(&[255u8, 0, 0, 128, 9, 9, 9, 0], 2, 1, image_webp::ColorType::Rgba8)
                .unwrap();
        }
        cases.push(("WebP ALPH", wp, SrcKind::Webp));

        let bm = dir.join("a.bmp");
        {
            let mut buf: Vec<u8> = Vec::new();
            image::codecs::bmp::BmpEncoder::new(&mut buf)
                .encode(&[255u8, 0, 0, 128, 9, 9, 9, 0], 2, 1, image::ExtendedColorType::Rgba8)
                .unwrap();
            std::fs::write(&bm, &buf).unwrap();
        }
        cases.push(("BMP 32-bpp", bm, SrcKind::Bmp));

        for (what, path, kind) in &cases {
            let shot = shot_for(path, *kind);
            let shipped = decode_full_rgb(&shot).unwrap_or_else(|e| panic!("{what}: {e}"));
            let (kept, kw, kh) = decode_full_pixels(&shot, Keep::ALL).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!((shipped.1, shipped.2), (kw, kh), "{what}: the two paths agree on the size");
            assert_eq!(
                shipped.0,
                flatten_like_the_display_path(&kept),
                "{what}: the display path is the kept pixels, flattened"
            );
        }
        assert_eq!(cases.len(), 8, "eight fixtures, re-counted against the list above");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─────────────────────────────── R5 ───────────────────────────────

    /// **R5 -- NO DARK FRINGE.** The witness downscaled 4x: every partly-covered pixel must still be
    /// RED, because `ResizeOptions::new()` premultiplies by alpha before the Lanczos and divides
    /// after. The second half is the DIFFERENTIAL (L28's sub-clause): the identical resize with
    /// `mul_div_alpha` turned off really does drag the transparent field's black into those pixels,
    /// so "no fringe" is a measurement and not a property of the fixture.
    ///
    /// FALSIFIER (L28): set `mul_div_alpha = false` on the options in `resize_pixels_to_long` and
    /// the first loop reddens with a dark green/blue pair on a pixel whose alpha is between.
    #[test]
    fn a_hard_edge_downscales_without_a_dark_halo() {
        let src = fringing_witness();
        let (small, sw, sh) = resize_pixels_to_long(Pixels::Rgba8(src.clone()), 64, 48, 16).unwrap();
        let Pixels::Rgba8(small) = small else { panic!("an RGBA8 resize stays RGBA8") };
        assert_eq!((sw, sh), (16, 12));
        let mut edges = 0;
        for p in small.chunks_exact(4) {
            if p[3] == 0 || p[3] == 255 {
                continue;
            }
            edges += 1;
            assert!(
                p[0] >= 253 && p[1] <= 2 && p[2] <= 2,
                "a partly-covered pixel must still be red, not the field's black: {p:?}"
            );
        }
        assert!(edges >= 20, "ANTI-VACUITY: the downscale really did make partly-covered pixels ({edges})");

        // The DIFFERENTIAL: the same convolution with the premultiply off.
        let mut straight_opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
        straight_opts.mul_div_alpha = false;
        let s = Image::from_vec_u8(64, 48, src, PixelType::U8x4).unwrap();
        let mut d = Image::new(16, 12, PixelType::U8x4);
        Resizer::new().resize(&s, &mut d, &straight_opts).unwrap();
        let straight = d.into_vec();
        let dark = straight
            .chunks_exact(4)
            .filter(|p| p[3] > 0 && p[3] < 255 && p[0] < 200)
            .count();
        assert!(dark > 0, "ANTI-VACUITY: without the premultiply the halo really is there ({dark} px)");
    }

    // ─────────────────────────────── R6 ───────────────────────────────

    /// **R6 -- THE CONVERSION MOVES COLOUR AND LEAVES ALPHA ALONE, AT BOTH DEPTHS.** A wide-gamut
    /// source under the PNG stop: the RGB triples change, every alpha sample is the one that went
    /// in. (falcon-color pins the same contract at its own door; this row pins the EXPORT, which is
    /// the thing that could have routed a four-channel buffer through a three-channel entry.)
    ///
    /// FALSIFIER (L28): route the `Rgba8` arm of `transform_pixels` to `transform_rgb` and the
    /// 8-bit alpha assert reddens; route the `Rgba16` arm to `transform_rgb16` and the 16-bit one
    /// does.
    #[test]
    fn the_srgb_conversion_never_touches_the_alpha_plane() {
        let dir = scratch("r6");
        let alphas8: Vec<u8> = vec![0, 1, 128, 255];
        let mut d8 = Vec::new();
        for (i, a) in alphas8.iter().enumerate() {
            d8.extend_from_slice(&[250, 20, 40 + i as u8, *a]);
        }
        let p8 = dir.join("p3.png");
        mint_png(&p8, 4, 1, png::ColorType::Rgba, png::BitDepth::Eight, &d8);
        let out8 = export_file(&p8, SrcKind::Png, WebFormat::Png, 100_000, Gamut::DisplayP3);
        let (c8, _, _, _, back8) = read_png_raw(&out8);
        assert_eq!(c8, png::ColorType::Rgba);
        for (i, a) in alphas8.iter().enumerate() {
            assert_eq!(back8[i * 4 + 3], *a, "8-bit alpha {i} is untouched by the convert");
        }
        assert_ne!(&back8[0..3], &d8[0..3], "ANTI-VACUITY: the P3 colour really was converted");

        let alphas16: Vec<u16> = vec![0, 1, 30_000, 65_535];
        let mut d16: Vec<u16> = Vec::new();
        for (i, a) in alphas16.iter().enumerate() {
            d16.extend_from_slice(&[64_000, 5_000, 10_000 + i as u16, *a]);
        }
        let p16 = dir.join("p3_16.png");
        mint_png(&p16, 4, 1, png::ColorType::Rgba, png::BitDepth::Sixteen, &be(&d16));
        let out16 = export_file(&p16, SrcKind::Png, WebFormat::Png, 100_000, Gamut::DisplayP3);
        let (c16, dep16, _, _, back16) = read_png_raw(&out16);
        assert_eq!((c16, dep16), (png::ColorType::Rgba, png::BitDepth::Sixteen));
        for (i, a) in alphas16.iter().enumerate() {
            let got = u16::from_be_bytes([back16[i * 8 + 6], back16[i * 8 + 7]]);
            assert_eq!(got, *a, "16-bit alpha {i} is untouched by the convert");
        }
        let first = u16::from_be_bytes([back16[0], back16[1]]);
        assert_ne!(first, 64_000, "ANTI-VACUITY: the 16-bit P3 colour really was converted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─────────────────────────────── R7 ───────────────────────────────

    /// A solid opaque white logo, `side` x `side`, as the export's own [`Watermark`].
    fn white_logo(side: u32, scale: f32, opacity: f32) -> Watermark {
        Watermark {
            rgba: vec![255u8; (side * side * 4) as usize],
            w: side,
            h: side,
            scale,
            pos_x: 0.5,
            pos_y: 0.5,
            opacity,
        }
    }

    /// **R7 -- THE MARK IS VISIBLE OVER A TRANSPARENT REGION** (Q5; the owner's veto list names the
    /// alternative). Stamped onto a fully transparent RGBA8 canvas: inside the mark's rect the
    /// output alpha is `logo_a x opacity` to within one count, outside it is still zero, and the
    /// colour inside is the mark's own -- which straight source-over gives and the shipped
    /// opaque-destination formula cannot.
    ///
    /// FALSIFIER (L28): drop the `dst[di + 3] = ...` write in `stamp_over`'s four-channel arm and
    /// the "inside" assert reddens with `0` on the left -- an invisible watermark, which is what
    /// the shipped arithmetic would have produced.
    #[test]
    fn the_mark_is_visible_over_a_fully_transparent_region() {
        let (w, h) = (64u32, 64u32);
        let mut px = Pixels::Rgba8(vec![0u8; (w * h * 4) as usize]);
        let wm = white_logo(32, 0.25, 0.5);
        let (ox, oy, lw, lh) = watermark_rect(w, h, &wm);
        assert_eq!((lw, lh), (16, 16), "the rect math is the shared one, and it sized the mark");
        stamp_watermark_pixels(&mut px, w, h, &wm).expect("stamp");
        let Pixels::Rgba8(v) = px else { panic!("RGBA8 in, RGBA8 out") };
        let at = |x: i64, y: i64| -> [u8; 4] {
            let i = ((y as usize * w as usize) + x as usize) * 4;
            [v[i], v[i + 1], v[i + 2], v[i + 3]]
        };
        let inside = at(ox + 8, oy + 8);
        assert!(
            inside[3].abs_diff(128) <= 1,
            "inside the mark a_out == logo_a x opacity: {inside:?}"
        );
        assert_eq!(&inside[0..3], &[255, 255, 255], "…and the colour is the mark's own, undiluted");
        assert_eq!(at(0, 0), [0, 0, 0, 0], "outside the rect nothing was written");
    }

    /// **R7 (b) -- THE WIDE STAMP IS THE SHIPPED ARITHMETIC OVER RGB8.** The generic body and the
    /// shipped `stamp_watermark` must produce byte-identical buffers on the same opaque 8-bit
    /// input. This is what lets `stamp_watermark_pixels` keep routing `Rgb8` to the shipped
    /// function without the two silently drifting: the shipped one is what the JPG deliverable and
    /// every pre-round PNG deliverable were stamped by.
    ///
    /// FALSIFIER (L28): change `stamp_over`'s three-channel branch to the four-channel formula (or
    /// swap `c_d` and `c_s`) and the byte equality reddens.
    #[test]
    fn the_wide_stamp_is_the_shipped_arithmetic_over_rgb8() {
        let (w, h) = (48u32, 32u32);
        let base: Vec<u8> = (0..(w * h * 3)).map(|i| (i % 251) as u8).collect();
        let wm = white_logo(24, 0.3, 0.62);
        let mut shipped = base.clone();
        stamp_watermark(&mut shipped, w, h, &wm).expect("shipped stamp");
        let mut wide = Pixels::Rgb8(base.clone());
        stamp_watermark_wide(&mut wide, w, h, &wm).expect("wide stamp");
        let Pixels::Rgb8(wide) = wide else { panic!("RGB8 in, RGB8 out") };
        assert_eq!(wide, shipped, "the wide body IS the shipped arithmetic at FULL == 255");
        assert_ne!(wide, base, "ANTI-VACUITY: the mark really was stamped");
    }

    // ─────────────────────────────── R8 ───────────────────────────────

    /// **R8 -- 16 BITS END TO END, BIG-ENDIAN.** A 4 096-pixel green ramp with 4 096 DISTINCT green
    /// values exports at `BitDepth::Sixteen` and reads back with more than 256 of them -- which is
    /// the whole claim, since an 8-bit round trip can carry 256 at most. And the trap: PNG is
    /// network byte order, so a known sample's two bytes are asserted in the order the file must
    /// carry them.
    ///
    /// FALSIFIER (L28): make `Keep::for_web` answer `alpha: true, depth: false` and the distinct
    /// count reddens at 256; delete the swap in `be16_bytes_in_place` and the byte-order assert
    /// reddens with `0x34` on the left.
    #[test]
    fn a_16_bit_source_exports_at_16_bits_in_network_byte_order() {
        let dir = scratch("r8");
        let src = dir.join("ramp.png");
        let (w, h) = (64u32, 64u32);
        let mut data: Vec<u16> = Vec::with_capacity((w * h * 3) as usize);
        for i in 0..(w * h) {
            data.extend_from_slice(&[1000, (i * 16) as u16, 2000]);
        }
        data[1] = 0x1234; // the byte-order probe, in the first pixel's green
        mint_png(&src, w, h, png::ColorType::Rgb, png::BitDepth::Sixteen, &be(&data));
        let out = export_file(&src, SrcKind::Png, WebFormat::Png, 100_000, Gamut::Srgb);
        let (color, depth, ow, oh, back) = read_png_raw(&out);
        assert_eq!((color, depth), (png::ColorType::Rgb, png::BitDepth::Sixteen));
        assert_eq!((ow, oh), (w, h));
        let greens: std::collections::BTreeSet<u16> = back
            .chunks_exact(6)
            .map(|p| u16::from_be_bytes([p[2], p[3]]))
            .collect();
        assert!(
            greens.len() > 256,
            "a 16-bit ramp must survive as more than 8 bits could carry: {} distinct greens",
            greens.len()
        );
        assert_eq!(greens.len(), 4096, "…and in fact all 4 096 of them");
        assert_eq!(
            (back[2], back[3]),
            (0x12, 0x34),
            "PNG is network byte order: the high byte is written first"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **R8 (b) -- THE SIXTEEN BITS SURVIVE THE sRGB CONVERSION TOO.** R8 above exports its ramp
    /// under `Gamut::Srgb`, where `export_color_action` answers `TagSrgbOnly` and
    /// `transform_pixels` is never called -- so the ONE stage that could quietly narrow a 16-bit
    /// buffer to eight bits and widen it back is not in R8's path at all. This row exports the
    /// same 4 096-value ramp with a WIDE-GAMUT source tag, which makes the action `ConvertToSrgb`
    /// and puts `falcon_color::transform_rgb16` -- and so `apply_pixel16` -- between the decode
    /// and the encoder.
    ///
    /// Two asserts, because a quantiser fails them in two different ways: MORE THAN 256 distinct
    /// greens survive (an 8-bit round trip can carry 256 at most), and they are NOT ALL MULTIPLES
    /// OF 257 -- 257 being the only factor an 8-bit sample re-widened to 16 bits can have, since
    /// `v * 65535 / 255 == v * 257`.
    ///
    /// FALSIFIER (L28), and it is the mutation that found this row missing (round 35's ruled
    /// tail, §R.S PIXELS Y1): quantise `apply_pixel16` in `falcon-color/src/lib.rs` --
    /// `((v * 255.0 + 0.5).clamp(0.0, 255.0) as u16) * 257` in place of
    /// `(v * 65535.0 + 0.5).clamp(0.0, 65535.0) as u16` -- and both asserts redden HERE, where
    /// every other row in this crate and every row in falcon-color stays green.
    #[test]
    fn the_16_bit_ramp_survives_the_srgb_conversion() {
        let dir = scratch("r8b");
        let src = dir.join("ramp_p3.png");
        let (w, h) = (64u32, 64u32);
        let mut data: Vec<u16> = Vec::with_capacity((w * h * 3) as usize);
        for i in 0..(w * h) {
            data.extend_from_slice(&[1000, (i * 16) as u16, 2000]);
        }
        mint_png(&src, w, h, png::ColorType::Rgb, png::BitDepth::Sixteen, &be(&data));
        // ANTI-VACUITY, before the export: the P3 tag really is what routes the buffer through the
        // 16-bit transform. Under `Gamut::Srgb` this answers `TagSrgbOnly` and the row proves nothing.
        assert_eq!(
            export_color_action(ExportTarget::Web, Gamut::DisplayP3),
            ExportColorAction::ConvertToSrgb,
            "the wide-gamut tag is what puts transform_rgb16 in the path"
        );
        let out = export_file(&src, SrcKind::Png, WebFormat::Png, 100_000, Gamut::DisplayP3);
        let (color, depth, ow, oh, back) = read_png_raw(&out);
        assert_eq!((color, depth), (png::ColorType::Rgb, png::BitDepth::Sixteen));
        assert_eq!((ow, oh), (w, h));
        let greens: std::collections::BTreeSet<u16> =
            back.chunks_exact(6).map(|p| u16::from_be_bytes([p[2], p[3]])).collect();
        assert!(
            greens.len() > 256,
            "the convert must not narrow to eight bits: {} distinct greens after ConvertToSrgb",
            greens.len()
        );
        assert!(
            greens.iter().any(|g| g % 257 != 0),
            "…and they are 16-bit values, not 8-bit samples re-widened by *257 ({} distinct)",
            greens.len()
        );
        // The colour really moved, so the two asserts above are about a CONVERTED ramp.
        assert_ne!(
            u16::from_be_bytes([back[0], back[1]]),
            1000,
            "ANTI-VACUITY: the P3 red really was converted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─────────────────────────────── R9 ───────────────────────────────

    /// **R9 -- THE EXPORT HOLDS ONE FRAME, NOT THREE.** Measured, in bytes, by the thread-local
    /// meter at the top of this file (see `alloc_probe`), on a Full-size RGBA16 export -- the
    /// widest deliverable the round can produce.
    ///
    /// The bound is stated IN FRAMES for the ramp this row exports (a frame is `w x h x 8` bytes at
    /// RGBA16) -- NOT because the peak is a multiple of the frame. It is one pixel frame plus one
    /// compressed deliverable, and the second term tracks the FILE: the round's verifier measured this
    /// shape at 1.456 frames on the ramp, 1.908 on photographic 16-bit content and 4.094 on incompressible
    /// noise, where `png`'s stored-only fallback fires and this row's own `< 2` bound would be FALSE.
    /// The ramp is the row's fixture; the other two contents are the record's (round 35 §R.V). One frame plus the
    /// encoder's own working set is what the round's shape costs; TWO frames is what any
    /// re-introduced copy costs, so the bound sits between them. At the parent commit the shape
    /// held the decode's buffer, `rotate_rgb`'s unconditional `to_vec`, `export_web_image`'s
    /// `rgb.to_vec()` AND the whole encoded file at once; the second half of this row rebuilds that
    /// shape and measures it, so the bound is a comparison and not a hopeful number.
    ///
    /// The fixture is ~1 MP rather than the 24 MP of the round's spec because this row runs under
    /// `cargo test`'s DEBUG profile, where a 24 MP deflate is minutes; the per-pixel bound carries
    /// to any size, and the round record does the multiplication.
    ///
    /// FALSIFIER (L28): restore `rgb.to_vec()` at the head of `export_web_to` (or make
    /// `rotate_pixels` copy at `turns == 0`) and the `held` assert reddens with a figure one frame
    /// higher; make the PNG arm build a `Vec<u8>` and hand it to the sink and it reddens further.
    #[test]
    fn a_full_size_16_bit_export_holds_about_one_frame() {
        let dir = scratch("r9");
        let out = dir.join("wide.png");
        let (w, h) = (1200u32, 800u32);
        let n = (w * h) as usize;
        let frame = n * 8; // RGBA16
        let make = || -> Pixels {
            let mut v: Vec<u16> = Vec::with_capacity(n * 4);
            for i in 0..n {
                let g = (i % 65536) as u16;
                v.extend_from_slice(&[g, g.wrapping_mul(7), g.wrapping_mul(13), 60_000]);
            }
            Pixels::Rgba16(v)
        };

        // BOTH measurements build their own source INSIDE the meter, so both figures are whole
        // peaks rather than deltas over a buffer someone else was already holding.
        let (_, held) = alloc_probe::measure(|| {
            let (px, w2, h2) = rotate_pixels(make(), w, h, 0);
            let spec =
                WebSpec { long: 100_000, quality: 80, wm: None, src: Gamut::Srgb, fmt: WebFormat::Png };
            export_web_file(px, w2, h2, &spec, &out).unwrap()
        });
        // The shape at the PARENT commit, rebuilt: the decode's buffer, the rotate copy, the resize
        // copy, and the encoded file held whole.
        let (_, parent) = alloc_probe::measure(|| {
            let src = make();
            let rotate_copy = match &src {
                Pixels::Rgba16(v) => v.clone(),
                other => panic!("layout changed: {}", other.layout()),
            };
            let resize_copy = rotate_copy.clone();
            let mut bytes: Vec<u8> = Vec::new();
            write_png(Pixels::Rgba16(resize_copy), w, h, &mut bytes).unwrap();
            (src.byte_len(), rotate_copy.len(), bytes.len())
        });
        eprintln!(
            "R9: frame {frame} B; export peak {held} B ({:.2} frames); parent shape {parent} B ({:.2} frames)",
            held as f64 / frame as f64,
            parent as f64 / frame as f64
        );
        assert!(
            held < frame * 2,
            "the export must hold ONE frame plus the encoder's own working set, never two ({frame} B \
             a frame): measured {held} B ({:.2} frames)",
            held as f64 / frame as f64
        );
        assert!(
            parent > frame * 3,
            "ANTI-VACUITY: the parent's copy-per-stage shape really is several frames ({parent} B, \
             {:.2} frames)",
            parent as f64 / frame as f64
        );
        assert!(
            std::fs::metadata(&out).unwrap().len() > 0,
            "…and the deliverable was actually written, so the measurement covers the encode"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **R9 (b) -- AT FULL SIZE NOTHING IS COPIED AT ALL.** Pointer identity, which is the only
    /// assertion a re-introduced `to_vec()` cannot survive.
    ///
    /// FALSIFIER (L28): make `rotate_pixels` return `src.to_vec()` at `turns == 0`, or restore
    /// `resize_to_long`'s old `rgb.to_vec()` caller, and the matching assert reddens.
    #[test]
    fn the_full_tier_hands_the_same_allocation_through() {
        let v = vec![7u8; 12 * 8 * 4];
        let addr = v.as_ptr();
        let (px, w, h) = rotate_pixels(Pixels::Rgba8(v), 12, 8, 0);
        assert_eq!((w, h), (12, 8));
        match &px {
            Pixels::Rgba8(o) => assert_eq!(o.as_ptr(), addr, "turns 0 hands the same allocation back"),
            other => panic!("layout changed: {}", other.layout()),
        }
        let (px, _, _) = resize_pixels_to_long(px, 12, 8, 100_000).expect("Full is a no-op");
        match &px {
            Pixels::Rgba8(o) => assert_eq!(o.as_ptr(), addr, "the Full tier's early return keeps it"),
            other => panic!("layout changed: {}", other.layout()),
        }
        // …and a turn really does move the pixels, so the identity above is not a broken rotate.
        let (turned, tw, th) = rotate_pixels(px, 12, 8, 1);
        assert_eq!((tw, th), (8, 12), "ANTI-VACUITY: a real turn transposes the frame");
        assert!(matches!(turned, Pixels::Rgba8(_)));
    }

    // ─────────────────────────────── R10 ───────────────────────────────

    /// **R10 -- THE HEADER IS THE SAME HEADER AT ALL FOUR CELLS.** The `sRGB` chunk, the substitute
    /// `gAMA`/`cHRM` and `Compression::Fast` ride on RGBA8, RGB16 and RGBA16 exactly as they ride on
    /// the RGB8 the round inherited -- byte equality against a header rebuilt by hand, per cell,
    /// which is the discipline `the_png_compression_level_is_the_one_the_doc_names` established for
    /// the one cell that existed before.
    ///
    /// FALSIFIER (L28): drop `set_source_srgb` (or either substitute) from `write_png` and all four
    /// cells redden on byte equality; change one scaled integer and the cells redden while the
    /// chunk-presence asserts stay green, which is why both are here.
    #[test]
    fn every_colour_type_and_depth_carries_the_same_chunk_set() {
        let (w, h) = (9u32, 7u32);
        let n = (w * h) as usize;
        let cells: Vec<(Pixels, png::ColorType, png::BitDepth)> = vec![
            (
                Pixels::Rgb8((0..n * 3).map(|i| (i % 251) as u8).collect()),
                png::ColorType::Rgb,
                png::BitDepth::Eight,
            ),
            (
                Pixels::Rgba8((0..n * 4).map(|i| (i % 251) as u8).collect()),
                png::ColorType::Rgba,
                png::BitDepth::Eight,
            ),
            (
                Pixels::Rgb16((0..n * 3).map(|i| (i * 613 % 65_521) as u16).collect()),
                png::ColorType::Rgb,
                png::BitDepth::Sixteen,
            ),
            (
                Pixels::Rgba16((0..n * 4).map(|i| (i * 613 % 65_521) as u16).collect()),
                png::ColorType::Rgba,
                png::BitDepth::Sixteen,
            ),
        ];
        for (px, color, depth) in cells {
            let layout = px.layout();
            let raw: Vec<u8> = match &px {
                Pixels::Rgb8(v) | Pixels::Rgba8(v) => v.clone(),
                Pixels::Rgb16(v) | Pixels::Rgba16(v) => v.iter().flat_map(|s| s.to_be_bytes()).collect(),
            };
            let mut shipped: Vec<u8> = Vec::new();
            write_png(px, w, h, &mut shipped).unwrap_or_else(|e| panic!("{layout}: {e}"));
            // The same chunk set, rebuilt here by hand.
            let mut hand: Vec<u8> = Vec::new();
            {
                let mut enc = png::Encoder::new(&mut hand, w, h);
                enc.set_color(color);
                enc.set_depth(depth);
                enc.set_compression(png::Compression::Fast);
                enc.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
                enc.set_source_gamma(png::ScaledFloat::from_scaled(45455));
                enc.set_source_chromaticities(png::SourceChromaticities {
                    white: (png::ScaledFloat::from_scaled(31270), png::ScaledFloat::from_scaled(32900)),
                    red: (png::ScaledFloat::from_scaled(64000), png::ScaledFloat::from_scaled(33000)),
                    green: (png::ScaledFloat::from_scaled(30000), png::ScaledFloat::from_scaled(60000)),
                    blue: (png::ScaledFloat::from_scaled(15000), png::ScaledFloat::from_scaled(6000)),
                });
                let mut wtr = enc.write_header().unwrap();
                wtr.write_image_data(&raw).unwrap();
                wtr.finish().unwrap();
            }
            assert_eq!(shipped.len(), hand.len(), "{layout}: same length");
            assert!(shipped == hand, "{layout}: byte for byte the same deliverable");
            let dec = png::Decoder::new(std::io::Cursor::new(shipped.clone()));
            let r = dec.read_info().expect("re-read");
            let i = r.info();
            assert_eq!(i.color_type, color, "{layout}: colour type");
            assert_eq!(i.bit_depth, depth, "{layout}: bit depth");
            assert_eq!(
                i.srgb,
                Some(png::SrgbRenderingIntent::Perceptual),
                "{layout}: the sRGB chunk rides every cell"
            );
            assert!(i.source_gamma.is_some(), "{layout}: the substitute gAMA");
            assert!(i.source_chromaticities.is_some(), "{layout}: the substitute cHRM");
            assert!(i.icc_profile.is_none(), "{layout}: no iCCP beside the sRGB chunk");
        }
    }

    /// **R10 (b) -- THE LENGTH GUARD IS `w x h x bpp`, NOT `w x h x 3`.** A buffer of the wrong
    /// width for its own layout is refused rather than written into a corrupt file, at all four.
    ///
    /// FALSIFIER (L28): put the `* 3` back in `write_png`'s `want` and the RGBA8 cell stops
    /// erroring (it is exactly 4/3 of the RGB8 length), so the assert reddens.
    #[test]
    fn the_png_length_guard_counts_the_layout_it_was_given() {
        let (w, h) = (5u32, 4u32);
        let n = (w * h) as usize;
        let mut sink: Vec<u8> = Vec::new();
        for px in [
            Pixels::Rgb8(vec![0u8; n * 3 - 1]),
            Pixels::Rgba8(vec![0u8; n * 4 - 1]),
            Pixels::Rgb16(vec![0u16; n * 3 - 1]),
            Pixels::Rgba16(vec![0u16; n * 4 - 1]),
        ] {
            let layout = px.layout();
            assert!(write_png(px, w, h, &mut sink).is_err(), "{layout}: a short buffer is refused");
        }
        for px in [
            Pixels::Rgb8(vec![0u8; n * 3]),
            Pixels::Rgba8(vec![0u8; n * 4]),
            Pixels::Rgb16(vec![0u16; n * 3]),
            Pixels::Rgba16(vec![0u16; n * 4]),
        ] {
            let layout = px.layout();
            let mut ok: Vec<u8> = Vec::new();
            assert!(write_png(px, w, h, &mut ok).is_ok(), "{layout}: the right length is accepted");
        }
    }

    // ─────────────────────────────── the riders ───────────────────────────────

    /// **Q9 -- THE WATERMARK LOGO LOADER NORMALISES LIKE THE PHOTO DECODER.** A 16-bit RGBA logo
    /// comes back as `w*h*4` BYTES (it used to come back as `w*h*8` bytes LABELLED RGBA8, which
    /// `stamp_watermark`'s guard happily accepted), and a palette logo is expanded instead of
    /// refused.
    ///
    /// FALSIFIER (L28): delete the `set_transformations` line in `load_png_rgba_tag` and the
    /// 16-bit length assert reddens with 512 on the left; drop `EXPAND` from it and the palette
    /// case goes back to `Err`.
    #[test]
    fn the_watermark_loader_expands_a_palette_and_narrows_a_16_bit_logo() {
        let dir = scratch("q9");
        let deep = dir.join("logo16.png");
        let mut d: Vec<u16> = Vec::new();
        for i in 0..(8 * 8) {
            d.extend_from_slice(&[65_535, i * 900, 0, 65_535]);
        }
        mint_png(&deep, 8, 8, png::ColorType::Rgba, png::BitDepth::Sixteen, &be(&d));
        let (rgba, w, h) = load_png_rgba(&deep).expect("a 16-bit RGBA logo loads");
        assert_eq!((w, h), (8, 8));
        assert_eq!(rgba.len(), 8 * 8 * 4, "RGBA8 means four BYTES a pixel, not four samples");
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255], "…and the high bytes are the colour");

        let pal = dir.join("logo_pal.png");
        {
            let file = std::fs::File::create(&pal).unwrap();
            let mut enc = png::Encoder::new(std::io::BufWriter::new(file), 2, 1);
            enc.set_color(png::ColorType::Indexed);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_palette(vec![0, 0, 255, 255, 255, 0]);
            let mut wtr = enc.write_header().unwrap();
            wtr.write_image_data(&[0, 1]).unwrap();
            wtr.finish().unwrap();
        }
        let (prgba, pw, ph) = load_png_rgba(&pal).expect("a palette logo is expanded, not refused");
        assert_eq!((pw, ph), (2, 1));
        assert_eq!(prgba, vec![0, 0, 255, 255, 255, 255, 0, 255], "the palette became opaque RGBA8");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **THE KEEP REQUEST IS THE STOP'S, AND ONLY THE PNG STOP KEEPS** (Q3). The table, plus the
    /// consequence a reader would otherwise have to trust: the same transparent source under the
    /// JPG stop still composites over WHITE (sheet 2.2c, 0.2 -- unchanged).
    ///
    /// FALSIFIER (L28): make `Keep::for_web` answer `Keep::ALL` for `Jpeg` and the white assert
    /// reddens (the JPEG arm would refuse an RGBA buffer outright, which is the honest failure).
    #[test]
    fn only_the_png_stop_asks_the_decoders_to_keep() {
        assert_eq!(Keep::for_web(WebFormat::Jpeg), Keep::NONE);
        assert_eq!(Keep::for_web(WebFormat::Png), Keep::ALL);
        assert!(!Keep::NONE.any() && Keep::ALL.any());
        let dir = scratch("q3");
        let src = dir.join("hole.png");
        // 32x32, left half opaque red, right half fully transparent. The halves are wide because
        // the JPG stop's encoder subsamples chroma 4:2:0 -- a two-pixel fixture would measure the
        // encoder's edge bleed rather than the decoder's composite, so the probe sits eight pixels
        // clear of the seam.
        let (w, h) = (32u32, 32u32);
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..h {
            for x in 0..w {
                if x < w / 2 {
                    data.extend_from_slice(&[255, 0, 0, 255]);
                } else {
                    data.extend_from_slice(&[0, 0, 0, 0]);
                }
            }
        }
        mint_png(&src, w, h, png::ColorType::Rgba, png::BitDepth::Eight, &data);
        let jpg = export_file(&src, SrcKind::Png, WebFormat::Jpeg, 100_000, Gamut::Srgb);
        assert!(jpg.starts_with(&[0xFF, 0xD8]), "the JPG stop still writes a JPEG");
        let (px, jw, jh) = {
            let mut d = jpeg_decoder::Decoder::new(std::io::Cursor::new(&jpg));
            let out = d.decode().expect("the JPEG decodes");
            let info = d.info().expect("info");
            (out, info.width, info.height)
        };
        assert_eq!((jw, jh), (w as u16, h as u16));
        let probe = ((16usize * w as usize) + 24) * 3;
        assert!(
            px[probe] > 240 && px[probe + 1] > 240 && px[probe + 2] > 240,
            "the transparent half is still composited over WHITE at the JPG stop: {:?}",
            &px[probe..probe + 3]
        );
        // ANTI-VACUITY: the opaque half really is red, so "white" is a measurement of the composite
        // and not of an all-white deliverable.
        let red = ((16usize * w as usize) + 8) * 3;
        assert!(px[red] > 200 && px[red + 1] < 60, "…and the opaque half is red: {:?}", &px[red..red + 3]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **THE `Pixels` TABLE IS A TABLE.** Every derived answer -- channels, alpha, depth, the log's
    /// layout noun, bytes per pixel, the byte length -- read off all four variants, so no consumer
    /// has to re-derive one of them and get it wrong.
    ///
    /// FALSIFIER (L28): return 3 for `Rgba8` in `channels()` and the `bytes_per_px` and
    /// `byte_len` cells redden with it; rename one layout string and the log row in
    /// `falcon/native` reddens too.
    #[test]
    fn the_pixels_table_answers_every_derived_question() {
        let cells: [(Pixels, usize, bool, bool, &str, usize); 4] = [
            (Pixels::Rgb8(vec![0; 12]), 3, false, false, "RGB8", 3),
            (Pixels::Rgba8(vec![0; 16]), 4, true, false, "RGBA8", 4),
            (Pixels::Rgb16(vec![0; 12]), 3, false, true, "RGB16", 6),
            (Pixels::Rgba16(vec![0; 16]), 4, true, true, "RGBA16", 8),
        ];
        for (px, ch, alpha, deep, layout, bpp) in cells {
            assert_eq!(px.channels(), ch, "{layout}: channels");
            assert_eq!(px.has_alpha(), alpha, "{layout}: alpha");
            assert_eq!(px.is_16bit(), deep, "{layout}: depth");
            assert_eq!(px.layout(), layout);
            assert_eq!(px.bytes_per_px(), bpp, "{layout}: bytes per pixel");
            assert_eq!(px.byte_len(), 4 * bpp, "{layout}: four pixels' worth of bytes");
            assert!(px.into_rgb8().is_ok() == (layout == "RGB8"), "{layout}: the Keep::NONE unwrap");
        }
    }

    /// **`apply_keep` NARROWS, IN THAT ORDER.** Flatten first, narrow second -- the other order
    /// composites samples that have already lost their low byte, i.e. it rounds twice. All four
    /// requests over an RGBA16 buffer, so the mixed ones are not merely expressible but pinned.
    ///
    /// FALSIFIER (L28): swap the two statements in `apply_keep` and the `{alpha: false, depth:
    /// false}` cell reddens by one count on the mid-alpha pixel.
    #[test]
    fn apply_keep_flattens_before_it_narrows() {
        let px = || Pixels::Rgba16(vec![40_000, 20_000, 10_000, 32_768]);
        assert_eq!(apply_keep(px(), Keep::ALL), px(), "keep both: the identity");
        assert_eq!(
            apply_keep(px(), Keep { alpha: true, depth: false }),
            Pixels::Rgba8(vec![156, 78, 39, 128]),
            "keep alpha only: the samples narrow, the channel stays"
        );
        let flat16 = match apply_keep(px(), Keep { alpha: false, depth: true }) {
            Pixels::Rgb16(v) => v,
            other => panic!("expected RGB16, got {}", other.layout()),
        };
        assert_eq!(flat16.len(), 3, "keep depth only: the channel goes, the width stays");
        let flat8 = match apply_keep(px(), Keep::NONE) {
            Pixels::Rgb8(v) => v,
            other => panic!("expected RGB8, got {}", other.layout()),
        };
        // Flatten at 16 bits, THEN narrow: the high byte of `over_white16`, not `over_white` of the
        // high byte. The two differ by a count here, which is why the order is pinned.
        let expect: Vec<u8> = flat16.iter().map(|&s| (s >> 8) as u8).collect();
        assert_eq!(flat8, expect, "Keep::NONE is the flatten, then the narrow");
    }

    // ────────────────────── the ruled tail (§R.S T1): the JXL depth term ──────────────────────

    /// **THE JXL ARM'S DEPTH TERM READS THE FILE, NOT THE LAYOUT.** [`super::jxl_keeps_depth`] is
    /// the whole of the decision `decode_jxl_rgb` makes between the `::<u16>` read and the shipped
    /// `::<u8>` one, and this row is the only reach a test has into that arm: there is no JXL
    /// encoder in this tree, so no fixture can mint a `.jxl` and the arm stays UNVERIFIED BY
    /// FIXTURE (§C.7, Q7 (iii)). The predicate is pure, so every depth the format admits is
    /// pinned here instead.
    ///
    /// EIGHT BITS IS THE CELL THAT MATTERS. Read at `u16`, an 8-bit sample comes back as
    /// `v * 257` and the deliverable is a 16-bit PNG twice the size of an 8-bit source -- the
    /// UP-conversion §0.3 forbids in as many words, and what round 35 shipped for every JXL at
    /// the PNG stop (its ruled tail, §R.S PIXELS R1).
    ///
    /// FALSIFIER (L28): widen the comparison in `jxl_keeps_depth` from `> 8` to `>= 8` and the
    /// 8-bit cell reddens; make the `FloatSample` arm answer `false` and the float cell reddens.
    #[test]
    fn the_jxl_depth_term_keeps_only_what_the_file_holds() {
        use jxl_oxide::image::BitDepth;
        let int = |b: u32| BitDepth::IntegerSample { bits_per_sample: b };
        assert!(!jxl_keeps_depth(&int(8)), "8 bits: nothing is ever up-converted");
        assert!(jxl_keeps_depth(&int(10)), "10 bits: the file holds more than a byte");
        assert!(jxl_keeps_depth(&int(12)), "12 bits");
        assert!(jxl_keeps_depth(&int(16)), "16 bits");
        assert!(
            jxl_keeps_depth(&BitDepth::FloatSample { bits_per_sample: 32, exp_bits: 8 }),
            "a float sample is always kept: jxl-oxide scales it into whichever type the buffer asks for"
        );
        assert!(
            jxl_keeps_depth(&BitDepth::FloatSample { bits_per_sample: 16, exp_bits: 5 }),
            "…and a half-float, which the FloatSample arm answers for without reading its width"
        );
        // The crate's own `Default` is the 8-bit integer case (jxl-image-0.13.0/src/lib.rs:438),
        // so a header that declares no depth takes the shipped 8-bit read rather than doubling
        // the deliverable.
        assert!(!jxl_keeps_depth(&BitDepth::default()), "the crate's default header is 8-bit integer");
    }
}

#[cfg(test)]
mod frost_resize_tests {
    #[test]
    fn area_frost_has_no_step_overshoot_and_transparent_white_does_not_glow() {
        let src:Vec<u8>=(0..64).flat_map(|_|(0..512).flat_map(|x|if x<247 {[64,64,64,255]}else{[192,192,192,255]})).collect();
        let (area,_,_)=super::downscale_frost_rgba(&src,512,64,160);
        assert!(area.chunks_exact(4).all(|p|(64..=192).contains(&p[0])));
        let (lanczos,_,_)=super::downscale_rgba(&src,512,64,160);
        assert!(lanczos.chunks_exact(4).any(|p|p[0]<64 || p[0]>192),"counterexample exercises the actual old filter");
        let (alpha,_,_)=super::downscale_frost_rgba(&[255,255,255,0,0,0,0,255],2,1,1);
        assert_eq!(&alpha[..3],&[0,0,0]); assert!((127..=128).contains(&alpha[3]));
    }
}
