//! Falcon GPU — GPU-resident RAW develop (PLAN §16). A persistent wgpu device +
//! pipelines that turn a decompressed Bayer CFA into a developed, downscaled RGB
//! image. The per-pixel develop (demosaic + black/white + WB + cam->sRGB matrix +
//! gamma) runs in a compute shader; a second pass downscales to the display size
//! on the GPU. Measured ~0.7 ms for the 47 Mpix develop vs ~775 ms on the CPU.
//!
//! This preview is not pixel-equivalent to CPU RAW development: its bilinear
//! demosaic and scalar levels differ. Highlight handling follows the CPU developer's
//! linear-RGB rule, including retaining sensor values above nominal white.
//! Manufactured exports use the CPU developer. RGGB only — the caller checks
//! `Cfa::rggb` and falls back to the CPU path otherwise.

use anyhow::{bail, Context, Result};
use falcon_decode::Cfa;

/// Develop math: bilinear RGGB demosaic over the crop region (absolute Bayer
/// phase), black/white rescale, white balance, cam->sRGB matrix, sRGB gamma.
const DEVELOP_WGSL: &str = r#"
struct U {
  full_dims: vec2<u32>,
  crop_origin: vec2<u32>,
  crop_dims: vec2<u32>,
  black: f32,
  white: f32,
  wb: vec4<f32>,
  m0: vec4<f32>,
  m1: vec4<f32>,
  m2: vec4<f32>,
};
@group(0) @binding(0) var cfa: texture_2d<u32>;
@group(0) @binding(1) var out_img: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> u: U;

fn s(x: i32, y: i32) -> f32 {
  let cx = clamp(x, 0, i32(u.full_dims.x) - 1);
  let cy = clamp(y, 0, i32(u.full_dims.y) - 1);
  let raw = f32(textureLoad(cfa, vec2<i32>(cx, cy), 0).r);
  // Retain sensor headroom like rawler's CPU rescale; clip only below black.
  return max((raw - u.black) / (u.white - u.black), 0.0);
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
  if (gid.x >= u.crop_dims.x || gid.y >= u.crop_dims.y) { return; }
  let ax = i32(gid.x + u.crop_origin.x);
  let ay = i32(gid.y + u.crop_origin.y);
  let px = (gid.x + u.crop_origin.x) & 1u;
  let py = (gid.y + u.crop_origin.y) & 1u;
  let c = s(ax, ay);
  var r: f32; var g: f32; var b: f32;
  // RGGB: (px,py) (0,0)=R (1,0)=G (0,1)=G (1,1)=B
  if (px == 0u && py == 0u) {
    r = c;
    g = 0.25 * (s(ax-1,ay) + s(ax+1,ay) + s(ax,ay-1) + s(ax,ay+1));
    b = 0.25 * (s(ax-1,ay-1) + s(ax+1,ay-1) + s(ax-1,ay+1) + s(ax+1,ay+1));
  } else if (px == 1u && py == 0u) {
    g = c;
    r = 0.5 * (s(ax-1,ay) + s(ax+1,ay));
    b = 0.5 * (s(ax,ay-1) + s(ax,ay+1));
  } else if (px == 0u && py == 1u) {
    g = c;
    r = 0.5 * (s(ax,ay-1) + s(ax,ay+1));
    b = 0.5 * (s(ax-1,ay) + s(ax+1,ay));
  } else {
    b = c;
    g = 0.25 * (s(ax-1,ay) + s(ax+1,ay) + s(ax,ay-1) + s(ax,ay+1));
    r = 0.25 * (s(ax-1,ay-1) + s(ax+1,ay-1) + s(ax-1,ay+1) + s(ax+1,ay+1));
  }
  let cam = vec3<f32>(r, g, b) * u.wb.xyz;
  let lin = vec3<f32>(dot(u.m0.xyz, cam), dot(u.m1.xyz, cam), dot(u.m2.xyz, cam));
  // rawler::imgop::raw::clip_euclidean_norm_avg, also used by Falcon X-Trans.
  // Independent channel clipping made saturated camera neutrals pink after WB.
  var cl = max(lin, vec3<f32>(0.0));
  let peak = max(cl.x, max(cl.y, cl.z));
  if (peak > 1.0) {
    let norm = length(cl) / sqrt(3.0);
    cl = (cl / peak + vec3<f32>(norm)) * 0.5;
  }
  cl = min(cl, vec3<f32>(1.0));
  let lo = cl * 12.92;
  let hi = 1.055 * pow(cl, vec3<f32>(1.0 / 2.4)) - 0.055;
  let srgb = select(hi, lo, cl <= vec3<f32>(0.0031308));
  textureStore(out_img, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(srgb, 1.0));
}
"#;

/// Fullscreen-triangle blit used to downscale the developed image with linear
/// filtering.
const BLIT_WGSL: &str = r#"
struct VSOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(@builtin(vertex_index) vid: u32) -> VSOut {
  var o: VSOut;
  let uv = vec2<f32>(f32((vid << 1u) & 2u), f32(vid & 2u));
  o.pos = vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
  o.uv = vec2<f32>(uv.x, 1.0 - uv.y);
  return o;
}
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@fragment fn fs(i: VSOut) -> @location(0) vec4<f32> { return textureSample(tex, smp, i.uv); }
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    full_dims: [u32; 2],
    crop_origin: [u32; 2],
    crop_dims: [u32; 2],
    black: f32,
    white: f32,
    wb: [f32; 4],
    m0: [f32; 4],
    m1: [f32; 4],
    m2: [f32; 4],
}

