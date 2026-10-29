//! v0.8.0 rotation: LIVE-GPU readback tests for the fused YUV convert's rotation UV transform —
//! the display-side half of the EXIF auto-orient round. A tiny asymmetric planar-YUV frame is
//! converted at each `turns` value on a headless device; the readback must show the output dims
//! swapped for odd turns and every pixel placed exactly where `falcon_decode::rotate_rgba` (the
//! CPU convention the ROI mapping + thumbs share) puts it. Self-skips without a GPU adapter.
//!
//! Colour stays out of scope here (src == dst ⇒ the gamut arm is off; cm_parity.rs owns the CM
//! maths) — luma-only pixels make the placement assertion byte-exact despite chroma filtering.

#[path = "../../test-support/hw_child.rs"]
mod hw_child;

use falcon_gpu::{YuvConvert, YuvPlanes};

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .ok()?;
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
}

/// Read an RGBA8 texture back to bytes (256-aligned rows stripped).
fn read_texture(device: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture, w: u32, h: u32) -> Vec<u8> {
    let unpadded = w * 4;
    let padded = unpadded.div_ceil(256) * 256;
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    queue.submit([enc.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buf.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    rx.recv().expect("map channel").expect("map readback");
    let mut out = Vec::with_capacity((unpadded * h) as usize);
    {
        let mapped = buf.slice(..).get_mapped_range();
        for row in 0..h {
            let start = (row * padded) as usize;
            out.extend_from_slice(&mapped[start..start + unpadded as usize]);
        }
    }
    buf.unmap();
    out
}

#[test]
fn yuv_convert_rotates_like_the_cpu_convention() {
    if !hw_child::run_here(module_path!(), "yuv_convert_rotates_like_the_cpu_convention") { return; }
    let Some((device, queue)) = device() else {
        eprintln!("skip: no GPU adapter");
        return;
    };
    let conv = YuvConvert::new(&device).expect("yuv pipeline");

    // A 4×2 luma-labelled frame (Y = unique per pixel, chroma neutral ⇒ grey levels).
    // Row 0: 10 40 70 100 / row 1: 140 170 200 230.
    let (w, h) = (4u32, 2u32);
    let y: Vec<u8> = vec![10, 40, 70, 100, 140, 170, 200, 230];
    // Full-res chroma planes (cw=w, ch=h) at the JFIF neutral 128 — no subsampling filter effects.
    let cb = vec![128u8; (w * h) as usize];
    let cr = vec![128u8; (w * h) as usize];
    let planes = YuvPlanes { y: &y, cb: &cb, cr: &cr, w, h, cw: w, ch: h };

    // The CPU-side reference: the same grey frame as RGBA, rotated by falcon-decode's convention.
    let cpu_rgba: Vec<u8> = y.iter().flat_map(|&v| [v, v, v, 255u8]).collect();

    for turns in 0..4u8 {
        let tex = conv.convert(
            &device,
            &queue,
            &planes,
            falcon_color::Gamut::Srgb,
            falcon_color::Gamut::Srgb, // src == dst: pure YUV→RGB + rotation, no gamut arm
            turns,
        )
        .expect("v0.8.165: the fused convert pre-checks the device limits and returns Result");
        let (ow, oh) = if turns & 1 == 1 { (h, w) } else { (w, h) };
        assert_eq!((tex.width(), tex.height()), (ow, oh), "turns {turns}: output dims swap");
        let gpu = read_texture(&device, &queue, &tex, ow, oh);
        let (want, ww, wh) = falcon_decode::rotate_rgba(&cpu_rgba, w, h, turns);
        assert_eq!((ww, wh), (ow, oh));
        // Compare the R channel per pixel with a ±2 LSB tolerance (BT.601 round-trip on the GPU).
        for i in 0..(ow * oh) as usize {
            let (g, c) = (gpu[i * 4] as i32, want[i * 4] as i32);
            assert!(
                (g - c).abs() <= 2,
                "turns {turns}: pixel {i} GPU {g} vs CPU convention {c} (gpu row: {:?})",
                &gpu[..(ow * 4) as usize]
            );
        }
    }
}
/// v0.8.167 (audit, finder 2) — **THE PRE-CHECK'S `Err` ARMS, RUN.**
///
/// v0.8.165 gave `YuvConvert::convert` a device-limit pre-check and a `Result`, closing QUEUE
/// §2 5c(ii) — and shipped no row that ever took the `Err`. Every existing row feeds it a frame
/// that fits, so the four `bail!`s were dead code as far as the suite was concerned: a refusal
/// that silently became a PANIC (the exact failure the pre-check exists to prevent, since
/// `create_texture` past the limit is an uncaptured error) would not have reddened anything.
///
/// The two shapes here are the two that can actually reach it in the field: a source past this
/// device's own `max_texture_dimension_2d` (the iGPU clause — a 16384-capped adapter meeting a
/// panorama), and a plane shorter than the dims declare (a truncated decode; without the check
/// wgpu slices out of bounds inside its own copy). Both must ANSWER, not panic — and the answer
/// must name the reason, because `native/src/support.rs` logs it verbatim.
///
/// FALSIFIER (L28): delete the `p.w > dim || p.h > dim` guard and the first block panics inside
/// wgpu instead of returning; delete the `p.y.len() < need(..)` guard and the second one does.
/// Weaken either message and the substring asserts redden.
#[test]
fn the_fused_convert_refuses_rather_than_panics() {
    if !hw_child::run_here(module_path!(), "the_fused_convert_refuses_rather_than_panics") { return; }
    let Some((device, queue)) = device() else {
        eprintln!("skip: no GPU adapter");
        return;
    };
    let conv = YuvConvert::new(&device).expect("build the fused pipeline");
    let dim = device.limits().max_texture_dimension_2d;

    // (1) OVER-LIMIT DIMS. One row of a frame one pixel wider than this device admits — the
    // planes are sized to the DECLARED dims so the only thing wrong is the size the device will
    // not take. Nothing is allocated: the check answers before `create_texture`.
    let (ow, oh) = (dim + 1, 1u32);
    let over = YuvPlanes {
        y: &vec![128u8; (ow as usize) * (oh as usize)],
        cb: &vec![128u8; ow.div_ceil(2) as usize],
        cr: &vec![128u8; ow.div_ceil(2) as usize],
        w: ow,
        h: oh,
        cw: ow.div_ceil(2),
        ch: 1,
    };
    let err = conv
        .convert(&device, &queue, &over, falcon_color::Gamut::Srgb, falcon_color::Gamut::Srgb, 0)
        .expect_err("a frame past the device's max texture dimension must be refused")
        .to_string();
    assert!(
        err.contains("max texture dimension") && err.contains(&dim.to_string()),
        "the refusal must name the limit that said no: {err}"
    );

    // (2) A SHORT PLANE. Dims this device certainly admits, and a Y plane one byte short of what
    // they declare — the truncated-decode shape.
    let (w, h) = (64u32, 32u32);
    let short_y = vec![128u8; (w as usize) * (h as usize) - 1];
    let chroma = vec![128u8; (w.div_ceil(2) as usize) * (h.div_ceil(2) as usize)];
    let short = YuvPlanes {
        y: &short_y,
        cb: &chroma,
        cr: &chroma,
        w,
        h,
        cw: w.div_ceil(2),
        ch: h.div_ceil(2),
    };
    let err = conv
        .convert(&device, &queue, &short, falcon_color::Gamut::Srgb, falcon_color::Gamut::Srgb, 0)
        .expect_err("a plane shorter than its dims declare must be refused")
        .to_string();
    assert!(
        err.contains("Y plane") && err.contains(&short_y.len().to_string()),
        "the refusal must say which plane is short and by how much: {err}"
    );

    // …and the SAME device still serves a well-formed frame afterwards: the refusals are
    // per-frame, which is the property `support.rs`'s v0.8.167 memo depends on.
    let y = vec![128u8; (w as usize) * (h as usize)];
    let ok = YuvPlanes {
        y: &y,
        cb: &chroma,
        cr: &chroma,
        w,
        h,
        cw: w.div_ceil(2),
        ch: h.div_ceil(2),
    };
    let tex = conv
        .convert(&device, &queue, &ok, falcon_color::Gamut::Srgb, falcon_color::Gamut::Srgb, 0)
        .expect("a well-formed frame must still convert after two refusals");
    assert_eq!((tex.width(), tex.height()), (w, h));
}
