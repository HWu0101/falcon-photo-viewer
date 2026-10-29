//! v0.8.145 (E2) — **THE STAGE GATE**: the WGSL NV12→RGB8 kernel must be byte-identical to the CPU
//! reference, and both must hit the same pinned digests.
//!
//! Why byte-identity and not a tolerance: Stage 0 measured HEVC decode to be bit-exact (one real
//! 896×1024 tile through NVDEC and through libavcodec came back byte-identical, 0 of 1 376 256
//! bytes different, on two different iOS generations). Every backend the epic can adopt therefore
//! hands back the *same* NV12, and this kernel is the only place a difference could enter. A
//! tolerance here would be a licence for the CPU fallback lane and the hardware lane to render the
//! same photo differently and call it agreement.
//!
//! Three separable failures, three separate assertions:
//!
//!  1. `the_twins_are_byte_identical_on_every_golden_row` — GPU bytes == CPU bytes.
//!  2. `the_golden_digests_hold_on_the_gpu_twin` — GPU digest == the pinned digest. (If only #1
//!     held, both twins could have moved together; if only #2 held, they could agree with the pin
//!     while disagreeing about the bytes they hashed — they cannot, but the pair says so.)
//!  3. `a_perturbed_coefficient_breaks_the_identity` — the gate BITES. A deliberately wrong
//!     coefficient handed to one twin must redden #1, or #1 is comparing nothing.
//!
//! GPU REQUIRED, and NOT skipped on this box: falcon-gpu's harness runs on the owner's RTX 5080.
//! The graceful skip below exists only for a genuinely adapter-less machine (headless CI), the same
//! convention `shader_compile.rs` already uses — a skip that ever fires HERE is a finding, so it
//! prints loudly.

#[path = "../../test-support/hw_child.rs"]
mod hw_child;

use falcon_decode::yuv_kernel::*;
use falcon_gpu::Nv12Kernel;

fn headless() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .ok()?;
    eprintln!("yuv_kernel_twin: adapter = {}", adapter.get_info().name);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_limits: adapter.limits(),
        ..Default::default()
    }))
    .ok()?;
    Some((device, queue))
}

/// The SAME two real-tile fixtures `falcon-decode/tests/yuv_kernel_pins.rs` includes, by the same
/// names, from the shared `crates/testdata/` directory — so both harnesses drive identical bytes
/// through identical golden rows and only the *converter* differs.
#[path = "../../test-support/private_tiles.rs"]
mod private_tiles;
fn fixtures() -> private_tiles::PrivateTiles { private_tiles::load() }

/// Report the first divergence in a way that says WHICH pixel and by how much, because "10 bytes
/// differ" and "10 bytes differ by 1 in the blue channel along the left edge" are different bugs.
fn first_diff(cpu: &[u8], gpu: &[u8], w: u32) -> Option<String> {
    if cpu.len() != gpu.len() {
        return Some(format!("length {} vs {}", cpu.len(), gpu.len()));
    }
    let n = cpu.iter().zip(gpu.iter()).filter(|(a, b)| a != b).count();
    let i = cpu.iter().zip(gpu.iter()).position(|(a, b)| a != b)?;
    let (px, ch) = (i / 3, i % 3);
    Some(format!(
        "{n} of {} bytes differ; first at pixel ({}, {}) channel {} — CPU {} vs GPU {} (max |delta| {})",
        cpu.len(),
        px as u32 % w,
        px as u32 / w,
        ["R", "G", "B"][ch],
        cpu[i],
        gpu[i],
        cpu.iter().zip(gpu.iter()).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap_or(0)
    ))
}

/// **The gate.** Every golden row, both twins, byte for byte.
#[test]
fn the_twins_are_byte_identical_on_every_golden_row() {
    if !hw_child::run_here(module_path!(), "the_twins_are_byte_identical_on_every_golden_row") { return; }
    let Some((device, queue)) = headless() else {
        eprintln!("!! yuv_kernel_twin: NO GPU ADAPTER — the E2 stage gate did NOT run");
        return;
    };
    let k = Nv12Kernel::new(&device).expect("NV12_RGB_WGSL must compile + build its pipeline");
    let tiles = fixtures();
    let fx = tiles.refs();
    let mut bad = Vec::new();
    for c in tiles.cases() {
        let (y, uv) = yuv_golden_planes(c, &fx);
        let f = Nv12Frame::packed(&y, &uv, c.w, c.h);
        let cpu = nv12_to_rgb8(&f, c.params).expect("CPU reference must convert");
        let gpu = k.convert(&device, &queue, &f, c.params).expect("GPU twin must convert");
        match first_diff(&cpu, &gpu, c.w) {
            None => println!("{:<40} {:>4}x{:<4} IDENTICAL ({} bytes)", c.name, c.w, c.h, cpu.len()),
            Some(d) => bad.push(format!("  {:<40} {d}", c.name)),
        }
    }
    assert!(
        bad.is_empty(),
        "CPU/GPU divergence on {} of {} golden rows:\n{}",
        bad.len(),
        GOLDEN_PINS.len(),
        bad.join("\n")
    );
}

