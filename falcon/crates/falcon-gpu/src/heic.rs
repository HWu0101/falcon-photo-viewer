//! v0.8.147 (E3 milestone 2) — **the HEIC grid's GPU assembly**: canvas compositing, the grid's
//! crop, `irot`/`imir`, the E2 kernel, and the tier downsample — one chain, one readback.
//!
//! # What this module is for
//!
//! E3-M1 (`falcon-hwdec`) decodes ONE tile at a time and hands back NV12. A 48 MP iPhone HEIC is a
//! 9 × 6 grid of 896 × 1024 tiles: 74 MB of NV12 that has to become one 8064 × 6048 RGB image,
//! rotated as the container says, at whatever size the asking tier wants. Doing that on the CPU
//! would mean a 74 MB composite, a 146 MB conversion and a 146 MB Lanczos — which is most of what
//! makes the existing WIC path cost 712–994 ms. So none of it happens on the CPU. The tiles are
//! written into a GPU canvas as they arrive, and the ONLY thing that comes back is the finished
//! RGB at the requested size.
//!
//! # The chain, in order, and where each of the contract's clauses lands
//!
//! The contract E3 must reproduce (plan non-negotiable #4) is: *packed RGB8, 24 bpp, stride `w*3`,
//! `irot` APPLIED, un-colour-managed in the file's gamut, dims byte-identical to `finish_source`
//! per `scale_to`.* Four passes answer it:
//!
//! 1. **COMPOSITE** — `queue.write_texture` per tile, at the tile's own mosaic origin, into two
//!    canvas textures: `R8Uint` at the mosaic's full size for luma and `Rg8Uint` at half for the
//!    interleaved chroma. There is no compositing *shader*: a tile's placement is a copy at an
//!    offset, and asking the GPU's copy engine for it is both faster and impossible to get subtly
//!    wrong in the way a hand-written blit can. The driver's stride is passed through as
//!    `bytes_per_row`, so a pitched decode surface is uploaded without a repack — E2 pinned that a
//!    stride cannot change the answer (`a_pitched_surface_matches_the_packed_one`).
//!
//!    **The canvas is NV12, not RGB, and that is the seam decision.** Composing in NV12 means the
//!    chroma upsample runs ACROSS tile boundaries — the pixel in mosaic column 895 takes half its
//!    chroma from tile 0's last chroma column and half from tile 1's first. Composing per-tile RGB
//!    instead would clamp each tile's upsample at its own edge and leave a real discontinuity on
//!    every one of the 13 internal seams of a 48 MP photo. The tiles are samples of ONE continuous
//!    image; treating them as one is both the correct reading and the seam-free one, and
//!    `tests/hw_photo.rs`'s seam hunt is what says so out loud.
//!
//! 2. **CONVERT** — [`crate::Nv12Kernel::encode`], the E2 kernel, over the whole mosaic, into a
//!    storage buffer of `r | g<<8 | b<<16` words. Not a fork and not a fusion: it is the same
//!    compiled pipeline `convert_with_coeffs` uses, called as a pass. Colour arithmetic still
//!    exists in exactly two places in this project.
//!
//! 3. **MAP + RESAMPLE-X** ([`FINISH_WGSL`]'s `main_h`) — the crop rect, `irot`, `imir` and the
//!    horizontal half of the downsample, all as ONE gather. Rotation is not a pass of its own
//!    because it is a coordinate map, and a coordinate map is free inside a gather that was going
//!    to index the source anyway; a separate rotate pass would have cost a 195 MB round trip
//!    through VRAM to move bytes that the resampler was about to read regardless.
//!
//! 4. **RESAMPLE-Y** (`main_v`) — the vertical half, skipped outright when the height does not
//!    change (which is every `scale_to` at or above the photo's long side, i.e. the native tier).
//!
//! # Why the resampler is Pillow's formulation
//!
//! `finish_source` downscales with `fast_image_resize`'s Lanczos3, which is a Pillow-derived
//! convolution: `support = 3 · max(in/out, 1)`, taps from `(int)(centre − support + 0.5)` to
//! `(int)(centre + support + 0.5)`, weights `lanczos3((k − centre + 0.5) / filter_scale)`
//! normalised to sum 1, u8 between the two passes. This shader is that formulation, written out.
//! It cannot be byte-identical — `fast_image_resize` accumulates in fixed point with quantised
//! coefficients and this accumulates in `f32` — and the milestone does not ask it to be: the DIMS
//! are the gate, the pixels are a report. What the shared formulation buys is that the residual is
//! a few LSB of quantisation rather than a different picture, which is what makes the tile-seam
//! hunt able to see a real defect through it.
//!
//! # INERT
//!
//! Nothing that ships calls this. It is reachable only from `falcon-hwdec` and that crate is a
//! workspace member for `cargo test --workspace` and nothing else.

use anyhow::{bail, Context, Result};

use falcon_color::Gamut;
use falcon_decode::yuv_kernel::{ChromaSiting, YuvCoeffs};

use crate::{ensure_custom_lut, CustomLutTex, Nv12Kernel};

/// v0.8.165 (WAVE 1) — **what the finish chain's LAST pass writes.**
///
/// # Why this enum exists rather than a second `finish` function
///
/// The colour transform is not a pass of its own. It is three lines at the point where the last
/// resample pass already holds the finished pixel in registers, exactly as `irot` is three lines
/// inside a gather that was going to index the source anyway ([`FINISH_WGSL`]'s own rationale for
/// not making rotation a pass). A separate CM pass over a 48 MP picture would cost a 195 MB read
/// and a 195 MB write of VRAM to move bytes the resampler had just written.
///
/// # The two contracts
///
/// [`FinishOut::SourceRgb`] is the v0.8.147 contract VERBATIM — packed RGB8, 24 bpp, stride `w*3`,
/// un-colour-managed in the file's own gamut — and it is byte-unchanged: with the managed bit
/// clear the shader's `copy_word`/`out_word` reduce to the identical `pack`/copy they were.
///
/// [`FinishOut::ManagedRgba`] is WAVE 1's: RGBA8, 32 bpp, stride `w*4`, colour-managed `src`→`dst`
/// by the shared `rot_uv_cm_core!` arithmetic (the same shader SOURCE `CM_SHADER` splices), alpha
/// opaque. `src == dst` is a pass-through — the colour is not round-tripped through
/// `linearize`/`encode`, which is `CM_SHADER`'s own discipline and the reason a P3 file on a P3
/// output is bit-exact rather than within-a-LSB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishOut {
    /// Packed RGB8, 24 bpp, in the file's own gamut. The shipped contract.
    SourceRgb,
    /// RGBA8, 32 bpp, already converted into `dst`. Display-ready: the caller stages it with a
    /// plain texture write and the renderer runs NO colour pass over it at all.
    ManagedRgba { src: Gamut, dst: Gamut },
}

impl FinishOut {
    /// Bytes per finished pixel — 3 for the source-gamut contract, 4 for the managed one.
    #[inline]
    pub fn bytes_per_px(self) -> usize {
        match self {
            FinishOut::SourceRgb => 3,
            FinishOut::ManagedRgba { .. } => 4,
        }
    }
}

/// The container's mirror, as `imir` spells it. Named rather than `bool`-ed because the two axes
/// are trivially confusable and a photo mirrored the wrong way looks perfectly plausible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mirror {
    /// Mirrored about a VERTICAL axis, i.e. left/right (`imir` bit 1).
    Vertical,
    /// Mirrored about a HORIZONTAL axis, i.e. top/bottom (`imir` bit 0).
    Horizontal,
}