/// v0.9.60 (W2-4 / round-4 findings, energy item 5): the RAW develop device is an INDEPENDENT
/// second wgpu device — its own `request_adapter`, unaffected by whatever preference the app's
/// renderer asked for. It asked for `HighPerformance` unconditionally, which on an Intel dGPU Mac
/// pins the discrete GPU and defeats automatic graphics switching for the rest of the session.
///
/// The host app sets this ONCE at boot, from its own battery probe (`support::on_battery_power`),
/// so this crate carries no platform FFI of its own. Nothing calls the setter on Windows, so the
/// preference read below is `HighPerformance` there — byte-identical behaviour to every build
/// before this one.
static LOW_POWER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Ask future [`GpuDeveloper::new`] calls for the low-power adapter (macOS on battery). Latched by
/// the caller at boot; the device is created lazily on first RAW use, so a boot-time latch is what
/// this device actually reads.
pub fn set_low_power(on: bool) {
    LOW_POWER.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// A persistent GPU context for developing RAW frames. Create once (it owns a
/// wgpu device + compiled pipelines) and reuse for every develop. `Send + Sync`,
/// so it can live in shared app state.
pub struct GpuDeveloper {
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter_name: String,
    develop_pipeline: wgpu::ComputePipeline,
    develop_bgl: wgpu::BindGroupLayout,
    blit_pipeline: wgpu::RenderPipeline,
    blit_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl GpuDeveloper {
    /// Initialise the GPU device and pipelines. Returns an error if no adapter is
    /// available (callers should fall back to the CPU develop path).
    pub fn new() -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            // v0.9.60 (W2-4): `HighPerformance` unless the host latched the battery posture.
            power_preference: if LOW_POWER.load(std::sync::atomic::Ordering::Relaxed) {
                wgpu::PowerPreference::LowPower
            } else {
                wgpu::PowerPreference::HighPerformance
            },
            ..Default::default()
        }))
        .context("no GPU adapter available")?;
        let adapter_name = adapter.get_info().name;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_limits: adapter.limits(), // full-sensor CFA can exceed the 8192 default
            ..Default::default()
        }))
        .context("failed to create GPU device")?;

        let cs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("develop"),
            source: wgpu::ShaderSource::Wgsl(DEVELOP_WGSL.into()),
        });
        let develop_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("develop-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
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
            ],
        });
        let cpl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&develop_bgl)],
            immediate_size: 0,
        });
        let develop_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("develop"),
            layout: Some(&cpl),
            module: &cs,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });

        let bs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
        });
        let blit_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bpl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&blit_bgl)],
            immediate_size: 0,
        });
        let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit"),
            layout: Some(&bpl),
            vertex: wgpu::VertexState {
                module: &bs,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &bs,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        Ok(Self {
            device,
            queue,
            adapter_name,
            develop_pipeline,
            develop_bgl,
            blit_pipeline,
            blit_bgl,
            sampler,
        })
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// Develop a CFA and downscale so the long side is `target_long`. Returns
    /// packed RGB8 plus its dimensions.
    pub fn develop(&self, cfa: &Cfa, target_long: u32) -> Result<(Vec<u8>, u32, u32)> {
        let (cw, ch) = (cfa.crop_w.max(1), cfa.crop_h.max(1));

        // v0.8.152 (R3-L2) — ASK THE DEVICE BEFORE ALLOCATING, the `HeicAssembler::mosaic_fits`
        // pattern this crate already owns.
        //
        // The three textures below are sized straight from `Cfa::width`/`height` and the crop, with
        // no limit test between the rawler buffer and `Device::create_texture` — whose default
        // uncaptured-error handling is a PANIC, so the `Result` this function returns was not the
        // failure mode a too-large source actually got. `extract_cfa` already bails on a padded or
        // short rawler buffer "so the caller falls back to CPU develop (B13)"; the DIMENSION case
        // had no equivalent. Latent rather than live — no shipping sensor exceeds 16384 px on a side
        // (Phase One's IQ4 is 14204 wide) — but a stitched or synthetic DNG reaches it, and a
        // decline that names the limit is a log line instead of a crash.
        let limits = self.device.limits();
        let dim = limits.max_texture_dimension_2d;
        for (w, h, what) in [(cfa.width, cfa.height, "CFA"), (cw, ch, "developed crop")] {
            if w > dim || h > dim {
                bail!("the {what} is {w}x{h}, over this device's max texture dimension {dim}");
            }
        }

        // CFA -> R16Uint texture.
        let full = wgpu::Extent3d { width: cfa.width, height: cfa.height, depth_or_array_layers: 1 };
        let cfa_tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cfa"),
            size: full,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R16Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &cfa_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&cfa.data),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(cfa.width * 2),
                rows_per_image: Some(cfa.height),
            },
            full,
        );

        // Developed crop (storage + sampleable for the downscale pass).
        let dev_tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dev"),
            size: wgpu::Extent3d { width: cw, height: ch, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        let uniforms = Uniforms {
            full_dims: [cfa.width, cfa.height],
            crop_origin: [cfa.crop_x, cfa.crop_y],
            crop_dims: [cw, ch],
            black: cfa.black,
            white: cfa.white,
            wb: [cfa.wb[0], cfa.wb[1], cfa.wb[2], 0.0],
            m0: [cfa.cam_to_srgb[0][0], cfa.cam_to_srgb[0][1], cfa.cam_to_srgb[0][2], 0.0],
            m1: [cfa.cam_to_srgb[1][0], cfa.cam_to_srgb[1][1], cfa.cam_to_srgb[1][2], 0.0],
            m2: [cfa.cam_to_srgb[2][0], cfa.cam_to_srgb[2][1], cfa.cam_to_srgb[2][2], 0.0],
        };
        let ubuf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("u"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&ubuf, 0, bytemuck::bytes_of(&uniforms));

        let cfa_view = cfa_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let dev_view = dev_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let develop_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.develop_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&cfa_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&dev_view) },
                wgpu::BindGroupEntry { binding: 2, resource: ubuf.as_entire_binding() },
            ],
        });

        // Downscale target (long side == target_long).
        let (pw, ph) = if cw >= ch {
            let pw = target_long.min(cw);
            (pw, ((pw as u64 * ch as u64) / cw as u64).max(1) as u32)
        } else {
            let ph = target_long.min(ch);
            (((ph as u64 * cw as u64) / ch as u64).max(1) as u32, ph)
        };
        let prev_tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("prev"),
            size: wgpu::Extent3d { width: pw, height: ph, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let prev_view = prev_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let blit_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.blit_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&dev_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });

        // Read-back buffer with 256-aligned rows.
        let unpadded = pw * 4;
        let padded = unpadded.div_ceil(256) * 256;
        // R3-L2, the buffer half: `create_buffer` panics on a size past the device's limit exactly
        // as `create_texture` does on a dimension. Asked HERE rather than at the top of the function
        // because the size is `padded × ph` at the DOWNSCALED preview dims, and bounding it by the
        // full crop would decline develops that work today.
        let readback = (padded as u64) * (ph as u64);
        if readback > limits.max_buffer_size {
            bail!(
                "the develop readback needs {readback} bytes, over this device's {} byte buffer limit",
                limits.max_buffer_size
            );
        }
        let out_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            // v0.8.181 (pre-merge review): the SAME number the guard above just proved, not a second
            // expression of it. `(padded * ph) as u64` multiplies in u32 and casts AFTER — the exact
            // arithmetic the `readback` line exists to bound, re-done in a width that cannot hold the
            // answer it was bounded against (a 65536-wide preview at 65536 rows wraps to a small
            // buffer and the copy below writes past it). One word, one number.
            size: readback,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None, timestamp_writes: None });
            cp.set_pipeline(&self.develop_pipeline);
            cp.set_bind_group(0, &develop_bg, &[]);
            cp.dispatch_workgroups(cw.div_ceil(8), ch.div_ceil(8), 1);
        }
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &prev_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.blit_pipeline);
            rp.set_bind_group(0, &blit_bg, &[]);
            rp.draw(0..3, 0..1);
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &prev_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &out_buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(ph),
                },
            },
            wgpu::Extent3d { width: pw, height: ph, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());

        // Surface a map failure (device-lost / OOM) as an Err instead of panicking the
        // worker on get_mapped_range — the caller then falls back to CPU develop (B12).
        let (map_tx, map_rx) = std::sync::mpsc::channel();
        out_buf.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = map_tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        map_rx
            .recv()
            .context("map_async channel dropped")?
            .context("failed to map readback buffer")?;

        // Strip row padding + alpha -> packed RGB8.
        let mut rgb = Vec::with_capacity((pw * ph * 3) as usize);
        {
            let mapped = out_buf.slice(..).get_mapped_range();
            for row in 0..ph {
                let start = (row * padded) as usize;
                let line = &mapped[start..start + unpadded as usize];
                for px in line.chunks_exact(4) {
                    rgb.extend_from_slice(&px[0..3]);
                }
            }
        }
        out_buf.unmap(); // always release the mapping before returning
        Ok((rgb, pw, ph))
    }
}

// LV3 single source (§6.0), v0.8.22: the rotation-UV remap + the `linearize`/`encode` TRC helpers are
// byte-for-byte shared by BOTH fast-tier shaders (YUV_WGSL and CM_SHADER). Hoisted into ONE place and
// spliced into each pipeline string via `concat!` so the two can never silently drift — pre-v0.8.22
// they were duplicated and "kept in sync by comment", and the lock-step note HAD already diverged
// (`::` + em-dash vs plain). A macro (not a const) so `concat!` inlines it at compile time; both
// assembled shaders stay `&'static str`. Byte-faithfulness is pinned by the `wgsl_single_source`
// unit tests below (YUV identical; CM identical once the unified comment is normalised).
macro_rules! rot_uv_cm_core {
    () => {
r#"fn rot_uv(uv: vec2<f32>, turns: u32) -> vec2<f32> {
    if (turns == 1u) { return vec2<f32>(uv.y, 1.0 - uv.x); }
    if (turns == 2u) { return vec2<f32>(1.0 - uv.x, 1.0 - uv.y); }
    if (turns == 3u) { return vec2<f32>(1.0 - uv.y, uv.x); }
    return uv;
}

// v0.8.177: kind 4 (a FAITHFUL SOURCE profile — a file whose embedded matrix/TRC profile matches no
// modeled gamut) LINEARISES via the per-channel FORWARD tone curve LUT (device→linear) held in rows
// 3..5 of the SAME `lut_tex` whose rows 0..2 carry the kind-2 destination encode LUT — one texture,
// one width in flags.w, one fetch shape. The fetch is the byte-twin of falcon_color's `lut_encode`,
// so the GPU fast tier and the CPU detail/ROI tiers reproduce the identical curve — no scrub↔detail
// colour seam, which is the same contract kind 2 rides on. `ch` = 0/1/2.
fn linearize(c: f32, kind: u32, ch: i32, n: u32) -> f32 {
    if (kind == 4u) {
        let x = clamp(c, 0.0, 1.0);
        let fpos = x * f32(n - 1u);
        let i0 = i32(floor(fpos));
        let i1 = min(i0 + 1, i32(n) - 1);
        let frac = fpos - floor(fpos);
        let v0 = textureLoad(lut_tex, vec2<i32>(i0, ch + 3), 0).r;
        let v1 = textureLoad(lut_tex, vec2<i32>(i1, ch + 3), 0).r;
        return mix(v0, v1, frac);
    }
    if (kind == 1u) { return pow(max(c, 0.0), 2.19921875); } // == falcon_color::ADOBE_GAMMA (563/256) — keep in lock-step
    if (kind == 3u) { return pow(max(c, 0.0), 2.6); } // == falcon_color::DCI_GAMMA (DCI-P3, v0.8.67) — keep in lock-step
    if (c <= 0.04045) { return c / 12.92; }
    return pow((c + 0.055) / 1.055, 2.4);
}
// v0.8.47: kind 2 (custom display profile) ENCODES via the per-channel inverse tone curve LUT
// (linear→device) bound as `lut_tex` — a CUSTOM_LUT_N-wide × 3-row (R/G/B) texture whose width `n`
// travels in flags.w. Manual textureLoad + mix (linear interp) is the byte-twin of falcon_color's
// `lut_encode`, so the GPU fast tier and the CPU detail/ROI tiers reproduce the identical curve — no
// scrub↔detail colour seam. Named gamuts keep their analytic encodes (kinds 0/1). `ch` = 0/1/2.
fn encode(c: f32, kind: u32, ch: i32, n: u32) -> f32 {
    let x = clamp(c, 0.0, 1.0);
    if (kind == 2u) {
        let fpos = x * f32(n - 1u);
        let i0 = i32(floor(fpos));
        let i1 = min(i0 + 1, i32(n) - 1);
        let frac = fpos - floor(fpos);
        let v0 = textureLoad(lut_tex, vec2<i32>(i0, ch), 0).r;
        let v1 = textureLoad(lut_tex, vec2<i32>(i1, ch), 0).r;
        return mix(v0, v1, frac);
    }
    if (kind == 1u) { return pow(x, 1.0 / 2.19921875); } // == falcon_color::ADOBE_GAMMA (563/256) — keep in lock-step
    if (kind == 3u) { return pow(x, 1.0 / 2.6); } // == falcon_color::DCI_GAMMA (DCI-P3, v0.8.67) — keep in lock-step
    if (x <= 0.0031308) { return x * 12.92; }
    return 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}

"#
    };
}

