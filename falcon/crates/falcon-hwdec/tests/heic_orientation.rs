//! Real-pixel regression for the iPhone selfie. Only private copies are modified.
#![cfg(windows)]

#[path = "../../test-support/hw_child.rs"]
mod hw_child;

#[path = "../../test-support/fixture_paths.rs"]
mod fixture_paths;

use falcon_decode::*;
use std::path::PathBuf;

#[test]
fn selfie_hardware_pixels_match_wic_for_both_mirrors_and_all_rotations() {
    if !hw_child::run_here(module_path!(), "selfie_hardware_pixels_match_wic_for_both_mirrors_and_all_rotations") { return; }
    let path = std::env::var_os("FALCON_SELFIE_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            fixture_paths::photos().join("IMG_3223.HEIC")
        });
    if !fixture_paths::require_file(&path) || !wic_heif_codec_present() {
        eprintln!("SKIP selfie pixels: fixture or Windows HEIF codec unavailable");
        return;
    }
    let original = std::fs::read(&path).unwrap();
    let src = falcon_hwdec::tile_source(&path).unwrap();
    let Ok(mut decoder) = falcon_hwdec::PhotoDecoder::new(&src, 8) else {
        eprintln!("SKIP selfie pixels: D3D11VA/assembly adapter unavailable");
        return;
    };
    let dir = std::env::temp_dir().join(format!(
        "falcon_selfie_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let copy = dir.join("selfie.heic");
    let shot = Shot {
        id: 0,
        name: "selfie".into(),
        has_raw: false,
        has_jpg: true,
        raw: None,
        jpg: Some(copy.clone()),
        kind: SrcKind::Heic,
        cloud_placeholder: false,
        sniffed: None,
    };
    let r = original.windows(4).position(|b| b == b"irot").unwrap() + 4;
    let m = original.windows(4).position(|b| b == b"imir").unwrap() + 4;
    assert_eq!((original[r], original[m]), (1, 0), "fixture changed");
    // This file associates the primary item (36) with colr/ispe/pixi/irot/imir, in that order.
    let association = original
        .windows(8)
        .position(|b| b == [0, 36, 5, 0x81, 3, 6, 0x84, 0x85])
        .unwrap();
    std::fs::write(&copy, &original).unwrap();
    let wic = browse_frame_rgba(&shot, 256, true, Lane::Native).unwrap();
    // Original is EXIF 5 (transpose). Undo it by transposing WIC's upright image, yielding
    // stored-space reference pixels. Transform these independently for each synthetic variant.
    let (sw, sh) = (wic.h, wic.w);
    let mut stored = vec![0u8; (sw * sh * 3) as usize];
    for y in 0..sh {
        for x in 0..sw {
            let a = ((y * sw + x) * 3) as usize;
            let b = ((x * wic.w + y) * 4) as usize;
            stored[a..a + 3].copy_from_slice(&wic.rgba[b..b + 3]);
        }
    }
    for reversed in [false, true] {
        for rot in 0..4 {
            for mirror in 0..2 {
                let mut bytes = original.clone();
                bytes[r] = rot;
                bytes[m] = mirror;
                if reversed {
                    bytes.swap(association + 6, association + 7);
                }
                std::fs::write(&copy, &bytes).unwrap();
                let src = falcon_hwdec::tile_source(&copy).unwrap();
                let hw = decoder.decode(&src, Some(256)).unwrap();
                let mut expected = stored.clone();
                if reversed {
                    reflect(&mut expected, sw, sh, mirror);
                }
                let (mut expected, ew, eh) = rotate_rgb(&expected, sw, sh, (4 - rot) % 4);
                if !reversed {
                    reflect(&mut expected, ew, eh, mirror);
                }
                assert_eq!((hw.w, hw.h), (ew, eh));
                let sum: u64 = hw
                    .rgb
                    .iter()
                    .zip(&expected)
                    .map(|(a, b)| u64::from(a.abs_diff(*b)))
                    .sum();
                let mean = sum as f64 / hw.rgb.len() as f64;
                eprintln!("selfie rot={rot} mirror={mirror} reversed={reversed}: mean RGB delta {mean:.3}");
                assert!(mean < 8.0, "wrong hardware transform: delta {mean}");
            }
        }
    }
    // Reader and Apply round-trip on an unmodified copy, for all pending clockwise rotations.
    std::fs::write(&copy, &original).unwrap();
    assert_eq!(
        orientation_to_turns(read_orientation(&shot, false).unwrap()),
        0
    );
    for turns in 0..4u8 {
        let abs = compose_exif_orientation(5, turns);
        let sidecar = dir.join("selfie.heic.xmp");
        std::fs::write(
            &sidecar,
            format!("<rdf:Description tiff:Orientation=\"{abs}\"/>"),
        )
        .unwrap();
        assert_eq!(
            orientation_to_turns(read_orientation(&shot, false).unwrap()),
            turns
        );
    }
    std::fs::remove_file(dir.join("selfie.heic.xmp")).unwrap();
    for expected in [1, 2, 3, 0] {
        let base = orientation_to_turns(read_orientation(&shot, false).unwrap());
        let result = apply_rotation(&RotApplyPlan {
            finished: Some(copy.clone()),
            finished_is_jpeg: false,
            raw: None,
            base_turns: base,
            delta: 1,
        });
        assert!(result.ok);
        assert_eq!(
            orientation_to_turns(read_orientation(&shot, false).unwrap()),
            expected
        );
    }
    std::fs::remove_file(dir.join("selfie.heic.xmp")).unwrap();
    std::fs::remove_file(copy).unwrap();
    std::fs::remove_dir(dir).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), original, "source changed");
}

fn reflect(rgb: &mut [u8], w: u32, h: u32, bit: u8) {
    let original = rgb.to_vec();
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = if bit == 0 {
                (x, h - 1 - y)
            } else {
                (w - 1 - x, y)
            };
            let dst = ((y * w + x) * 3) as usize;
            let src = ((sy * w + sx) * 3) as usize;
            rgb[dst..dst + 3].copy_from_slice(&original[src..src + 3]);
        }
    }
}
