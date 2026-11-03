//! P5 (PLAN §65, the v0.9.x cross-platform transition): the finished-image decode layer behind a
//! trait, so a future macOS ImageIO decoder can slot in where Windows uses nvJPEG/WIC without
//! touching the render-worker call sites.
//!
//! The trait is deliberately narrow — exactly the two shapes the workers need:
//!   * [`ImageDecoder::decode_scaled`] — "≥ `target_long`, best-effort, caller finishes" (the
//!     display path). `target_long == 0` means native. This is today's `fast_frame_rgba` contract
//!     ([`crate::fast_frame_rgba`]); the caller applies the near-stop finish so CPU + accelerated
//!     decoders share ONE finish rule (see [`crate::near_stop_skip_resize`]).
//!   * [`ImageDecoder::decode_full`] — the native full-resolution RGB buffer the ROI/zoom cropper
//!     works from (fed to [`crate::crop_region_norm`]). RGB only — never YUV.
//!
//! There is **no** `decode_region`: the ROI worker stays decode-full + crop (`crop_region_norm` /
//! `crop_region_yuv`), per PLAN §65.
//!
//! Fallback is by RETURN VALUE, never `#[cfg]`: a decoder that can't handle a shot returns
//! [`DecodeError::Unsupported`] and the caller `.or_else`s onto the portable [`CpuDecoder`]. This is
//! what folds today's `use_nvjpeg` / `is_jpeg_source` / `yuv_enabled` gates into the decoder layer:
//! the app keeps the runtime `use_nvjpeg` toggle (it picks WHICH decoder instance a worker uses),
//! while "is this a JPEG the hardware unit accepts?" becomes an internal `Err(Unsupported)`.
//!
//! Layout preference is exposed by [`DecodeCaps::yields_yuv`]: only the nvJPEG decoder (a future
//! macOS twin would say `false`) yields planar YUV, and that flag is what lets a non-YUV build always
//! take the RGB(A) upload arm.
//!
//! The three universal implementors live here ([`CpuDecoder`], [`WicDecoder`]); the GPU one
//! (`NvJpegDecoder`) lives in `falcon-nvjpeg` (it wraps the CUDA context and depends on this trait),
//! and the future `ImageIODecoder` is its macOS analogue.

use crate::Shot;

/// Decoded pixels in the decoder's native layout. RGB variants are universal; planar YUV is an
/// nvJPEG-only capability (see [`DecodeCaps::yields_yuv`]) — the app converts it to RGB(A) on the GPU
/// at upload (the fused `YuvConvert` pass), so a decoder that can't produce it simply never does.
pub enum DecodedPixels {
    /// Interleaved RGBA8, tight-packed `w*h*4`.
    Rgba8 { data: Vec<u8>, w: u32, h: u32 },
    /// Interleaved RGB8, tight-packed `w*h*3`.
    Rgb8 { data: Vec<u8>, w: u32, h: u32 },
    /// Tight planar Y/Cb/Cr as stored in the bitstream. `w`/`h` = luma dims; `cw`/`ch` = chroma
    /// dims (per the SOF subsampling divisors). Mirrors `falcon_nvjpeg::YuvFrame`.
    PlanarYuv { y: Vec<u8>, cb: Vec<u8>, cr: Vec<u8>, w: u32, h: u32, cw: u32, ch: u32 },
}

impl DecodedPixels {
    /// Luma / image dimensions `(w, h)` regardless of layout.
    pub fn dims(&self) -> (u32, u32) {
        match *self {
            DecodedPixels::Rgba8 { w, h, .. }
            | DecodedPixels::Rgb8 { w, h, .. }
            | DecodedPixels::PlanarYuv { w, h, .. } => (w, h),
        }
    }

    /// Take the packed RGB8 buffer + dims, or `None` for a non-RGB8 layout. Convenience for the
    /// ROI/zoom source path, which only ever asks for `Rgb8` (via [`ImageDecoder::decode_full`]).
    pub fn into_rgb8(self) -> Option<(Vec<u8>, u32, u32)> {
        match self {
            DecodedPixels::Rgb8 { data, w, h } => Some((data, w, h)),
            _ => None,
        }
    }
}