// v0.8.147 (E3-M2): the HEIC grid's GPU assembly — canvas compositing, crop, irot/imir and the
// tier downsample.
//
// v0.8.165 (WAVE 1): …and, at the end of that chain, the gamut transform — which is why this `mod`
// is declared HERE, below `rot_uv_cm_core!`, rather than at the top of the file where it used to
// sit. `macro_rules!` is TEXTUALLY scoped: a macro is visible to a child module only if the `mod`
// declaration comes after it. `FINISH_WGSL` splices the identical `linearize`/`encode` core that
// `CM_SHADER` and `YUV_WGSL` splice, so the HEIC lane's colour arithmetic is not merely "the same
// maths" as the JPEG chain's — it is the same BYTES of shader source, and `wgsl_single_source`
// asserts it for all three.
pub mod heic;

// 28 October (round 2b review P2): the time limit for the tests that reach the driver, shared with the
// other crates' hardware tests (`heic::tests`' readback row uses it).
#[cfg(test)]
#[path = "../../test-support/hw_child.rs"]
mod hw_child;

// v0.8.166 (WAVE 2), narrowed v0.8.167: the band plan — how one big transfer is cut into pieces.
// It served two call sites when it landed; the 08-06 audit reverted the renderer-side one (see the
// module header), so it is now the HEIC assembly READBACK's planner and nothing else.
pub mod band;

// ───────────────────────── U2 — fused YCbCr→RGB + gamut convert (PLAN §39.3) ─────────────────────────
// Turns nvJPEG's PLANAR YUV output (the JPEG's native representation) into the RGBA texture the app
// displays, in ONE f32 pass: JFIF BT.601 FULL-RANGE YCbCr→RGB, then (when managed) the exact
// `falcon-color` gamut maths — linearise src TRC → 3×3 → encode dst TRC — with a SINGLE final
// quantisation on the render-target write (round-to-nearest, matching the CPU path). That is ONE
// FEWER 8-bit quantisation than the RGBI path (which quantises nvJPEG's convert to 8-bit BEFORE
// the CPU transform re-quantises) — the colour-accuracy analysis' headline.
//
// Chroma siting: JFIF chroma samples are CENTERED (interstitial). For a half-width plane the texel
// centres land at luma-normalised (2i+1)/W — exactly where linear sampling at the SHARED normalised
// uv interpolates — so a plain bilinear fetch reproduces the JFIF siting with no phase offset.
// The Y plane samples at matching dims (texel-centre uv ⇒ an exact fetch).
//
// Lives in falcon-gpu so the app's upload thread AND the parity harness (spikes-decode-bench)
// run the IDENTICAL implementation — the ship-gate compares this against the RGBI+CPU path.
const YUV_WGSL: &str = concat!(
r#"
struct Params {
    m0: vec4<f32>,   // src->dst matrix row 0 (xyz) + pad
    m1: vec4<f32>,
    m2: vec4<f32>,
    flags: vec4<u32>, // x = src TRC kind, y = dst TRC kind,
                      // z = bit0 gamut-managed (src != dst) | bits1-2 = display turns (0..3, 90° CW),
                      // w = the inverse-tone-curve LUT width (== falcon_color::CUSTOM_LUT_N; used only
                      //     when dst kind == 2, custom — the LUT itself is bound as `lut_tex`)
};
@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var cb_tex: texture_2d<f32>;
@group(0) @binding(2) var cr_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(4) var<uniform> P: Params;
// v0.8.47: per-channel inverse tone curve LUT (linear->device) for a custom display profile, N x 3
// (R/G/B rows). Fetched via textureLoad in `encode` (kind 2). A 1x3 identity stand-in when unmanaged.
@group(0) @binding(5) var lut_tex: texture_2d<f32>;

struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VOut {
    var out: VOut;
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    out.uv = vec2<f32>(x, y);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

// v0.8.0 rotation: map an OUTPUT (upright) uv back to the SOURCE uv it samples, for a source shown
// rotated `turns` × 90° CW. The output texture's dims are swapped for odd turns by the caller, so the
// fullscreen pass fills the oriented target while reading the unrotated planes. Matches
// falcon_decode::display_to_source_uv exactly (one convention across GPU + ROI-crop + CPU thumbs).
"#,
    rot_uv_cm_core!(),
r#"@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let uv = rot_uv(in.uv, (P.flags.z >> 1u) & 3u);
    let yv = textureSampleLevel(y_tex, samp, uv, 0.0).r;
    // JFIF neutral chroma is the INTEGER 128 → 128/255 in unorm (NOT 0.5 = 127.5): the parity
    // gate measured the 0.5-centred version ~1 LSB off EVERYWHERE, amplified to ~6 LSB near
    // black through Adobe RGB's pure-power TRC (infinite slope at 0).
    let cb = textureSampleLevel(cb_tex, samp, uv, 0.0).r - 0.5019608;
    let cr = textureSampleLevel(cr_tex, samp, uv, 0.0).r - 0.5019608;
    // JFIF BT.601 FULL-RANGE (Y 0..1 direct)
    var r = clamp(yv + 1.402 * cr, 0.0, 1.0);
    var g = clamp(yv - 0.344136 * cb - 0.714136 * cr, 0.0, 1.0);
    var b = clamp(yv + 1.772 * cb, 0.0, 1.0);
    if ((P.flags.z & 1u) == 1u) {
        let st = P.flags.x;
        let dt = P.flags.y;
        let n = P.flags.w; // tone-curve LUT width (read when dt == 2 or st == 4)
        let lr = linearize(r, st, 0, n);
        let lg = linearize(g, st, 1, n);
        let lb = linearize(b, st, 2, n);
        let m0 = P.m0.xyz; let m1 = P.m1.xyz; let m2 = P.m2.xyz;
        r = encode(m0.x * lr + m0.y * lg + m0.z * lb, dt, 0, n);
        g = encode(m1.x * lr + m1.y * lg + m1.z * lb, dt, 1, n);
        b = encode(m2.x * lr + m2.y * lg + m2.z * lb, dt, 2, n);
    }
    return vec4<f32>(r, g, b, 1.0);
}
"#,
);