/// Everything the assembly needs to know about a HEIF grid's geometry. Every field comes from E1's
/// `HeifGridPlan`; this struct exists so `falcon-gpu` does not have to name E1's types (and so the
/// falsifiers have one obvious place to bend a number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridGeometry {
    /// `cols × tile_w` by `rows × tile_h` — the canvas the tiles compose onto.
    pub mosaic_w: u32,
    pub mosaic_h: u32,
    /// The rect of the mosaic that survives the grid trim and any `clap`, PRE-rotation.
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_w: u32,
    pub crop_h: u32,
    /// `irot` as quarter-turns COUNTER-CLOCKWISE: 0, 1, 2, 3 for 0°, 90°, 180°, 270°.
    pub rot_quarters: u32,
    pub mirror: Option<Mirror>,
}

impl GridGeometry {
    /// Post-crop, POST-rotation dimensions — the size the shipping WIC path answers with, and the
    /// size `finish_source` is handed.
    pub fn display(&self) -> (u32, u32) {
        if self.rot_quarters % 2 == 1 {
            (self.crop_h, self.crop_w)
        } else {
            (self.crop_w, self.crop_h)
        }
    }

    fn validate(&self) -> Result<()> {
        if self.mosaic_w == 0 || self.mosaic_h == 0 {
            bail!("empty mosaic {}x{}", self.mosaic_w, self.mosaic_h);
        }
        // The NV12 canvas has one chroma sample per 2×2 luma quad, so an odd mosaic edge would
        // leave a half-populated chroma column. Every real grid's tiles are even in both axes.
        if self.mosaic_w % 2 != 0 || self.mosaic_h % 2 != 0 {
            bail!("mosaic {}x{} is not even in both axes", self.mosaic_w, self.mosaic_h);
        }
        if self.crop_w == 0 || self.crop_h == 0 {
            bail!("empty crop {}x{}", self.crop_w, self.crop_h);
        }
        // v0.8.152 (R3-L9): `checked_add`, not `+`. This is the L28 class the tree already fixed
        // once and WROTE DOWN — `bmff_children`'s doc comment records that `i + size > end` "in
        // RELEASE WRAPS, and a wrapped sum ≤ end passed the guard" — and it was back here on a
        // `pub struct` with `pub` fields, where nothing at the type level bounds the inputs. Every
        // in-tree `GridGeometry` comes from `photo::geometry` over a validated `HeifGridPlan`, so
        // nothing reaches it today; a wrapped guard is not a thing to leave sitting in a validator.
        if self.crop_x.checked_add(self.crop_w).is_none_or(|v| v > self.mosaic_w)
            || self.crop_y.checked_add(self.crop_h).is_none_or(|v| v > self.mosaic_h)
        {
            bail!(
                "crop {}x{}+{}+{} falls outside the {}x{} mosaic",
                self.crop_w,
                self.crop_h,
                self.crop_x,
                self.crop_y,
                self.mosaic_w,
                self.mosaic_h
            );
        }
        if self.rot_quarters > 3 {
            bail!("irot quarter-turns {} is not 0..=3", self.rot_quarters);
        }
        Ok(())
    }
}

/// One decoded tile as the assembler wants it: the decoder's own bytes and the decoder's own
/// stride, borrowed. Deliberately NOT `falcon_hwdec::Nv12Image` — this crate cannot depend on that
/// one (the dependency runs the other way), and a borrowed view is what keeps the upload from
/// copying a photo's worth of tiles into a second buffer on the way past.
#[derive(Debug, Clone, Copy)]
pub struct Nv12Tile<'a> {
    /// `stride × h` bytes of luma followed by `stride × h/2` bytes of interleaved chroma — the
    /// D3D11 NV12 staging layout, verbatim.
    pub data: &'a [u8],
    pub stride: u32,
    pub w: u32,
    pub h: u32,
}

/// The two canvas textures a photo composites into, plus the geometry they were sized for.
///
/// Held across the whole decode: tiles land in it one at a time as the decoder produces them, so
/// the largest thing in system memory at any moment is one tile, not a photo.
pub struct Canvas {
    y: wgpu::Texture,
    uv: wgpu::Texture,
    geom: GridGeometry,
}

impl Canvas {
    pub fn geometry(&self) -> GridGeometry {
        self.geom
    }
}

