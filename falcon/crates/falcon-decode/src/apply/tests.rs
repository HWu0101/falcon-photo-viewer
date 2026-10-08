//! Stage-2 apply-pipeline test battery (spec items T-a … T-l, minus T-k which needs the real corpus
//! and lives in `tests/real.rs`). Fixtures are synthetic in-memory buffers + unique temp-dir files —
//! never the repo or a user file. Every patch test also asserts the rest-of-file bytes are untouched.

use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

// ───────────────────────────── fixtures ─────────────────────────────

static TMP_CTR: AtomicU64 = AtomicU64::new(0);

/// A unique, freshly-created temp dir (no tempfile crate in the tree — mirror the native-side style).
fn tmp_dir() -> PathBuf {
    let n = TMP_CTR.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("falcon_apply_test_{pid}_{nanos}_{n}"));
    std::fs::create_dir_all(&d).expect("create temp dir");
    d
}

fn u16b(en: Endian, v: u16) -> [u8; 2] {
    if en == Endian::Little {
        v.to_le_bytes()
    } else {
        v.to_be_bytes()
    }
}
fn u32b(en: Endian, v: u32) -> [u8; 4] {
    if en == Endian::Little {
        v.to_le_bytes()
    } else {
        v.to_be_bytes()
    }
}
/// A 4-byte inline value field holding SHORT `v` (first 2 bytes = value in `en`, rest padding).
fn short_val(en: Endian, v: u16) -> [u8; 4] {
    let s = u16b(en, v);
    [s[0], s[1], 0, 0]
}

/// Build a minimal but STRUCTURALLY-EXACT JPEG (SOI + APP1/Exif/TIFF/IFD0 + a trailer) from a list of
/// IFD0 entries `(tag, type, count, value_field)`. Returns the bytes + each entry tag's ABSOLUTE
/// value-field offset (so a test can read/verify the 2 patched bytes). Mirrors the real HWU_0141.JPG
/// layout: TIFF header at 0x0C, IFD0 at 0x14, first entry value at 0x1E.
fn build_jpeg(en: Endian, entries: &[(u16, u16, u32, [u8; 4])], trailer: &[u8]) -> (Vec<u8>, Vec<(u16, usize)>) {
    let mut tiff = Vec::new();
    tiff.extend_from_slice(if en == Endian::Little { b"II" } else { b"MM" });
    tiff.extend_from_slice(&u16b(en, 42)); // magic
    tiff.extend_from_slice(&u32b(en, 8)); // IFD0 at TIFF-relative offset 8
    tiff.extend_from_slice(&u16b(en, entries.len() as u16)); // IFD0 count
    let entries_start_in_tiff = tiff.len(); // 10
    for (tag, typ, cnt, val) in entries {
        tiff.extend_from_slice(&u16b(en, *tag));
        tiff.extend_from_slice(&u16b(en, *typ));
        tiff.extend_from_slice(&u32b(en, *cnt));
        tiff.extend_from_slice(val);
    }
    tiff.extend_from_slice(&u32b(en, 0)); // next-IFD = none

    let mut payload = Vec::new();
    payload.extend_from_slice(b"Exif\x00\x00");
    let tiff_start_in_payload = payload.len(); // 6
    payload.extend_from_slice(&tiff);

    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
    jpeg.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes()); // APP1 length (big-endian)
    let payload_start_in_jpeg = jpeg.len(); // 6
    jpeg.extend_from_slice(&payload);
    jpeg.extend_from_slice(trailer);

    let tiff_start = payload_start_in_jpeg + tiff_start_in_payload; // 12
    let entries_start = tiff_start + entries_start_in_tiff; // 22
    let offs = entries
        .iter()
        .enumerate()
        .map(|(i, (tag, _, _, _))| (*tag, entries_start + i * 12 + 8))
        .collect();
    (jpeg, offs)
}

/// A standard portrait fixture: Orientation=`o` among ImageWidth/Orientation/ImageLength (exercises the
/// multi-entry walk, T-b), plus a filler trailer so "rest of file" is non-trivial (T-g).
fn portrait_jpeg(en: Endian, o: u16) -> (Vec<u8>, usize) {
    let entries = [
        (0x0100u16, 4u16, 1u32, u32b(en, 8192)), // ImageWidth  (LONG)
        (0x0112, 3, 1, short_val(en, o)),         // Orientation (SHORT) — the middle entry
        (0x0101, 4, 1, u32b(en, 5464)),           // ImageLength (LONG)
    ];
    let trailer: Vec<u8> = (0..256u32).map(|i| (i * 7 % 251) as u8).chain([0xFF, 0xD9]).collect();
    let (bytes, offs) = build_jpeg(en, &entries, &trailer);
    let voff = offs.iter().find(|(t, _)| *t == 0x0112).unwrap().1;
    (bytes, voff)
}