/// LV3 single source (§6.0): the GPU fast-tier colour-manage WGSL, relocated here beside its
/// `YUV_WGSL` sibling (which folds in the IDENTICAL `linearize`/`encode`/`Params` maths) so the
/// colour-management core lives in ONE file — the app's upload thread imports it as
/// `falcon_gpu::CM_SHADER`. (A third verbatim copy still lives in the `mixed_load` spike — out of
/// scope here; a future round can point it at this const too.)
///
/// WGSL for the GPU fast-tier colour transform (CM-1.1). A fullscreen-triangle pass that reads the
/// freshly-uploaded source texture and writes it colour-managed into the output texture: it applies
/// EXACTLY the `falcon-color` maths (linearise src TRC → 3×3 src→dst matrix → clip → encode dst TRC),
/// so the scrub tier (GPU, here) and the on-stop detail/ROI tiers (CPU, `transform_rgba`) land on the
/// same colour — removing the wide-gamut "colour pop" that used to appear when a fast frame gave way to
/// the managed detail frame. The source is `Rgba8Unorm` (NOT `…Srgb`), so the sampler returns the raw
/// ENCODED values and we linearise manually; nearest sampling at matching dims is an exact 1:1 copy.
pub const CM_SHADER: &str = concat!(
r#"
struct Params {
    m0: vec4<f32>,   // src->dst matrix row 0 (xyz) + pad
    m1: vec4<f32>,
    m2: vec4<f32>,
    flags: vec4<u32>, // x = src TRC kind, y = dst TRC kind (0 sRGB piecewise / 1 Adobe 2.199 /
                      // 2 = Custom faithful tone curve — encoded via the inverse-LUT texture `lut_tex`,
                      // its width in flags.w), z = bit0 colour-managed | bits1-2 = display turns (0..3)
};
@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_samp: sampler;
@group(0) @binding(2) var<uniform> P: Params;
// v0.8.47: per-channel inverse tone curve LUT (linear->device) for a custom display profile, N x 3
// (R/G/B rows). Fetched via textureLoad in `encode` (kind 2). A 1x3 identity stand-in when unmanaged.
@group(0) @binding(3) var lut_tex: texture_2d<f32>;

struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VOut {
    var out: VOut;
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    out.uv = vec2<f32>(x, y);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

// v0.8.0 rotation: OUTPUT (upright) uv → SOURCE uv, for a source shown rotated `turns` × 90° CW. The
// caller swaps the dst texture's dims for odd turns. Matches falcon_decode::display_to_source_uv.
"#,
    rot_uv_cm_core!(),
r#"@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let s = textureSampleLevel(src_tex, src_samp, rot_uv(in.uv, (P.flags.z >> 1u) & 3u), 0.0);
    // Pure-rotation (or identity) upload: not colour-managed — pass the sampled pixel straight
    // through (bit-exact colour, only repositioned). The managed branch below runs the full CM maths.
    if ((P.flags.z & 1u) == 0u) { return s; }
    let st = P.flags.x;
    let dt = P.flags.y;
    let n = P.flags.w; // tone-curve LUT width (read when dt == 2 or st == 4)
    let lr = linearize(s.r, st, 0, n);
    let lg = linearize(s.g, st, 1, n);
    let lb = linearize(s.b, st, 2, n);
    let m0 = P.m0.xyz; let m1 = P.m1.xyz; let m2 = P.m2.xyz;
    let orr = encode(m0.x * lr + m0.y * lg + m0.z * lb, dt, 0, n);
    let og  = encode(m1.x * lr + m1.y * lg + m1.z * lb, dt, 1, n);
    let ob  = encode(m2.x * lr + m2.y * lg + m2.z * lb, dt, 2, n);
    return vec4<f32>(orr, og, ob, s.a);
}
"#,
);

// ───────────────────────── v0.8.47 custom-profile inverse-LUT texture ─────────────────────────
// The GPU twin of falcon_color's per-channel inverse tone curve LUT. Both fast-tier shaders
// (`YUV_WGSL`, `CM_SHADER`) `textureLoad` it in the kind-2 encode branch. Built ONCE per custom-profile
// generation and reused every frame (no per-frame allocation), keyed by `falcon_color::custom_profile_gen`.

/// A cached `CUSTOM_LUT_N × 6` R32Float LUT texture. `gen` is the `falcon_color` custom-profile
/// generation it was built for; `src_id` the faithful-source profile index (v0.8.177) — the texture
/// is stale when EITHER moves.
///
/// ROWS 0..2 — the active custom DESTINATION profile's `linear → device` inverse tone curve (R/G/B),
/// read by the kind-2 `encode` branch. ROWS 3..5 — the faithful SOURCE profile's `device → linear`
/// FORWARD curve (R/G/B), read by the kind-4 `linearize` branch. One texture rather than two because
/// the two tables share a width (`CUSTOM_LUT_N`, which travels in `flags.w`), a format and a fetch
/// shape; a second binding would have meant a new bind-group entry in all three shaders for no
/// arithmetic difference.
///
/// R32Float (not R16Unorm) so no `TEXTURE_FORMAT_16BIT_NORM` device feature is required; it's a core
/// unfilterable sampled format and `textureLoad` needs no filtering. Values are the CPU's u16 LUT
/// dequantised (`u16 / 65535`), so the GPU texel equals the CPU's `lut_encode` sample exactly.
pub struct CustomLutTex {
    gen: u64,
    /// `None` when no faithful source is active — rows 3..5 then hold the identity forward curve.
    src_id: Option<u16>,
    _tex: wgpu::Texture, // kept alive; the view is what the bind group references
    pub view: wgpu::TextureView,
}

/// The texture's row count: 3 destination (inverse) + 3 source (forward). Mirrors the `ch` and
/// `ch + 3` row indices the shader core's `encode`/`linearize` use.
const LUT_ROWS: u32 = 6;

/// Build the LUT texture for the installed custom DESTINATION profile (or a gamma-2.2 stand-in
/// matching the CPU fallback when none is loaded) and the given faithful SOURCE gamut (or an
/// identity forward curve when `src` is a modeled gamut). Always a full `CUSTOM_LUT_N × 6` texture,
/// so the shader's `flags.w` width is always valid and no `textureLoad` can land out of bounds.
fn build_custom_lut(device: &wgpu::Device, queue: &wgpu::Queue, src: falcon_color::Gamut) -> CustomLutTex {
    let n = falcon_color::CUSTOM_LUT_N as u32;
    let (gen, mut u16_data): (u64, Vec<u16>) = match falcon_color::custom_encode_lut() {
        Some((g, arc)) => (g, arc.to_vec()),
        None => {
            // No profile: replicate the CPU's analytic gamma-2.2 fallback across all three rows.
            let ch = falcon_color::gamma_encode_lut(2.2);
            let mut v = Vec::with_capacity(3 * ch.len());
            for _ in 0..3 {
                v.extend_from_slice(&ch);
            }
            (falcon_color::custom_profile_gen(), v)
        }
    };
    debug_assert_eq!(u16_data.len(), 3 * n as usize, "custom LUT must be 3 x CUSTOM_LUT_N");
    // v0.8.177 — rows 3..5. The faithful source's forward LUT is the SAME `Arc<[u16]>` the CPU
    // `Trc::SrcLut` samples, so parity is by construction and not by a second implementation.
    let src_id = match falcon_color::source_linearize_lut(src) {
        Some((id, lut)) => {
            u16_data.extend_from_slice(&lut);
            Some(id)
        }
        None => {
            // No faithful source: an identity forward curve (device value == linear value). Never
            // read — kind 4 is only reachable from a `SourceIcc` gamut — but the rows must exist so
            // the texture's shape is one thing, not two.
            u16_data.extend((0..3 * n).map(|k| {
                let x = (k % n) as f32 / (n - 1) as f32;
                (x * 65535.0 + 0.5) as u16
            }));
            None
        }
    };
    debug_assert_eq!(u16_data.len(), LUT_ROWS as usize * n as usize, "LUT must be 6 x CUSTOM_LUT_N");
    // Dequantise to f32 in [0,1] — identical to the CPU's `lut_encode` (v/65535), both directions.
    let data: Vec<f32> = u16_data.iter().map(|&v| v as f32 / 65535.0).collect();
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("falcon-custom-lut"),
        size: wgpu::Extent3d { width: n, height: LUT_ROWS, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&data),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(n * 4), rows_per_image: Some(LUT_ROWS) },
        wgpu::Extent3d { width: n, height: LUT_ROWS, depth_or_array_layers: 1 },
    );
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    CustomLutTex { gen, src_id, _tex: tex, view }
}

/// Refresh a cached LUT texture iff the custom-profile generation OR the faithful source profile has
/// changed — a cheap no-op on the steady-state hot path (same pair → the texture is reused, zero
/// allocation).
///
/// v0.8.177 takes `src`: browsing a folder that mixes an ordinary sRGB JPEG with a ProPhoto TIFF
/// rebuilds this texture as the two alternate. That is a 98 KB upload on a file change, not on a
/// frame — the same order as the profile-change rebuild this function already did.
pub fn ensure_custom_lut(
    cache: &mut Option<CustomLutTex>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    src: falcon_color::Gamut,
) {
    let gen = falcon_color::custom_profile_gen();
    let src_id = falcon_color::source_linearize_lut(src).map(|(i, _)| i);
    let stale = match cache.as_ref() {
        Some(c) => c.gen != gen || c.src_id != src_id,
        None => true,
    };
    if stale {
        *cache = Some(build_custom_lut(device, queue, src));
    }
}