/// Why a decode did not produce pixels. The load-bearing variant is [`DecodeError::Unsupported`]:
/// it is the "this decoder can't handle this shot — try the next one" signal the caller `.or_else`s
/// on. The others are informational (the workers only distinguish success from failure).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// This decoder does not handle this shot (wrong format, or the hardware unit rejected the
    /// bitstream). The caller falls through to another decoder.
    Unsupported,
    /// The source is a recognised format but malformed / truncated.
    Corrupt,
    /// A cloud (Files-On-Demand) placeholder that isn't hydrated locally — no bytes to decode yet.
    NotHydrated,
    /// A filesystem/read error reaching the source bytes.
    Io,
    /// The decode would allocate past the process's memory bound (the decompression-bomb guard).
    Oom,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DecodeError::Unsupported => "unsupported by this decoder",
            DecodeError::Corrupt => "corrupt or truncated source",
            DecodeError::NotHydrated => "cloud placeholder not hydrated",
            DecodeError::Io => "I/O error reading source",
            DecodeError::Oom => "decode exceeds the memory bound",
        };
        f.write_str(s)
    }
}

impl std::error::Error for DecodeError {}

/// Static capabilities of a decoder instance — the app inspects these to wire the pipeline (rather
/// than `#[cfg]`-branching on the platform). Presently just the planar-YUV capability.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeCaps {
    /// True when [`ImageDecoder::decode_scaled`] can return [`DecodedPixels::PlanarYuv`]. Only the
    /// nvJPEG decoder sets this; it drives the YUV upload arm. A `false`-capability build (CPU-only,
    /// or a future non-YUV platform decoder) always takes the RGB(A) path.
    pub yields_yuv: bool,
}

/// The per-worker finished-image decoder seam. Instances are `&mut self` and NOT shared across
/// workers (no locking) — each render worker owns its own, exactly like today's per-worker nvJPEG
/// contexts. RAW *develop* is deliberately outside this trait (it is a demosaic pipeline, not a
/// container decode — see `develop_raw_rgb_full` / `extract_cfa`); only a RAW's *embedded* JPEG
/// preview rides through here (via `jpeg_source`).
pub trait ImageDecoder {
    /// Static capabilities (see [`DecodeCaps`]).
    fn caps(&self) -> DecodeCaps;

    /// Cheap header-only dimension probe (no pixel decode). `None` for a shot this decoder can't
    /// size. Every implementor delegates to [`crate::source_dimensions`] — dims are format-derived,
    /// not decoder-specific — so the seam is uniform across platforms.
    fn probe_dims(&mut self, shot: &Shot) -> Option<(u32, u32)>;

    /// Decode to a buffer whose long side is ≥ `target_long` (best-effort; `0` = native), in the
    /// decoder's preferred layout. The CALLER finishes (near-stop skip + resize + RGBA expand, or a
    /// YUV upload). `Err(Unsupported)` ⇒ try another decoder.
    fn decode_scaled(&mut self, shot: &Shot, target_long: u32) -> Result<DecodedPixels, DecodeError>;

    /// v0.8.101 (S1/S2): [`decode_scaled`](Self::decode_scaled) for a specific browse
    /// [`Lane`](crate::Lane), reporting WHERE the pixels came from.
    ///
    /// The default delegates and answers [`FrameSource::MainImage`](crate::FrameSource::MainImage),
    /// which is not a shortcut — it is the truth for every accelerated decoder in the tree
    /// (`NvJpegDecoder`, `WicDecoder`, `ImageIODecoder`): none of them has an embedded-preview
    /// door, so none of them can produce anything else. Only [`CpuDecoder`], which owns the format
    /// spine and therefore the HEIC lanes, overrides it.
    ///
    /// The `FrameSource` is not decoration: the fast tier stamps its cache bucket from it
    /// ([`FrameSource::cache_dim`](crate::FrameSource::cache_dim)), so a preview-sourced frame that
    /// ever reached that tier would be quarantined rather than served as a scrub frame.
    fn decode_scaled_lane(
        &mut self,
        shot: &Shot,
        target_long: u32,
        _lane: crate::Lane,
    ) -> Result<(DecodedPixels, crate::FrameSource), DecodeError> {
        self.decode_scaled(shot, target_long).map(|p| (p, crate::FrameSource::MainImage))
    }