fn write_tmp(dir: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

/// Assert two same-length byte vecs differ ONLY within `[off, off+2)` — the core in-place invariant.
fn differs_only_at(before: &[u8], after: &[u8], off: usize) {
    assert_eq!(before.len(), after.len(), "file length must not change (in-place 2-byte patch)");
    for i in 0..before.len() {
        if (off..off + 2).contains(&i) {
            continue;
        }
        assert_eq!(before[i], after[i], "byte {i} changed outside the 2-byte orientation value");
    }
}

// ───────────────────────────── T-j: the compose algebra (32 cells + round-trips) ─────────────────────────────

#[test]
fn t_j_compose_exif_orientation_all_cells_and_roundtrips() {
    // turns_of must match the public display-side mapping bit-for-bit.
    for o in 1..=8u8 {
        assert_eq!(turns_of(o), crate::orientation_to_turns(o as u32), "turns_of parity for {o}");
    }
    for o in 1..=8u8 {
        let mirrored = is_mirrored(o);
        // δ=0 is the identity for every valid orientation.
        assert_eq!(compose_exif_orientation(o, 0), o, "δ=0 identity for {o}");
        for d in 0..4u8 {
            let c = compose_exif_orientation(o, d);
            // (1) mirror CLASS preserved.
            assert_eq!(is_mirrored(c), mirrored, "mirror class preserved: compose({o},{d})={c}");
            assert!((1..=8).contains(&c), "compose stays a valid EXIF value");
            // (2) rotation composes in TURNS — this is the "round-trip vs display compose" property:
            //     what the user saw = turns(o)+d, and re-reading the applied file must equal it.
            assert_eq!(
                turns_of(c),
                (turns_of(o) + d) & 3,
                "turns compose: compose({o},{d})={c}"
            );
            // (3) full round-trip: apply δ then (4-δ) returns to the same value.
            assert_eq!(compose_exif_orientation(c, (4 - d) & 3), o, "round-trip compose({o},{d})");
        }
    }
    // The documented cycles, explicitly.
    assert_eq!([compose_exif_orientation(1, 1), compose_exif_orientation(6, 1), compose_exif_orientation(3, 1), compose_exif_orientation(8, 1)], [6, 3, 8, 1]);
    assert_eq!([compose_exif_orientation(2, 1), compose_exif_orientation(5, 1), compose_exif_orientation(4, 1), compose_exif_orientation(7, 1)], [5, 4, 7, 2]);
    // An out-of-range current is treated as upright, non-mirrored.
    assert_eq!(compose_exif_orientation(0, 1), 6);
    assert_eq!(compose_exif_orientation(9, 2), 3);
    // turns_to_orientation round-trips the non-mirrored reps.
    for t in 0..4u8 {
        assert_eq!(turns_of(turns_to_orientation(t)), t);
    }
}

// ───────────────────────────── T-a: LE + BE both patch correctly ─────────────────────────────

#[test]
fn t_a_little_and_big_endian_patch() {
    for en in [Endian::Little, Endian::Big] {
        let dir = tmp_dir();
        let (bytes, voff) = portrait_jpeg(en, 6);
        let p = write_tmp(&dir, "shot.jpg", &bytes);
        // 6 (90° CW) + 1 turn = 3 (180°).
        let target = compose_exif_orientation(6, 1);
        assert_eq!(target, 3);
        assert_eq!(patch_jpeg_orientation(&p, 6, target).unwrap(), JpegPatch::Patched);
        let after = std::fs::read(&p).unwrap();
        // exactly the 2 value bytes changed; they now encode 3 in this endianness.
        differs_only_at(&bytes, &after, voff);
        assert_eq!(&after[voff..voff + 2], &u16b(en, 3));
        // and the NORMAL bounded reader agrees.
        assert_eq!(exif_orientation(&p), Some(3));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ───────────────────────────── T-b: walk finds Orientation among many; offset exact ─────────────────────────────

#[test]
fn t_b_ifd_walk_offset_math_exact() {
    let en = Endian::Little;
    let (bytes, voff) = portrait_jpeg(en, 8); // Orientation is the MIDDLE of 3 entries
    let loc = locate_jpeg_orientation(&bytes).expect("locate");
    assert_eq!(loc.value_offset as usize, voff, "value offset lands on the middle entry, not entry 0");
    assert_eq!(loc.endian, Endian::Little);
    // the 2 bytes at the located offset decode to the current orientation (8).
    assert_eq!(loc.endian.u16(&bytes[voff..voff + 2]), 8);
    // sanity: the located offset is entries_start(22) + 1*12 + 8 = 42 = 0x2A
    assert_eq!(voff, 42);
}

// ───────────────────────────── T-c: inline SHORT ok; offset-style/odd entry → refuse ─────────────────────────────

#[test]
fn t_c_short_inline_ok_offsetstyle_refused() {
    let en = Endian::Little;
    // inline SHORT is handled (covered by T-a); here an Orientation entry typed LONG (4) with count 1 —
    // a value class we never expect for Orientation — must REFUSE, not misread the 4-byte field.
    let (bytes, _) = build_jpeg(
        en,
        &[(0x0112, 4, 1, u32b(en, 6))], // wrong TYPE
        &[0xFF, 0xD9],
    );
    assert_eq!(locate_jpeg_orientation(&bytes), Err(LocateErr::MalformedOrientation));
    // count != 1 (an offset-style array) is likewise refused.
    let (bytes2, _) = build_jpeg(en, &[(0x0112, 3, 2, [4, 0, 0, 0])], &[0xFF, 0xD9]);
    assert_eq!(locate_jpeg_orientation(&bytes2), Err(LocateErr::MalformedOrientation));
}

// ───────────────────────────── T-d: no EXIF → sidecar route; bad magic → refuse, zero bytes ─────────────────────────────

#[test]
fn t_d_no_exif_and_bad_magic() {
    let dir = tmp_dir();
    // (a) a structurally-valid JPEG with a JFIF APP0 but NO APP1/EXIF → the PATCHER errs (NoExifApp1)
    //     and writes nothing…
    let plain: Vec<u8> = vec![
        0xFF, 0xD8, // SOI
        0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, // APP0/JFIF
        0xFF, 0xD9, // EOI
    ];
    let p = write_tmp(&dir, "noexif.jpg", &plain);
    let before = std::fs::read(&p).unwrap();
    assert!(matches!(patch_jpeg_orientation(&p, 1, 6), Err(PatchErr::Locate(LocateErr::NoExifApp1))));
    assert_eq!(std::fs::read(&p).unwrap(), before, "no bytes written on a no-EXIF refuse");
    // …and the ORCHESTRATOR routes that same file to a sidecar (never a forced patch).
    let report = apply_rotation(&RotApplyPlan {
        finished: Some(p.clone()),
        finished_is_jpeg: true,
        raw: None,
        base_turns: 0,
        delta: 1,
    });
    assert!(report.ok);
    assert_eq!(report.finished_action, SideAction::SidecarCreated);
    assert!(sidecar_fullname(&p).exists(), "a JPG-side sidecar was created for the tagless JPEG");

    // (b) corrupt TIFF magic → refuse (Err), zero bytes written.
    let en = Endian::Little;
    let mut bad = build_jpeg(en, &[(0x0112, 3, 1, short_val(en, 6))], &[0xFF, 0xD9]).0;
    // TIFF magic lives at file offset 0x0C+2 = 0x0E; corrupt it (42 → 43).
    bad[0x0E] = 43;
    let pb = write_tmp(&dir, "badmagic.jpg", &bad);
    let before_b = std::fs::read(&pb).unwrap();
    assert!(matches!(patch_jpeg_orientation(&pb, 6, 3), Err(PatchErr::Locate(LocateErr::BadTiff))));
    assert_eq!(std::fs::read(&pb).unwrap(), before_b, "no bytes written on a bad-magic refuse");
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── T-e: wrong/missing tag → refuse, never patch a neighbour ─────────────────────────────

#[test]
fn t_e_missing_orientation_never_touches_neighbour() {
    let dir = tmp_dir();
    let en = Endian::Little;
    // IFD0 with ImageWidth + ImageLength but NO Orientation.
    let (bytes, _) = build_jpeg(
        en,
        &[(0x0100, 4, 1, u32b(en, 8192)), (0x0101, 4, 1, u32b(en, 5464))],
        &[0xFF, 0xD9],
    );
    assert_eq!(locate_jpeg_orientation(&bytes), Err(LocateErr::NoOrientation));
    let p = write_tmp(&dir, "no_orient.jpg", &bytes);
    let before = std::fs::read(&p).unwrap();
    assert!(matches!(patch_jpeg_orientation(&p, 1, 6), Err(PatchErr::Locate(LocateErr::NoOrientation))));
    assert_eq!(std::fs::read(&p).unwrap(), before, "a missing-tag refuse must not patch a neighbouring field");
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── T-f: truncated → Err, no OOB, no partial write ─────────────────────────────

#[test]
fn t_f_truncated_no_oob_no_write() {
    let dir = tmp_dir();
    let en = Endian::Little;
    let (full, voff) = portrait_jpeg(en, 6);
    // Truncate BEFORE the orientation value offset — the IFD walk runs off the end.
    for cut in [voff - 1, voff / 2, 14, 10] {
        let trunc = &full[..cut.min(full.len())];
        let r = locate_jpeg_orientation(trunc);
        assert!(r.is_err(), "truncation at {cut} must Err, got {r:?}");
        // and a truncated FILE patches nothing.
        let p = write_tmp(&dir, "trunc.jpg", trunc);
        let before = std::fs::read(&p).unwrap();
        assert!(patch_jpeg_orientation(&p, 6, 3).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), before, "no partial write on a truncated file");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── T-g: rest-of-file bytes unchanged after a patch ─────────────────────────────

#[test]
fn t_g_rest_of_file_unchanged() {
    let dir = tmp_dir();
    let en = Endian::Big; // exercise BE here too
    let (bytes, voff) = portrait_jpeg(en, 6);
    let p = write_tmp(&dir, "hashme.jpg", &bytes);
    let before = std::fs::read(&p).unwrap();
    patch_jpeg_orientation(&p, 6, compose_exif_orientation(6, 1)).unwrap();
    let after = std::fs::read(&p).unwrap();
    differs_only_at(&before, &after, voff);
    // fold-hash of everything EXCEPT the 2 value bytes is identical before/after (the T-g invariant).
    let fold = |b: &[u8]| -> u64 {
        b.iter().enumerate().filter(|(i, _)| !(voff..voff + 2).contains(i)).fold(1469598103934665603u64, |h, (_, &x)| (h ^ x as u64).wrapping_mul(1099511628211))
    };
    assert_eq!(fold(&before), fold(&after), "hash of all other bytes must be unchanged");
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── T-h: XMP create-fresh / surgical-update / bounded read ─────────────────────────────

#[test]
fn t_h_xmp_create_surgical_and_bounded() {
    let dir = tmp_dir();
    // create-fresh matches the probe xpacket shape.
    let side = dir.join("HWU_0141.xmp");
    assert_eq!(write_xmp_sidecar(&side, 6).unwrap(), SidecarKind::CreatedFresh);
    let txt = std::fs::read_to_string(&side).unwrap();
    assert!(txt.starts_with('\u{feff}'), "leading UTF-8 BOM");
    assert!(txt.contains("<?xpacket begin=") && txt.contains("<?xpacket end=\"w\"?>"), "xpacket wrapper");
    assert!(txt.contains("x:xmpmeta") && txt.contains("rdf:RDF") && txt.contains("http://ns.adobe.com/tiff/1.0/"));
    assert!(txt.contains("tiff:Orientation=\"6\""), "carries the target orientation");
    // re-applying the SAME target is a no-op.
    assert_eq!(write_xmp_sidecar(&side, 6).unwrap(), SidecarKind::AlreadyTarget);
    // a surgical update changes ONLY the value; everything else byte-identical.
    let before = std::fs::read_to_string(&side).unwrap();
    assert_eq!(write_xmp_sidecar(&side, 3).unwrap(), SidecarKind::Updated);
    let after = std::fs::read_to_string(&side).unwrap();
    assert_eq!(before.replace("tiff:Orientation=\"6\"", "tiff:Orientation=\"3\""), after, "only the value digit changed");

    // surgical update PRESERVES a foreign namespace/property byte-for-byte outside the replaced value.
    let foreign = "\u{feff}<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  <rdf:Description rdf:about=\"\" xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" tiff:Orientation=\"1\" dc:creator=\"Someone &amp; Co\">\n   <dc:subject><rdf:Bag><rdf:li>keep me</rdf:li></rdf:Bag></dc:subject>\n  </rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>";
    let fpath = dir.join("foreign.CR3.xmp");
    std::fs::write(&fpath, foreign).unwrap();
    assert_eq!(write_xmp_sidecar(&fpath, 6).unwrap(), SidecarKind::Updated);
    let out = std::fs::read_to_string(&fpath).unwrap();
    assert!(out.contains("dc:creator=\"Someone &amp; Co\""), "foreign attribute preserved (entity untouched)");
    assert!(out.contains("<rdf:li>keep me</rdf:li>"), "foreign element preserved");
    assert_eq!(out, foreign.replace("tiff:Orientation=\"1\"", "tiff:Orientation=\"6\""), "ONLY the tiff value changed");

    // INSERT when tiff:Orientation is absent (namespace also injected).
    let noorient = "<?xpacket begin=\"\"?><x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\"></rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>";
    let ipath = dir.join("insert.CR3.xmp");
    std::fs::write(&ipath, noorient).unwrap();
    assert_eq!(write_xmp_sidecar(&ipath, 8).unwrap(), SidecarKind::Updated);
    let ins = std::fs::read_to_string(&ipath).unwrap();
    assert!(ins.contains("xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\"") && ins.contains("tiff:Orientation=\"8\""));

    // an UNRECOGNISED existing sidecar (no rdf:Description) is refused, NOT clobbered.
    let upath = dir.join("weird.CR3.xmp");
    std::fs::write(&upath, "not xmp at all").unwrap();
    assert!(matches!(write_xmp_sidecar(&upath, 6), Err(SidecarErr::Unrecognised)));
    assert_eq!(std::fs::read_to_string(&upath).unwrap(), "not xmp at all", "foreign file untouched");

    // bounded read refuses an oversized sidecar (> 4 MB).
    let big = dir.join("huge.CR3.xmp");
    std::fs::write(&big, vec![b'x'; (XMP_READ_CAP + 16) as usize]).unwrap();
    assert!(matches!(write_xmp_sidecar(&big, 6), Err(SidecarErr::TooLarge)));
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────── 2026-09-09 F1: rotation sidecars preserve foreign XML and report safe refusal ─────────

#[test]
fn t_f1_orientation_unrecognised_forms_refuse_without_changing_file() {
    let dir = tmp_dir();
    let path = dir.join("foreign.png.xmp");
    for property in [
        "tiff:Orientation = \"1\"",
        "tiff:Orientation\t=\n'1'",
        "tiff:Orientation=1",
    ] {
        let original = format!(
            "<rdf:Description xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" {property}/>"
        );
        std::fs::write(&path, &original).unwrap();
        assert!(matches!(write_xmp_sidecar(&path, 6), Err(SidecarErr::Unrecognised)), "unsupported property must refuse: {property}");
        assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes(), "refusal preserves every foreign byte");
    }
    let element = "<rdf:Description xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\"><tiff:Orientation xml:lang=\"en\">1</tiff:Orientation></rdf:Description>";
    std::fs::write(&path, element).unwrap();
    assert!(matches!(write_xmp_sidecar(&path, 6), Err(SidecarErr::Unrecognised)));
    assert_eq!(std::fs::read(&path).unwrap(), element.as_bytes());
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "refusal creates no temp sidecar");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t_f1_orientation_namespace_is_local_and_foreign_bytes_survive() {
    let dir = tmp_dir();
    let path = dir.join("namespace.png.xmp");
    let original = "<?xpacket begin=\"\"?><x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" dc:creator=\"Someone &amp; Co\"/><rdf:Description rdf:about=\"\" xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" tiff:Make=\"Camera\"/></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>";
    std::fs::write(&path, original).unwrap();
    assert_eq!(write_xmp_sidecar(&path, 6).unwrap(), SidecarKind::Updated);
    let expected = original.replacen("<rdf:Description", "<rdf:Description xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" tiff:Orientation=\"6\"", 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected, "the edited Description gets its own binding; sibling and foreign content are untouched");
    assert_eq!(write_xmp_sidecar(&path, 6).unwrap(), SidecarKind::AlreadyTarget);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected, "repeat write is inert");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t_f1_orientation_namespace_whitespace_and_quoted_delimiter() {
    let dir = tmp_dir();
    let path = dir.join("local.png.xmp");
    // XML allows whitespace around '=' and a literal '>' within a quoted attribute. Neither may
    // make a local namespace declaration invisible and cause a duplicate xmlns:tiff attribute.
    for declaration in [
        "xmlns:tiff = \"http://ns.adobe.com/tiff/1.0/\"",
        "xmlns:tiff= 'http://ns.adobe.com/tiff/1.0/'",
        "xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\"",
    ] {
        let original = format!("<rdf:Description xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" rdf:about=\"a > b\" {declaration}/>");
        std::fs::write(&path, &original).unwrap();
        assert_eq!(write_xmp_sidecar(&path, 8).unwrap(), SidecarKind::Updated);
        let expected = original.replacen("<rdf:Description", "<rdf:Description tiff:Orientation=\"8\"", 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected, "reuse the valid local namespace exactly as written");
    }
    // Similar XML names do not mean this property or namespace already exists.
    let different = "<rdf:Description xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:tiffExtra=\"urn:foreign\" tiffExtra:Orientation=\"1\"/>";
    std::fs::write(&path, different).unwrap();
    assert_eq!(write_xmp_sidecar(&path, 6).unwrap(), SidecarKind::Updated);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), different.replacen("<rdf:Description", "<rdf:Description xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" tiff:Orientation=\"6\"", 1));
    let longer_property = "<rdf:Description xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" tiff:OrientationExtra=\"foreign\"/>";
    std::fs::write(&path, longer_property).unwrap();
    assert_eq!(write_xmp_sidecar(&path, 6).unwrap(), SidecarKind::Updated);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), longer_property.replacen("<rdf:Description", "<rdf:Description tiff:Orientation=\"6\"", 1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t_f1_orientation_ambiguous_namespace_or_broken_tag_refuses() {
    let dir = tmp_dir();
    let path = dir.join("refuse.png.xmp");
    for original in [
        "<rdf:Description xmlns:tiff=\"urn:foreign-schema\"/>",
        "<rdf:Description xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\"/>",
        "<rdf:Description rdf:about=\"unfinished\"",
        "<rdf:Description rdf:about=\"unterminated >",
        "<rdf:DescriptionExtra rdf:about=\"\"/>",
    ] {
        std::fs::write(&path, original).unwrap();
        assert!(matches!(write_xmp_sidecar(&path, 6), Err(SidecarErr::Unrecognised)), "ambiguous target must refuse: {original}");
        assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A local namespace insertion must not reinterpret inherited foreign names. The same conservative
/// refusal also covers an unrelated sibling's conflicting declaration when the target needs a binding.
#[test]
fn t_f1_orientation_inherited_foreign_namespace_refuses_without_rebinding() {
    let dir = tmp_dir();
    let photo = write_tmp(&dir, "foreign-prefix.png", b"synthetic tagless PNG stand-in");
    let sidecar = sidecar_fullname(&photo);
    let plan = RotApplyPlan { finished: Some(photo.clone()), finished_is_jpeg: false, raw: None, base_turns: 0, delta: 1 };
    for original in [
        r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:tiff="urn:foreign"><rdf:Description tiff:Other="keep"><tiff:Child>keep too</tiff:Child></rdf:Description></rdf:RDF>"#,
        "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:tiff \t=\n'urn:foreign'><rdf:Description tiff:Other=\"keep\"/></rdf:RDF>",
        r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about=""/><rdf:Description xmlns:tiff="urn:foreign" tiff:Other="keep"/></rdf:RDF>"#,
    ] {
        std::fs::write(&sidecar, original).unwrap();
        assert!(matches!(write_xmp_sidecar(&sidecar, 6), Err(SidecarErr::Unrecognised)), "must not rebind foreign namespace: {original}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), original.as_bytes());
        for _ in 0..2 {
            let report = apply_rotation(&plan);
            assert!(!report.ok, "the apply drain must retain the pending rotation on retry");
            assert!(matches!(report.finished_action, SideAction::Failed(_)));
            assert_eq!(report.raw_action, SideAction::Skipped);
            assert_eq!(std::fs::read(&sidecar).unwrap(), original.as_bytes());
            assert_eq!(std::fs::read(&photo).unwrap(), b"synthetic tagless PNG stand-in");
        }
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "refusal leaves no temporary output");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t_f1_orientation_compatible_inherited_or_local_binding_preserves_other_properties() {
    let dir = tmp_dir();
    let sidecar = dir.join("compatible.png.xmp");
    let inherited = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:tiff = 'http://ns.adobe.com/tiff/1.0/'><rdf:Description tiff:Other="keep"><tiff:Child>keep too</tiff:Child></rdf:Description></rdf:RDF>"#;
    std::fs::write(&sidecar, inherited).unwrap();
    assert_eq!(write_xmp_sidecar(&sidecar, 6).unwrap(), SidecarKind::Updated);
    let expected = inherited.replacen("<rdf:Description", "<rdf:Description xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" tiff:Orientation=\"6\"", 1);
    assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), expected, "same-URI redeclaration keeps every pre-existing name's meaning");

    // An explicit correct binding on the target already protects its names from the foreign
    // ancestor. Only the Orientation attribute is inserted; no namespace is added or changed.
    let local = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:tiff="urn:foreign"><rdf:Description xmlns:tiff="http://ns.adobe.com/tiff/1.0/" tiff:Other="keep"/><rdf:Description tiff:Other="foreign sibling"/></rdf:RDF>"#;
    std::fs::write(&sidecar, local).unwrap();
    assert_eq!(write_xmp_sidecar(&sidecar, 8).unwrap(), SidecarKind::Updated);
    assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), local.replacen("<rdf:Description", "<rdf:Description tiff:Orientation=\"8\"", 1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t_f1_orientation_refusal_reaches_apply_failure_contract() {
    let dir = tmp_dir();
    let photo = write_tmp(&dir, "pending.png", b"synthetic tagless PNG stand-in");
    let sidecar = sidecar_fullname(&photo);
    let original = "<rdf:Description xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" tiff:Orientation = \"1\"/>";
    std::fs::write(&sidecar, original).unwrap();
    let plan = RotApplyPlan { finished: Some(photo.clone()), finished_is_jpeg: false, raw: None, base_turns: 0, delta: 1 };
    // The native apply drain only clears its pending rotation on report.ok. Run twice to ensure
    // retrying an unchanged foreign sidecar still fails safely instead of quietly clearing the edit.
    for _ in 0..2 {
        let report = apply_rotation(&plan);
        assert!(!report.ok, "the caller must retain the pending rotation");
        assert!(matches!(report.finished_action, SideAction::Failed(_)));
        assert_eq!(report.raw_action, SideAction::Skipped);
        assert_eq!(std::fs::read(&sidecar).unwrap(), original.as_bytes());
        assert_eq!(std::fs::read(&photo).unwrap(), b"synthetic tagless PNG stand-in");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────── T-h2 (v0.8.69, E/H2): xmp:Rating create-fresh / surgical-update / 0-on-clear / composability ─────────

#[test]
fn t_h2_xmp_rating_create_surgical_idempotent_and_clear() {
    let dir = tmp_dir();
    // create-fresh carries the target rating in the standard xpacket shape.
    let side = dir.join("HWU_0141.CR3.xmp");
    assert_eq!(write_xmp_rating_sidecar(&side, 5).unwrap(), SidecarKind::CreatedFresh);
    let txt = std::fs::read_to_string(&side).unwrap();
    assert!(txt.starts_with('\u{feff}'), "leading UTF-8 BOM");
    assert!(txt.contains("<?xpacket begin=") && txt.contains("<?xpacket end=\"w\"?>"), "xpacket wrapper");
    assert!(txt.contains("http://ns.adobe.com/xap/1.0/"), "xmp basic-schema namespace");
    assert!(txt.contains("xmp:Rating=\"5\""), "carries the target rating");
    // the fresh sidecar parses back as valid XMP with the rating recoverable by the same locate forms.
    assert!(txt.contains("<rdf:Description"), "valid rdf:Description anchor");

    // idempotence: writing the SAME rating twice is a no-op → identical bytes.
    assert_eq!(write_xmp_rating_sidecar(&side, 5).unwrap(), SidecarKind::AlreadyTarget);
    assert_eq!(std::fs::read_to_string(&side).unwrap(), txt, "re-write of same rating changed nothing");

    // surgical update changes ONLY the digit; everything else byte-identical.
    let before = std::fs::read_to_string(&side).unwrap();
    assert_eq!(write_xmp_rating_sidecar(&side, 2).unwrap(), SidecarKind::Updated);
    let after = std::fs::read_to_string(&side).unwrap();
    assert_eq!(before.replace("xmp:Rating=\"5\"", "xmp:Rating=\"2\""), after, "only the rating digit changed");

    // the 0-on-clear rule: a cleared rating writes "0" (the standard "unrated"), NOT a deleted property.
    assert_eq!(write_xmp_rating_sidecar(&side, 0).unwrap(), SidecarKind::Updated);
    let cleared = std::fs::read_to_string(&side).unwrap();
    assert!(cleared.contains("xmp:Rating=\"0\""), "clear writes 0, not a removed property");
    assert!(!cleared.contains("xmp:Rating=\"2\""), "the prior rating is gone (can't resurrect elsewhere)");

    // COMPOSABILITY: a rating write into a rotation-only (tiff:Orientation) sidecar INSERTS xmp:Rating +
    // xmlns:xmp and preserves the tiff data byte-for-byte — so a RAW's basename.xmp carries both schemas.
    let rot_only = "\u{feff}<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Falcon\">\n <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  <rdf:Description rdf:about=\"\"\n    xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\"\n   tiff:Orientation=\"6\"/>\n </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>";
    let cpath = dir.join("compose.CR3.xmp");
    std::fs::write(&cpath, rot_only).unwrap();
    assert_eq!(write_xmp_rating_sidecar(&cpath, 4).unwrap(), SidecarKind::Updated);
    let composed = std::fs::read_to_string(&cpath).unwrap();
    assert!(composed.contains("tiff:Orientation=\"6\""), "rotation preserved byte-for-byte");
    assert!(composed.contains("xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"") && composed.contains("xmp:Rating=\"4\""), "rating + xmlns injected");
    // and the reverse composes too: a later Orientation write into THIS rating-bearing sidecar keeps the rating.
    assert_eq!(write_xmp_sidecar(&cpath, 3).unwrap(), SidecarKind::Updated);
    let both = std::fs::read_to_string(&cpath).unwrap();
    assert!(both.contains("xmp:Rating=\"4\"") && both.contains("tiff:Orientation=\"3\""), "both schemas coexist");

    // foreign content (a real dc:creator + xmpMM block, which must NOT be mistaken for the xmp: decl) is
    // preserved outside the replaced value; the xmp:Rating= needle carries its '=' so xmp:RatingPercent survives.
    let foreign = "<?xpacket begin=\"\"?><x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmlns:xmpMM=\"http://ns.adobe.com/xap/1.0/mm/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmp:Rating=\"1\" xmp:RatingPercent=\"20\" dc:creator=\"Someone &amp; Co\"/></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>";
    let fpath = dir.join("foreign.CR3.xmp");
    std::fs::write(&fpath, foreign).unwrap();
    assert_eq!(write_xmp_rating_sidecar(&fpath, 5).unwrap(), SidecarKind::Updated);
    let out = std::fs::read_to_string(&fpath).unwrap();
    assert_eq!(out, foreign.replace("xmp:Rating=\"1\"", "xmp:Rating=\"5\""), "ONLY the xmp:Rating value changed");
    assert!(out.contains("xmp:RatingPercent=\"20\"") && out.contains("dc:creator=\"Someone &amp; Co\""), "neighbours + entity untouched");

    // an UNRECOGNISED sidecar (no rdf:Description) is refused, never clobbered.
    let upath = dir.join("weird.CR3.xmp");
    std::fs::write(&upath, "not xmp at all").unwrap();
    assert!(matches!(write_xmp_rating_sidecar(&upath, 3), Err(SidecarErr::Unrecognised)));
    assert_eq!(std::fs::read_to_string(&upath).unwrap(), "not xmp at all", "foreign file untouched");

    // bounded read refuses an oversized sidecar (> 4 MB).
    let big = dir.join("huge.CR3.xmp");
    std::fs::write(&big, vec![b'x'; (XMP_READ_CAP + 16) as usize]).unwrap();
    assert!(matches!(write_xmp_rating_sidecar(&big, 3), Err(SidecarErr::TooLarge)));
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────── v0.8.70 (E-audit): xmp:Rating namespace scoping / refuse-don't-duplicate / read-back ─────────

/// FIX 2: on a multi-Description packet that declares `xmlns:xmp` ONLY on a LATER sibling, the insert must
/// add BOTH `xmlns:xmp` and `xmp:Rating` to the FIRST Description (the span it injects into) — never emit a
/// bare, undeclared `xmp:` prefix. Asserts both substrings land inside the FIRST Description's opening tag.
#[test]
fn t_fix2_xmp_rating_namespace_scoped_to_first_description() {
    // first Description has NO xmlns:xmp; the SECOND declares it. A global presence test would wrongly skip
    // the decl on the first, producing an undeclared prefix.
    let multi = "\u{feff}<?xpacket begin=\"\u{feff}\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" dc:format=\"image/x-canon-cr3\"/>\n  <rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:CreatorTool=\"Other\"/>\n </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>";
    let (out, changed) = xmp_surgical_set_rating(multi, 3).expect("insert into the first Description");
    assert!(changed, "an insert is a change");
    // isolate the FIRST Description's opening tag in the OUTPUT.
    let d0 = out.find("<rdf:Description").expect("first Description present");
    let first_tag = &out[d0..d0 + out[d0..].find('>').expect("first tag closes")];
    assert!(first_tag.contains("xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\""), "xmlns:xmp injected into the FIRST tag: {first_tag}");
    assert!(first_tag.contains("xmp:Rating=\"3\""), "xmp:Rating injected into the FIRST tag: {first_tag}");
    // the second Description's own xmlns:xmp is untouched, and dc:format on the first survives.
    assert!(out.contains("dc:format=\"image/x-canon-cr3\""), "first Description's foreign attr preserved");
    assert!(out.contains("xmp:CreatorTool=\"Other\""), "second Description preserved");
    // well-formedness sanity: exactly one xmp:Rating in the whole packet (no duplicate).
    assert_eq!(out.matches("xmp:Rating=").count(), 1, "exactly one xmp:Rating attribute");
}

/// FIX 3: a sidecar carrying `xmp:Rating` in a form neither locate matched (whitespace around '=') must be
/// REFUSED (Unrecognised), never rewritten with a DUPLICATE xmp:Rating. The file is left byte-for-byte.
/// A sidecar carrying only `xmp:RatingPercent` (a different property) still accepts a normal insert.
#[test]
fn t_fix3_unmatched_xmp_rating_refuses_not_duplicates() {
    let dir = tmp_dir();
    // whitespace form `xmp:Rating = "3"` — the `xmp:Rating=` needle (no space) misses it.
    let ws = "<?xpacket begin=\"\"?><x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:Rating = \"3\"/></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>";
    let wpath = dir.join("ws.CR3.xmp");
    std::fs::write(&wpath, ws).unwrap();
    assert!(matches!(write_xmp_rating_sidecar(&wpath, 5), Err(SidecarErr::Unrecognised)), "whitespace form refused");
    assert_eq!(std::fs::read_to_string(&wpath).unwrap(), ws, "refused file untouched (no duplicate inserted)");
    // an unquoted `xmp:Rating=3` also refuses rather than duplicating.
    assert_eq!(xmp_surgical_set_rating("<rdf:Description rdf:about=\"\" xmp:Rating=3 />", 4), None, "unquoted form refused");
    // a sidecar with ONLY xmp:RatingPercent (no xmp:Rating) still accepts an insert — mentions_xmp_rating
    // must not confuse the two.
    let (out, changed) = xmp_surgical_set_rating(
        "<rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:RatingPercent=\"40\"/>",
        2,
    ).expect("insert alongside xmp:RatingPercent");
    assert!(changed && out.contains("xmp:Rating=\"2\"") && out.contains("xmp:RatingPercent=\"40\""), "both coexist: {out}");
    assert!(!mentions_xmp_rating("<rdf:Description xmp:RatingPercent=\"40\"/>"), "RatingPercent is not the Rating property");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `sidecar_rating` read-back (FIX 1b input): pulls the raw value from both locate forms; a missing /
/// unrecognised sidecar → None. This is the detector the backfill 0-correction keys on.
#[test]
fn t_fix1_sidecar_rating_read_back() {
    let dir = tmp_dir();
    let mk = |name: &str, body: &str| { let p = dir.join(name); std::fs::write(&p, body).unwrap(); p };
    let attr = mk("a.xmp", "<rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:Rating=\"3\"/>");
    assert_eq!(sidecar_rating(&attr).as_deref(), Some("3"));
    let neg = mk("n.xmp", "<rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:Rating=\"-1\"/>");
    assert_eq!(sidecar_rating(&neg).as_deref(), Some("-1"));
    let zero = mk("z.xmp", "<rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:Rating=\"0\"/>");
    assert_eq!(sidecar_rating(&zero).as_deref(), Some("0"));
    let elem = mk("e.xmp", "<rdf:Description><xmp:Rating>4</xmp:Rating></rdf:Description>");
    assert_eq!(sidecar_rating(&elem).as_deref(), Some("4"));
    assert_eq!(sidecar_rating(&dir.join("absent.xmp")), None, "missing sidecar → None");
    let none = mk("none.xmp", "<rdf:Description rdf:about=\"\"/>");
    assert_eq!(sidecar_rating(&none), None, "no xmp:Rating → None");
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── T-i: idempotent crash recovery (re-apply is a no-op) ─────────────────────────────

#[test]
fn t_i_idempotent_reapply_no_double_write() {
    let dir = tmp_dir();
    let en = Endian::Little;
    let (bytes, _voff) = portrait_jpeg(en, 6);
    let p = write_tmp(&dir, "crash.jpg", &bytes);
    let (expected, target) = (6u8, compose_exif_orientation(6, 1)); // pinned target = 3
    // 1) apply — patches 6 → 3.
    assert_eq!(patch_jpeg_orientation(&p, expected, target).unwrap(), JpegPatch::Patched);
    let applied = std::fs::read(&p).unwrap();
    // 2) "crash before the delta/JSON clear" — the delta is still present, so Apply runs again with the
    //    SAME pinned (expected, target). The CAS sees current == target → success, NO write.
    assert_eq!(patch_jpeg_orientation(&p, expected, target).unwrap(), JpegPatch::AlreadyTarget);
    assert_eq!(std::fs::read(&p).unwrap(), applied, "re-apply wrote nothing — file bytes identical (no double-rotation)");
    // a third re-apply is likewise inert.
    assert_eq!(patch_jpeg_orientation(&p, expected, target).unwrap(), JpegPatch::AlreadyTarget);
    assert_eq!(std::fs::read(&p).unwrap(), applied);
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── T-l: RAW+JPG pair, one side fails → shot keeps its delta ─────────────────────────────

#[test]
fn t_l_pair_apply_and_one_side_failure() {
    let dir = tmp_dir();
    let en = Endian::Little;
    // A pair: a valid JPEG (in-place) + a fake CR3 whose sidecar write we FORCE to fail by pre-planting
    // an UNRECOGNISED sidecar at its basename path.
    let (jbytes, jvoff) = portrait_jpeg(en, 6);
    let jpg = write_tmp(&dir, "PAIR.JPG", &jbytes);
    let raw = write_tmp(&dir, "PAIR.CR3", &[0u8; 32]); // rawler reads None → treated as upright
    std::fs::write(dir.join("PAIR.xmp"), "garbage, no description").unwrap(); // forces RAW-side Unrecognised
    let jbefore = std::fs::read(&jpg).unwrap();

    let report = apply_rotation(&RotApplyPlan {
        finished: Some(jpg.clone()),
        finished_is_jpeg: true,
        raw: Some(raw.clone()),
        base_turns: 1, // JPG believed at 6 (90°); expected reconstructs to 6
        delta: 1,
    });
    // JPG side succeeded (patched), RAW side failed → the shot is NOT ok (caller keeps the delta).
    assert_eq!(report.finished_action, SideAction::Patched);
    assert!(matches!(report.raw_action, SideAction::Failed(_)));
    assert!(!report.ok, "a one-side failure keeps the shot's delta");
    // the JPG was still correctly patched in place (only 2 bytes).
    let jafter = std::fs::read(&jpg).unwrap();
    differs_only_at(&jbefore, &jafter, jvoff);
    assert_eq!(exif_orientation(&jpg), Some(3));

    // Now the SUCCESS path: fresh dir, no planted sidecar → both sides apply, sidecar created fresh.
    let dir2 = tmp_dir();
    let (jb2, _) = portrait_jpeg(en, 6);
    let jpg2 = write_tmp(&dir2, "OK.JPG", &jb2);
    let raw2 = write_tmp(&dir2, "OK.CR3", &[0u8; 32]);
    let rep2 = apply_rotation(&RotApplyPlan {
        finished: Some(jpg2.clone()),
        finished_is_jpeg: true,
        raw: Some(raw2.clone()),
        base_turns: 1,
        delta: 1,
    });
    assert!(rep2.ok);
    assert_eq!(rep2.finished_action, SideAction::Patched);
    assert_eq!(rep2.raw_action, SideAction::SidecarCreated);
    assert_eq!(rep2.new_base_turns, 2, "finished-side base becomes (1 + 1) turns");
    assert!(dir2.join("OK.xmp").exists(), "RAW sidecar created at basename.xmp");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

// ───────────────────────────── T-m: sidecar READ-BACK (both forms) + naming isolation (G1) ─────────────────────────────

#[test]
fn t_m_sidecar_orientation_read_and_naming_isolation() {
    let dir = tmp_dir();
    let raw = write_tmp(&dir, "HWU.CR3", &[0u8; 32]);
    // attribute form on the RAW's basename sidecar.
    std::fs::write(dir.join("HWU.xmp"),
        "\u{feff}<?xpacket?><rdf:Description xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" tiff:Orientation=\"6\"/><?xpacket end=\"w\"?>").unwrap();
    assert_eq!(sidecar_orientation(&raw, true), Some(6), "attribute-form basename sidecar read");
    // element form.
    std::fs::write(dir.join("HWU.xmp"), "<rdf:Description><tiff:Orientation>8</tiff:Orientation></rdf:Description>").unwrap();
    assert_eq!(sidecar_orientation(&raw, true), Some(8), "element-form basename sidecar read");
    // out-of-range value → None (fall through to embedded).
    std::fs::write(dir.join("HWU.xmp"), "<rdf:Description tiff:Orientation=\"9\"/>").unwrap();
    assert_eq!(sidecar_orientation(&raw, true), None, "out-of-range value rejected");
    // no sidecar at all → None.
    let lone = write_tmp(&dir, "LONE.CR3", &[0u8; 32]);
    assert_eq!(sidecar_orientation(&lone, true), None, "absent sidecar → None");

    // NAMING ISOLATION for a RAW+JPG pair: the RAW reads basename.xmp ONLY; the finished reads
    // fullname.xmp ONLY — the two must never cross-orient each other.
    let jpg = write_tmp(&dir, "PAIR.JPG", &[0xFF, 0xD8, 0xFF, 0xD9]);
    let raw2 = write_tmp(&dir, "PAIR.CR3", &[0u8; 32]);
    std::fs::write(dir.join("PAIR.xmp"), "<rdf:Description tiff:Orientation=\"3\"/>").unwrap();     // RAW basename
    std::fs::write(dir.join("PAIR.JPG.xmp"), "<rdf:Description tiff:Orientation=\"6\"/>").unwrap(); // JPG fullname
    assert_eq!(sidecar_orientation(&raw2, true), Some(3), "RAW reads basename.xmp only");
    assert_eq!(sidecar_orientation(&jpg, false), Some(6), "finished reads fullname.xmp only, NOT the RAW's basename");
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── T-n: double-Apply on a RAW sidecar ADVANCES, not resets (finding #1, G1) ─────────────────────────────

#[test]
fn t_n_double_apply_raw_sidecar_advances_not_resets() {
    let dir = tmp_dir();
    // RAW-only shot; the fake CR3's embedded orientation reads as upright (rawler None → 1) and NEVER
    // changes (the RAW is never patched) — the exact desync condition finding #1 describes.
    let raw = write_tmp(&dir, "A.CR3", &[0u8; 32]);

    // Apply +2 (180°): sidecar written at compose(1,2) = 3.
    let rep1 = apply_rotation(&RotApplyPlan {
        finished: None, finished_is_jpeg: false, raw: Some(raw.clone()), base_turns: 0, delta: 2,
    });
    assert!(rep1.ok);
    assert_eq!(rep1.new_base_turns, 2);
    assert_eq!(sidecar_orientation(&raw, true), Some(3), "first Apply writes 180° (orientation 3)");

    // Apply +1 more — cumulative intent is 270°. Embedded is STILL 1, but the sidecar-aware read composes
    // on the WRITTEN 3, so target = compose(3,1) = 8 (270°), NOT the buggy compose(1,1) = 6 (90°).
    let rep2 = apply_rotation(&RotApplyPlan {
        finished: None, finished_is_jpeg: false, raw: Some(raw.clone()), base_turns: 2, delta: 1,
    });
    assert!(rep2.ok);
    assert_eq!(rep2.new_base_turns, 3);
    assert_eq!(sidecar_orientation(&raw, true), Some(8), "second Apply ADVANCES to 270° (8), not back to 90° (6)");
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── T-p: a non-UTF-8 sidecar is REFUSED, bytes untouched (finding #2, G5) ─────────────────────────────

#[test]
fn t_p_non_utf8_sidecar_refused_bytes_untouched() {
    let dir = tmp_dir();
    // A valid-STRUCTURE sidecar (ASCII markup) carrying a stray non-UTF-8 byte (0xE9 Latin-1 'é') inside a
    // dc:rights value — the exact foreign-bytes case finding #2 describes. A surgical update must REFUSE
    // (NotUtf8), never lossily transcoding the 0xE9 to U+FFFD, leaving the file byte-for-byte intact.
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"<rdf:Description xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\" tiff:Orientation=\"1\" dc:rights=\"Cr");
    bytes.push(0xE9);
    bytes.extend_from_slice(b"e\"/>");
    let side = dir.join("foreign.xmp"); // RAW basename sidecar
    std::fs::write(&side, &bytes).unwrap();
    let before = std::fs::read(&side).unwrap();

    // Direct write refuses.
    let r = write_xmp_sidecar(&side, 6);
    assert!(matches!(r, Err(SidecarErr::NotUtf8)), "non-UTF-8 sidecar refused, got {r:?}");
    assert_eq!(std::fs::read(&side).unwrap(), before, "non-UTF-8 sidecar bytes untouched (no U+FFFD, no clobber)");

    // Orchestrated: the RAW-side apply surfaces it as a FAILED side (shot keeps its delta), never a
    // create-fresh over the existing file.
    let raw = write_tmp(&dir, "foreign.CR3", &[0u8; 32]); // sidecar_basename → foreign.xmp (same file)
    let rep = apply_rotation(&RotApplyPlan {
        finished: None, finished_is_jpeg: false, raw: Some(raw.clone()), base_turns: 0, delta: 1,
    });
    assert!(!rep.ok, "one failed side → shot keeps its delta");
    assert!(matches!(rep.raw_action, SideAction::Failed(_)));
    assert_eq!(std::fs::read(&side).unwrap(), before, "still byte-for-byte untouched after the orchestrated apply");
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── T-q: write_atomic cleans the tmp orphan on a rename failure (finding #4, G6) ─────────────────────────────

#[test]
fn t_q_write_atomic_cleans_tmp_on_rename_failure() {
    let dir = tmp_dir();
    // A read-only existing sidecar makes the atomic rename-over fail (ERROR_ACCESS_DENIED on Windows).
    let side = dir.join("ro.CR3.xmp");
    std::fs::write(&side, "<rdf:Description tiff:Orientation=\"1\"/>").unwrap();
    let mut perm = std::fs::metadata(&side).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(&side, perm).unwrap();

    // A surgical 1 → 6 update writes ro.CR3.xmp.tmp then tries to rename over the read-only target.
    let r = write_xmp_sidecar(&side, 6);
    let tmp = side.with_extension("xmp.tmp");
    if r.is_err() {
        // rename failed (read-only enforced) → the deterministic tmp orphan must have been removed.
        assert!(!tmp.exists(), "tmp orphan must be removed after a failed rename");
    }
    // (Some filesystems permit rename-over-read-only; the invariant under test is only 'no orphan when
    // the rename FAILS'. Restore writability so temp-dir cleanup succeeds.)
    if let Ok(md) = std::fs::metadata(&side) {
        let mut p = md.permissions();
        p.set_readonly(false);
        let _ = std::fs::set_permissions(&side, p);
    }
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── T-r: CAS 'else refuse' — fresh matches NEITHER expected nor target (finding #10, G8) ─────────────────────────────

#[test]
fn t_r_cas_refuse_fresh_matches_neither() {
    let dir = tmp_dir();
    let en = Endian::Little;
    // File is at orientation 6. patch with expected=1, target=3 — the fresh 6 matches NEITHER → the core
    // write-safety clause must refuse (PatchErr::Cas) and write ZERO bytes.
    let (bytes, _voff) = portrait_jpeg(en, 6);
    let p = write_tmp(&dir, "changed.jpg", &bytes);
    let before = std::fs::read(&p).unwrap();
    let r = patch_jpeg_orientation(&p, 1, 3);
    assert!(matches!(r, Err(PatchErr::Cas { expected: 1, target: 3, found: 6 })), "fresh not in {{expected,target}} → Cas, got {r:?}");
    assert_eq!(std::fs::read(&p).unwrap(), before, "CAS refuse writes ZERO bytes (file byte-identical)");
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── T-s: orchestrator Cas → sidecar fallback (finding #10, G8) ─────────────────────────────

#[test]
fn t_s_orchestrator_cas_falls_back_to_sidecar() {
    let dir = tmp_dir();
    let en = Endian::Little;
    // JPEG actually at orientation 6, but the plan believes base_turns=0 (expected=1). With delta=2 the
    // target is compose(1,2)=3, so the in-place CAS sees fresh 6 ∉ {1,3} → Cas → the orchestrator writes a
    // fullname sidecar composed on the FOUND value (compose(6,2)=8), never a blind in-place write.
    let (bytes, _jvoff) = portrait_jpeg(en, 6);
    let p = write_tmp(&dir, "drift.JPG", &bytes);
    let before = std::fs::read(&p).unwrap();
    let rep = apply_rotation(&RotApplyPlan {
        finished: Some(p.clone()), finished_is_jpeg: true, raw: None, base_turns: 0, delta: 2,
    });
    assert!(rep.ok, "the sidecar fallback succeeds");
    assert_eq!(rep.finished_action, SideAction::SidecarCreated);
    // v0.8.103 (V1): the FILE is still composed on the found value (8, asserted below) — that half is
    // unchanged and is the one that matters for what lands on disk. The REPORT is in the caller's own
    // residual terms, so it is (base 0 + delta 2) = 2, not the written value's absolute turns (3).
    //
    // This assertion used to read `turns_of(8)`, a leftover from before v0.8.102 changed
    // `new_base_turns` from FILE-ABSOLUTE to RESIDUAL; it survived that change only because `consumed`
    // was 0 here, which made the two numbers coincide. They no longer do, because the report is now
    // derived from the plan rather than from the file, and this test's whole premise is a plan that
    // DISAGREES with the file. A drifted base is re-synced by the caller's orientation-cache
    // invalidate on the same drain (main.rs), not by the apply report second-guessing the plan.
    assert_eq!(rep.new_base_turns, 2, "the report is the caller's own (base + delta), residual");
    assert_eq!(std::fs::read(&p).unwrap(), before, "no in-place write on a CAS mismatch");
    assert_eq!(sidecar_orientation(&p, false), Some(8), "fullname sidecar composed on the found orientation");
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── sidecar_path_for (v0.8.13 delete-path naming) ─────────────────────────────

#[test]
fn sidecar_path_for_raw_uses_basename() {
    // A RAW's sidecar drops the extension: HWU_0141.CR3 → HWU_0141.xmp (Adobe/LR convention). This is
    // the name the delete path must collect so an applied-rotation sidecar rides the RAW to the bin.
    assert_eq!(
        sidecar_path_for(Path::new("/shoot/HWU_0141.CR3"), true),
        PathBuf::from("/shoot/HWU_0141.xmp")
    );
    // Case + alternate RAW extension: only the extension is replaced, the stem is preserved verbatim.
    assert_eq!(
        sidecar_path_for(Path::new("/s/A7R00042.ARW"), true),
        PathBuf::from("/s/A7R00042.xmp")
    );
}

#[test]
fn sidecar_path_for_finished_uses_fullname() {
    // A finished file's sidecar APPENDS `.xmp` to the whole name: HWU_0141.JPG → HWU_0141.JPG.xmp, so it
    // can never collide with a paired RAW's basename sidecar (HWU_0141.xmp).
    assert_eq!(
        sidecar_path_for(Path::new("/shoot/HWU_0141.JPG"), false),
        PathBuf::from("/shoot/HWU_0141.JPG.xmp")
    );
    assert_eq!(
        sidecar_path_for(Path::new("/s/scan.tif"), false),
        PathBuf::from("/s/scan.tif.xmp")
    );
}

// ───────────────────────────── T-t (v0.8.103, V1): the finished-side report is DERIVED, not probed ─────────────────────────────

/// **The V1 pin.** `apply_rotation`'s finished-side `new_base_turns` is computed from two values the
/// plan already carries, so no probe — and therefore no probe FAILURE — can move it.
///
/// v0.8.102 recovered the FILE-ABSOLUTE → RESIDUAL gap by re-running `decoder_consumed_turns` on the
/// apply worker. For a HEIC that is a COM `CreateDecoderFromFilename` over the file, and it degraded
/// SILENTLY to 0 on a sharing violation (OneDrive/AV/Photos holding it), a COM failure or a codec
/// hiccup. A degraded probe on IMG_2814 reported `new_base_turns = 2` where the user had asked for
/// one quarter-turn: the drain re-based the shot a turn past what was on screen, `old_eff != new_eff`
/// fired, and every tier re-rendered it wrong — the F1/F2 RED, reproduced by its own fix.
///
/// The first block below is that exact file shape (absolute 6 on disk, residual base 0 because the
/// decoder already spent the turn) driven through the real `apply_rotation` with NOTHING to probe.
/// The sweep then walks every absolute × base × delta the caller could present, mirror class
/// included, and pins both halves of the fork at once: the FILE gets `compose(cur, delta)` and the
/// REPORT gets `(base + delta) & 3`.
///
/// FALSIFIER (L28): restore the v0.8.102 subtraction of a separately-supplied `consumed` and the
/// sweep's report rows fail on every absolute whose turns disagree with `base` — which is precisely
/// the set a stale-or-failed probe could produce. Change `residual_turns` to use `turns_of(target)`
/// alone (the pre-v0.8.102 FILE-ABSOLUTE report) and every row with `abs` not upright fails.
#[test]
fn t_t_finished_report_is_derived_not_probed() {
    let dir = tmp_dir();
    // (a) the IMG_2814 shape, on the non-JPEG (sidecar-only) arm every HEIC takes. There is no
    //     `decoder_consumed_turns` input to this plan at all — that is the point.
    let heic = write_tmp(&dir, "IMG_2814.HEIC", &[0u8; 32]);
    std::fs::write(dir.join("IMG_2814.HEIC.xmp"), "<rdf:Description tiff:Orientation=\"6\"/>").unwrap();
    let rep = apply_rotation(&RotApplyPlan {
        finished: Some(heic.clone()),
        finished_is_jpeg: false,
        raw: None,
        base_turns: 0, // RESIDUAL: WIC already honoured the container's irot
        delta: 1,
    });
    assert!(rep.ok, "{:?}", rep.finished_action);
    assert_eq!(sidecar_orientation(&heic, false), Some(3), "on disk: compose(6, 1) = 3, FILE-ABSOLUTE");
    assert_eq!(rep.new_base_turns, 1, "in the report: base 0 + delta 1, RESIDUAL — no probe involved");

    // (b) the whole table. Every absolute the file could carry (1..=8, both mirror cycles), every
    //     residual base the caller could hold, every non-zero delta.
    for abs in 1u8..=8 {
        for base in 0u8..4 {
            for delta in 1u8..4 {
                let d2 = tmp_dir();
                let f = write_tmp(&d2, "X.HEIC", &[0u8; 32]);
                std::fs::write(
                    d2.join("X.HEIC.xmp"),
                    format!("<rdf:Description tiff:Orientation=\"{abs}\"/>"),
                )
                .unwrap();
                let r = apply_rotation(&RotApplyPlan {
                    finished: Some(f.clone()),
                    finished_is_jpeg: false,
                    raw: None,
                    base_turns: base,
                    delta,
                });
                assert!(r.ok, "abs {abs} base {base} delta {delta}: {:?}", r.finished_action);
                assert_eq!(
                    sidecar_orientation(&f, false),
                    Some(compose_exif_orientation(abs, delta)),
                    "abs {abs} base {base} delta {delta}: the FILE is composed on its own absolute \
                     value, mirror class preserved"
                );
                assert_eq!(
                    r.new_base_turns,
                    (base + delta) & 3,
                    "abs {abs} base {base} delta {delta}: the REPORT is the caller's own residual \
                     arithmetic — independent of the file's absolute value, and so of any probe of it"
                );
                let _ = std::fs::remove_dir_all(&d2);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// v0.8.103 (V4): the JPEG in-place arm's `consumed == 0` premise is the CAS, not an assumption.
///
/// A JPEG whose EXIF tag agrees with the plan's base is patched in place and reports `base + delta`;
/// a JPEG whose tag does NOT agree cannot match the CAS and is routed to the sidecar arm, which never
/// mutates the tag. Those are the only two outcomes, and the second is where a non-zero `consumed`
/// (the transposed-EXIF-dims anomaly `decoder_consumed_turns`'s doc describes) necessarily lands,
/// because a non-zero consumed shifts the file's tag away from `expected` by exactly those turns.
///
/// FALSIFIER (L28): make the in-place path unconditional (patch on a CAS mismatch instead of falling
/// through) and the second block below writes 2 bytes into a file whose tag the app misread — the
/// blind write the whole write-safety contract exists to refuse.
#[test]
fn t_u_jpeg_in_place_is_gated_by_the_cas_not_by_an_assumption() {
    let dir = tmp_dir();
    let en = Endian::Little;
    // Agreeing: tag 6, base 1 → expected 6, CAS matches, in-place patch, report (1 + 1).
    let (b1, voff) = portrait_jpeg(en, 6);
    let p1 = write_tmp(&dir, "AGREE.JPG", &b1);
    let before1 = std::fs::read(&p1).unwrap();
    let r1 = apply_rotation(&RotApplyPlan {
        finished: Some(p1.clone()), finished_is_jpeg: true, raw: None, base_turns: 1, delta: 1,
    });
    assert_eq!(r1.finished_action, SideAction::Patched);
    assert_eq!(r1.new_base_turns, 2);
    differs_only_at(&before1, &std::fs::read(&p1).unwrap(), voff);
    assert!(!sidecar_fullname(&p1).exists(), "an agreeing JPEG never grows a sidecar");

    // Disagreeing: same tag 6, base 0 → expected 1, target compose(1,2) = 3, so the file's fresh 6 is
    // in NEITHER slot and the CAS refuses. ZERO bytes written to the JPEG and the turn goes to a
    // sidecar instead — the deliberate route for any non-zero `consumed`. (delta 2 rather than 1
    // because with delta 1 the target would BE 6: the CAS would match on the target slot and report
    // `AlreadyTarget`, which is the idempotency path, not a disagreement.)
    let (b2, _) = portrait_jpeg(en, 6);
    let p2 = write_tmp(&dir, "DRIFT.JPG", &b2);
    let before2 = std::fs::read(&p2).unwrap();
    let r2 = apply_rotation(&RotApplyPlan {
        finished: Some(p2.clone()), finished_is_jpeg: true, raw: None, base_turns: 0, delta: 2,
    });
    assert_eq!(r2.finished_action, SideAction::SidecarCreated);
    assert_eq!(std::fs::read(&p2).unwrap(), before2, "the tag is NOT rewritten when the CAS refuses");
    assert_eq!(sidecar_orientation(&p2, false), Some(8), "compose(6, 2) = 8 in the sidecar");

    // The third outcome the CAS can produce: the file is ALREADY at the target (a crash-resumed
    // Apply, or a base that is stale by exactly the delta). No write, no sidecar, and the report is
    // still the caller's own (base + delta) — the idempotency contract in the module doc.
    let (b3, _) = portrait_jpeg(en, 6);
    let p3 = write_tmp(&dir, "AT_TARGET.JPG", &b3);
    let before3 = std::fs::read(&p3).unwrap();
    let r3 = apply_rotation(&RotApplyPlan {
        finished: Some(p3.clone()), finished_is_jpeg: true, raw: None, base_turns: 0, delta: 1,
    });
    assert_eq!(r3.finished_action, SideAction::AlreadyTarget, "expected 1, target 6, found 6");
    assert_eq!(r3.new_base_turns, 1);
    assert_eq!(std::fs::read(&p3).unwrap(), before3, "an already-at-target apply writes nothing");
    assert!(!sidecar_fullname(&p3).exists(), "…and grows no sidecar either");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sidecar_path_for_pair_names_never_collide() {
    // A RAW+JPG pair sharing a stem yields two DISTINCT sidecar paths — the delete path can safely collect
    // both without one masking the other.
    let raw = sidecar_path_for(Path::new("/s/HWU_0141.CR3"), true);
    let jpg = sidecar_path_for(Path::new("/s/HWU_0141.JPG"), false);
    assert_ne!(raw, jpg);
    assert_eq!(raw, PathBuf::from("/s/HWU_0141.xmp"));
    assert_eq!(jpg, PathBuf::from("/s/HWU_0141.JPG.xmp"));
}

fn rotation_readonly(path: &Path, readonly: bool) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(if readonly { 0o444 } else { 0o644 });
    }
    #[cfg(not(unix))] permissions.set_readonly(readonly);
    std::fs::set_permissions(path, permissions).unwrap();
}
fn one_rotation(path: &Path, jpeg: bool, raw: bool) -> RotApplyPlan {
    RotApplyPlan { finished: (!raw).then(|| path.to_owned()), finished_is_jpeg: jpeg,
        raw: raw.then(|| path.to_owned()), base_turns: 0, delta: 1 }
}
#[test]
fn rotation_readonly_raw_and_png_write_only_their_sidecars() {
    for raw in [false, true] {
        let dir = tmp_dir();
        let bytes = portrait_jpeg(Endian::Little, 1).0;
        let source = write_tmp(&dir, if raw { "original.CR3" } else { "original.png" }, &bytes);
        rotation_readonly(&source, true);
        let report = apply_rotation(&one_rotation(&source, false, raw));
        rotation_readonly(&source, false);
        assert!(report.ok, "{:?} / {:?}", report.finished_action, report.raw_action);
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert_eq!(sidecar_orientation(&source, raw), Some(6));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
#[test]
fn rotation_denied_sidecar_preserves_both_pair_members_and_reports_path() {
    let dir = tmp_dir();
    let bytes = portrait_jpeg(Endian::Little, 1).0;
    let jpg = write_tmp(&dir, "pair.jpg", &bytes);
    let raw = write_tmp(&dir, "pair.CR3", b"RAW");
    let xmp = sidecar_basename(&raw);
    write_xmp_sidecar(&xmp, 1).unwrap();
    let old_xmp = std::fs::read(&xmp).unwrap();
    rotation_readonly(&xmp, true);
    let plan = RotApplyPlan { finished: Some(jpg.clone()), finished_is_jpeg: true, raw: Some(raw), base_turns: 0, delta: 1 };
    let report = apply_rotation(&plan);
    rotation_readonly(&xmp, false);
    assert!(!report.ok); assert_eq!(report.new_base_turns, 0);
    match &report.finished_action { SideAction::Failed(reason) => {
        assert_eq!(reason.path, xmp); assert_eq!(reason.reason, RotationFailureReason::ReadOnly);
    }, _ => panic!("expected path-specific refusal") }
    assert_eq!(std::fs::read(&jpg).unwrap(), bytes);
    assert_eq!(std::fs::read(&xmp).unwrap(), old_xmp);
    assert!(apply_rotation(&plan).ok);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn rotation_readonly_patch_target_is_refused_then_recovers() {
    let dir = tmp_dir(); let bytes = portrait_jpeg(Endian::Little, 1).0;
    let jpg = write_tmp(&dir, "patch.jpg", &bytes); let plan = one_rotation(&jpg, true, false);
    rotation_readonly(&jpg, true); let report = apply_rotation(&plan); rotation_readonly(&jpg, false);
    assert!(!report.ok); assert_eq!(report.new_base_turns, 0);
    assert_eq!(std::fs::read(&jpg).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    assert!(apply_rotation(&plan).ok); assert_eq!(exif_orientation(&jpg), Some(6));
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn rotation_readonly_jpeg_can_use_structural_or_cas_sidecar_fallback() {
    for bytes in [vec![0xff, 0xd8, 0xff, 0xd9], portrait_jpeg(Endian::Little, 3).0] {
        let dir = tmp_dir(); let jpg = write_tmp(&dir, "fallback.jpg", &bytes);
        rotation_readonly(&jpg, true); let report = apply_rotation(&one_rotation(&jpg, true, false));
        rotation_readonly(&jpg, false);
        assert!(report.ok, "{:?} / {:?}", report.finished_action, report.raw_action); assert!(sidecar_fullname(&jpg).is_file());
        assert_eq!(std::fs::read(&jpg).unwrap(), bytes);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
#[test]
fn rotation_already_target_needs_no_write_access() {
    let dir = tmp_dir(); let bytes = portrait_jpeg(Endian::Little, 6).0;
    let jpg = write_tmp(&dir, "already.jpg", &bytes);
    rotation_readonly(&jpg, true); let report = apply_rotation(&one_rotation(&jpg, true, false));
    rotation_readonly(&jpg, false);
    assert!(report.ok); assert_eq!(report.finished_action, SideAction::AlreadyTarget);
    assert_eq!(std::fs::read(&jpg).unwrap(), bytes); assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}
#[cfg(unix)]
#[test]
fn rotation_in_place_patch_does_not_require_writable_parent_but_sidecar_does() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp_dir(); let bytes = portrait_jpeg(Endian::Little, 1).0;
    let jpg = write_tmp(&dir, "in-place.jpg", &bytes);
    let png = write_tmp(&dir, "sidecar.png", &bytes);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let patched = apply_rotation(&one_rotation(&jpg, true, false));
    let refused = apply_rotation(&one_rotation(&png, false, false));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(patched.ok); assert_eq!(exif_orientation(&jpg), Some(6)); assert!(!refused.ok);
    assert!(!sidecar_fullname(&png).exists()); assert_eq!(std::fs::read(&png).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn rotation_preflight_creates_no_probe_and_missing_pair_cannot_patch_jpeg() {
    let dir = tmp_dir(); let bytes = portrait_jpeg(Endian::Little, 1).0;
    let jpg = write_tmp(&dir, "pair.jpg", &bytes);
    let png = write_tmp(&dir, "other.png", &bytes);
    assert!(permissions::check_rotation_write_access(&one_rotation(&png, false, false)).is_ok());
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    let plan = RotApplyPlan { finished: Some(jpg.clone()), finished_is_jpeg: true,
        raw: Some(dir.join("missing.CR3")), base_turns: 0, delta: 1 };
    assert!(!apply_rotation(&plan).ok); assert_eq!(std::fs::read(&jpg).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Existing XMP is enough: Apply must not open either kind of sidecar-only original.
/// Windows uses the maintainer's exclusive sharing lock; Unix removes read access.
#[cfg(any(windows, unix))]
#[test]
fn rotation_existing_sidecar_does_not_open_locked_original() {
    for raw in [true, false] {
        let dir = tmp_dir();
        let bytes = b"synthetic original, embedded orientation is unnecessary";
        let source = write_tmp(&dir, if raw { "locked.CR3" } else { "locked.png" }, bytes);
        write_xmp_sidecar(&sidecar_path_for(&source, raw), 1).unwrap();
        #[cfg(windows)]
        let lock = {
            use std::os::windows::fs::OpenOptionsExt;
            let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&source).unwrap();
            assert_eq!(std::fs::File::open(&source).unwrap_err().raw_os_error(), Some(32));
            lock
        };
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o000)).unwrap();
            assert_eq!(std::fs::File::open(&source).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        }
        let report = apply_rotation(&one_rotation(&source, false, raw));
        #[cfg(windows)] drop(lock);
        #[cfg(unix)] rotation_readonly(&source, false);
        assert!(report.ok, "{:?} / {:?}", report.finished_action, report.raw_action);
        assert_eq!(sidecar_orientation(&source, raw), Some(6));
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