/// The tone-curve LUT width for the shader uniform's `flags.w` — `CUSTOM_LUT_N` when EITHER side of
/// this transform reads the LUT texture (a `Custom` destination's kind-2 encode, or a faithful
/// source's kind-4 linearise), 0 when neither does (the analytic branches ignore it).
///
/// v0.8.177: one function, called by all three uniform packers. It used to be an inlined
/// `if dst == Custom` at each of them, and adding a second condition to three copies is how two of
/// them end up with one condition.
pub fn lut_width_for(src: falcon_color::Gamut, dst: falcon_color::Gamut) -> u32 {
    if dst == falcon_color::Gamut::Custom || src.is_source_profile() {
        falcon_color::CUSTOM_LUT_N as u32
    } else {
        0
    }
}

/// Borrowed planar-YUV input for [`YuvConvert::convert`].
pub struct YuvPlanes<'a> {
    pub y: &'a [u8],
    pub cb: &'a [u8],
    pub cr: &'a [u8],
    pub w: u32,
    pub h: u32,
    pub cw: u32,
    pub ch: u32,
}

/// The fused YUV→RGBA(+gamut) pipeline. Built once per device (the app's upload thread, or the
/// parity harness's headless device) and reused per frame.
pub struct YuvConvert {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// v0.8.47: the custom-profile inverse-LUT texture, cached by profile generation (interior
    /// mutability so `convert(&self, …)` can refresh it without a per-frame allocation).
    lut: std::cell::RefCell<Option<CustomLutTex>>,
}

impl YuvConvert {
    pub fn new(device: &wgpu::Device) -> Result<YuvConvert> {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("falcon-yuv-shader"),
            source: wgpu::ShaderSource::Wgsl(YUV_WGSL.into()),
        });
        let tex_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("falcon-yuv-bgl"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                tex_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // binding 5: the custom-profile inverse-LUT (R32Float — see CustomLutTex), fetched via textureLoad (no
                // sampler) — declared non-filterable to keep it independent of the chroma sampler.
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
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
            label: Some("falcon-yuv-pl"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("falcon-yuv-pipeline"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        // Linear + clamp-to-edge: exact fetch on the matching-dims Y plane, JFIF-centred
        // interpolation on the half-res chroma planes, edge clamp for odd widths.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("falcon-yuv-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Ok(YuvConvert { pipeline, layout, sampler, lut: std::cell::RefCell::new(None) })
    }

    /// Upload the three planes + run the fused pass → the display-ready RGBA texture.
    /// `src`/`dst` = the falcon-color gamuts; src == dst skips the gamut arm entirely (exact).
    /// `turns` (0..3, 90° CW) rotates the output to upright at sample time: the output texture's dims
    /// are swapped for odd turns and each fragment reads the rotated source uv (the unrotated planes
    /// stay valid). turns 0 is byte-identical to the pre-rotation path.
    ///
    /// **R3-L2 — CLOSED v0.8.165 (QUEUE §2 item 5c(ii)).**
    ///
    /// v0.8.152 named the defect and could not fix it from inside the crates partition: the plane
    /// and output textures below are sized straight from `YuvPlanes`, so a source past the device's
    /// `max_texture_dimension_2d` reached `create_texture` and its default uncaptured-error
    /// handling, which is a panic — and this function returned a bare `wgpu::Texture`, with nowhere
    /// to put a decline. The other four sites the finding names (`GpuDeveloper::develop`'s three,
    /// `Nv12Kernel::convert_with_coeffs`) took the `HeicAssembler::mosaic_fits` treatment then;
    /// these two are the remainder, and closing them needed the two `native/src/support.rs` call
    /// sites (the Detail and RoiYuv upload arms) to have somewhere to fall back TO — which the
    /// same wave that moves HEIC's colour onto the GPU had to build anyway.
    ///
    /// The check asks the DEVICE, never a constant. That is the iGPU clause of the owner's
    /// directive: a discrete card reports 32768 here and an integrated one commonly 16384, so a
    /// hard-coded bound is either wrong on one of them or wrong on the next one. `Err` names the
    /// limit that said no, in the `mosaic_fits` shape, so a decline is a sentence in a log.
    ///
    /// v0.8.167: the `Err` is a statement about THIS FRAME and the call sites treat it as one — see
    /// `native/src/support.rs`'s per-shot refusal memo. v0.8.166 band-sliced the three plane
    /// uploads here and answered a submit count; the 08-06 audit reverted that (a renderer frame
    /// SAMPLES the texture it is waiting for, so splitting the copy cannot let it start earlier),
    /// and the planes are `create_texture_with_data` again, byte-identical to v0.8.165.
    pub fn convert(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        p: &YuvPlanes,
        src: falcon_color::Gamut,
        dst: falcon_color::Gamut,
        turns: u8,
    ) -> Result<wgpu::Texture> {
        use wgpu::util::DeviceExt;
        // The OUTPUT is the largest thing here and its dims are the source's (swapped for odd
        // turns), so bounding the source bounds both. The chroma planes are half-size by
        // construction — no plane can exceed a luma plane that fits.
        let limits = device.limits();
        let dim = limits.max_texture_dimension_2d;
        if p.w == 0 || p.h == 0 {
            bail!("YUV frame {}x{} is empty", p.w, p.h);
        }
        if p.w > dim || p.h > dim {
            bail!("YUV frame {}x{} exceeds this device's max texture dimension {dim}", p.w, p.h);
        }
        // The plane uploads read `w*h` and `cw*ch` bytes; a caller that under-declared a plane
        // would otherwise slice out of bounds inside wgpu's own copy.
        let need = |n: u32, m: u32| (n as usize) * (m as usize);
        if p.y.len() < need(p.w, p.h) {
            bail!("the Y plane carries {} bytes, needs {}", p.y.len(), need(p.w, p.h));
        }
        if p.cb.len() < need(p.cw, p.ch) || p.cr.len() < need(p.cw, p.ch) {
            bail!(
                "a chroma plane carries {}/{} bytes, needs {}",
                p.cb.len(),
                p.cr.len(),
                need(p.cw, p.ch)
            );
        }
        let plane = |data: &[u8], w: u32, h: u32, label: &str| {
            device.create_texture_with_data(
                queue,
                &wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                data,
            )
        };
        let y_tex = plane(p.y, p.w, p.h, "falcon-yuv-y");
        let cb_tex = plane(p.cb, p.cw, p.ch, "falcon-yuv-cb");
        let cr_tex = plane(p.cr, p.cw, p.ch, "falcon-yuv-cr");
        // Oriented output: swap w/h for a quarter/three-quarter turn (the shader fills this target
        // by reading the rotated source uv). Even turns (0/180°) keep the source dims.
        let (ow, oh) = if turns & 1 == 1 { (p.h, p.w) } else { (p.w, p.h) };
        let out = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("falcon-frame"),
            size: wgpu::Extent3d { width: ow, height: oh, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        // Uniform: 3× vec4<f32> matrix rows (std140) + the TRC/managed flags.
        let managed = src != dst;
        let m = falcon_color::src_to_dst_matrix(src, dst);
        let mut u = [0u8; 64];
        for (r, row) in m.iter().enumerate() {
            for (col, &val) in row.iter().enumerate() {
                let off = r * 16 + col * 4;
                u[off..off + 4].copy_from_slice(&val.to_le_bytes());
            }
        }
        u[48..52].copy_from_slice(&src.trc_kind().to_le_bytes());
        u[52..56].copy_from_slice(&dst.trc_kind().to_le_bytes());
        // flags.z: bit0 = managed (src != dst), bits1-2 = display turns (0..3). One packed u32 so the
        // 64-byte std140 uniform layout is unchanged.
        u[56..60].copy_from_slice(&((managed as u32) | (((turns as u32) & 3) << 1)).to_le_bytes());
        // flags.w: the tone-curve LUT width — the shader indexes `lut_tex` by it for a custom (kind 2)
        // output AND (v0.8.177) for a faithful-source (kind 4) input. 0 for the analytic pairs.
        u[60..64].copy_from_slice(&lut_width_for(src, dst).to_le_bytes());
        let ubuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("falcon-yuv-ubuf"),
            contents: &u,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        // Refresh the LUT texture if the destination profile OR the faithful source changed (no-op
        // on the hot path).
        ensure_custom_lut(&mut self.lut.borrow_mut(), device, queue, src);
        let lut_ref = self.lut.borrow();
        let lut_view = &lut_ref.as_ref().expect("ensure_custom_lut populates the cache").view;
        let views = [
            y_tex.create_view(&wgpu::TextureViewDescriptor::default()),
            cb_tex.create_view(&wgpu::TextureViewDescriptor::default()),
            cr_tex.create_view(&wgpu::TextureViewDescriptor::default()),
        ];
        let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("falcon-yuv-bind"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&views[0]) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&views[1]) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&views[2]) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                wgpu::BindGroupEntry { binding: 4, resource: ubuf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(lut_view) },
            ],
        });
        let mut enc =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("falcon-yuv-enc") });
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("falcon-yuv-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &out_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &bind, &[]);
            rp.draw(0..3, 0..1);
        }
        queue.submit(std::iter::once(enc.finish()));
        Ok(out)
    }
}