    /// Decode the FULL native-resolution source to packed RGB8 (never YUV) — the buffer the ROI/zoom
    /// region cropper works from. `Err(Unsupported)` ⇒ try another decoder.
    fn decode_full(&mut self, shot: &Shot) -> Result<DecodedPixels, DecodeError>;

    /// Decode the FULL native-resolution source to [`DecodedPixels::PlanarYuv`], or
    /// `Err(Unsupported)` when this decoder can't yield planar YUV for this shot. This is the ROI
    /// **YUV tile** route's real contract — "planar YUV or nothing" — and is DISTINCT from
    /// [`decode_scaled`](Self::decode_scaled): it must NEVER fall through to an RGB decode. A YUV
    /// miss here (gray / exotic-SOF JPEG, or the YUV latch off) returns `Unsupported` so the caller
    /// bails to the RGB source route WITHOUT paying a wasted full-resolution RGBI decode.
    ///
    /// The default impl returns `Err(Unsupported)`: only [`DecodeCaps::yields_yuv`] decoders
    /// override it. This is by design — [`CpuDecoder`]/[`WicDecoder`] inherit the default, so the
    /// future macOS `ImageIODecoder` (also `yields_yuv = false`) never produces YUV and the ROI YUV
    /// route is dead code off-Windows, matching `caps().yields_yuv`.
    fn decode_yuv(&mut self, _shot: &Shot) -> Result<DecodedPixels, DecodeError> {
        Err(DecodeError::Unsupported)
    }
}

/// Classify a [`crate::decode_source_rgb`] failure for the trait's error channel. The workers only
/// distinguish success from failure, so the precise variant is informational; an unsupported-format
/// shot maps to [`DecodeError::Unsupported`], everything else to [`DecodeError::Corrupt`].
fn classify_cpu_err(shot: &Shot) -> DecodeError {
    if shot.is_unsupported() {
        DecodeError::Unsupported
    } else if shot.cloud_placeholder {
        DecodeError::NotHydrated
    } else {
        DecodeError::Corrupt
    }
}

/// The portable, always-present decoder: the pure-Rust `decode_source_rgb` spine (JPEG DCT
/// shrink-on-load, PNG/TIFF/WebP, and — on Windows — HEIC/exotic-TIFF via WIC transitively). It is
/// the universal fallback every accelerated decoder `.or_else`s onto, and the ONLY decoder the
/// fast/thumbnail tiers use. Zero-sized + stateless, so a per-worker instance is free.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuDecoder;

impl CpuDecoder {
    pub fn new() -> Self {
        CpuDecoder
    }
}

impl ImageDecoder for CpuDecoder {
    fn caps(&self) -> DecodeCaps {
        DecodeCaps { yields_yuv: false }
    }

    fn probe_dims(&mut self, shot: &Shot) -> Option<(u32, u32)> {
        crate::source_dimensions(shot)
    }

    fn decode_scaled(&mut self, shot: &Shot, target_long: u32) -> Result<DecodedPixels, DecodeError> {
        let scale_to = if target_long == 0 { None } else { Some(target_long) };
        match crate::decode_source_rgb(shot, scale_to) {
            Ok((data, w, h)) => Ok(DecodedPixels::Rgb8 { data, w, h }),
            Err(_) => Err(classify_cpu_err(shot)),
        }
    }

    /// v0.8.101 (S1/S2): the ONE override in the tree. `CpuDecoder` owns the format spine, so it is
    /// the only decoder that can reach the HEIC lanes at all — and `decode_scaled` above stays
    /// exactly what it was (`Lane::Native`), so every call site that has not asked for a lane keeps
    /// the pre-v0.8.101 behaviour byte for byte.
    fn decode_scaled_lane(
        &mut self,
        shot: &Shot,
        target_long: u32,
        lane: crate::Lane,
    ) -> Result<(DecodedPixels, crate::FrameSource), DecodeError> {
        let scale_to = if target_long == 0 { None } else { Some(target_long) };
        // v0.9.66 (the round-7 sync): `decode_source_rgb_lane` gained an `Option` outer layer at
        // v0.8.171 — `Ok(None)` is the hardware lane ABANDONING a decode mid-grid because the app
        // moved on. This trait cannot express that (its callers are the ROI/full-res routes, which
        // arm no supersession signal and so can never be superseded), and it must not be reported
        // as a decode failure either — that would latch a shot nothing is wrong with. It is mapped
        // to `Unsupported`, the trait's own "I did not take it, ask someone else" answer, which is
        // the one value every caller already handles by falling through.
        match crate::decode_source_rgb_lane(shot, scale_to, lane) {
            Ok(Some((data, w, h, source, _route))) => {
                Ok((DecodedPixels::Rgb8 { data, w, h }, source))
            }
            Ok(None) => Err(DecodeError::Unsupported),
            Err(_) => Err(classify_cpu_err(shot)),
        }
    }