/// The compiled assembly: the E2 kernel plus the two finish passes. Build once per device and
/// reuse — the same shape [`crate::GpuDeveloper`] and [`crate::Nv12Kernel`] already have.
pub struct HeicAssembler {
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter_name: String,
    limits: wgpu::Limits,
    nv12: Nv12Kernel,
    map_h: wgpu::ComputePipeline,
    resize_v: wgpu::ComputePipeline,
    finish_bgl: wgpu::BindGroupLayout,
    /// v0.8.165: the custom-profile inverse-tone-curve LUT texture on THIS device, cached by
    /// profile generation — the same `CustomLutTex` the renderer's CM pipeline holds, built from
    /// the same `falcon_color::custom_encode_lut`, so a custom output profile reaches the HEIC
    /// finish pass by exactly the route it reaches the JPEG one. A `Mutex` (not `YuvConvert`'s
    /// `RefCell`): an assembler is shared across the whole decode pool through an `Arc`.
    lut: std::sync::Mutex<Option<CustomLutTex>>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct FinishUniforms {
    /// x = mosaic_w, y = mosaic_h, z = crop_x, w = crop_y
    m: [u32; 4],
    /// x = crop_w, y = crop_h, z = rot quarter-turns CCW, w = mirror (0 none, 1 vertical axis, 2 horizontal axis)
    g: [u32; 4],
    /// x = disp_w, y = disp_h, z = this pass's output width, w = this pass's output height
    d: [u32; 4],
    /// x = source width of this pass, y = source height of this pass, z/w = pad
    s: [u32; 4],
    /// v0.8.165: the src→dst matrix rows, packed exactly as `create_texture_cm` packs them for
    /// `CM_SHADER` (std140, one `vec4` per row, the 4th lane pad).
    c0: [f32; 4],
    c1: [f32; 4],
    c2: [f32; 4],
    /// v0.8.165: x = src TRC kind, y = dst TRC kind, z = bit0 "this pass writes the FINISHED
    /// RGBA" | bit1 "…and converts the gamut (src != dst)", w = the inverse-LUT width
    /// (`CUSTOM_LUT_N` for a Custom dst, 0 otherwise — the named-gamut branches ignore it). The
    /// bit1/bit0 split is `CM_SHADER`'s `flags.z` bit0 discipline: a pass-through must not
    /// round-trip an unconverted pixel through `linearize`/`encode`.
    cf: [u32; 4],
}

/// The finish shader: the crop/rotate/mirror gather and both halves of the Lanczos3 downsample.
///
/// Public so a harness can assert its shape, the same way [`crate::NV12_RGB_WGSL`] and
/// [`crate::CM_SHADER`] are. Unlike the E2 kernel this one is `f32` throughout and deliberately so:
/// it is reproducing `fast_image_resize`'s convolution, which is float-shaped arithmetic, and it is
/// a REPORTED difference rather than a byte-pinned one. The byte-pinned kernel is the other file.
pub const FINISH_WGSL: &str = concat!(
r#"
struct Fin {
    m: vec4<u32>,   // mosaic_w, mosaic_h, crop_x, crop_y
    g: vec4<u32>,   // crop_w, crop_h, rot_quarters (CCW), mirror
    d: vec4<u32>,   // disp_w, disp_h, out_w, out_h
    s: vec4<u32>,   // src_w, src_h, pad, pad
    c0: vec4<f32>,  // v0.8.165: src->dst matrix row 0 (xyz) + pad
    c1: vec4<f32>,
    c2: vec4<f32>,
    cf: vec4<u32>,  // x = src TRC kind, y = dst TRC kind,
                    // z = bit0 write finished RGBA | bit1 convert the gamut,
                    // w = the inverse-tone-curve LUT width (read only when dst kind == 2)
};
@group(0) @binding(0) var<storage, read> src_px: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst_px: array<u32>;
@group(0) @binding(2) var<uniform> F: Fin;
// v0.8.165: the per-channel inverse tone curve LUT (linear->device) for a custom display profile,
// N x 3 (R/G/B rows) — the SAME texture and the same `textureLoad` the two fast-tier shaders use.
@group(0) @binding(3) var lut_tex: texture_2d<f32>;

const PI: f32 = 3.14159265358979323846;

fn sinc(x: f32) -> f32 {
    if (x == 0.0) { return 1.0; }
    let p = PI * x;
    return sin(p) / p;
}

// Lanczos with a = 3 — the filter `FilterType::Lanczos3` names, support 3.
fn lanczos3(x: f32) -> f32 {
    if (x <= -3.0 || x >= 3.0) { return 0.0; }
    return sinc(x) * sinc(x / 3.0);
}

fn unpack(w: u32) -> vec3<f32> {
    return vec3<f32>(f32(w & 255u), f32((w >> 8u) & 255u), f32((w >> 16u) & 255u));
}

fn pack(c: vec3<f32>) -> u32 {
    let q = clamp(floor(c + vec3<f32>(0.5)), vec3<f32>(0.0), vec3<f32>(255.0));
    return u32(q.x) | (u32(q.y) << 8u) | (u32(q.z) << 16u);
}
"#,
    rot_uv_cm_core!(),
r#"
// ── v0.8.165 (WAVE 1) — THE LAST PASS'S WRITE ───────────────────────────────────────────────────
//
// `cf.z` bit 0 CLEAR is the v0.8.147 contract, unchanged in every bit: the source-gamut word with
// a zero top byte, which the readback compacts to 24 bpp. SET means this pass is the LAST one and
// the caller asked for a finished frame, so it writes RGBA8 with an opaque alpha — and, when bit 1
// is also set (src != dst), colour-managed through the `linearize`/`encode` core spliced in above.
// That core is the same shader SOURCE `CM_SHADER` splices, and `pack`'s round-half-up quantisation
// is `falcon_color::apply_pixel`'s `(v * 255.0 + 0.5)` — the two ends of the fidelity gate.
//
// The bit-1 pass-through matters: `linearize` then `encode` is not an identity in f32, so a P3
// file on a P3 output would drift by up to an LSB per channel if the unmanaged case went through
// the maths. `CM_SHADER` refuses it for the same reason and this refuses it in the same shape.
fn out_word(c: vec3<f32>) -> u32 {
    if ((F.cf.z & 1u) == 0u) { return pack(c); }
    if ((F.cf.z & 2u) == 0u) { return pack(c) | (255u << 24u); }
    let x = clamp(c / 255.0, vec3<f32>(0.0), vec3<f32>(1.0));
    let st = F.cf.x;
    let dt = F.cf.y;
    let n = F.cf.w;
    let lr = linearize(x.r, st, 0, n);
    let lg = linearize(x.g, st, 1, n);
    let lb = linearize(x.b, st, 2, n);
    let m0 = F.c0.xyz; let m1 = F.c1.xyz; let m2 = F.c2.xyz;
    let orr = encode(m0.x * lr + m0.y * lg + m0.z * lb, dt, 0, n);
    let og  = encode(m1.x * lr + m1.y * lg + m1.z * lb, dt, 1, n);
    let ob  = encode(m2.x * lr + m2.y * lg + m2.z * lb, dt, 2, n);
    return pack(vec3<f32>(orr, og, ob) * 255.0) | (255u << 24u);
}

// The identity-copy arms' write. With the managed bit clear this is the literal word copy the two
// passes did before v0.8.165 — no unpack, no repack, no float anywhere near an untouched pixel.
fn copy_word(w: u32) -> u32 {
    if ((F.cf.z & 1u) == 0u) { return w; }
    return out_word(unpack(w));
}

// The DISPLAY-space gather: undo the mirror, undo the rotation, offset by the crop, and index the
// mosaic. `u`,`v` are display coordinates and the result is a word index into the mosaic RGB buffer.
fn mosaic_index(u: i32, v: i32) -> u32 {
    var a = u;
    var b = v;
    if (F.g.w == 1u) {
        a = i32(F.d.x) - 1 - a;      // mirrored about a vertical axis: left <-> right
    } else if (F.g.w == 2u) {
        b = i32(F.d.y) - 1 - b;      // mirrored about a horizontal axis: top <-> bottom
    }
    let cw = i32(F.g.x);
    let ch = i32(F.g.y);
    var x: i32;
    var y: i32;
    switch (F.g.z) {
        case 1u: { x = cw - 1 - b; y = a; }          // 90 CCW:  src (x,y) -> (y, cw-1-x)
        case 2u: { x = cw - 1 - a; y = ch - 1 - b; } // 180
        case 3u: { x = b;          y = ch - 1 - a; } // 270 CCW (= 90 CW)
        default: { x = a;          y = b; }          // 0
    }
    let mx = i32(F.m.z) + clamp(x, 0, cw - 1);
    let my = i32(F.m.w) + clamp(y, 0, ch - 1);
    return u32(my) * F.m.x + u32(mx);
}

// PASS A — crop + irot + imir, then resample along the DISPLAY X axis.
// Output is (out_w x disp_h); the source row index is a display row, so the map does all the work.
@compute @workgroup_size(8, 8)
fn main_h(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ow = F.d.z;
    let oh = F.d.y;                 // pass A never changes the height
    if (gid.x >= ow || gid.y >= oh) { return; }
    let iw = F.d.x;                 // display width
    let oy = i32(gid.y);

    if (ow == iw) {
        // Identity on this axis: a straight mapped copy. A Lanczos with scale 1 would low-pass an
        // image nobody asked to resize, so the equal case is a copy and not a convolution.
        dst_px[gid.y * ow + gid.x] = copy_word(src_px[mosaic_index(i32(gid.x), oy)]);
        return;
    }

    let ss = f32(iw) / f32(ow);
    let fscale = max(ss, 1.0);
    let support = 3.0 * fscale;
    let center = (f32(gid.x) + 0.5) * ss;
    var kmin = i32(center - support + 0.5);
    if (kmin < 0) { kmin = 0; }
    var kmax = i32(center + support + 0.5);
    if (kmax > i32(iw)) { kmax = i32(iw); }

    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    for (var k = kmin; k < kmax; k = k + 1) {
        let wt = lanczos3((f32(k) - center + 0.5) / fscale);
        wsum = wsum + wt;
        acc = acc + wt * unpack(src_px[mosaic_index(k, oy)]);
    }
    if (wsum != 0.0) { acc = acc / wsum; }
    dst_px[gid.y * ow + gid.x] = out_word(acc);
}

// PASS B — resample along Y. The source is pass A's output, already in display orientation, so
// there is no map here at all.
@compute @workgroup_size(8, 8)
fn main_v(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ow = F.d.z;
    let oh = F.d.w;
    if (gid.x >= ow || gid.y >= oh) { return; }
    let ih = F.s.y;

    if (oh == ih) {
        dst_px[gid.y * ow + gid.x] = copy_word(src_px[u32(gid.y) * ow + gid.x]);
        return;
    }

    let ss = f32(ih) / f32(oh);
    let fscale = max(ss, 1.0);
    let support = 3.0 * fscale;
    let center = (f32(gid.y) + 0.5) * ss;
    var kmin = i32(center - support + 0.5);
    if (kmin < 0) { kmin = 0; }
    var kmax = i32(center + support + 0.5);
    if (kmax > i32(ih)) { kmax = i32(ih); }

    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    for (var k = kmin; k < kmax; k = k + 1) {
        let wt = lanczos3((f32(k) - center + 0.5) / fscale);
        wsum = wsum + wt;
        acc = acc + wt * unpack(src_px[u32(k) * ow + gid.x]);
    }
    if (wsum != 0.0) { acc = acc / wsum; }
    dst_px[gid.y * ow + gid.x] = out_word(acc);
}
"#,
);

impl HeicAssembler {
    /// Build on a device of the caller's own. E5 will pass the app's renderer device here; the
    /// tests and the probe use [`HeicAssembler::headless`].
    pub fn on_device(device: wgpu::Device, queue: wgpu::Queue, adapter_name: String) -> Result<Self> {
        let limits = device.limits();
        let nv12 = Nv12Kernel::new(&device)?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("falcon-heic-finish"),
            source: wgpu::ShaderSource::Wgsl(FINISH_WGSL.into()),
        });
        let storage = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let finish_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("falcon-heic-finish-bgl"),
            entries: &[
                storage(0, true),
                storage(1, false),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // v0.8.165: the custom-profile inverse-LUT (R32Float — see `CustomLutTex`), fetched
                // via `textureLoad` in the shared core's kind-2 encode branch, so no sampler. Bound
                // on EVERY finish, managed or not: `ensure_custom_lut` always populates the cache
                // (a gamma-2.2 stand-in when no profile is installed), so the binding is never
                // absent and the unmanaged path simply never reads it.
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("falcon-heic-finish-pl"),
            bind_group_layouts: &[Some(&finish_bgl)],
            immediate_size: 0,
        });
        let make = |entry: &str, label: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&pl),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let map_h = make("main_h", "falcon-heic-map-h");
        let resize_v = make("main_v", "falcon-heic-resize-v");
        Ok(HeicAssembler {
            device,
            queue,
            adapter_name,
            limits,
            nv12,
            map_h,
            resize_v,
            finish_bgl,
            lut: std::sync::Mutex::new(None),
        })
    }

    /// A device of this module's own, asking for the ADAPTER'S limits rather than the defaults: a
    /// 48 MP mosaic's RGB is a 198 MB storage buffer and the 128 MiB default binding size would
    /// refuse it. [`HeicAssembler::mosaic_fits`] is the honest pre-check for the same fact.
    pub fn headless() -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .context("no GPU adapter available")?;
        let adapter_name = adapter.get_info().name;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .context("failed to create GPU device")?;
        Self::on_device(device, queue, adapter_name)
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Can this device hold a mosaic that size? `Err` names the limit that says no, so a decline is
    /// a sentence in a log rather than a wgpu validation panic. E5's capability probe wants this.
    pub fn mosaic_fits(&self, geom: &GridGeometry) -> Result<()> {
        geom.validate()?;
        let dim = self.limits.max_texture_dimension_2d;
        if geom.mosaic_w > dim || geom.mosaic_h > dim {
            bail!(
                "mosaic {}x{} exceeds this device's max texture dimension {dim}",
                geom.mosaic_w,
                geom.mosaic_h
            );
        }
        let px = (geom.mosaic_w as u64) * (geom.mosaic_h as u64) * 4;
        if px > u64::from(self.limits.max_storage_buffer_binding_size) {
            bail!(
                "the mosaic's RGB is {px} bytes, over this device's {} byte storage binding limit",
                self.limits.max_storage_buffer_binding_size
            );
        }
        if px > self.limits.max_buffer_size {
            bail!(
                "the mosaic's RGB is {px} bytes, over this device's {} byte buffer limit",
                self.limits.max_buffer_size
            );
        }
        Ok(())
    }

    /// Allocate the NV12 canvas for one photo. Zero-initialised by wgpu, so a grid whose tiles do
    /// not cover the mosaic reads black rather than whatever was in VRAM.
    pub fn canvas(&self, geom: GridGeometry) -> Result<Canvas> {
        self.mosaic_fits(&geom)?;
        let make = |w: u32, h: u32, fmt: wgpu::TextureFormat, label: &str| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: fmt,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let y = make(geom.mosaic_w, geom.mosaic_h, wgpu::TextureFormat::R8Uint, "falcon-heic-canvas-y");
        let uv = make(
            geom.mosaic_w / 2,
            geom.mosaic_h / 2,
            wgpu::TextureFormat::Rg8Uint,
            "falcon-heic-canvas-uv",
        );
        Ok(Canvas { y, uv, geom })
    }

    /// Place one decoded tile at luma origin `(dst_x, dst_y)` in the canvas.
    ///
    /// The origin is the CALLER'S arithmetic on purpose: it is `col × tile_w`, `row × tile_h` in
    /// every real case, and making it an argument is what lets the seam falsifier put a tile one
    /// pixel out and prove the seam hunt can see it. Both the origin and the tile size must be even
    /// — a tile on an odd boundary has no chroma sample of its own to land on.
    pub fn write_tile(&self, canvas: &Canvas, tile: &Nv12Tile<'_>, dst_x: u32, dst_y: u32) -> Result<()> {
        if tile.w % 2 != 0 || tile.h % 2 != 0 {
            bail!("tile {}x{} is not even in both axes", tile.w, tile.h);
        }
        if dst_x % 2 != 0 || dst_y % 2 != 0 {
            bail!("tile origin ({dst_x},{dst_y}) is not on a chroma sample");
        }
        // v0.8.152 (R3-L9): same class, same fix — and this one takes its origin from the CALLER'S
        // arithmetic by design (that is what lets the seam falsifier put a tile two pixels out).
        if dst_x.checked_add(tile.w).is_none_or(|v| v > canvas.geom.mosaic_w)
            || dst_y.checked_add(tile.h).is_none_or(|v| v > canvas.geom.mosaic_h)
        {
            bail!(
                "tile {}x{} at ({dst_x},{dst_y}) falls outside the {}x{} mosaic",
                tile.w,
                tile.h,
                canvas.geom.mosaic_w,
                canvas.geom.mosaic_h
            );
        }
        let stride = tile.stride as usize;
        if stride < tile.w as usize {
            bail!("tile stride {stride} is narrower than its {} px row", tile.w);
        }
        let y_bytes = stride * tile.h as usize;
        let uv_bytes = stride * (tile.h as usize / 2);
        if tile.data.len() < y_bytes + uv_bytes {
            bail!("tile carries {} bytes, needs {}", tile.data.len(), y_bytes + uv_bytes);
        }
        fn dest(tex: &wgpu::Texture, x: u32, y: u32) -> wgpu::TexelCopyTextureInfo<'_> {
            wgpu::TexelCopyTextureInfo {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            }
        }
        self.queue.write_texture(
            dest(&canvas.y, dst_x, dst_y),
            &tile.data[..y_bytes],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(tile.stride),
                rows_per_image: Some(tile.h),
            },
            wgpu::Extent3d { width: tile.w, height: tile.h, depth_or_array_layers: 1 },
        );
        self.queue.write_texture(
            dest(&canvas.uv, dst_x / 2, dst_y / 2),
            &tile.data[y_bytes..y_bytes + uv_bytes],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(tile.stride),
                rows_per_image: Some(tile.h / 2),
            },
            wgpu::Extent3d { width: tile.w / 2, height: tile.h / 2, depth_or_array_layers: 1 },
        );
        Ok(())
    }

    /// The dims `finish_source` would answer with for this photo at `scale_to`, derived through
    /// falcon-decode's OWN [`falcon_decode::scaled_dims`] so the two cannot drift.
    pub fn output_dims(geom: &GridGeometry, scale_to: Option<u32>) -> (u32, u32) {
        let (w, h) = geom.display();
        match scale_to {
            // `resize_to_long` is a no-op when the long side already fits — including the
            // equality case, which is why this is `<=` and not `<`.
            Some(long) if w.max(h) > long => falcon_decode::scaled_dims(w, h, long),
            _ => (w, h),
        }
    }

    /// Convert, crop, rotate, downsample, read back. The ONE readback in the chain, and it carries
    /// the finished picture and nothing else.
    ///
    /// `coeffs`/`siting` come from the file's SPS VUI (never guessed — E2's `params_from_vui`
    /// refuses what it does not model), and are passed as the resolved Q16 integers so a container
    /// `colr` that overrides the VUI has a way in that does not route through `YuvParams`.
    pub fn finish(
        &self,
        canvas: &Canvas,
        siting: ChromaSiting,
        coeffs: YuvCoeffs,
        scale_to: Option<u32>,
    ) -> Result<(Vec<u8>, u32, u32)> {
        let (ow, oh) = Self::output_dims(&canvas.geom, scale_to);
        self.finish_to(canvas, siting, coeffs, ow, oh)
    }

    /// [`HeicAssembler::finish`] with the output dims stated outright rather than derived.
    ///
    /// Only two callers want this: E5, which may know a tier's exact frame size, and the dims
    /// falsifier, which perturbs one scale computation and requires the gate to redden.
    pub fn finish_to(
        &self,
        canvas: &Canvas,
        siting: ChromaSiting,
        coeffs: YuvCoeffs,
        ow: u32,
        oh: u32,
    ) -> Result<(Vec<u8>, u32, u32)> {
        self.finish_out(canvas, siting, coeffs, ow, oh, FinishOut::SourceRgb)
    }

    /// v0.8.165 (WAVE 1) — [`HeicAssembler::finish_to`] with the OUTPUT CONTRACT stated.
    ///
    /// [`FinishOut::SourceRgb`] is `finish_to` and is byte-unchanged from v0.8.147.
    /// [`FinishOut::ManagedRgba`] folds the gamut transform into the pass that was already writing
    /// the finished pixel and hands back display-ready RGBA8 — so the CPU never expands RGB→RGBA
    /// (a 146→195 MB pass measured at 50–76 ms on a 48 MP photo) and never runs
    /// `falcon_color::transform_rgba` over it (110–240 ms), and the renderer stages it with a plain
    /// texture write instead of a colour pass into a second 195 MB texture.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_out(
        &self,
        canvas: &Canvas,
        siting: ChromaSiting,
        coeffs: YuvCoeffs,
        ow: u32,
        oh: u32,
        out_mode: FinishOut,
    ) -> Result<(Vec<u8>, u32, u32)> {
        let geom = canvas.geom;
        geom.validate()?;
        if ow == 0 || oh == 0 {
            bail!("output {ow}x{oh} is empty");
        }
        let (dw, dh) = geom.display();
        if ow > dw || oh > dh {
            bail!("output {ow}x{oh} is larger than the {dw}x{dh} display image — this path never upscales");
        }
        let (mw, mh) = (geom.mosaic_w, geom.mosaic_h);

        let words = |w: u32, h: u32| (w as u64) * (h as u64) * 4;
        let storage = |size: u64, label: &str| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        let rgb_mosaic = storage(words(mw, mh), "falcon-heic-rgb-mosaic");

        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("falcon-heic-enc") });

        // ── 2. CONVERT — the E2 kernel over the whole mosaic ──
        let y_view = canvas.y.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = canvas.uv.create_view(&wgpu::TextureViewDescriptor::default());
        self.nv12.encode(
            &self.device,
            &self.queue,
            &mut enc,
            &y_view,
            &uv_view,
            &rgb_mosaic,
            mw,
            mh,
            mw / 2,
            mh / 2,
            siting,
            coeffs,
        );

        // ── 3. MAP + RESAMPLE-X, and ── 4. RESAMPLE-Y when the height actually changes ──
        let a = storage(words(ow, dh), "falcon-heic-pass-a");
        let mirror = match geom.mirror {
            None => 0u32,
            Some(Mirror::Vertical) => 1,
            Some(Mirror::Horizontal) => 2,
        };
        // v0.8.165: the colour half of the uniform. The matrix and the TRC kinds are packed exactly
        // as `create_texture_cm` packs them for `CM_SHADER`, because the shader that reads them is
        // the same source text. `last` marks the pass that writes the finished pixel — the CM runs
        // AFTER the resample, which is the order the CPU path used (assemble in the source gamut,
        // then `transform_rgba`), so this is a relocation of the transform and not a reordering.
        let (cm_rows, cm_flags) = match out_mode {
            FinishOut::SourceRgb => ([[0.0f32; 4]; 3], [0u32; 4]),
            FinishOut::ManagedRgba { src, dst } => {
                let m = falcon_color::src_to_dst_matrix(src, dst);
                let mut rows = [[0.0f32; 4]; 3];
                for (r, row) in m.iter().enumerate() {
                    rows[r][..3].copy_from_slice(row);
                }
                let lut_w = crate::lut_width_for(src, dst);
                (rows, [src.trc_kind(), dst.trc_kind(), 1 | (((src != dst) as u32) << 1), lut_w])
            }
        };
        let uni = |d: [u32; 4], s: [u32; 4], last: bool| FinishUniforms {
            m: [mw, mh, geom.crop_x, geom.crop_y],
            g: [geom.crop_w, geom.crop_h, geom.rot_quarters, mirror],
            d,
            s,
            c0: cm_rows[0],
            c1: cm_rows[1],
            c2: cm_rows[2],
            cf: if last { cm_flags } else { [cm_flags[0], cm_flags[1], 0, cm_flags[3]] },
        };
        // The LUT texture on THIS device, refreshed only when the profile generation moves.
        let mut lut_guard = self.lut.lock().unwrap_or_else(|e| e.into_inner());
        // v0.8.177: keyed on the faithful SOURCE as well as the destination profile.
        let lut_src = match out_mode {
            FinishOut::ManagedRgba { src, .. } => src,
            FinishOut::SourceRgb => Gamut::Srgb,
        };
        ensure_custom_lut(&mut lut_guard, &self.device, &self.queue, lut_src);
        let lut_view = &lut_guard.as_ref().expect("ensure_custom_lut populates the cache").view;

        let v_pass = oh != dh;
        self.pass(
            &mut enc,
            &self.map_h,
            &rgb_mosaic,
            &a,
            lut_view,
            uni([dw, dh, ow, dh], [dw, dh, 0, 0], !v_pass),
            ow,
            dh,
        );

        let (final_buf, final_bytes) = if !v_pass {
            (a, words(ow, dh))
        } else {
            let b = storage(words(ow, oh), "falcon-heic-pass-b");
            self.pass(
                &mut enc,
                &self.resize_v,
                &a,
                &b,
                lut_view,
                uni([dw, dh, ow, oh], [ow, dh, 0, 0], true),
                ow,
                oh,
            );
            (b, words(ow, oh))
        };

        self.queue.submit([enc.finish()]);
        // ── v0.8.181 (pre-merge review): THE LUT LOCK ENDS AT THE SUBMIT. ───────────────────────
        // The guard used to live to the end of this function, which put the whole readback AND the
        // whole CPU unpack inside it — and an assembler is ONE `Arc` shared by the entire decode
        // pool, so every other worker's `finish_out` queued behind a mutex it needed only to read a
        // texture view. It does not need it that long: the bind groups built above hold their own
        // reference to the view, and the commands that use them are submitted on the line above, so
        // from here on nothing in this function reads `lut_guard`. What the extra scope actually
        // cost was the ~20–25 ms of per-frame CPU unpack that could have overlapped another
        // worker's DMA (see `read_back`'s own measurements). Released here.
        drop(lut_guard);
        // v0.8.166 (WAVE 2): the copy back is no longer part of that submission — it is its own
        // band-sliced, double-buffered chain. See [`HeicAssembler::read_back`].
        //
        // v0.8.181: the result is CAPTURED, not `?`-propagated here — see the reclamation note
        // below, which an early return used to skip.
        let px = self.read_back(&final_buf, final_bytes, ow, oh, out_mode);
        // v0.8.165 (iGPU deliverable 3): the storage buffers and the readback buffer die at the end
        // of this scope, and on an iGPU every one of them is SYSTEM RAM — a 48 MP photo's mosaic
        // alone is 198 MB. wgpu frees a dropped resource on the owning device's next poll, and this
        // device's next poll is the NEXT photo's — so a browse that stops on a HEIC used to leave
        // the whole chain resident until something else asked the assembler for work. Dropping and
        // polling here is the v0.2.34 lesson (`device.poll(Poll)` to reclaim, or an iGPU's RAM
        // climbs) applied to the one device in this app that had no per-tick poll of its own.
        // v0.8.166: `read_back`'s own band buffers dropped at its return, so this one poll still
        // reclaims every allocation the chain made — including the new ones.
        //
        // v0.8.181 (pre-merge review): ON EVERY EXIT, WHICH IS THE ONLY WAY THIS PARAGRAPH IS TRUE.
        // The readback's `?` sat ABOVE this block, so the one path where the reclamation matters
        // most — a failed map on a struggling adapter — was the one path that ran none of it: the
        // mosaic and the finished buffer went out of scope with no poll behind them, and this
        // device's next poll is the NEXT photo's. On an iGPU that is a 198 MB mosaic left in system
        // RAM until something else asks the assembler for work, after an error that suggests
        // nothing else will. Same drops, same poll, then the error propagates.
        drop(final_buf);
        drop(rgb_mosaic);
        let _ = self.device.poll(wgpu::PollType::Poll);
        Ok((px?, ow, oh))
    }

    /// v0.8.166 (WAVE 2) — **the readback, band-sliced and pipelined against its own unpack.**
    ///
    /// WHAT WAS MEASURED. The 08-06 campaign's one regression: on `heicbig`, WAVE 1's GPU-colour
    /// arm published at 86/80 ms against the CPU arm's 73/66, and the hypothesis was the extra
    /// bytes — the managed contract reads back 4 bytes per pixel (195 MB at 48 MP) where the source
    /// contract read 3 (146 MB), and that device→host burst shares one adapter with the renderer's
    /// own host→device upload.
    ///
    /// WHAT GROUNDING CORRECTED. The two transfers cannot overlap FOR THE SAME FRAME and never
    /// could: this function runs on a decode worker and returns a `Vec`, and only then does
    /// `det_tx` carry the frame to the U1 upload thread. So the spec's stated minimum ("the readback
    /// must not overlap the renderer's own upload of the SAME frame") holds structurally at tip.
    /// What the frame actually paid was SERIAL: one ~195 MB DMA, and THEN one ~195 MB CPU copy out
    /// of the mapped range — each ~20–25 ms at 48 MP, one after the other, plus a single ~195 MB
    /// host-visible allocation to hold it.
    ///
    /// SO THIS PIPELINES THE PAIR THAT ACTUALLY EXISTS. Two band buffers alternate: band k+1's copy
    /// is submitted BEFORE band k is mapped and drained, so the DMA of one band runs while the CPU
    /// unpacks the previous one. The span becomes ≈ max(DMA, unpack) instead of DMA + unpack, and
    /// the peak host-visible allocation falls from the whole picture to
    /// `2 × band::BAND_TARGET_BYTES` = 32 MiB — which on an iGPU is 32 MiB of SYSTEM RAM instead of
    /// 195 MB, the same shared-memory argument the upload side makes, on the other end of the wire.
    ///
    /// FAIL-SOFT: a picture small enough that `band::plan` answers `whole` takes the v0.8.165 path
    /// verbatim — one buffer, one copy, one map — so the cheap case pays nothing for the machinery.
    /// So does every picture when `FALCON_HEIC_READBACK_BANDS=0` (v0.8.167): that lever is this
    /// pipeline's own A/B arm, and it replaced `FALCON_PUBLISH_BANDS`, which governed the
    /// renderer-side banding the 08-06 audit reverted. See [`crate::band`] for why that half went
    /// and this half stayed.
    ///
    /// BYTE-IDENTITY: the bands are a partition of one linear buffer and the unpack is per-word, so
    /// the output is the same bytes in the same order as the one-shot form. `band::plan` cuts on
    /// whole rows, so every offset here is a multiple of `ow*4` — comfortably inside
    /// `copy_buffer_to_buffer`'s 4-byte alignment requirement.
    fn read_back(
        &self,
        final_buf: &wgpu::Buffer,
        final_bytes: u64,
        ow: u32,
        oh: u32,
        out_mode: FinishOut,
    ) -> Result<Vec<u8>> {
        read_back_planned(
            &self.device,
            &self.queue,
            final_buf,
            final_bytes,
            ow,
            oh,
            out_mode,
            crate::band::plan(ow, oh, 4),
        )
    }
}