// ───────────────── v0.8.145 (E2) — the GPU TWIN of the deterministic NV12→RGB8 kernel ─────────────────
//
// `falcon_decode::yuv_kernel::nv12_to_rgb8` is the reference; this is the same arithmetic on the GPU,
// and BYTE-IDENTITY between them is the E2 stage gate (`tests/yuv_kernel_twin.rs`). Not "close",
// not "within a tolerance" — the same bytes, because HEVC decode is bit-exact and this kernel is
// therefore the ONLY place two Falcon backends could ever disagree about a HEIC's colour.
//
// THREE THINGS MAKE THE IDENTITY PROVABLE RATHER THAN HOPEFUL, and all three are deliberate:
//
//  1. **Integer only.** Not one `f32` appears below. Float would put the answer at the mercy of the
//     driver's fused-multiply-add fusion, its `pow`/`mix` precision and its rounding mode — none of
//     which is specified by WGSL and none of which is stable across vendors. WGSL's integer
//     arithmetic, by contrast, is exactly two's-complement, and `>>` on `i32` is exactly an
//     arithmetic shift. That is why the CPU reference is fixed-point in the first place: it exists
//     to be reproducible HERE.
//
//  2. **Textures, not samplers.** The chroma upsample is computed with explicit `textureLoad` +
//     integer weights rather than handed to a `FilterMode::Linear` sampler. Hardware bilinear is
//     specified only to ~8 bits of sub-texel precision (and differs between vendors within that),
//     so a sampler-based upsample could not be byte-pinned at all. `YUV_WGSL` above legitimately
//     uses one — its contract is a displayed frame, not a pinned digest.
//
//  3. **The coefficients travel, they are not transcribed.** The Q16 integers come from
//     `YuvParams::coeffs()` and are uploaded as a uniform, so the numbers are single-source with
//     the CPU side and only the *arithmetic* is written twice. A shader that hard-coded them would
//     let the two drift silently the day someone edited one table.
//
// The workgroup is 8×8 like `DEVELOP_WGSL`; each invocation writes one packed pixel to a storage
// buffer as `r | g<<8 | b<<16`, and `convert` unpacks that to the 24 bpp stride-`w*3` contract.
/// The GPU twin's WGSL. Public so the twin harness can assert its shape, exactly as `CM_SHADER` is.
pub const NV12_RGB_WGSL: &str = r#"
struct Params {
    dims: vec4<u32>,  // x = w, y = h, z = chroma w, w = chroma h
    cfg:  vec4<u32>,  // x = siting (0 = LEFT / co-sited, 1 = CENTER / interstitial)
    co1:  vec4<i32>,  // x = ay, y = y_off, z = r_cr, w = b_cb   (all Q16)
    co2:  vec4<i32>,  // x = g_cb, y = g_cr                       (all Q16)
};
@group(0) @binding(0) var y_tex: texture_2d<u32>;    // R8Uint,  w x h
@group(0) @binding(1) var uv_tex: texture_2d<u32>;   // Rg8Uint, cw x ch (Cb in .x, Cr in .y)
@group(0) @binding(2) var<storage, read_write> out_px: array<u32>;
@group(0) @binding(3) var<uniform> P: Params;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= P.dims.x || gid.y >= P.dims.y) { return; }
    let x = i32(gid.x);
    let y = i32(gid.y);
    let cw = i32(P.dims.z);
    let ch = i32(P.dims.w);

    // Horizontal chroma tap — the twin of falcon_decode::yuv_kernel::h_tap.
    // LEFT: chroma i sits at luma 2i, so the position in chroma units is x/2 (weights 0 or 1/2).
    // CENTER: chroma i sits at luma 2i + 1/2, so the position is x/2 - 1/4. Biased by one whole
    // chroma sample (q = 2x + 3) so the shift runs over a non-negative value, then de-biased —
    // x = 0 yields i0 = -1, which the edge clamp turns into the replicate the geometry asks for.
    var i0: i32;
    var fx4: i32;
    if (P.cfg.x == 0u) {
        i0 = x >> 1u;
        fx4 = (x & 1) * 2;
    } else {
        let q = 2 * x + 3;
        i0 = (q >> 2u) - 1;
        fx4 = q & 3;
    }
    // Vertical: BOTH sitings are interstitial, so this is the centre formula unconditionally.
    let qy = 2 * y + 3;
    let j0 = (qy >> 2u) - 1;
    let fy4 = qy & 3;

    let i0c = clamp(i0, 0, cw - 1);
    let i1c = clamp(i0 + 1, 0, cw - 1);
    let j0c = clamp(j0, 0, ch - 1);
    let j1c = clamp(j0 + 1, 0, ch - 1);

    let c00 = vec2<i32>(textureLoad(uv_tex, vec2<i32>(i0c, j0c), 0).xy);
    let c01 = vec2<i32>(textureLoad(uv_tex, vec2<i32>(i1c, j0c), 0).xy);
    let c10 = vec2<i32>(textureLoad(uv_tex, vec2<i32>(i0c, j1c), 0).xy);
    let c11 = vec2<i32>(textureLoad(uv_tex, vec2<i32>(i1c, j1c), 0).xy);

    // Exact bilinear at x16 precision — no intermediate rounding, so the upsample contributes
    // zero error of its own and the only rounding in the kernel is the final shift.
    let gx = 4 - fx4;
    let gy = 4 - fy4;
    let u16v = gy * (gx * c00.x + fx4 * c01.x) + fy4 * (gx * c10.x + fx4 * c11.x);
    let v16v = gy * (gx * c00.y + fx4 * c01.y) + fy4 * (gx * c10.y + fx4 * c11.y);
    let du = u16v - 2048;   // 2048 == 128 x 16
    let dv = v16v - 2048;

    let yv = i32(textureLoad(y_tex, vec2<i32>(x, y), 0).x);
    let yt = P.co1.x * (yv - P.co1.y) * 16;
    // 524288 == 1 << 19 == half of the 2^20 scale the numerator carries: round-half-up, and the
    // arithmetic >> makes that true for negative numerators too, exactly as on the CPU.
    let r = (yt + P.co1.z * dv + 524288) >> 20u;
    let g = (yt + P.co2.x * du + P.co2.y * dv + 524288) >> 20u;
    let b = (yt + P.co1.w * du + 524288) >> 20u;

    let rp = u32(clamp(r, 0, 255));
    let gp = u32(clamp(g, 0, 255));
    let bp = u32(clamp(b, 0, 255));
    out_px[gid.y * P.dims.x + gid.x] = rp | (gp << 8u) | (bp << 16u);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Nv12Uniforms {
    dims: [u32; 4],
    cfg: [u32; 4],
    co1: [i32; 4],
    co2: [i32; 4],
}

