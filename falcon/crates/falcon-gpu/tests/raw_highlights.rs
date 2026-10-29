//! Real GPU pixel regressions, independent of the RAW decompressor and UI.
#[path = "../../test-support/hw_child.rs"]
mod hw_child;

use falcon_decode::Cfa;

#[test]
fn clipped_neutrals_and_bright_colours_follow_cpu_highlight_policy() {
    if !hw_child::run_here(module_path!(), "clipped_neutrals_and_bright_colours_follow_cpu_highlight_policy") { return; }
    let gpu = match falcon_gpu::GpuDeveloper::new() {
        Ok(gpu) => gpu,
        Err(e) => {
            eprintln!("SKIP raw_highlights: no GPU: {e}");
            return;
        }
    };
    let identity = [[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]];
    let canon = [
        [1.5943218, -0.62378603, 0.029464187],
        [-0.17381532, 1.6337733, -0.4599579],
        [-0.017072674, -0.44437557, 1.4614482],
    ];
    // Known gamma and CPU-policy reference pixels. No image-wide "bright = white" rule.
    for (name, sensor, wb, matrix, expected) in [
        (
            "unclipped colour",
            [0.25, 0.5, 0.75],
            [1.; 3],
            identity,
            [137, 188, 225],
        ),
        (
            "bright red below clipping",
            [0.8, 0.1, 0.1],
            [1.; 3],
            identity,
            [231, 89, 89],
        ),
        (
            "red above nominal white",
            [2., 0., 0.],
            [1.; 3],
            identity,
            [255, 200, 200],
        ),
        (
            "blue above nominal white",
            [0., 0., 2.],
            [1.; 3],
            identity,
            [200, 200, 255],
        ),
        ("neutral white", [1.; 3], [1.; 3], identity, [255; 3]),
        (
            "Canon saturated daylight",
            [1.; 3],
            [1.875, 1., 1.5605469],
            canon,
            [255; 3],
        ),
        (
            "tungsten balanced grey",
            [0.25, 0.5, 0.125],
            [2., 1., 4.],
            identity,
            [188; 3],
        ),
        (
            "tungsten saturated",
            [1.; 3],
            [2., 1., 4.],
            identity,
            [255; 3],
        ),
    ] {
        let data = (0..12 * 12)
            .map(|i| {
                let channel = match (i % 12 % 2, i / 12 % 2) {
                    (0, 0) => 0,
                    (1, 1) => 2,
                    _ => 1,
                };
                (512. + sensor[channel] * 10000.) as u16
            })
            .collect();
        let cfa = Cfa {
            data,
            width: 12,
            height: 12,
            crop_x: 2,
            crop_y: 2,
            crop_w: 8,
            crop_h: 8,
            black: 512.,
            white: 10512.,
            wb,
            cam_to_srgb: matrix,
            rggb: true,
        };
        let (rgb, w, h) = gpu.develop(&cfa, 8).expect(name);
        assert_eq!((w, h), (8, 8));
        for pixel in rgb.chunks_exact(3) {
            for channel in 0..3 {
                assert!(
                    (pixel[channel] as i32 - expected[channel]).abs() <= 1,
                    "{name}: {pixel:?}, expected {expected:?}"
                );
            }
        }
    }
}