/// [`HeicAssembler::read_back`]'s body with the plan STATED — the seam the parity gate drives.
///
/// It is a free function taking `device`/`queue` for exactly one reason: the byte-identity of the
/// banded form against the whole form is the load-bearing claim of WAVE 2's HEIC half, and a claim
/// that can only be tested by assembling a real 48 MP photo is a claim that does not get tested on
/// a machine without the corpus. This form is drivable with a synthetic buffer on any adapter, so
/// `tests::the_banded_readback_is_byte_identical_to_the_whole_one` is a REAL gate rather than a
/// conditional one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_back_planned(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    final_buf: &wgpu::Buffer,
    final_bytes: u64,
    ow: u32,
    oh: u32,
    out_mode: FinishOut,
    plan: crate::band::BandPlan,
) -> Result<Vec<u8>> {
    {
        let n = (ow as usize) * (oh as usize);
        let mut px: Vec<u8> = Vec::with_capacity(n * out_mode.bytes_per_px());
        // The read buffer always holds ONE WORD PER PIXEL — the storage address space has no byte
        // writes, so the shader writes words whatever the output contract is. `out_mode` decides
        // only how many of each word's bytes survive the unpack below.
        let row = u64::from(ow) * 4;
        let alloc = |size: u64| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("falcon-heic-read"),
                size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        };
        // Copy `len` bytes at `off` of the finished buffer into `buf`, submit it, and arm the map.
        // Answers the submission index so the drain can wait for THAT band rather than for the most
        // recent one (which is the next band, still running — waiting for it would undo the
        // pipeline).
        let issue = |buf: &wgpu::Buffer,
                     off: u64,
                     len: u64|
         -> (wgpu::SubmissionIndex, std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>) {
            let mut e = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("falcon-heic-read-band"),
            });
            e.copy_buffer_to_buffer(final_buf, off, buf, 0, len);
            let idx = queue.submit([e.finish()]);
            let (tx, rx) = std::sync::mpsc::channel();
            buf.slice(0..len).map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
            (idx, rx)
        };
        // ── v0.8.149 (A12): THE ONE UNBOUNDED WAIT IN THIS LANE, stated rather than left to be
        // discovered. `Wait` with no timeout has none by construction, so a wedged or
        // TDR-recovering adapter parks the calling thread for as long as the driver takes — and
        // this function runs on a fast-pool worker or on the sole detail worker, both of which the
        // app's own tick counts as ACTIVE while they are in it.
        //
        // v0.8.181 (pre-merge review) — HOW WIDE THAT PARK REALLY IS. A12 wrote "THIS WORKER
        // THREAD", and the paragraph below drew the containment claim from it: one stalled worker,
        // everything else falling soft. That is not the shape. All [`crate::heic`] work in this
        // process goes through ONE `HeicAssembler` — one `wgpu::Device`, held by the whole decode
        // pool through an `Arc` — so a wedged adapter parks every worker that reaches this wait,
        // which is every HEIC assembly in flight, up to all `hwheic::MAX_SESSIONS` of them. The
        // containment that survives is the one at the level below: files that have not started yet
        // never get a session, `HW_WAIT_MS` expires on them, and they fall soft to WIC. So the
        // honest bound is "the assemblies already running are lost; the queue behind them is not."
        //
        // It is left indefinite deliberately, and the reason is that the alternative is worse:
        // wgpu's poll has no cancel, so a "timed out" return would leave the submission still
        // running against buffers this function is about to drop. The bound that DOES exist is one
        // level up and it is the honest one — `hwheic::HW_WAIT_MS` bounds how long the NEXT file
        // waits for a session slot, so a wedged adapter costs the folder the assemblies already
        // in flight and a fall-soft to WIC for everything behind them, not a frozen browse.
        //
        // If this ever needs a real bound it is a `Device::lost` callback plus a rebuilt assembler,
        // not a timeout here. Recorded so the next reader does not have to re-derive that.
        // v0.8.166: the wait is now PER BAND (`submission_index: Some(idx)`) instead of "the most
        // recent submission" — that targeting is what lets band k+1 keep running while band k is
        // drained. Everything above about indefiniteness is unchanged.
        let drain = |idx: wgpu::SubmissionIndex,
                     rx: std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>|
         -> Result<()> {
            let _ = device.poll(wgpu::PollType::Wait {
                submission_index: Some(idx),
                timeout: None,
            });
            // The targeted wait maintains the device, which is what fires the map callback. The
            // second chance exists because "maintain ran" and "this buffer's mapping resolved" are
            // two facts, and only the first is guaranteed by the call above.
            let r = match rx.try_recv() {
                Ok(r) => r,
                Err(_) => {
                    let _ = device.poll(wgpu::PollType::wait_indefinitely());
                    rx.recv().context("heic assembly readback channel dropped")?
                }
            };
            r.context("failed to map the heic assembly readback buffer")?;
            Ok(())
        };
        // The one unpack: the shader writes one word per pixel, and the 24 bpp contract wants three
        // of the four bytes.
        //
        // v0.8.165: …and the MANAGED contract wants all four, which makes this a bulk copy instead
        // of a 48-million-iteration compaction that the caller then had to UNDO by expanding
        // RGB→RGBA again (`browse_frame_rgba`'s `exp_ms`, 50–76 ms at 48 MP, against a 195 MB
        // allocation). One `extend_from_slice` replaces both passes and the intermediate buffer.
        let unpack = |px: &mut Vec<u8>, mapped: &[u8]| match out_mode {
            FinishOut::SourceRgb => {
                for word in mapped.chunks_exact(4) {
                    px.extend_from_slice(&word[..3]);
                }
            }
            // The words are already `r | g<<8 | b<<16 | 255<<24`, i.e. RGBA8 in memory order on
            // every little-endian target — which is the byte order `Rgba8Unorm` wants.
            FinishOut::ManagedRgba { .. } => px.extend_from_slice(mapped),
        };

        if plan.is_whole() {
            // The v0.8.165 shape verbatim: one buffer, one copy, one map, one unpack.
            let buf = alloc(final_bytes);
            let (idx, rx) = issue(&buf, 0, final_bytes);
            drain(idx, rx)?;
            unpack(&mut px, &buf.slice(0..final_bytes).get_mapped_range());
            buf.unmap();
            return Ok(px);
        }

        // BANDED. Two buffers sized to the largest band; band k uses buffer k & 1, so the copy of
        // k+1 never targets the buffer k is mapped into.
        let band_bytes = row * u64::from(plan.rows);
        let bufs = [alloc(band_bytes), alloc(band_bytes)];
        let span = |k: u32| {
            let (first, rows) = plan.band(k, oh);
            (row * u64::from(first), row * u64::from(rows))
        };
        let mut pending = {
            let (off, len) = span(0);
            Some((issue(&bufs[0], off, len), len))
        };
        for k in 0..plan.bands {
            let ((idx, rx), len) = pending.take().expect("every band arms the next before draining");
            // Arm k+1 BEFORE draining k — this line IS the pipeline: its DMA runs on the device
            // while the CPU below copies band k out of the other buffer.
            if k + 1 < plan.bands {
                let (noff, nlen) = span(k + 1);
                pending = Some((issue(&bufs[((k + 1) & 1) as usize], noff, nlen), nlen));
            }
            let buf = &bufs[(k & 1) as usize];
            drain(idx, rx)?;
            unpack(&mut px, &buf.slice(0..len).get_mapped_range());
            buf.unmap();
        }
        Ok(px)
    }
}