/// The compiled NV12→RGB8 compute pipeline. Built once per device and reused, the same shape as
/// [`YuvConvert`] — E3 will hold one of these beside its decoder pool.
pub struct Nv12Kernel {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

impl Nv12Kernel {
    pub fn new(device: &wgpu::Device) -> Result<Nv12Kernel> {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("falcon-nv12-kernel"),
            source: wgpu::ShaderSource::Wgsl(NV12_RGB_WGSL.into()),
        });
        let uint_tex = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Texture {
                // Uint, not Float: an unorm sample would put the driver's 8-bit→float conversion in
                // the middle of an arithmetic chain that has to be exact.
                sample_type: wgpu::TextureSampleType::Uint,
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("falcon-nv12-bgl"),
            entries: &[
                uint_tex(0),
                uint_tex(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("falcon-nv12-pl"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("falcon-nv12-kernel"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        Ok(Nv12Kernel { pipeline, layout })
    }

    /// Convert one NV12 surface to packed RGB8 (24 bpp, stride `w*3`) on the GPU, reading the
    /// result back to the CPU.
    ///
    /// The readback is what makes this a *twin* rather than a *stage*: E3 will keep the output on
    /// the GPU and composite it there. Here the point is to hand back the identical `Vec<u8>` the
    /// CPU reference returns, so the two can be compared byte for byte.
    ///
    /// The source planes are repacked to tight rows before upload. That is an upload detail, not
    /// part of the kernel — the arithmetic never sees a stride — and E3 will bind the decoder's own
    /// surface instead of copying. `a_pitched_surface_matches_the_packed_one` on the CPU side is
    /// what pins that a stride cannot change the answer.
    pub fn convert(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &falcon_decode::yuv_kernel::Nv12Frame,
        params: falcon_decode::yuv_kernel::YuvParams,
    ) -> Result<Vec<u8>> {
        self.convert_with_coeffs(device, queue, frame, params.siting, params.coeffs())
    }

    /// [`Nv12Kernel::convert`] with the Q16 coefficients supplied directly instead of derived from
    /// a [`falcon_decode::yuv_kernel::YuvParams`].
    ///
    /// Two callers want this. The epic's own: the plan notes that a container `colr` **overrides**
    /// the VUI, so E3 may resolve a matrix the VUI did not name and needs a way in that does not
    /// route through `YuvParams`. And the twin harness's: it is how
    /// `a_perturbed_coefficient_breaks_the_identity` hands ONE twin a deliberately wrong
    /// coefficient and proves the byte-identity gate actually bites.
    pub fn convert_with_coeffs(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &falcon_decode::yuv_kernel::Nv12Frame,
        siting: falcon_decode::yuv_kernel::ChromaSiting,
        c: falcon_decode::yuv_kernel::YuvCoeffs,
    ) -> Result<Vec<u8>> {
        // v0.8.152 (R3-L1) — PARITY WITH THE CPU TWIN'S FRONT DOOR.
        //
        // `nv12_to_rgb8` opens with `f.validate()?`, which was the ONLY call site of
        // `Nv12Frame::validate` in the tree; this side checked `w == 0 || h == 0` and then SLICED
        // `frame.y[src..src + w]`. For one and the same `Nv12Frame` — a short plane, an
        // under-declared stride, a frame past the proven i32 window — the CPU twin returned a named
        // `Err` and the GPU twin PANICKED, which is a strange asymmetry in a pair whose byte
        // identity is this epic's whole colour argument. Same function now, same named errors, and
        // `validate` hands back the chroma dims so nothing recomputes them.
        let (w, h) = (frame.w, frame.h);
        let (cw, ch) = frame.validate()?;
        let limits = device.limits();
        // R3-L2: and the device's own answer BEFORE `create_texture`, whose default uncaptured-error
        // handling is a panic — so without this the `Result` this function returns is not the
        // failure mode a too-large frame actually got. `HeicAssembler::mosaic_fits` is the pattern.
        if w > limits.max_texture_dimension_2d || h > limits.max_texture_dimension_2d {
            anyhow::bail!(
                "NV12 frame {w}x{h} exceeds this device's max texture dimension {}",
                limits.max_texture_dimension_2d
            );
        }
        let rgb_bytes = (w as u64) * (h as u64) * 4;
        if rgb_bytes > limits.max_storage_buffer_binding_size {
            anyhow::bail!(
                "the frame's RGBA is {rgb_bytes} bytes, over this device's {} byte storage binding limit",
                limits.max_storage_buffer_binding_size
            );
        }
        if rgb_bytes > limits.max_buffer_size {
            anyhow::bail!(
                "the frame's RGBA is {rgb_bytes} bytes, over this device's {} byte buffer limit",
                limits.max_buffer_size
            );
        }

        // Tight repack for upload (see the doc comment).
        let mut yt = vec![0u8; (w * h) as usize];
        for r in 0..h as usize {
            let src = r * frame.y_stride;
            yt[r * w as usize..(r + 1) * w as usize]
                .copy_from_slice(&frame.y[src..src + w as usize]);
        }
        let uvw = 2 * cw as usize;
        let mut uvt = vec![0u8; uvw * ch as usize];
        for r in 0..ch as usize {
            let src = r * frame.uv_stride;
            uvt[r * uvw..(r + 1) * uvw].copy_from_slice(&frame.uv[src..src + uvw]);
        }

        let make = |data: &[u8], tw: u32, th: u32, fmt: wgpu::TextureFormat, bpp: u32, label: &str| {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: fmt,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(tw * bpp),
                    rows_per_image: Some(th),
                },
                wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
            );
            tex
        };
        let y_tex = make(&yt, w, h, wgpu::TextureFormat::R8Uint, 1, "falcon-nv12-y");
        let uv_tex = make(&uvt, cw, ch, wgpu::TextureFormat::Rg8Uint, 2, "falcon-nv12-uv");

        let px = (w as u64) * (h as u64) * 4;
        let out_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("falcon-nv12-out"),
            size: px,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let read_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("falcon-nv12-read"),
            size: px,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let y_view = y_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = uv_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let mut enc =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("falcon-nv12-enc") });
        self.encode(device, queue, &mut enc, &y_view, &uv_view, &out_buf, w, h, cw, ch, siting, c);
        enc.copy_buffer_to_buffer(&out_buf, 0, &read_buf, 0, px);
        queue.submit([enc.finish()]);
        let _ = device.poll(wgpu::PollType::wait_indefinitely());

        // Surface a map failure (device-lost / OOM) as an Err rather than panicking, like `develop`.
        let (tx, rx) = std::sync::mpsc::channel();
        read_buf.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv()
            .context("nv12 readback channel dropped")?
            .context("failed to map the nv12 readback buffer")?;

        let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
        {
            let mapped = read_buf.slice(..).get_mapped_range();
            for word in mapped.chunks_exact(4) {
                rgb.extend_from_slice(&word[0..3]); // r, g, b — the 4th byte is the unused pad
            }
        }
        read_buf.unmap();
        Ok(rgb)
    }

    /// v0.8.147 (E3-M2) — **the kernel as a PASS rather than a round trip.**
    ///
    /// [`Nv12Kernel::convert_with_coeffs`] uploads, dispatches and reads back, because E2's job was
    /// to hand the twin harness a `Vec<u8>` to compare. E3's assembly has the planes on the GPU
    /// already (a whole photo's mosaic, 74 MB of it) and wants the RGB to STAY there, so this is
    /// that same dispatch with the upload and the readback taken off either end.
    ///
    /// It is emphatically **not a second kernel**: `self.pipeline` and `self.layout` are the ones
    /// [`Nv12Kernel::new`] built from [`NV12_RGB_WGSL`], the uniform is the same
    /// [`Nv12Uniforms`] laid out the same way, and `convert_with_coeffs` now *calls this* — so the
    /// twin gate in `falcon-gpu/tests/yuv_kernel_twin.rs` is testing this exact code path and the
    /// colour arithmetic still exists in exactly two places (here's WGSL and the CPU reference).
    /// The alternative — a compositing shader with the conversion inlined — would have made a
    /// third, and the E3 charter forbids it for that reason.
    ///
    /// `out` must be a `STORAGE` buffer of at least `w · h · 4` bytes; each word is
    /// `r | g<<8 | b<<16` exactly as the readback path unpacks it.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        enc: &mut wgpu::CommandEncoder,
        y_view: &wgpu::TextureView,
        uv_view: &wgpu::TextureView,
        out: &wgpu::Buffer,
        w: u32,
        h: u32,
        cw: u32,
        ch: u32,
        siting: falcon_decode::yuv_kernel::ChromaSiting,
        c: falcon_decode::yuv_kernel::YuvCoeffs,
    ) {
        use falcon_decode::yuv_kernel::ChromaSiting;
        let u = Nv12Uniforms {
            dims: [w, h, cw, ch],
            cfg: [matches!(siting, ChromaSiting::Center) as u32, 0, 0, 0],
            co1: [c.ay, c.y_off, c.r_cr, c.b_cb],
            co2: [c.g_cb, c.g_cr, 0, 0],
        };
        let ubuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("falcon-nv12-ubuf"),
            size: std::mem::size_of::<Nv12Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&ubuf, 0, bytemuck::bytes_of(&u));
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("falcon-nv12-bind"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(y_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(uv_view) },
                wgpu::BindGroupEntry { binding: 2, resource: out.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: ubuf.as_entire_binding() },
            ],
        });
        let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("falcon-nv12-pass"),
            timestamp_writes: None,
        });
        cp.set_pipeline(&self.pipeline);
        cp.set_bind_group(0, &bind, &[]);
        cp.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
    }
}

#[cfg(test)]
mod wgsl_single_source {
    // v0.8.22 hoisted the shared rot_uv/linearize/encode block into the `rot_uv_cm_core!` macro, spliced
    // into BOTH fast-tier shaders via `concat!`. Because both shaders embed the SAME macro expansion,
    // drift between their shared cores is now structurally impossible — so the pre-v0.8.22 frozen-copy
    // byte-diff tests were retired (v0.8.47). These structural checks guard the single-source contract and
    // the v0.8.47 inverse-LUT encode path; the live-GPU boot (both pipelines must compile + render) is the
    // empirical backstop.

    #[test]
    fn both_shaders_embed_the_shared_core() {
        // Single source: both production shaders contain the identical hoisted core, verbatim.
        assert!(super::YUV_WGSL.contains(rot_uv_cm_core!()), "YUV_WGSL lost the shared core");
        assert!(super::CM_SHADER.contains(rot_uv_cm_core!()), "CM_SHADER lost the shared core");
    }

