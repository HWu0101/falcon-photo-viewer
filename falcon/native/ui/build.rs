//! Build script of the interface crate: everything the Slint compiler reads while the `slint!`
//! macros in `lib.rs` expand. These settings moved here from `native/build.rs` with the markup
//! (build-time round, 11 November 2026); what they do is unchanged.

#[path = "../build_translations.rs"]
#[allow(dead_code)] // the `packs.rs` half is the app's (`native/build.rs`)
mod build_translations;

fn main() {
    guard_slint_scale_factor();
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let native = manifest.parent().expect("the interface crate sits in falcon/native/ui");
    build_translations::build_slint_bundle(&native.join("translations"));
    // v0.8.46: DEBUG builds emit Slint element debug info so the headless geometry rigs
    // (tipgeom_tests.rs) can query real elements via i-slint-backend-testing's ElementHandle
    // (find_by_element_type_name / absolute_position). The env var is read by the slint!
    // proc-macro at expansion time; `cargo:rustc-env` sets it inside the rustc process.
    // RELEASE stays without it — the shipping exe carries no element-name metadata.
    //
    // v0.8.103 (T2): `FALCON_RIG_DEBUG_INFO=1` in the BUILD environment also arms it, so a
    // deliberate `cargo test --release` run of the geometry rig is possible. Without that
    // escape hatch a release rig run built a binary in which EVERY ElementHandle lookup
    // finds zero elements, and ~26 tests failed on downstream `count == 0` assertions that
    // blamed the UI. The default is unchanged: shipping exes carry no element metadata,
    // because nothing sets this variable in a normal build or in the milestone protocol.
    println!("cargo:rerun-if-env-changed=FALCON_RIG_DEBUG_INFO");
    let debug_profile = std::env::var("PROFILE").as_deref() == Ok("debug");
    let rig_opt_in = std::env::var("FALCON_RIG_DEBUG_INFO").as_deref() == Ok("1");
    if debug_profile || rig_opt_in {
        println!("cargo:rustc-env=SLINT_EMIT_DEBUG_INFO=1");
    }
}

/// v0.8.103 (T3): `SLINT_SCALE_FACTOR` is read by `i-slint-compiler` at BUILD time and BAKES a
/// constant device-pixel scale into the generated UI. It is a debugging aid, and a stray one in the
/// build environment would silently change the geometry of every shipped pixel — the title-bar
/// contract, the device-pixel snapping the confirm dialogs depend on, the tooltip centring the rigs
/// measure — with no trace in the source tree and nothing in the log to explain it. A build is the
/// wrong place to be quietly reinterpreted, so fail LOUDLY and make the override explicit.
///
/// v0.8.104 (rider Y5): the guard MIRRORS THE COMPILER'S OWN ACCEPTANCE. i-slint-compiler parses the
/// variable as a float and uses it only when that succeeds and the value is positive; anything else
/// it ignores entirely. Panicking on `SLINT_SCALE_FACTOR=` (empty), `=auto`, or `=0` therefore broke
/// builds the compiler would have left completely alone — a guard failing on inputs that could not
/// have changed a pixel. It now fires on exactly the set that CAN.
fn guard_slint_scale_factor() {
    println!("cargo:rerun-if-env-changed=SLINT_SCALE_FACTOR");
    println!("cargo:rerun-if-env-changed=FALCON_ALLOW_SLINT_SCALE");
    let Ok(sf) = std::env::var("SLINT_SCALE_FACTOR") else { return };
    // i-slint-compiler 1.17 lib.rs:243-246, verbatim:
    //   std::env::var("SLINT_SCALE_FACTOR").ok().and_then(|x| x.parse::<f32>().ok()).filter(|f| *f > 0.)
    // Anything that does not survive that chain bakes NOTHING, so there is nothing to guard.
    // (No `is_finite` term on purpose — `inf` parses and passes `> 0.`, so the compiler would take
    // it, and a guard must not be narrower than the thing it guards.)
    if !sf.parse::<f32>().is_ok_and(|f| f > 0.0) {
        return;
    }
    if std::env::var("FALCON_ALLOW_SLINT_SCALE").as_deref() == Ok("1") {
        println!("cargo:warning=SLINT_SCALE_FACTOR={sf} is BAKED INTO THIS BUILD (allowed by FALCON_ALLOW_SLINT_SCALE=1) — the resulting binary's device-pixel geometry is NOT what a normal build produces. Do not ship it.");
        return;
    }
    panic!(
        "SLINT_SCALE_FACTOR={sf} is set in the build environment.\n\
         i-slint-compiler reads this at BUILD time and bakes a constant scale factor into the \
         generated UI, so it would silently change the device-pixel geometry of the shipped \
         binary — every snapped border, the title-bar layout contract, and the tooltip centring \
         the headless rigs measure.\n\
         Falcon takes its scale factor from the window at RUNTIME (`win_sf`); nothing in this \
         project needs the build-time constant.\n\
         FIX: unset SLINT_SCALE_FACTOR before building. If you genuinely intend a scale-pinned \
         experimental build, set FALCON_ALLOW_SLINT_SCALE=1 alongside it and this becomes a \
         warning — never for a milestone or a shipped exe."
    );
}
