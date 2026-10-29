//! Integration test: GPU-develop a real Canon CFA and sanity-check the result.
//! Self-skips if the test folder or a GPU is unavailable.

#[path = "../../test-support/hw_child.rs"]
mod hw_child;

#[path = "../../test-support/fixture_paths.rs"]
mod fixture_paths;

use std::path::Path;

use falcon_decode::{extract_cfa, scan_folder};
use falcon_gpu::GpuDeveloper;


#[test]
fn gpu_develops_real_cfa() {
    if !hw_child::run_here(module_path!(), "gpu_develops_real_cfa") { return; }
    let dir = Path::new(fixture_paths::photos());
    if !dir.exists() {
        eprintln!("skip: no test folder");
        return;
    }
    let shots = scan_folder(dir).expect("scan");
    let raw = shots.iter().find(|s| s.has_raw).expect("a raw shot");
    let cfa = extract_cfa(raw).expect("extract cfa");
    assert!(cfa.rggb, "test camera should be RGGB");
    assert!(cfa.crop_w >= 4000 && cfa.crop_h >= 3000, "crop {}x{}", cfa.crop_w, cfa.crop_h);

    let dev = match GpuDeveloper::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip: no GPU available: {e:?}");
            return;
        }
    };
    let (rgb, w, h) = dev.develop(&cfa, 2048).expect("gpu develop");
    assert_eq!(rgb.len(), (w * h * 3) as usize);
    assert!(w.max(h) <= 2048 && w.max(h) >= 1024, "preview {w}x{h}");

    // Not a blank frame: average brightness should be a sane mid-range value.
    let avg = rgb.iter().map(|&b| b as f64).sum::<f64>() / rgb.len() as f64;
    assert!(avg > 5.0 && avg < 250.0, "suspicious average brightness {avg}");
}