    fn decode_full(&mut self, shot: &Shot) -> Result<DecodedPixels, DecodeError> {
        match crate::decode_source_rgb(shot, None) {
            Ok((data, w, h)) => Ok(DecodedPixels::Rgb8 { data, w, h }),
            Err(_) => Err(classify_cpu_err(shot)),
        }
    }
}

/// The OS-image-codec decoder (Windows WIC). WIC opens any format its installed codecs handle —
/// HEIC (HEVC Image Extension), exotic TIFF (CCITT/YCbCr/CMYK), and the common formats — and yields
/// packed RGB8. It is the direct structural analogue of the future macOS `ImageIODecoder`
/// (CGImageSource), which is why it lives here as a first-class implementor even though the shipping
/// Windows call sites reach WIC transitively through [`CpuDecoder`] (for HEIC + the TIFF fallback).
/// RGB only (`yields_yuv = false`).
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WicDecoder;

#[cfg(windows)]
impl WicDecoder {
    pub fn new() -> Self {
        WicDecoder
    }

    /// The shot's finished-image path, or `Err(Unsupported)` when there is no finished PICTURE —
    /// a RAW-only shot (its preview rides the JPEG path), or a shot whose finished slot is a
    /// PASSENGER (v1.0.0-rc TAIL 4, re-verification Y2: `has_jpg`, not `jpg.is_some()`; the file is
    /// there but it is not what this decoder was asked for, and it is reachable in theory through
    /// the CMYK/WIC route).
    fn path(shot: &Shot) -> Result<&std::path::Path, DecodeError> {
        shot.jpg.as_deref().filter(|_| shot.has_jpg).ok_or(DecodeError::Unsupported)
    }
}

#[cfg(windows)]
impl ImageDecoder for WicDecoder {
    fn caps(&self) -> DecodeCaps {
        DecodeCaps { yields_yuv: false }
    }

    fn probe_dims(&mut self, shot: &Shot) -> Option<(u32, u32)> {
        crate::wic_dimensions(Self::path(shot).ok()?)
    }

    fn decode_scaled(&mut self, shot: &Shot, target_long: u32) -> Result<DecodedPixels, DecodeError> {
        let path = Self::path(shot)?;
        let (rgb, w, h) = crate::wic_decode_rgb24(path, "WIC: no installed codec for this file")
            .map_err(|_| DecodeError::Corrupt)?;
        // WIC has no shrink-on-load; downscale to the target caller-side (0 = native), mirroring the
        // PNG/TIFF finish in `decode_source_rgb`.
        let scale_to = if target_long == 0 { None } else { Some(target_long) };
        let (data, w, h) = crate::finish_source(rgb, w, h, scale_to).map_err(|_| DecodeError::Corrupt)?;
        Ok(DecodedPixels::Rgb8 { data, w, h })
    }

    fn decode_full(&mut self, shot: &Shot) -> Result<DecodedPixels, DecodeError> {
        let path = Self::path(shot)?;
        let (data, w, h) = crate::wic_decode_rgb24(path, "WIC: no installed codec for this file")
            .map_err(|_| DecodeError::Corrupt)?;
        Ok(DecodedPixels::Rgb8 { data, w, h })
    }
}