impl HeicAssembler {
    #[allow(clippy::too_many_arguments)]
    fn pass(
        &self,
        enc: &mut wgpu::CommandEncoder,
        pipeline: &wgpu::ComputePipeline,
        src: &wgpu::Buffer,
        dst: &wgpu::Buffer,
        lut_view: &wgpu::TextureView,
        u: FinishUniforms,
        gx: u32,
        gy: u32,
    ) {
        let ubuf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("falcon-heic-finish-ubuf"),
            size: std::mem::size_of::<FinishUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&ubuf, 0, bytemuck::bytes_of(&u));
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("falcon-heic-finish-bind"),
            layout: &self.finish_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: src.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: dst.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: ubuf.as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(lut_view),
                },
            ],
        });
        let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("falcon-heic-finish-pass"),
            timestamp_writes: None,
        });
        cp.set_pipeline(pipeline);
        cp.set_bind_group(0, &bind, &[]);
        cp.dispatch_workgroups(gx.div_ceil(8), gy.div_ceil(8), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(rot: u32) -> GridGeometry {
        GridGeometry {
            mosaic_w: 8064,
            mosaic_h: 6144,
            crop_x: 0,
            crop_y: 0,
            crop_w: 8064,
            crop_h: 6048,
            rot_quarters: rot,
            mirror: None,
        }
    }

    #[test]
    fn display_swaps_on_the_quarter_turns_and_only_those() {
        assert_eq!(geom(0).display(), (8064, 6048));
        assert_eq!(geom(1).display(), (6048, 8064));
        assert_eq!(geom(2).display(), (8064, 6048));
        assert_eq!(geom(3).display(), (6048, 8064));
    }

    /// The dims contract, as arithmetic: `finish_source` resizes only when the long side EXCEEDS
    /// the ask, and its rounding is `scaled_dims`'. Both halves matter — an off-by-one on the
    /// `>` would resize a photo that was already the right size and change its short side.
    #[test]
    fn output_dims_are_finish_sources() {
        let g = geom(0);
        assert_eq!(HeicAssembler::output_dims(&g, None), (8064, 6048));
        assert_eq!(HeicAssembler::output_dims(&g, Some(8192)), (8064, 6048));
        assert_eq!(HeicAssembler::output_dims(&g, Some(8064)), (8064, 6048));
        assert_eq!(
            HeicAssembler::output_dims(&g, Some(2880)),
            falcon_decode::scaled_dims(8064, 6048, 2880)
        );
        // …and the rotated photo scales on its OWN long side, which is the height.
        let r = geom(3);
        assert_eq!(r.display(), (6048, 8064));
        assert_eq!(
            HeicAssembler::output_dims(&r, Some(2880)),
            falcon_decode::scaled_dims(6048, 8064, 2880)
        );
    }

    #[test]
    fn a_crop_outside_the_mosaic_is_refused() {
        let mut g = geom(0);
        g.crop_h = 6200;
        assert!(g.validate().is_err());
        g = geom(0);
        g.mosaic_w = 8065;
        assert!(g.validate().is_err(), "an odd mosaic edge has no chroma column to match");
    }

    /// The finish shader's shape. Not a colour pin — this shader is float by design — but the
    /// properties the geometry depends on, so a rewrite that still compiles cannot silently lose
    /// the identity-copy arm (which is what keeps a native-tier decode from being low-passed) or
    /// swap a rotation case.
    #[test]
    fn the_finish_shader_keeps_its_shape() {
        let s = FINISH_WGSL;
        assert!(s.contains("fn main_h"), "the map/horizontal pass is gone");
        assert!(s.contains("fn main_v"), "the vertical pass is gone");
        assert_eq!(s.matches("if (ow == iw)").count(), 1, "pass A lost its identity-copy arm");
        assert_eq!(s.matches("if (oh == ih)").count(), 1, "pass B lost its identity-copy arm");
        assert!(s.contains("sinc(x) * sinc(x / 3.0)"), "the filter is no longer Lanczos3");
        assert!(s.contains("3.0 * fscale"), "the support must stretch with the reduction");
        // All four rotations must be spelled out; a missing arm would silently become `default`.
        for arm in ["case 1u:", "case 2u:", "case 3u:", "default:"] {
            assert!(s.contains(arm), "mosaic_index lost its `{arm}` rotation arm");
        }
    }

    /// v0.8.166 (WAVE 2) — **THE READBACK PARITY GATE.**
    ///
    /// The banded, double-buffered readback must hand back the SAME BYTES, in the same order, as
    /// the one-shot form it replaces — for BOTH output contracts, because the two unpack the words
    /// differently (24 bpp drops a byte per pixel, 32 bpp does not) and a band boundary is exactly
    /// where a per-word unpack could lose or duplicate a pixel.
    ///
    /// It runs on a synthetic buffer on any adapter, deliberately: the alternative was to test this
    /// only where a 48 MP HEIC corpus and a D3D11 video device both exist, which is one machine.
    ///
    /// v0.8.167 (audit): and it asserts both forms against the SOURCE PAYLOAD, not merely against
    /// each other. Whole-vs-banded alone is satisfied by two identically WRONG readbacks — the
    /// commonest way for that to happen being a change to the unpack, which both arms share — so
    /// `expect` below is built from `words` independently of either code path and every row is
    /// checked against it. Without that, this gate could not tell "the bands are a partition" from
    /// "the reader dropped the same byte twice".
    ///
    /// FALSIFIER: in `read_back_planned`, drop the `off` from `span` (copy every band from offset
    /// 0), or arm the next band into `bufs[k & 1]` instead of `bufs[(k+1) & 1]` so consecutive
    /// bands share a buffer, and this reddens with a wrong-VALUE mismatch — the payload makes every
    /// word a function of its own index. Change `unpack`'s `SourceRgb` arm to keep all four bytes
    /// and the SOURCE assert reddens while the whole-vs-banded one would still pass.
    #[test]
    fn the_banded_readback_is_byte_identical_to_the_whole_one() {
        if !crate::hw_child::run_here(module_path!(), "the_banded_readback_is_byte_identical_to_the_whole_one") { return; }
        let Some((device, queue)) = test_device() else {
            eprintln!("skip: no GPU adapter");
            return;
        };
        // 512 x 300 "pixels" of one word each = 600 KB; a 16-row band target cuts it into 19.
        let (ow, oh) = (512u32, 300u32);
        let bytes = u64::from(ow) * u64::from(oh) * 4;
        let words: Vec<u8> = (0..(ow as usize) * (oh as usize))
            .flat_map(|i| {
                // r | g<<8 | b<<16 | 255<<24, every channel a function of the pixel index.
                [(i & 0xff) as u8, ((i >> 3) & 0xff) as u8, ((i >> 6) & 0xff) as u8, 255u8]
            })
            .collect();
        let src = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("parity-src"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&src, 0, &words);
        queue.submit(core::iter::empty::<wgpu::CommandBuffer>());

        let tight =
            crate::band::plan_with(ow, oh, 4, u64::from(ow) * 4 * 16, 0, crate::band::BAND_MAX);
        assert!(tight.bands > 8, "the gate needs a genuinely multi-band plan, got {tight:?}");
        for mode in
            [FinishOut::SourceRgb, FinishOut::ManagedRgba { src: Gamut::Srgb, dst: Gamut::Srgb }]
        {
            // What the contract SAYS the bytes are, derived from the payload alone: 24 bpp keeps
            // three bytes of each word, 32 bpp keeps all four. Neither arm below can influence it.
            let expect: Vec<u8> = match mode {
                FinishOut::SourceRgb => {
                    words.chunks_exact(4).flat_map(|w| w[..3].to_vec()).collect()
                }
                FinishOut::ManagedRgba { .. } => words.clone(),
            };
            let whole = read_back_planned(
                &device,
                &queue,
                &src,
                bytes,
                ow,
                oh,
                mode,
                crate::band::BandPlan::whole(oh),
            )
            .expect("whole readback");
            let banded = read_back_planned(&device, &queue, &src, bytes, ow, oh, mode, tight)
                .expect("banded readback");
            assert_eq!(
                whole.len(),
                (ow as usize) * (oh as usize) * mode.bytes_per_px(),
                "{mode:?}: the whole readback is the wrong length"
            );
            // THE SOURCE ASSERT — both arms against the payload, so "equal to each other" cannot
            // stand in for "correct".
            assert!(
                whole == expect,
                "{mode:?}: the whole readback is not the source payload at byte {:?}",
                whole.iter().zip(&expect).position(|(a, b)| a != b)
            );
            assert!(
                banded == expect,
                "{mode:?}: {tight:?} is not the source payload at byte {:?}",
                banded.iter().zip(&expect).position(|(a, b)| a != b)
            );
            assert_eq!(
                banded.len(),
                whole.len(),
                "{mode:?}: {tight:?} produced {} bytes against the whole form's {}",
                banded.len(),
                whole.len()
            );
            let diff = whole.iter().zip(&banded).position(|(a, b)| a != b);
            assert!(
                diff.is_none(),
                "{mode:?}: {tight:?} differs from the whole readback at byte {diff:?} (pixel {:?})",
                diff.map(|i| i / mode.bytes_per_px())
            );
        }
    }

    /// A headless adapter for the parity gate; `None` self-skips, like every live-GPU row in this
    /// crate.
    fn test_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
    }
}
