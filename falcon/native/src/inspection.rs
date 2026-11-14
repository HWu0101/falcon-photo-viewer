//! Current-photo inspection is independent of background collection discovery.
//! Kept separate so real pointer events can exercise the production handlers in tests.
//! Every callback here refuses only while `inspection-blocked` holds (a dialog or the welcome
//! guide). The unfinished-boundary wait `modal-open` adds does not apply, and a menu owns the pointer through its own
//! overlay, so the Fit key and the Mac menu rows keep working while a menu is open, as before.
use crate::*;

#[allow(clippy::too_many_arguments)]
pub(crate) fn wire(
    app: &MainWindow,
    pan_at_press: &Rc<RefCell<(f32, f32)>>,
    last_input: &Rc<Cell<Instant>>,
    shown_dims: &Rc<RefCell<(u32, u32)>>,
    roi: &Rc<RoiZoom>,
    meta: &Rc<crate::meta::PerShotMeta>,
    dims_req_tx: &std::sync::mpsc::Sender<(usize, u64)>,
    generation: &Arc<AtomicU64>,
    current: &Rc<RefCell<usize>>,
    rot: &Rc<RotState>,
) {
    // ── pan / zoom ──
    // G1: single-view zoom/pan/1:1 are inert while `photo-ready` is false — the stage is showing
    // a thumb stand-in, a placeholder, or (scrim-dimmed) stale pixels, none of which can be
    // meaningfully focus-checked; interacting with them was the "still-interactable stale shot"
    // bug. Compare is exempt (these callbacks are only wired from single-view markup anyway).
    let awz = app.as_weak();
    let pap = pan_at_press.clone();
    let photo_gate = app.as_weak();
    app.on_pan_start(move || {
        if photo_gate.upgrade().is_some_and(|a| a.get_inspection_blocked()) { return; }
        if let Some(a) = awz.upgrade() {
            if !a.get_compare() && !a.get_photo_ready() {
                return;
            }
            *pap.borrow_mut() = (a.get_pan_x(), a.get_pan_y());
        }
    });
    let awz = app.as_weak();
    let pap = pan_at_press.clone();
    // v0.8.176 (W2): the stage DRAG — the owner's second report ("click→zoom→drag, frames drop for the
    // first ~1-5 s"). It is the single highest-rate input surface in the app and it stamped no clock
    // at all, so the publish pace could not see the one gesture whose whole value is smoothness.
    let li_pan = last_input.clone();
    let photo_gate = app.as_weak();
    app.on_pan_move(move |dx, dy| {
        if photo_gate.upgrade().is_some_and(|a| a.get_inspection_blocked()) { return; }
        if let Some(a) = awz.upgrade() {
            if !a.get_compare() && !a.get_photo_ready() {
                return;
            }
            li_pan.set(Instant::now());
            let (sw, sh) = (a.get_stage_w(), a.get_stage_h());
            let z = a.get_zoom();
            let (w, h) = (a.get_base_w() * z, a.get_base_h() * z);
            let (sx, sy) = *pap.borrow();
            let (mut px, mut py) = (sx + dx, sy + dy);
            clamp_pan(&mut px, &mut py, w, h, sw, sh);
            a.set_pan_x(px);
            a.set_pan_y(py);
        }
    });
    // v0.8.182 (2026-08-13 owner report): THE COMPARE DRAG, which stamped nothing at all — "in A/B
    // compare mode and zoomed in (on both images), the dragging experience is not smooth". Unlike
    // the stage drag above, the compare pan never crosses into Rust: the markup writes
    // `cmp-pan-fx/fy` straight into both halves' transforms and the only Rust-side writes are the
    // programmatic resets. So `last_input` stayed stale for the whole gesture, the publish arm never
    // answered `Park`, the fast hold never engaged, and the speculative tiers ran at FULL rate
    // exactly while the user dragged. (The owner's log carries it as an absence: not one "PARKED by
    // a STAGE/PANEL GESTURE" line in the entire compare section.) Same clock and same one-line shape
    // as `on_pan_move`; `last_motion` is deliberately NOT stamped — a pan is input, not browse, and
    // stamping the browse clock would hold back the sharp frame of the photograph being dragged.
    let li_cmp = last_input.clone();
    app.on_cmp_pan_input(move || li_cmp.set(Instant::now()));
    let awz = app.as_weak();
    let li_zoom = last_input.clone();
    let photo_gate = app.as_weak();
    app.on_zoom_at(move |cx, cy, delta| {
        if photo_gate.upgrade().is_some_and(|a| a.get_inspection_blocked()) { return; }
        // v0.8.127: a ZERO delta is not a zoom-out. `step` below is a two-arm ternary, so the tilt
        // events that now reach here with `wheel-zoom-delta` = 0 would otherwise shrink the
        // photograph — which is precisely the bug the mapping exists to remove.
        if delta == 0.0 {
            return;
        }
        if let Some(a) = awz.upgrade() {
            if !a.get_compare() && !a.get_photo_ready() {
                return; // G1: no zoom over a stand-in/stale frame
            }
            let (sw, sh) = (a.get_stage_w(), a.get_stage_h());
            let (bw, bh) = (a.get_base_w(), a.get_base_h());
            let z0 = a.get_zoom();
            // Wheel up (positive delta-y here) zooms in. `None` = the clamp swallowed the notch.
            let Some(z1) = support::zoom_notch_target(z0, delta, MAX_ZOOM) else {
                return;
            };
            // v0.8.176 (W2): the zoom half of the owner's "click→zoom→drag". A zoom notch reaches
            // here rather than the wheel-nav path, so it stamped nothing either.
            //
            // v0.8.179 (W4 wave-2 tail, skeptic A Y-2): …and it stamps BELOW the clamp now. Above
            // it, a notch at zoom == 1.0 or MAX_ZOOM — which does nothing the user can see — armed
            // the whole interaction posture anyway: the fast tier held, the detail tier parked and
            // the drain took one frame a tick, all for a gesture the app had just discarded. A user
            // at the ends of the range spins the wheel exactly like a user in the middle of it.
            li_zoom.set(Instant::now());
            let (w0, h0) = (bw * z0, bh * z0);
            let (w1, h1) = (bw * z1, bh * z1);
            let x0 = (sw - w0) / 2.0 + a.get_pan_x();
            let y0 = (sh - h0) / 2.0 + a.get_pan_y();
            let fx = if w0 > 0.0 { (cx - x0) / w0 } else { 0.5 };
            let fy = if h0 > 0.0 { (cy - y0) / h0 } else { 0.5 };
            let (mut px, mut py) = (cx - fx * w1 - (sw - w1) / 2.0, cy - fy * h1 - (sh - h1) / 2.0);
            if z1 <= 1.0 {
                px = 0.0;
                py = 0.0;
            } else {
                clamp_pan(&mut px, &mut py, w1, h1, sw, sh);
            }
            a.set_zoom(z1);
            a.set_pan_x(px);
            a.set_pan_y(py);
        }
    });
    let awz = app.as_weak();
    let photo_gate = app.as_weak();
    app.on_pan_reset(move || {
        if photo_gate.upgrade().is_some_and(|a| a.get_inspection_blocked()) { return; }
        if let Some(a) = awz.upgrade() {
            a.set_zoom(1.0);
            a.set_pan_x(0.0);
            a.set_pan_y(0.0);
            a.set_cmp_pan_fx(0.0);
            a.set_cmp_pan_fy(0.0);
        }
    });
    // Single click on the photo: toggle 1:1 (one native image pixel ≈ one physical screen
    // pixel) at the click point; click again (any zoom) returns to fit. The 1:1 zoom factor
    // = native_px / (fit_logical_width × scale_factor); the shown-texture dims give the
    // native size (full-res when settled — which it is whenever you can click).
    let awz = app.as_weak();
    let sd_click = shown_dims.clone();
    let roi_click = roi.clone(); // v0.8.31 (§64): roi_src source-dim memo via RoiZoom
    let meta_click = meta.clone(); // v0.9.4 (§66/P4a): dims_requested via PerShotMeta
    let dims_req_click = dims_req_tx.clone();
    let generation_click = generation.clone();
    let li_click = last_input.clone();
    let cur_click = current.clone();
    let rot_click = rot.clone(); // v0.8.0: roi_src is UNORIENTED — swap w/h for odd effective turns
    let trace = std::env::var_os("FALCON_TRACE_INSPECTION").is_some();
    let photo_gate = app.as_weak();
    app.on_photo_clicked(move |cx, cy| {
        if photo_gate.upgrade().is_some_and(|a| a.get_inspection_blocked()) { return; }
        let Some(a) = awz.upgrade() else { return };
        if !a.get_compare() && !a.get_photo_ready() {
            return; // G1: 1:1 over a stand-in/stale frame is meaningless
        }
        li_click.set(Instant::now());
        if (a.get_zoom() - 1.0).abs() > 0.01 {
            a.set_zoom(1.0);
            a.set_pan_x(0.0);
            a.set_pan_y(0.0);
            return;
        }
        // 1:1 = one SOURCE pixel ≈ one screen pixel. Use the source long-side (cached header probe),
        // NOT the displayed texture (ROI/detail_cap caps that below source) — else "1:1" on a
        // >detail_cap image only zooms partway. The ROI overlay then fills the zoomed region sharply.
        // G7: on a cache miss there is NO synchronous header read here any more (it blocked the UI
        // thread — a hitch on every cold 1:1 click, worse on slow/locked I/O). Fire the async dims
        // worker (deduped; settle usually pre-filled it anyway) and zoom NOW from the shown
        // texture's width: exact for sources ≤ detail_cap, a slight undershoot beyond — corrected
        // as soon as the dims land (the ROI tile then sharpens to true 1:1 regardless).
        let nw = {
            let ci = *cur_click.borrow();
            // v0.8.0: `roi_src` holds the file's UNORIENTED header dims, but `base_w`/zoom are
            // display-space (oriented) — for an odd-turns shot the displayed WIDTH maps to the
            // source HEIGHT, so pick the ORIENTED source width (mirrors `step_zoom_pct`). The
            // `shown_dims` fallback is already oriented (the presented texture's size).
            let eff = rot_click.effective(ci);
            let cached = roi_click
                .src
                .borrow()
                .get(&ci)
                .map(|&(w, h)| if eff & 1 == 1 { h } else { w })
                .filter(|&w| w > 0);
            if cached.is_none() && meta_click.dims_requested.borrow_mut().insert(ci) {
                let _ = dims_req_click.send((ci, generation_click.load(Ordering::Relaxed)));
            }
            cached.unwrap_or_else(|| sd_click.borrow().0)
        };
        let bw = a.get_base_w();
        let sf = a.window().scale_factor();
        if bw <= 0.0 || nw == 0 || sf <= 0.0 {
            return;
        }
        let z1 = (nw as f32 / (bw * sf)).clamp(1.0, MAX_ZOOM);
        if (z1 - 1.0).abs() < 0.01 {
            return; // image already fits at (near) native res — nothing to zoom into
        }
        let (sw, sh) = (a.get_stage_w(), a.get_stage_h());
        let bh = a.get_base_h();
        // pan is 0 at fit (z0 == 1), so the photo is centred; keep the clicked point fixed.
        let fx = if bw > 0.0 { (cx - (sw - bw) / 2.0) / bw } else { 0.5 };
        let fy = if bh > 0.0 { (cy - (sh - bh) / 2.0) / bh } else { 0.5 };
        let (w1, h1) = (bw * z1, bh * z1);
        let (mut px, mut py) = (cx - fx * w1 - (sw - w1) / 2.0, cy - fy * h1 - (sh - h1) / 2.0);
        clamp_pan(&mut px, &mut py, w1, h1, sw, sh);
        a.set_zoom(z1);
        a.set_pan_x(px);
        a.set_pan_y(py);
        if trace { log_event(&format!("inspection: click accepted opening={} ready={} zoom={z1:.3}",
            a.get_opening_photo(), a.get_photo_ready())); }
    });

}