/// The GPU twin must land on the SAME pinned digests as the CPU reference — an independent
/// statement from "the twins agree", and the one that would catch both twins moving together.
#[test]
fn the_golden_digests_hold_on_the_gpu_twin() {
    if !hw_child::run_here(module_path!(), "the_golden_digests_hold_on_the_gpu_twin") { return; }
    let Some((device, queue)) = headless() else {
        eprintln!("!! yuv_kernel_twin: NO GPU ADAPTER — the E2 digest check did NOT run");
        return;
    };
    let k = Nv12Kernel::new(&device).expect("pipeline");
    let tiles = fixtures();
    let fx = tiles.refs();
    let mut bad = Vec::new();
    for c in tiles.cases() {
        let (y, uv) = yuv_golden_planes(c, &fx);
        let f = Nv12Frame::packed(&y, &uv, c.w, c.h);
        let gpu = k.convert(&device, &queue, &f, c.params).expect("GPU twin must convert");
        let d = sha256_hex(&gpu);
        if d != c.digest {
            bad.push(format!("  {:<40} pinned {} got {d}", c.name, c.digest));
        }
    }
    assert!(bad.is_empty(), "GPU digest drift on {} row(s):\n{}", bad.len(), bad.join("\n"));
}

/// **THE FALSIFIER — the gate must BITE**, plus the measurement of how hard it bites.
///
/// Hand the GPU twin one deliberately wrong matrix coefficient and the byte-identity above must
/// FAIL. If it cannot be made to fail, the identity test is comparing nothing and every
/// "IDENTICAL" line it prints is worthless.
///
/// The realistic slip is a **transcribed coefficient from the wrong matrix** — BT.709's `b_cb`
/// (121609) inside an otherwise BT.601 conversion. That is the mistake a hand-maintained shader
/// constant table actually makes, and it is what part A perturbs.
///
/// Part B then measures the gate's RESOLUTION, because the first attempt at this test asked the
/// wrong question and is worth recording. Perturbing `b_cb` by ONE Q16 unit (1/65536 of the
/// coefficient) changed **not a single output byte**, and that is arithmetically correct rather
/// than a weak gate: the luma term is an exact multiple of 2^20, so a pixel's blue output is
/// `clamp(Y + f(du), 0, 255)` and a 1-unit coefficient error only moves `f` for a `du` whose
/// numerator happens to sit within `|du| ≤ 2048` of a 2^20 rounding boundary — roughly one chance
/// in 512 per distinct `du`, over the ~510 distinct `du` this pattern contains. A perturbation
/// that changes no output byte **is not a fidelity difference at all**, so a gate that ignores it
/// is behaving exactly right. Part B sweeps δ upward and reports the smallest perturbation that
/// does move a byte — the honest statement of what this gate can and cannot resolve.
#[test]
fn a_perturbed_coefficient_breaks_the_identity() {
    if !hw_child::run_here(module_path!(), "a_perturbed_coefficient_breaks_the_identity") { return; }
    let Some((device, queue)) = headless() else {
        eprintln!("!! yuv_kernel_twin: NO GPU ADAPTER — the falsifier did NOT run");
        return;
    };
    let k = Nv12Kernel::new(&device).expect("pipeline");
    // The lattice row: every (U,V) pair, so a coefficient error cannot hide in an unvisited corner.
    let (w, h) = (512u32, 512u32);
    let (y, uv) = yuv_synth_nv12(YuvSynthPattern::Cube, w, h);
    let f = Nv12Frame::packed(&y, &uv, w, h);
    let params =
        YuvParams { matrix: YuvMatrix::Bt601, range: YuvRange::Full, siting: ChromaSiting::Left };
    let base = params.coeffs();

    // Control: unperturbed, the twins agree. Without this the rest proves nothing.
    let cpu = nv12_to_rgb8(&f, params).expect("cpu");
    let gpu = k.convert(&device, &queue, &f, params).expect("gpu");
    assert_eq!(
        first_diff(&cpu, &gpu, w),
        None,
        "the control must be identical, or the falsifier proves nothing"
    );

    // ── A: the wrong matrix's coefficient, in one twin only. ──
    let wrong = YuvParams { matrix: YuvMatrix::Bt709, ..params }.coeffs().b_cb;
    assert_ne!(wrong, base.b_cb, "the two matrices must actually differ here");
    let gpu_bad = k
        .convert_with_coeffs(&device, &queue, &f, params.siting, YuvCoeffs { b_cb: wrong, ..base })
        .expect("gpu (perturbed)");
    let d = first_diff(&cpu, &gpu_bad, w);
    assert!(
        d.is_some(),
        "BT.709's b_cb inside a BT.601 conversion produced IDENTICAL output — the gate does not bite"
    );
    println!("falsifier A (709 b_cb {wrong} for 601's {}): {}", base.b_cb, d.unwrap());

    // ── B: the gate's resolution, measured rather than assumed. ──
    let mut smallest = None;
    for delta in 1..=256i32 {
        let bent = YuvCoeffs { b_cb: base.b_cb + delta, ..base };
        let g = k.convert_with_coeffs(&device, &queue, &f, params.siting, bent).expect("gpu");
        if g != cpu {
            let n = cpu.iter().zip(g.iter()).filter(|(a, b)| a != b).count();
            smallest = Some((delta, n));
            break;
        }
    }
    let (delta, n) = smallest.expect("no coefficient error up to 256 Q16 units moved a single byte");
    println!(
        "falsifier B: the smallest b_cb error that moves a byte is {delta} Q16 units \
         ({:.6} of a coefficient), and it moves {n} of {} bytes",
        delta as f64 / 65536.0,
        cpu.len()
    );
    assert!(delta <= 256, "the gate cannot resolve coefficient errors under 256 Q16 units");
}