/// The macOS system-image-codec decoder (Image I/O / `CGImageSource`) — the accelerated finished-image
/// decoder on Apple hardware and the structural twin of [`WicDecoder`], sitting in the same detail/ROI
/// accel slot `NvJpegDecoder` occupies on Windows. It serves the two formats where the system codec is
/// the win or the only option — **JPEG** (Apple media-engine hardware decode) and **HEIC** (native, no
/// install) — and returns [`DecodeError::Unsupported`] for every other kind so PNG/WebP/GIF/JXL/BMP
/// (proven pure-Rust, identical cross-platform colour) and TIFF (the `tiff` crate → Image I/O fallback,
/// kept uniform across tiers) fall through to [`CpuDecoder`]. RGB only (`yields_yuv = false`); a decode
/// miss is reported as `Unsupported` so the caller `.or_else`s onto the CPU decoder — fallback by return
/// value, never `#[cfg]`, exactly like `NvJpegDecoder`. HEIC is ALSO reachable through `CpuDecoder`
/// (via `decode_heic` → the same `imageio_decode_rgb`) so the CPU-only fast/thumbnail tiers decode it
/// too; this decoder is the scale-on-load hardware path for the detail/ROI tiers.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Default)]
pub struct ImageIODecoder;

#[cfg(target_os = "macos")]
impl ImageIODecoder {
    pub fn new() -> Self {
        ImageIODecoder
    }

    /// The formats the system codec serves in the accel slot (shared pure policy — see
    /// `crate::imageio_serves`): JPEG (hardware) + HEIC (native). Everything else falls through.
    fn serves(kind: crate::SrcKind) -> bool {
        crate::imageio_serves(kind)
    }

    /// The shot's finished-image path, or `Err(Unsupported)` for a RAW-only shot (no standalone file —
    /// its embedded JPEG preview rides the CPU path's `jpeg_source`, exactly as for [`WicDecoder`]).
    /// v1.0.0-rc TAIL 4 (re-verification Y2): and `has_jpg`, so a PASSENGER is declined here too —
    /// the same reason, and the same one-term shape as its Windows twin.
    fn path(shot: &Shot) -> Result<&std::path::Path, DecodeError> {
        shot.jpg.as_deref().filter(|_| shot.has_jpg).ok_or(DecodeError::Unsupported)
    }
}

#[cfg(target_os = "macos")]
impl ImageDecoder for ImageIODecoder {
    fn caps(&self) -> DecodeCaps {
        DecodeCaps { yields_yuv: false }
    }

    fn probe_dims(&mut self, shot: &Shot) -> Option<(u32, u32)> {
        // Dims are format-derived, not decoder-specific — delegate to the shared free function (which
        // routes HEIC to Image I/O's header probe). Matches the CpuDecoder / NvJpegDecoder probe.
        crate::source_dimensions(shot)
    }

    fn decode_scaled(&mut self, shot: &Shot, target_long: u32) -> Result<DecodedPixels, DecodeError> {
        // Open-time check (2 November): this slot serves only JPEG/HEIC by the bytes; a passenger
        // falls through to the CPU decoder and the RAW's preview.
        let checked = crate::shot_by_bytes(shot);
        if !Self::serves(checked.kind) {
            return Err(DecodeError::Unsupported); // PNG/WebP/GIF/JXL/BMP/TIFF → CPU
        }
        let path = Self::path(&checked)?;
        // Image I/O scales on load (the ladder picks a subsample stop ≥ target), so — unlike WicDecoder —
        // there is no caller-side finish here: the buffer is already ≥ target and the render worker's
        // near-stop finish trims it to exact, identical to the nvJPEG on-device-scaled arm.
        let scale_to = (target_long != 0).then_some(target_long);
        match crate::imageio_decode_rgb(path, scale_to) {
            Ok((data, w, h)) => Ok(DecodedPixels::Rgb8 { data, w, h }),
            // A decode miss (unreadable, corrupt, or a HEIC variant the codec rejects) → Unsupported so
            // the caller falls to the CPU decoder — the same contract NvJpegDecoder uses on a hardware
            // reject. For JPEG the pure-Rust jpeg-decoder gets a real second chance; for HEIC the CPU
            // decoder re-enters Image I/O via decode_heic and lands the same honest end state.
            Err(_) => Err(DecodeError::Unsupported),
        }
    }

    fn decode_full(&mut self, shot: &Shot) -> Result<DecodedPixels, DecodeError> {
        let checked = crate::shot_by_bytes(shot); // Open-time check (2 November), as above.
        if !Self::serves(checked.kind) {
            return Err(DecodeError::Unsupported);
        }
        let path = Self::path(&checked)?;
        match crate::imageio_decode_rgb(path, None) {
            Ok((data, w, h)) => Ok(DecodedPixels::Rgb8 { data, w, h }),
            Err(_) => Err(DecodeError::Unsupported),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_source_rgb, source_dimensions, Shot, SrcKind};
    use std::path::PathBuf;

    fn testkit_dir() -> Option<PathBuf> {
        // The curated fixtures the app itself boots against (see TESTING.md). Only present on the
        // dev box; the test no-ops elsewhere so CI without the kit still passes.
        let d = dirs_local_falcon()?.join("testkit").join("standard");
        d.is_dir().then_some(d)
    }

    fn dirs_local_falcon() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Falcon"))
    }