    /// v0.8.165 (WAVE 1): …and THREE shaders now, because the HEIC assembly's finish pass applies
    /// the gamut transform itself.
    ///
    /// This is the whole fidelity argument for moving HEIC's colour off the CPU, as one assertion:
    /// the arithmetic the hardware lane runs is not "equivalent to" the JPEG chain's, it is the
    /// SAME BYTES OF SHADER SOURCE — one `linearize`, one `encode`, one custom-profile LUT fetch,
    /// spliced into all three pipelines by one macro.
    ///
    /// FALSIFIER (L28): hand-write `linearize`/`encode` into `FINISH_WGSL` instead of splicing the
    /// macro — the shader still compiles, still renders, and the colour would then be free to drift
    /// from `CM_SHADER`'s the day somebody edits one of them. THIS assert is what reddens.
    #[test]
    fn the_heic_finish_pass_embeds_the_same_shared_core() {
        assert!(
            super::heic::FINISH_WGSL.contains(rot_uv_cm_core!()),
            "FINISH_WGSL lost the shared core — the HEIC lane's colour is no longer the JPEG \
             chain's colour by construction"
        );
        // …and it must actually USE it: the managed write is the only reason the core is there.
        let s = super::heic::FINISH_WGSL;
        assert!(s.contains("fn out_word"), "the managed write is gone");
        // v0.8.177: `linearize` gained `ch` + the LUT width so a faithful SOURCE profile (kind 4) can
        // sample its own per-channel forward curve. All three shaders pass the same argument shape.
        assert!(s.contains("linearize(x.r, st, 0, n)"), "out_word no longer linearises through the core");
        assert!(s.contains("encode(m0.x * lr"), "out_word no longer encodes through the core");
        // The `src == dst` pass-through — CM_SHADER's own discipline, and the reason a P3 file on
        // a P3 output is bit-exact instead of within-an-LSB.
        assert!(
            s.contains("if ((F.cf.z & 2u) == 0u) { return pack(c) | (255u << 24u); }"),
            "the unmanaged (src == dst) arm must not round-trip through linearize/encode"
        );
    }

    #[test]
    fn shared_core_encodes_custom_via_lut() {
        // The kind-2 (custom display profile) encode now samples the inverse-tone-curve LUT texture,
        // NOT a single pow(x, 1/γ). Assert the shared core carries the LUT fetch + the Adobe lock-step.
        let core = rot_uv_cm_core!();
        assert!(core.contains("kind == 2u"), "shared core lost the custom (kind 2) encode branch");
        assert!(core.contains("textureLoad(lut_tex"), "kind-2 encode must fetch the inverse-LUT texture");
        assert!(!core.contains("dst_inv_gamma"), "single-gamma custom encode must be gone");
        assert!(core.contains("2.19921875"), "shared core lost the Adobe gamma literal");
    }

    /// v0.8.177 — the SOURCE half of the same contract: a faithful source profile (kind 4)
    /// LINEARISES through rows 3..5 of the same LUT texture the kind-2 encode reads rows 0..2 of.
    ///
    /// FALSIFIER: delete the kind-4 arm from the shared core and the shaders still compile, still
    /// render, and every faithful-source photograph is silently linearised with the sRGB piecewise
    /// curve instead of its own — a ~28%-chroma-class error with no error message. These asserts are
    /// what redden. The `ch + 3` row offset is pinned explicitly because an off-by-one there reads
    /// the DESTINATION's inverse curve as if it were a forward one.
    #[test]
    fn shared_core_linearises_a_faithful_source_via_lut() {
        let core = rot_uv_cm_core!();
        assert!(core.contains("kind == 4u"), "shared core lost the faithful-source (kind 4) linearise branch");
        assert!(
            core.contains("textureLoad(lut_tex, vec2<i32>(i0, ch + 3), 0)"),
            "kind-4 linearise must fetch rows 3..5 (the FORWARD curve), not rows 0..2"
        );
        assert!(
            core.contains("fn linearize(c: f32, kind: u32, ch: i32, n: u32)"),
            "linearize must take the channel and the LUT width for the kind-4 fetch"
        );
        assert_eq!(
            core.matches("textureLoad(lut_tex").count(),
            4,
            "two fetches per direction: kind-2 encode (i0,i1) and kind-4 linearise (i0,i1)"
        );
        // Kind 4 is a SOURCE-side kind only, exactly as kind 2 is a destination-side one: `encode`
        // must NOT grow a kind-4 arm, or a faithful gamut selected as an output would encode
        // through a forward curve.
        let enc = &core[core.find("fn encode").expect("shared core has an encode")..];
        assert!(!enc.contains("kind == 4u"), "kind 4 must never appear in the encode direction");
    }

    /// The LUT width must be asserted for BOTH readers, or a faithful source on a NAMED destination
    /// gets `n = 0` and every `textureLoad` index becomes garbage. This is the exact hole a
    /// `if dst == Custom` inlined at three call sites would have left.
    #[test]
    fn lut_width_covers_both_lut_readers() {
        use falcon_color::{Gamut, CUSTOM_LUT_N};
        let n = CUSTOM_LUT_N as u32;
        assert_eq!(super::lut_width_for(Gamut::Srgb, Gamut::AdobeRgb), 0, "neither side reads the LUT");
        assert_eq!(super::lut_width_for(Gamut::Srgb, Gamut::Custom), n, "kind-2 encode reads it");
        assert_eq!(super::lut_width_for(Gamut::SourceIcc(0), Gamut::Srgb), n, "kind-4 linearise reads it");
        assert_eq!(super::lut_width_for(Gamut::SourceIcc(0), Gamut::Custom), n, "both read it");
    }

    #[test]
    fn dcip3_kind3_gamma_in_lockstep() {
        // v0.8.67: TRC kind 3 = the DCI-P3 pure gamma. Both `linearize` and `encode` in the shared
        // core must carry the 2.6 arm (== falcon_color::DCI_GAMMA), so BOTH assembled shaders get it.
        let core = rot_uv_cm_core!();
        assert_eq!(core.matches("kind == 3u").count(), 2, "linearize AND encode need the kind-3 arm");
        assert!(core.contains("pow(max(c, 0.0), 2.6)"), "kind-3 linearize must be the pure 2.6 gamma");
        assert!(core.contains("pow(x, 1.0 / 2.6)"), "kind-3 encode must be the pure 1/2.6 gamma");
        assert!((falcon_color::DCI_GAMMA - 2.6).abs() < 1e-6, "WGSL literal out of lock-step with DCI_GAMMA");
        assert_eq!(falcon_color::Gamut::DciP3.trc_kind(), 3);
    }

    #[test]
    fn yuv_body_constants_pinned() {
        // v0.8.48 audit rider: the YUV fragment BODY is NOT inside the shared `rot_uv_cm_core!` macro,
        // and the retired pre-v0.8.22 frozen-copy byte-diff tests were the only cargo-test pin on it —
        // this restores one cheaply. The JFIF BT.601 full-range coefficients and the INTEGER-128 chroma
        // offset (128/255 = 0.5019608, NOT 0.5 = 127.5 — the parity gate measured the 0.5-centred
        // version ~1 LSB off everywhere, amplified to ~6 LSB near black through Adobe RGB's pure-power
        // TRC) are load-bearing for GPU↔CPU parity.
        let s = super::YUV_WGSL;
        assert_eq!(
            s.matches("- 0.5019608").count(),
            2,
            "BOTH cb and cr must subtract the integer-128 chroma offset (128/255)"
        );
        for lit in ["1.402 * cr", "0.344136 * cb", "0.714136 * cr", "1.772 * cb"] {
            assert!(s.contains(lit), "YUV_WGSL lost the BT.601 term `{lit}`");
        }
    }

    #[test]
    fn both_shaders_bind_the_lut_texture() {
        // Each shader must declare a `lut_tex` global for the shared `encode` to reference (YUV @5, CM @3).
        assert!(super::YUV_WGSL.contains("@binding(5) var lut_tex: texture_2d<f32>"), "YUV_WGSL missing lut_tex");
        assert!(super::CM_SHADER.contains("@binding(3) var lut_tex: texture_2d<f32>"), "CM_SHADER missing lut_tex");
        // flags.w now carries the LUT width (read as u32), no longer a bitcast float gamma.
        assert!(!super::YUV_WGSL.contains("bitcast<f32>(P.flags.w)"), "YUV_WGSL still bitcasts flags.w");
        assert!(!super::CM_SHADER.contains("bitcast<f32>(P.flags.w)"), "CM_SHADER still bitcasts flags.w");
    }
}