/// The shader's shape, so the properties the byte-identity depends on cannot silently rot into
/// something that still compiles and still passes on this driver but not the next one.
#[test]
fn the_twin_shader_stays_integer_only() {
    let s = falcon_gpu::NV12_RGB_WGSL;
    assert!(s.contains("@compute @workgroup_size(8, 8)"), "the twin lost its compute entry");
    // No float type may appear: f32/f16 anywhere puts the driver's rounding into the chain.
    assert!(!s.contains("f32"), "the twin must be integer-only — no f32");
    assert!(!s.contains("f16"), "the twin must be integer-only — no f16");
    // No sampler: hardware bilinear is only ~8-bit-precise and differs between vendors, so the
    // chroma upsample has to be explicit textureLoad + integer weights.
    assert!(!s.contains("sampler"), "the twin must not sample — the upsample is explicit");
    assert!(s.contains("textureLoad(uv_tex"), "the twin must load chroma explicitly");
    // The rounding step, BUILT FROM THE CPU REFERENCE'S OWN CONSTANTS and then required to appear
    // verbatim once per channel — so the WGSL literals cannot drift out of lock-step with
    // `ROUND_ADDEND`/`NUM_SHIFT`, in either direction, without this failing.
    //
    // Two earlier drafts of this assertion were both weaker, and both are worth recording. A bare
    // `contains("524288")` stayed GREEN while the live arithmetic said 524287, because the
    // shader's own explanatory comment contains the number — a shape guard a comment can satisfy
    // guards nothing. And asserting the two constants against hard-coded 524288/20 was a
    // tautology over compile-time constants that said nothing about the shader at all.
    let step = format!(
        "+ {}) >> {}u",
        falcon_decode::yuv_kernel::ROUND_ADDEND,
        falcon_decode::yuv_kernel::NUM_SHIFT
    );
    assert_eq!(
        s.matches(step.as_str()).count(),
        3,
        "all three channels must round with the CPU reference's own `{step}`"
    );
    assert!(s.contains("- 2048"), "the twin lost the x16 neutral-chroma offset (128 x 16)");
    // Uint textures, not unorm — an unorm fetch would insert the driver's 8-bit -> float convert.
    assert!(s.contains("texture_2d<u32>"), "the twin must read the planes as Uint");
    // The coefficients arrive as a uniform; a hard-coded number here is a drift waiting to happen.
    for lit in ["1.402", "1.772", "0.344136", "1.5748"] {
        assert!(!s.contains(lit), "the twin hard-codes the coefficient {lit} instead of taking the uniform");
    }
}