    fn jpeg_shot(path: PathBuf) -> Shot {
        Shot {
            id: 0,
            name: "t".into(),
            has_raw: false,
            has_jpg: true,
            raw: None,
            jpg: Some(path),
            kind: SrcKind::Jpeg,
            cloud_placeholder: false,
            sniffed: None,
        }
    }

    /// The parity gate's byte-identity check as a unit test: `CpuDecoder::decode_scaled` /
    /// `decode_full` must return the SAME pixels as a direct `decode_source_rgb` call, because they
    /// wrap it. Runs against every JPG in the standard testkit (skips if the kit is absent).
    #[test]
    fn cpu_decoder_is_byte_identical_to_decode_source_rgb() {
        let Some(dir) = testkit_dir() else { return };
        let mut cpu = CpuDecoder::new();
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("jpg"))
                != Some(true)
            {
                continue;
            }
            let shot = jpeg_shot(p.clone());

            // scaled at a 2048 target
            let direct = decode_source_rgb(&shot, Some(2048)).unwrap();
            let via = cpu.decode_scaled(&shot, 2048).unwrap();
            assert!(matches!(&via, DecodedPixels::Rgb8 { .. }), "CPU decode_scaled yields Rgb8");
            let (vd, vw, vh) = via.into_rgb8().unwrap();
            assert_eq!((vd.as_slice(), vw, vh), (direct.0.as_slice(), direct.1, direct.2),
                "decode_scaled parity for {p:?}");

            // full native
            let direct_full = decode_source_rgb(&shot, None).unwrap();
            let via_full = cpu.decode_full(&shot).unwrap().into_rgb8().unwrap();
            assert_eq!((via_full.0.as_slice(), via_full.1, via_full.2),
                (direct_full.0.as_slice(), direct_full.1, direct_full.2),
                "decode_full parity for {p:?}");

            // probe_dims mirrors source_dimensions
            assert_eq!(cpu.probe_dims(&shot), source_dimensions(&shot), "probe_dims parity for {p:?}");
            checked += 1;
        }
        assert!(checked > 0, "expected at least one testkit JPG to exercise");
    }

    #[test]
    fn cpu_decoder_caps_are_rgb_only() {
        assert!(!CpuDecoder::new().caps().yields_yuv);
    }

    /// The trait default for [`ImageDecoder::decode_yuv`]: a non-YUV decoder (CpuDecoder, and by the
    /// same default WicDecoder / a future macOS ImageIODecoder) yields `Err(Unsupported)` and NEVER
    /// pixels — the ROI YUV tile route bails to the RGB source path off it. This pins the "planar YUV
    /// or nothing" contract so a non-YUV decoder can never smuggle an RGB decode through decode_yuv.
    #[test]
    fn cpu_decoder_decode_yuv_is_unsupported() {
        let mut cpu = CpuDecoder::new();
        let shot = jpeg_shot(PathBuf::from("whatever.jpg"));
        assert!(matches!(cpu.decode_yuv(&shot), Err(DecodeError::Unsupported)));
        // Caps agree: a non-YUV decoder advertises it, so the call site never reaches decode_yuv on it.
        assert!(!cpu.caps().yields_yuv);
    }

    #[test]
    fn unsupported_shot_maps_to_unsupported_error() {
        let mut cpu = CpuDecoder::new();
        let shot = Shot {
            id: 0,
            name: "x".into(),
            has_raw: false,
            has_jpg: false,
            raw: None,
            jpg: Some(PathBuf::from("nonexistent.avif")),
            kind: SrcKind::Unsupported,
            cloud_placeholder: false,
            sniffed: None,
        };
        assert!(matches!(cpu.decode_scaled(&shot, 1024), Err(DecodeError::Unsupported)));
        assert!(matches!(cpu.decode_full(&shot), Err(DecodeError::Unsupported)));
    }
}
