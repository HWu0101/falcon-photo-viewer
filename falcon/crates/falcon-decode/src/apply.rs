//! v0.8.0 stage 2 — the ROTATION APPLY pipeline: Falcon's FIRST mutation of user originals.
//!
//! Two write paths, chosen per file, plus a hard-won write-safety contract (CODEBASE_REVIEW_2026-07
//! §2 theme 2 / §5, security perspective):
//!   * **JPEG in-place 2-byte patch** — re-parse the APP1/TIFF/IFD0 structure FROM SCRATCH at apply
//!     time (never a stage-1 cached offset), locate the `Orientation` SHORT's value field, and
//!     compare-and-swap the 2 bytes at that offset. A 2-byte `pwrite` at a verified offset cannot
//!     truncate; a full-file temp+rename on a 50 MB JPG would break hardlinks, churn OneDrive, and add
//!     a disk-space failure mode — so the in-place patch is deliberate (contract item 2). Post-write we
//!     read the value back through the NORMAL bounded EXIF reader and assert it equals the target.
//!   * **XMP sidecar** — RAW (CR3, …) NEVER gets an in-place patch (contract item 3); nor does a
//!     finished PNG/TIFF/WebP/HEIC, nor a JPEG whose EXIF is absent/anomalous (no 2 bytes to patch).
//!     These get a `.xmp` sidecar written via a full tmp+atomic-rename. An EXISTING sidecar is updated
//!     by a SURGICAL text-level replace of `tiff:Orientation` only — no XML parser touches untrusted
//!     bytes (entity-safe by construction), foreign namespaces/content are preserved byte-for-byte, and
//!     the read is bounded (4 MB) so a folder-planted multi-GB `.xmp` can't OOM the worker. A sidecar
//!     that is not valid UTF-8 is REFUSED, never lossily transcoded: its foreign non-UTF-8 bytes are
//!     never rewritten to U+FFFD — the update fails, the shot keeps its delta, the file is left
//!     byte-for-byte intact (we never clobber it and never create-fresh over an existing sidecar).
//!
//! **Read-back (item 9).** An APPLIED rotation on the sidecar route lives ONLY in the `.xmp`, so the
//! display reader ([`sidecar_orientation`], consulted first by `read_orientation`) reads it back and it
//! WINS over the file's immutable embedded orientation (Adobe convention) — an applied rotation thus
//! round-trips through Falcon's own reader, and a SECOND Apply composes on the value the first wrote.
//! v0.8.102: the value in the sidecar is FILE-ABSOLUTE (see [`apply_rotation`]'s convention note), so
//! `read_orientation` subtracts [`crate::decoder_consumed_turns`] from it on the way back in — the one
//! translation that keeps a HEIC's absolute sidecar and Falcon's residual display base agreeing.
//! v0.8.103 (V1): only the READ side probes for that number. The WRITE side does not: it is implied
//! by two values [`apply_rotation`] already holds, so the apply path has no probe left to fail (the
//! probe's silent-to-zero degradation on a locked/held file re-armed the very RED it was added for).
//!
//! **Idempotency / journal-free crash recovery (contract item 5).** The per-file caller sequence is
//! patch → read-back verify → clear that file's delta → flush the selection JSON. The delta map in the
//! selection JSON IS the recovery journal — no separate WAL. The patcher itself is idempotent under a
//! repeated `(expected → target)` apply: a re-run whose file is ALREADY at `target` performs NO write
//! and reports [`SideAction::AlreadyTarget`]. So a crash that leaves a delta un-cleared cannot cause a
//! double-write on the next Apply *as long as the caller pins the same target* (it recomputes target
//! from the same in-memory base+delta) — the CAS sees current == target and clears the delta benignly.
//! (The one residual, documented for the architect: a crash in the sub-millisecond window between the
//! file's fsync and the JSON rename leaves the file at target while the base re-reads as target on the
//! NEXT process launch — the caller then can no longer recompute the original target, so that ONE shot
//! shows a double rotation until the user re-rotates it. Non-corrupting, single-shot, user-recoverable;
//! fully closing it needs the WAL the review floated. Journal-free is the spec's deliberate choice.)

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::exif_orientation; // the bounded (4 MB-capped) kamadak reader — used for the read-back verify

// ───────────────────────────── the orientation ALGEBRA (pure, 32-cell table) ─────────────────────────────

/// EXIF `Orientation` value (1..=8) → clockwise quarter-turns (0..=3), the SAME mapping the display
/// side uses (`falcon_decode::orientation_to_turns`). Duplicated here as a tiny private helper so the
/// apply algebra is self-contained/testable; kept bit-identical to the public one (a test pins it).
fn turns_of(o: u8) -> u8 {
    match o {
        3 | 4 => 2,
        5 | 6 => 1,
        7 | 8 => 3,
        _ => 0,
    }
}

/// The non-mirrored EXIF orientations indexed by clockwise quarter-turns: 0→1 (upright), 1→6 (90° CW),
/// 2→3 (180°), 3→8 (270° CW). The clockwise rotation cycle 1→6→3→8→1.
const NONMIRR_BY_TURNS: [u8; 4] = [1, 6, 3, 8];
/// The MIRRORED EXIF orientations indexed by clockwise quarter-turns THE WAY FALCON READS THEM
/// (`orientation_to_turns`: 2→0, 5→1, 4→2, 7→3), so the mirrored cycle is 2→5→4→7→2. This ordering —
/// not the physical-EXIF 2→7→4→5 rotation cycle — is what keeps a Falcon round-trip stable: Falcon
/// DROPS the mirror on display (S1), so `orientation_to_turns(compose(o,δ)) == turns(o)+δ` must hold
/// for the applied file to re-display exactly as the user saw it (test T-j). The consequence, noted in
/// the module doc: a *mirrored* file (rare — real cameras write 1/3/6/8) still reads differently in an
/// EXIF-honoring app than in Falcon's mirror-dropped view, exactly as it already did before apply.
const MIRR_BY_TURNS: [u8; 4] = [2, 5, 4, 7];

/// True for the mirrored EXIF orientations (2/4/5/7).
fn is_mirrored(o: u8) -> bool {
    matches!(o, 2 | 4 | 5 | 7)
}

/// The FILE-side rotation composer (contract item 6): compose a new EXIF `Orientation` value from the
/// CURRENT value and a clockwise quarter-turn `delta`, **preserving the mirror class**. The non-mirrored
/// cycle {1,6,3,8} and the mirrored cycle {2,5,4,7} each rotate within themselves; a value outside
/// 1..=8 is treated as 1 (upright, non-mirrored). Pure — the 32-cell table + round-trips are unit-tested.
///
/// This is DISTINCT from the display-side `compose_turns` (which drops the mirror and works in turns):
/// here we must keep the mirror bit so an in-place patch never silently de-mirrors the original.
pub fn compose_exif_orientation(current: u8, delta_turns: u8) -> u8 {
    let cur = if (1..=8).contains(&current) { current } else { 1 };
    let new_turns = ((turns_of(cur) + (delta_turns & 3)) & 3) as usize;
    if is_mirrored(cur) {
        MIRR_BY_TURNS[new_turns]
    } else {
        NONMIRR_BY_TURNS[new_turns]
    }
}

/// The non-mirrored EXIF `Orientation` representing `turns` clockwise quarter-turns (0→1, 1→6, 2→3,
/// 3→8). Falcon's in-memory base is mirror-dropped TURNS, so this reconstructs the CAS `expected` value
/// for the JPEG in-place path; a genuinely mirrored file's fresh EXIF won't match this representative,
/// so it (safely) falls to the sidecar path instead of an in-place patch.
pub fn turns_to_orientation(turns: u8) -> u8 {
    NONMIRR_BY_TURNS[(turns & 3) as usize]
}

/// v0.8.102: [`compose_exif_orientation`]'s INVERSE in the turn argument — the same value with
/// `turns` clockwise quarter-turns REMOVED, mirror class preserved (`minus(o, δ)` ≡
/// `compose(o, 4 - δ)`, spelled out because "subtract the turns the decoder already made" is what
/// the two call sites mean and reads wrong as an addition).
///
/// This is the arithmetic that translates a FILE-ABSOLUTE orientation (what an EXIF tag or an XMP
/// `tiff:Orientation` states — see [`apply_rotation`]'s convention note) into the RESIDUAL terms
/// Falcon's display base speaks after v0.8.101's S4, and it is used on both sides of that boundary:
/// `read_orientation` subtracts on the way in, `apply_rotation` subtracts to report `new_base_turns`.
/// A value outside 1..=8 is treated as 1, exactly like `compose_exif_orientation`.
pub fn orientation_minus_turns(current: u8, turns: u8) -> u8 {
    let cur = if (1..=8).contains(&current) { current } else { 1 };
    let new_turns = ((turns_of(cur) + 4 - (turns & 3)) & 3) as usize;
    if is_mirrored(cur) {
        MIRR_BY_TURNS[new_turns]
    } else {
        NONMIRR_BY_TURNS[new_turns]
    }
}

// ───────────────────────────── the JPEG in-place 2-byte patch ─────────────────────────────

/// Read at most this many bytes to LOCATE the EXIF APP1 + IFD0 Orientation entry. Real files carry the
/// EXIF IFD in the first APP1 segment (itself ≤ 64 KB by the 2-byte segment length), immediately after
/// SOI + an optional small APP0/JFIF; 1 MB is a generous ceiling that still bounds a crafted preamble.
/// A file whose Orientation isn't locatable within this prefix falls to the sidecar path (never a
/// forced patch).
const JPEG_LOCATE_CAP: usize = 1024 * 1024;

/// Byte order of the embedded TIFF header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Endian {
    Little,
    Big,
}
impl Endian {
    fn u16(self, b: &[u8]) -> u16 {
        match self {
            Endian::Little => u16::from_le_bytes([b[0], b[1]]),
            Endian::Big => u16::from_be_bytes([b[0], b[1]]),
        }
    }
    fn u32(self, b: &[u8]) -> u32 {
        match self {
            Endian::Little => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            Endian::Big => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        }
    }
    fn enc16(self, v: u16) -> [u8; 2] {
        match self {
            Endian::Little => v.to_le_bytes(),
            Endian::Big => v.to_be_bytes(),
        }
    }
}

/// A located Orientation SHORT: the ABSOLUTE file offset of its 2-byte value field + the endianness to
/// read/write it. The value itself is deliberately NOT carried — the patch re-reads it FRESH from disk
/// at write time (contract item 1), never trusting the locate buffer's copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OrientLoc {
    value_offset: u64,
    endian: Endian,
}

/// Why a JPEG couldn't be located/patched in place — every arm routes the caller to the sidecar path
/// or a clean refusal, never a blind write (contract items 1, 3; tests T-c/T-d/T-e/T-f).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocateErr {
    /// No APP1 segment carrying the `Exif\0\0` header (or the JPEG SOI is missing) → sidecar.
    NoExifApp1,
    /// The TIFF header inside APP1 is malformed (bad byte-order mark or magic) → refuse/sidecar.
    BadTiff,
    /// The IFD0 walk completed but carries no `Orientation` (0x0112) entry → sidecar.
    NoOrientation,
    /// An `Orientation` entry exists but is structurally wrong (not a 1-count SHORT, or its computed
    /// value offset lies outside the buffer) → REFUSE (never patch a neighbouring field). Tests T-c/T-e.
    MalformedOrientation,
    /// The buffer/file ends before a field the walk needs (a truncated JPEG) → refuse, no OOB. Test T-f.
    Truncated,
}

/// Locate the IFD0 `Orientation` SHORT inside a JPEG PREFIX buffer, returning the ABSOLUTE file offset
/// of its 2-byte value + endianness + current value. Pure (operates on an in-memory prefix) so the
/// endianness / IFD-walk / bounds math is unit-testable without a real file (tests T-a/T-b/T-c/T-f).
/// Every out-of-range index is checked — a truncated or crafted buffer yields an `Err`, never a panic.
fn locate_jpeg_orientation(buf: &[u8]) -> Result<OrientLoc, LocateErr> {
    // SOI.
    if buf.len() < 2 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return Err(LocateErr::NoExifApp1);
    }
    // Walk marker segments until the EXIF APP1 (skip APP0/JFIF, other APPn, etc.). Each segment is
    // FF <marker> <len:2 big-endian> <payload len-2>. SOI/EOI/RSTn have no length; a scan start (SOS,
    // 0xDA) means no EXIF ahead → stop.
    let mut i = 2usize;
    loop {
        if i + 4 > buf.len() {
            return Err(LocateErr::NoExifApp1); // ran off the prefix without finding EXIF
        }
        if buf[i] != 0xFF {
            return Err(LocateErr::NoExifApp1); // not a marker where one is required
        }
        let marker = buf[i + 1];
        if marker == 0xD9 || marker == 0xDA {
            return Err(LocateErr::NoExifApp1); // EOI / start-of-scan: no EXIF
        }
        let seg_len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        if seg_len < 2 {
            return Err(LocateErr::BadTiff); // impossible segment length
        }
        let payload_start = i + 4;
        let payload_end = i + 2 + seg_len;
        if payload_end > buf.len() {
            return Err(LocateErr::Truncated);
        }
        if marker == 0xE1 && buf[payload_start..].starts_with(b"Exif\x00\x00") {
            return locate_in_tiff(buf, payload_start + 6, payload_end);
        }
        i = payload_end; // next segment
    }
}

/// Given the absolute offset of the TIFF header (just past `Exif\0\0`) and the APP1 payload end, parse
/// the byte order + IFD0 and find the Orientation entry. All indexing is bounds-checked against
/// `tiff_end` (the APP1 boundary — TIFF offsets are relative to `tiff_start` and must stay inside it).
fn locate_in_tiff(buf: &[u8], tiff_start: usize, tiff_end: usize) -> Result<OrientLoc, LocateErr> {
    if tiff_start + 8 > tiff_end || tiff_end > buf.len() {
        return Err(LocateErr::Truncated);
    }
    let endian = match &buf[tiff_start..tiff_start + 2] {
        b"II" => Endian::Little,
        b"MM" => Endian::Big,
        _ => return Err(LocateErr::BadTiff),
    };
    if endian.u16(&buf[tiff_start + 2..tiff_start + 4]) != 42 {
        return Err(LocateErr::BadTiff); // TIFF magic
    }
    let ifd0_rel = endian.u32(&buf[tiff_start + 4..tiff_start + 8]) as usize;
    let ifd0 = tiff_start
        .checked_add(ifd0_rel)
        .ok_or(LocateErr::MalformedOrientation)?;
    if ifd0 + 2 > tiff_end {
        return Err(LocateErr::Truncated);
    }
    let count = endian.u16(&buf[ifd0..ifd0 + 2]) as usize;
    // Each entry is 12 bytes: tag(2) type(2) count(4) value/offset(4).
    let entries_start = ifd0 + 2;
    if entries_start + count * 12 > tiff_end {
        return Err(LocateErr::Truncated);
    }
    for e in 0..count {
        let eo = entries_start + e * 12;
        let tag = endian.u16(&buf[eo..eo + 2]);
        if tag != 0x0112 {
            continue;
        }
        // Orientation MUST be a single SHORT (type 3, count 1), stored INLINE in the first 2 bytes of
        // the 4-byte value field. Anything else (a hand-crafted offset-style / wrong-type entry) →
        // REFUSE, never patch a neighbour (contract item 1 / tests T-c, T-e).
        let typ = endian.u16(&buf[eo + 2..eo + 4]);
        let cnt = endian.u32(&buf[eo + 4..eo + 8]);
        if typ != 3 || cnt != 1 {
            return Err(LocateErr::MalformedOrientation);
        }
        let value_field = eo + 8; // inline value: first 2 bytes of the value field
        // (value_field + 2 ≤ tiff_end is already guaranteed by the entries bound above.)
        return Ok(OrientLoc { value_offset: value_field as u64, endian });
    }
    Err(LocateErr::NoOrientation)
}

/// The outcome of a JPEG in-place patch attempt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JpegPatch {
    /// The 2 bytes were written and read back == target.
    Patched,
    /// The file was ALREADY at target — no bytes written (idempotent crash-recovery / re-apply).
    AlreadyTarget,
}

/// A JPEG in-place patch could not be performed.
#[derive(Debug)]
pub enum PatchErr {
    /// Structural — the caller should try the sidecar path (or refuse). Carries the locate reason.
    Locate(LocateErr),
    /// The fresh 2-byte value matched NEITHER `expected` NOR `target` — the file changed under us or the
    /// caller's belief is stale. REFUSE (contract item 1); the caller falls back to a sidecar.
    Cas { expected: u8, target: u8, found: u16 },
    /// The post-write read-back did not equal target (a partial/failed write) — REFUSE, reported failure.
    Verify { target: u8, found: Option<u32> },
    /// An OS I/O error (permissions, locked file, read-only SD card, …).
    Io(std::io::Error),
}
impl std::fmt::Display for PatchErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatchErr::Locate(e) => write!(f, "no in-place EXIF Orientation ({e:?})"),
            PatchErr::Cas { expected, target, found } => {
                write!(f, "CAS refused (expected {expected} or {target}, file has {found})")
            }
            PatchErr::Verify { target, found } => {
                write!(f, "read-back {found:?} != target {target}")
            }
            PatchErr::Io(e) => write!(f, "io: {e}"),
        }
    }
}
impl From<std::io::Error> for PatchErr {
    fn from(e: std::io::Error) -> PatchErr {
        PatchErr::Io(e)
    }
}

/// Locate-verify-write-verify a JPEG's EXIF `Orientation`, in place, 2 bytes only. Opens the file ONCE
/// read+write (no reopen TOCTOU), reads a bounded prefix to LOCATE the value offset FROM SCRATCH, then
/// compare-and-swaps: the freshly-re-read 2-byte value must equal `target` (→ already-applied, no write)
/// or `expected` (→ write `target`), else refuse. After writing it flushes to disk and reads the value
/// back through the NORMAL bounded EXIF reader (`exif_orientation`) and asserts == target. `expected` /
/// `target` are EXIF values (1..=8); the caller computes `target = compose_exif_orientation(expected,
/// delta)`. A truncated/anomalous/tagless JPEG returns `Err(Locate(..))` and NEVER writes a byte.
pub fn patch_jpeg_orientation(path: &Path, expected: u8, target: u8) -> Result<JpegPatch, PatchErr> {
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let len = f.metadata()?.len();
    // Read the locate prefix from the SAME handle we will write through.
    let cap = JPEG_LOCATE_CAP.min(len as usize);
    let mut buf = vec![0u8; cap];
    f.seek(SeekFrom::Start(0))?;
    read_full(&mut f, &mut buf)?;
    let loc = locate_jpeg_orientation(&buf).map_err(PatchErr::Locate)?;
    // Re-read the 2 value bytes FRESH from disk at the located offset (never trust the prefix copy for
    // the swap decision — the contract's "read the current 2-byte value fresh").
    f.seek(SeekFrom::Start(loc.value_offset))?;
    let mut cur = [0u8; 2];
    read_full(&mut f, &mut cur)?;
    let fresh = loc.endian.u16(&cur);
    if fresh == target as u16 {
        return Ok(JpegPatch::AlreadyTarget); // idempotent — crash recovery / re-apply
    }
    if fresh != expected as u16 {
        return Err(PatchErr::Cas { expected, target, found: fresh });
    }
    // Write the 2 target bytes at the verified offset (cannot truncate — same 2-byte span).
    f.seek(SeekFrom::Start(loc.value_offset))?;
    f.write_all(&loc.endian.enc16(target as u16))?;
    f.flush()?;
    crate::file_io::sync_file(&f)?; // durability: the value must survive a crash before the read-back / delta clear
    drop(f); // close before the normal reader re-opens it
    // Read-back through the NORMAL bounded path (contract item 2 / the A1 bounded-read discipline).
    match exif_orientation(path) {
        Some(v) if v == target as u32 => Ok(JpegPatch::Patched),
        other => Err(PatchErr::Verify { target, found: other }),
    }
}

/// `read_exact`-equivalent that treats a short read as `UnexpectedEof` (a truncated file → clean Err,
/// never a partial/garbage parse). `std::io::Read::read_exact` already does this; wrapped for clarity.
fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<()> {
    f.read_exact(buf)
}

// ───────────────────────────── the XMP sidecar (create-fresh + surgical update) ─────────────────────────────

/// Cap on reading an EXISTING sidecar before a surgical update (contract item 4 — a folder-planted
/// multi-GB `.xmp` must not OOM the worker). Same 4 MB idiom as the EXIF / ICCP metadata caps.
const XMP_READ_CAP: u64 = 4 * 1024 * 1024;

/// What a sidecar write did (for the report/log).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SidecarKind {
    /// No sidecar existed → wrote a fresh minimal xpacket.
    CreatedFresh,
    /// An existing sidecar's `tiff:Orientation` was surgically set to target (value changed).
    Updated,
    /// An existing sidecar already carried `tiff:Orientation == target` → no write.
    AlreadyTarget,
}

/// A sidecar write failed in a way that must NOT corrupt a foreign file.
#[derive(Debug)]
pub enum SidecarErr {
    /// An existing sidecar's structure/property/namespace was unrecognised — refuse to rewrite it
    /// (it may be a foreign file we don't understand); report failure, keep the delta.
    Unrecognised,
    /// The existing sidecar exceeded [`XMP_READ_CAP`] — refuse to read/modify it.
    TooLarge,
    /// The existing sidecar is not valid UTF-8 — refuse rather than lossily transcode it to U+FFFD and
    /// clobber the foreign bytes (the byte-for-byte-preservation contract). Report failure, keep the delta.
    NotUtf8,
    Io(std::io::Error),
}
impl std::fmt::Display for SidecarErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SidecarErr::Unrecognised => write!(f, "existing sidecar structure unrecognised — not modified"),
            SidecarErr::TooLarge => write!(f, "existing sidecar exceeds the 4 MB read cap — not modified"),
            SidecarErr::NotUtf8 => write!(f, "existing sidecar is not valid UTF-8 — not modified"),
            SidecarErr::Io(e) => write!(f, "io: {e}"),
        }
    }
}
impl From<std::io::Error> for SidecarErr {
    fn from(e: std::io::Error) -> SidecarErr {
        SidecarErr::Io(e)
    }
}

/// The minimal Adobe XMP xpacket for a fresh sidecar carrying just `tiff:Orientation`. Shape matches the
/// standard `<?xpacket begin="\u{feff}" …?> … <?xpacket end="w"?>` wrapper (the probe's xpacket shape),
/// with the UTF-8 BOM inside the `begin` attribute. `x:xmptk` credits Falcon.
fn fresh_xmp(target: u8) -> String {
    format!(
        "\u{feff}<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Falcon\">\n\
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
  <rdf:Description rdf:about=\"\"\n\
    xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\"\n\
   tiff:Orientation=\"{target}\"/>\n\
 </rdf:RDF>\n\
</x:xmpmeta>\n\
<?xpacket end=\"w\"?>"
    )
}

/// Surgically set `tiff:Orientation` to `target` in an EXISTING sidecar's text, PRESERVING everything
/// else byte-for-byte (no XML parser — a pure text find/replace/insert, so hostile entities/DTDs in a
/// planted file are never expanded). Returns the new text + whether the value actually changed, or
/// `None` when the structure is unrecognised (no `<rdf:Description`) so the caller refuses rather than
/// clobbering foreign data. Handles: attribute form `tiff:Orientation="N"` / `'N'`, element form
/// `<tiff:Orientation>N</tiff:Orientation>`, and INSERTION (+ the `xmlns:tiff` decl if missing) when the
/// tag is absent. An existing property in an unsupported form is refused, never duplicated. Namespace
/// insertion is scoped to the target opening tag. Tests T-h/F1 pin preservation and refusal through the
/// actual file writer.
fn xmp_surgical_set(existing: &str, target: u8) -> Option<(String, bool)> {
    let tgt = target.to_string();
    // 1) attribute form: tiff:Orientation="…" or '…'
    if let Some(a) = existing.find("tiff:Orientation=") {
        let after = a + "tiff:Orientation=".len();
        let bytes = existing.as_bytes();
        let quote = *bytes.get(after)?;
        if quote == b'"' || quote == b'\'' {
            let vstart = after + 1;
            let vend = vstart + existing[vstart..].find(quote as char)?;
            if existing[vstart..vend] == tgt {
                return Some((existing.to_string(), false));
            }
            let mut out = String::with_capacity(existing.len() + 2);
            out.push_str(&existing[..vstart]);
            out.push_str(&tgt);
            out.push_str(&existing[vend..]);
            return Some((out, true));
        }
    }
    // 2) element form: <tiff:Orientation>N</tiff:Orientation>
    if let Some(o) = existing.find("<tiff:Orientation>") {
        let vstart = o + "<tiff:Orientation>".len();
        let close = "</tiff:Orientation>";
        let vend = vstart + existing[vstart..].find(close)?;
        if existing[vstart..vend] == tgt {
            return Some((existing.to_string(), false));
        }
        let mut out = String::with_capacity(existing.len() + 2);
        out.push_str(&existing[..vstart]);
        out.push_str(&tgt);
        out.push_str(&existing[vend..]);
        return Some((out, true));
    }
    // A property can be present without matching either supported locate form, e.g. the valid XML
    // `tiff:Orientation = "1"`. As with the rating writer, refuse instead of adding a duplicate and
    // corrupting foreign XML. The failure propagates through apply_rotation so its delta stays pending.
    if mentions_tiff_orientation(existing) {
        return None;
    }
    // 3) absent → INSERT into the first <rdf:Description …>. Only that opening tag's xmlns:tiff can
    // suppress our declaration: a binding on a later sibling is not in scope here. A new local binding
    // must not change existing foreign tiff-prefixed names inherited from an ancestor.
    let (name_end, has_tiff_ns) = tiff_insertion_context(existing)?;
    let mut inject = String::new();
    if !has_tiff_ns {
        if !tiff_namespace_redeclaration_safe(existing) {
            return None;
        }
        inject.push_str(" xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\"");
    }
    inject.push_str(&format!(" tiff:Orientation=\"{tgt}\""));
    let mut out = String::with_capacity(existing.len() + inject.len());
    out.push_str(&existing[..name_end]);
    out.push_str(&inject);
    out.push_str(&existing[name_end..]);
    Some((out, true))
}

/// The rotation twin of `mentions_xmp_rating`: detect unsupported forms without confusing a longer
/// property name (such as `tiff:OrientationExtra`) with the property we own. A mention in foreign text
/// can conservatively refuse an insertion; it must never cause us to rewrite that text.
fn mentions_tiff_orientation(s: &str) -> bool {
    const NAME: &str = "tiff:Orientation";
    s.match_indices(NAME).any(|(at, _)| {
        !s[at + NAME.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    })
}

/// Adding a local TIFF binding can change the meaning of existing tiff-prefixed attributes or
/// descendants if that prefix currently inherits a foreign URI. Without a full XML scope parser,
/// require every apparent xmlns:tiff declaration in the packet to name the expected URI before
/// adding a binding. This deliberately also refuses a conflicting unrelated sibling or a declaration
/// mentioned in foreign text; preserving an unsupported packet is safer than rebinding its data.
/// A target that already has the correct local binding does not need this conservative check.
fn tiff_namespace_redeclaration_safe(existing: &str) -> bool {
    const NAME: &str = "xmlns:tiff";
    let xml_space = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    existing.match_indices(NAME).all(|(at, _)| {
        let tail = &existing[at + NAME.len()..];
        // A longer namespace name, including a non-ASCII suffix, is unrelated to xmlns:tiff.
        if !tail.chars().next().is_some_and(|c| xml_space(c) || c == '=') {
            return true;
        }
        let Some(tail) = tail.trim_start_matches(xml_space).strip_prefix('=') else { return false; };
        let tail = tail.trim_start_matches(xml_space);
        let Some(quote) = tail.chars().next() else { return false; };
        if quote != '\'' && quote != '"' { return false; }
        let value = &tail[1..];
        let Some(end) = value.find(quote) else { return false; };
        // Do not expand entities or guess at equivalent URI spellings in a foreign packet.
        &value[..end] == "http://ns.adobe.com/tiff/1.0/"
    })
}

/// Find the rotation insertion point and whether that Description itself binds the TIFF namespace.
/// Inspect only the opening tag's quoted attributes; do not parse/expand XML entities or reserialize
/// the packet. XML whitespace around '=' and '>' inside a quoted foreign value are legal and must not
/// hide a namespace, otherwise injecting another xmlns:tiff would itself corrupt valid input.
fn tiff_insertion_context(existing: &str) -> Option<(usize, bool)> {
    const TAG: &str = "<rdf:Description";
    let name_end = existing.find(TAG)? + TAG.len();
    let mut tail = &existing[name_end..];
    let mut has_tiff_ns = false;
    let xml_space = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    loop {
        let before = tail.len();
        tail = tail.trim_start_matches(xml_space);
        if tail.starts_with('>') || tail.starts_with("/>") {
            return Some((name_end, has_tiff_ns));
        }
        // Attributes require a separating space; this also rejects rdf:DescriptionExtra and a
        // missing '>' without reaching into a sibling's opening tag.
        if tail.len() == before {
            return None;
        }
        let attr_end = tail.find(|c: char| xml_space(c) || c == '=')?;
        let attr = &tail[..attr_end];
        if attr.is_empty() || attr.contains(['<', '>', '/', '\'', '"']) {
            return None;
        }
        tail = tail[attr_end..].trim_start_matches(xml_space);
        tail = tail.strip_prefix('=')?.trim_start_matches(xml_space);
        let quote = tail.chars().next()?;
        if quote != '\'' && quote != '"' {
            return None;
        }
        tail = &tail[1..];
        let value_end = tail.find(quote)?;
        let value = &tail[..value_end];
        if value.contains('<') {
            return None;
        }
        if attr == "xmlns:tiff" {
            if has_tiff_ns || value != "http://ns.adobe.com/tiff/1.0/" {
                return None;
            }
            has_tiff_ns = true;
        }
        tail = &tail[value_end + 1..];
    }
}

/// Synchronize a unique sibling temp before replacing an XMP sidecar. Runs on
/// the Apply worker and shares review JSON's macOS SMB synchronization fallback.
/// A failed stage removes only its own temp and keeps the previous target.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    crate::file_io::write_atomic(path, data)
}

/// Create or surgically-update `sidecar_path` so its `tiff:Orientation == target`. A missing sidecar
/// gets a fresh minimal xpacket; an existing one is read BOUNDED (4 MB) and text-surgically updated,
/// preserving all foreign content; an unrecognised existing structure is refused (never clobbered).
pub fn write_xmp_sidecar(sidecar_path: &Path, target: u8) -> Result<SidecarKind, SidecarErr> {
    match std::fs::metadata(sidecar_path) {
        Ok(m) if m.is_file() => {
            if m.len() > XMP_READ_CAP {
                return Err(SidecarErr::TooLarge);
            }
            let existing = read_bounded_string(sidecar_path, XMP_READ_CAP)?;
            match xmp_surgical_set(&existing, target) {
                Some((_, false)) => Ok(SidecarKind::AlreadyTarget),
                Some((new_text, true)) => {
                    write_atomic(sidecar_path, new_text.as_bytes())?;
                    Ok(SidecarKind::Updated)
                }
                None => Err(SidecarErr::Unrecognised),
            }
        }
        _ => {
            write_atomic(sidecar_path, fresh_xmp(target).as_bytes())?;
            Ok(SidecarKind::CreatedFresh)
        }
    }
}

// ───────────── the xmp:Rating sidecar (v0.8.69, E/H2 — the OUTPUT contract) ─────────────
//
// Ratings must be able to LEAVE the app so Lightroom / Bridge / competitors read them. The star rating
// is the Adobe basic-schema `xmp:Rating` (0..=5), NOT the `tiff:` namespace the rotation path uses — so
// this is a PARALLEL create/surgical-update path, deliberately kept SEPARATE from the security-reviewed
// `tiff:Orientation` code above (zero risk of regressing the rotation writer) while reusing its proven
// primitives verbatim: the bounded 4 MB read (`read_bounded_string`), the strict-UTF-8 refusal, the
// tmp+fsync+rename `write_atomic`, and the `SidecarKind`/`SidecarErr` vocabulary. The two schemas are
// byte-compatible: a rating write into an Orientation-only sidecar (or vice-versa) surgically INSERTS its
// own property + xmlns without disturbing the other, so a RAW's `basename.xmp` can carry both at once.
//
// A CLEARED rating writes `xmp:Rating="0"` (the standard "unrated") rather than deleting the property —
// so a previously-synced rating can never resurrect in another tool. In-file XMP (embedding into the JPEG
// itself) is deliberately OUT OF SCOPE this round: sidecar-only, even for JPEGs (a documented limitation;
// LR reads embedded XMP for a JPEG, not `NAME.JPG.xmp`, so a JPEG's rating sidecar is seen by digiKam /
// XnView / Bridge but not Lightroom — an in-file round is possible later).

/// The Adobe basic-schema namespace URI for `xmp:Rating`.
const XMP_BASIC_NS: &str = "http://ns.adobe.com/xap/1.0/";

/// The minimal Adobe XMP xpacket for a fresh sidecar carrying just `xmp:Rating` (0..=5). Shape mirrors
/// [`fresh_xmp`] (the `tiff:Orientation` template) exactly — same xpacket wrapper, same `x:xmptk="Falcon"`
/// credit — so the two are structurally identical apart from the property + its xmlns.
fn fresh_xmp_rating(rating: u8) -> String {
    format!(
        "\u{feff}<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Falcon\">\n\
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
  <rdf:Description rdf:about=\"\"\n\
    xmlns:xmp=\"{XMP_BASIC_NS}\"\n\
   xmp:Rating=\"{rating}\"/>\n\
 </rdf:RDF>\n\
</x:xmpmeta>\n\
<?xpacket end=\"w\"?>"
    )
}

/// Surgically set `xmp:Rating` to `rating` in an EXISTING sidecar's text, PRESERVING everything else
/// byte-for-byte (no XML parser — a pure text find/replace/insert, so hostile entities/DTDs in a planted
/// file are never expanded). The `xmp:Rating` twin of [`xmp_surgical_set`]. Returns the new text + whether
/// the value actually changed, or `None` when the structure is unrecognised (no `<rdf:Description`) so the
/// caller refuses rather than clobbering foreign data. Handles attribute form `xmp:Rating="N"` / `'N'`,
/// element form `<xmp:Rating>N</xmp:Rating>`, and INSERTION (+ the `xmlns:xmp` decl if missing) when the
/// tag is absent. The `xmp:Rating=` needle carries its `=` so it never matches `xmp:RatingPercent=`, and
/// the `xmlns:xmp=` presence test carries its `=` so it never mistakes `xmlns:xmpMM=` for the decl.
fn xmp_surgical_set_rating(existing: &str, rating: u8) -> Option<(String, bool)> {
    let val = rating.to_string();
    // 1) attribute form: xmp:Rating="…" or '…'
    if let Some(a) = existing.find("xmp:Rating=") {
        let after = a + "xmp:Rating=".len();
        let bytes = existing.as_bytes();
        let quote = *bytes.get(after)?;
        if quote == b'"' || quote == b'\'' {
            let vstart = after + 1;
            let vend = vstart + existing[vstart..].find(quote as char)?;
            if existing[vstart..vend] == val {
                return Some((existing.to_string(), false));
            }
            let mut out = String::with_capacity(existing.len() + 2);
            out.push_str(&existing[..vstart]);
            out.push_str(&val);
            out.push_str(&existing[vend..]);
            return Some((out, true));
        }
    }
    // 2) element form: <xmp:Rating>N</xmp:Rating>
    if let Some(o) = existing.find("<xmp:Rating>") {
        let vstart = o + "<xmp:Rating>".len();
        let close = "</xmp:Rating>";
        let vend = vstart + existing[vstart..].find(close)?;
        if existing[vstart..vend] == val {
            return Some((existing.to_string(), false));
        }
        let mut out = String::with_capacity(existing.len() + 2);
        out.push_str(&existing[..vstart]);
        out.push_str(&val);
        out.push_str(&existing[vend..]);
        return Some((out, true));
    }
    // v0.8.70 (FIX 3): the property may be PRESENT in a form neither locate matched — e.g.
    // `xmp:Rating = "3"` (whitespace around '='), an unquoted `xmp:Rating=3`, or a namespaced element
    // variant we don't parse. INSERTING here would create a DUPLICATE xmp:Rating. Refuse instead (skip +
    // the caller's failure logging). Safe refusal over clever parsing. `mentions_xmp_rating` ignores
    // `xmp:RatingPercent` (a different property), so a sidecar carrying only that still accepts an insert.
    if mentions_xmp_rating(existing) {
        return None;
    }
    // 3) absent → INSERT into the first <rdf:Description …>. Preserve everything; inject the attribute
    //    (and the xmp xmlns if the FIRST Description doesn't already declare it) right after the tag name.
    let desc = existing.find("<rdf:Description")?;
    let name_end = desc + "<rdf:Description".len();
    // v0.8.70 (FIX 2): scope the xmlns:xmp presence test to the FIRST rdf:Description's OPENING TAG span
    // only (the exact span we inject into). A GLOBAL substring test could see xmlns:xmp declared on a
    // LATER sibling Description and wrongly skip the decl here → a bare, undeclared xmp: prefix on the
    // first Description (namespace-malformed; other tools may drop the property or the whole packet). A
    // duplicate xmlns on a different element is legal XML, so injecting it whenever the first tag lacks it
    // is always safe. A tag with no closing '>' is malformed → refuse (never inject into broken markup).
    let tag_end = name_end + existing[name_end..].find('>')?;
    let has_xmp_ns = existing[desc..tag_end].contains("xmlns:xmp=");
    let mut inject = String::new();
    if !has_xmp_ns {
        inject.push_str(&format!(" xmlns:xmp=\"{XMP_BASIC_NS}\""));
    }
    inject.push_str(&format!(" xmp:Rating=\"{val}\""));
    let mut out = String::with_capacity(existing.len() + inject.len());
    out.push_str(&existing[..name_end]);
    out.push_str(&inject);
    out.push_str(&existing[name_end..]);
    Some((out, true))
}

/// True when the text mentions the `xmp:Rating` PROPERTY in any form (attribute or element, any spacing),
/// but NOT `xmp:RatingPercent` / `xmp:RatingFoo` (a different property that merely shares the prefix). Used
/// by [`xmp_surgical_set_rating`]'s FIX-3 refusal: if the two exact locate forms missed yet the property is
/// present, refuse rather than duplicate it. An occurrence counts only when the char AFTER `xmp:Rating` is
/// not an XML-name continuation char (letter/digit/`_`/`-`/`.`) — so `xmp:RatingPercent="20"` never trips it.
fn mentions_xmp_rating(s: &str) -> bool {
    let needle = "xmp:Rating";
    let mut start = 0;
    while let Some(rel) = s[start..].find(needle) {
        let after = start + rel + needle.len();
        let continues_name = s[after..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
        if !continues_name {
            return true;
        }
        start = after;
    }
    false
}

/// Read the `xmp:Rating` raw value string from an existing sidecar (the READ twin of the write path's two
/// locate forms — attribute `="N"`/`'N'`, element `<xmp:Rating>N</…>`). Bounded 4 MB read; a missing /
/// oversized / unreadable / non-UTF-8 sidecar, or one with no recognisable `xmp:Rating`, → `None`. The
/// `xmp:Rating=` needle carries its `=` so it never matches `xmp:RatingPercent=`. Used by the backfill
/// 0-correction to detect a STALE non-zero rating on a now-unrated photo's already-written sidecar (so it
/// is cleared to `0` in place) — it NEVER creates a sidecar, so a never-rated photo stays sidecar-less.
pub fn sidecar_rating(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > XMP_READ_CAP {
        return None;
    }
    xmp_read_rating(&read_bounded_string(path, XMP_READ_CAP).ok()?)
}

/// Extract the `xmp:Rating` raw value (trimmed) from sidecar text — the READ-ONLY twin of
/// [`xmp_surgical_set_rating`]'s two locate forms. Attribute form first, then element; any absent /
/// malformed structure → `None`. Value semantics (0 vs -1 vs a real star) are the caller's to interpret.
fn xmp_read_rating(existing: &str) -> Option<String> {
    // 1) attribute form: xmp:Rating="…" / '…'
    if let Some(a) = existing.find("xmp:Rating=") {
        let after = a + "xmp:Rating=".len();
        if let Some(&quote) = existing.as_bytes().get(after) {
            if quote == b'"' || quote == b'\'' {
                let vstart = after + 1;
                if let Some(rel) = existing[vstart..].find(quote as char) {
                    return Some(existing[vstart..vstart + rel].trim().to_string());
                }
            }
        }
    }
    // 2) element form: <xmp:Rating>N</xmp:Rating>
    if let Some(o) = existing.find("<xmp:Rating>") {
        let vstart = o + "<xmp:Rating>".len();
        if let Some(rel) = existing[vstart..].find("</xmp:Rating>") {
            return Some(existing[vstart..vstart + rel].trim().to_string());
        }
    }
    None
}

/// Create or surgically-update `sidecar_path` so its `xmp:Rating == rating` (0..=5; a cleared rating is
/// `0`). A missing sidecar gets a fresh minimal xpacket; an existing one is read BOUNDED (4 MB) and
/// text-surgically updated, preserving all foreign content (a rotation `tiff:Orientation` written earlier
/// survives byte-for-byte); an unrecognised existing structure is refused (never clobbered). Mirrors
/// [`write_xmp_sidecar`] exactly but for the rating property. The write is a durable tmp+fsync+rename.
pub fn write_xmp_rating_sidecar(sidecar_path: &Path, rating: u8) -> Result<SidecarKind, SidecarErr> {
    match std::fs::metadata(sidecar_path) {
        Ok(m) if m.is_file() => {
            if m.len() > XMP_READ_CAP {
                return Err(SidecarErr::TooLarge);
            }
            let existing = read_bounded_string(sidecar_path, XMP_READ_CAP)?;
            match xmp_surgical_set_rating(&existing, rating) {
                Some((_, false)) => Ok(SidecarKind::AlreadyTarget),
                Some((new_text, true)) => {
                    write_atomic(sidecar_path, new_text.as_bytes())?;
                    Ok(SidecarKind::Updated)
                }
                None => Err(SidecarErr::Unrecognised),
            }
        }
        _ => {
            write_atomic(sidecar_path, fresh_xmp_rating(rating).as_bytes())?;
            Ok(SidecarKind::CreatedFresh)
        }
    }
}

/// Read at most `cap` bytes of a text file and decode it as STRICT UTF-8. The `take` caps the allocation
/// exactly as `read_exif_bounded` does for EXIF. A non-UTF-8 file returns [`SidecarErr::NotUtf8`] rather
/// than a lossy `from_utf8_lossy` transcode — writing that lossy string back would rewrite any foreign
/// non-UTF-8 bytes (a Latin-1 `0xE9`, a Windows-1252 smart-quote) as U+FFFD, violating the byte-for-byte
/// preservation contract. Files WITHIN the cap are read whole (the caller refuses oversized ones first),
/// so a multi-byte char is never split at the boundary → no false NotUtf8 on a legitimate sidecar.
fn read_bounded_string(path: &Path, cap: u64) -> Result<String, SidecarErr> {
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.take(cap).read_to_end(&mut buf)?;
    // v0.8.152 (R3-L7) — the TOCTOU that could TRUNCATE a foreign file, closed.
    //
    // Both writers `fs::metadata()` the sidecar and refuse it over the cap, then read it here. If
    // the file GREW past the cap between the stat and this read, `take(cap)` silently handed back a
    // 4 MB PREFIX; `xmp_surgical_set` only needs to find `<rdf:Description`, so it succeeded against
    // the prefix, and `write_atomic` then wrote that prefix over the whole file. Everything past
    // 4 MB — foreign namespaces this module's own contract promises to preserve "byte-for-byte" —
    // was gone. Many grow cases were caught incidentally by the strict-UTF-8 refusal splitting a
    // multi-byte char, which is luck, not a guard.
    //
    // `>=`, not `>`: reading exactly `cap` bytes is indistinguishable from reading the first `cap`
    // bytes of something longer, and a file that is exactly at the cap is one the callers' `>` test
    // was already within one byte of refusing.
    //
    // v0.8.153 (skeptic A / Y7) — YES, THIS DELIBERATELY DISAGREES WITH THE CONFIG CAPS. Falcon's
    // other size guard, `support::over_config_cap`, is `len > CONFIG_MAX_BYTES`, and its own test
    // says "exactly at the cap is allowed". The two boundaries differ because the two questions do.
    // `over_config_cap` is given the file's LENGTH by `fs::metadata`, so "exactly at the cap" is a
    // fact it knows. This function is given only what a capped read returned, and `take(cap)`
    // reports `cap` bytes for a `cap`-byte file and for a 2 GB one alike — so `>=` is the only
    // honest boundary HERE. Losing one legal 4 MB-exactly sidecar is the price of never silently
    // truncating a larger one, and a real sidecar is kilobytes.
    if buf.len() as u64 >= cap {
        return Err(SidecarErr::TooLarge);
    }
    match std::str::from_utf8(&buf) {
        Ok(s) => Ok(s.to_string()),
        Err(_) => Err(SidecarErr::NotUtf8),
    }
}

// ───────────────────────────── the sidecar READ-BACK (item 9 / round-trip) ─────────────────────────────

/// Falcon's applied-rotation READ-BACK (findings #3/#9): consult the shot's own XMP sidecar so an applied
/// rotation round-trips through the app's own reader. Adobe convention — the sidecar WINS over the file's
/// embedded orientation. Naming MIRRORS the write side EXACTLY and never crosses: a RAW reads
/// `basename.xmp` ONLY; a finished file reads `fullname.xmp` ONLY (a RAW+JPG pair's two sidecars are
/// written separately and must never orient each other). Bounded 4 MB read; a missing / oversized /
/// unreadable / non-UTF-8 / malformed sidecar → `None` (the caller falls through to the embedded read).
pub fn sidecar_orientation(path: &Path, is_raw: bool) -> Option<u8> {
    let sc = sidecar_path_for(path, is_raw);
    let meta = std::fs::metadata(&sc).ok()?;
    if !meta.is_file() || meta.len() > XMP_READ_CAP {
        return None;
    }
    xmp_read_orientation(&read_bounded_string(&sc, XMP_READ_CAP).ok()?)
}

/// Extract `tiff:Orientation` (1..=8) from an existing sidecar's text — the READ-ONLY twin of
/// `xmp_surgical_set`'s two locate forms (attribute `="N"` / `'N'`, or element `<tiff:Orientation>N</…>`);
/// any absent / malformed / out-of-range value → `None`. Falls through attribute→element so a malformed
/// attribute never masks a valid element.
fn xmp_read_orientation(existing: &str) -> Option<u8> {
    // 1) attribute form: tiff:Orientation="…" / '…'
    if let Some(a) = existing.find("tiff:Orientation=") {
        let after = a + "tiff:Orientation=".len();
        if let Some(&quote) = existing.as_bytes().get(after) {
            if quote == b'"' || quote == b'\'' {
                let vstart = after + 1;
                if let Some(rel) = existing[vstart..].find(quote as char) {
                    if let Some(n) = parse_orient(&existing[vstart..vstart + rel]) {
                        return Some(n);
                    }
                }
            }
        }
    }
    // 2) element form: <tiff:Orientation>N</tiff:Orientation>
    if let Some(o) = existing.find("<tiff:Orientation>") {
        let vstart = o + "<tiff:Orientation>".len();
        if let Some(rel) = existing[vstart..].find("</tiff:Orientation>") {
            if let Some(n) = parse_orient(&existing[vstart..vstart + rel]) {
                return Some(n);
            }
        }
    }
    None
}

/// Parse a trimmed orientation string to a valid EXIF value (1..=8), else `None`.
fn parse_orient(s: &str) -> Option<u8> {
    match s.trim().parse::<u8>() {
        Ok(n) if (1..=8).contains(&n) => Some(n),
        _ => None,
    }
}

/// The CURRENT orientation a sidecar UPDATE must compose onto (findings #1). The sidecar WINS over the
/// file's immutable embedded orientation, so a SECOND Apply advances on the value the FIRST Apply wrote —
/// the embedded EXIF/RAW never changes on the sidecar route, so composing on it silently dropped prior
/// turns. Falls back to the file's embedded EXIF/RAW orientation, then upright.
fn current_for_sidecar(path: &Path, is_raw: bool) -> u8 {
    sidecar_orientation(path, is_raw)
        .or_else(|| {
            if is_raw {
                crate::raw_orientation(path).map(|o| o as u8)
            } else {
                exif_orientation(path).map(|o| o as u8)
            }
        })
        .unwrap_or(1)
}

// ───────────────────────────── the per-shot orchestrator ─────────────────────────────

/// What one file SIDE (the finished image, or the RAW) did during apply.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SideAction {
    /// This side wasn't present for the shot (e.g. a JPEG-only shot has no RAW side).
    Skipped,
    /// JPEG in-place 2-byte patch written.
    Patched,
    /// The file was already at target (in-place OR sidecar) — no bytes written.
    AlreadyTarget,
    /// An XMP sidecar was created fresh.
    SidecarCreated,
    /// An existing XMP sidecar was surgically updated.
    SidecarUpdated,
    /// This side FAILED (the shot keeps its delta). Carries a short reason for the log.
    Failed(String),
}
impl SideAction {
    fn ok(&self) -> bool {
        !matches!(self, SideAction::Failed(_))
    }
}

/// The plan for applying one shot's rotation to disk — built by the caller from the shot + its delta.
pub struct RotApplyPlan {
    /// The finished-image path (JPG/PNG/TIFF/WebP/HEIC), or `None` for a RAW-only shot.
    pub finished: Option<PathBuf>,
    /// True when `finished` is a JPEG (the ONLY in-place-patchable format; others → sidecar).
    pub finished_is_jpeg: bool,
    /// The RAW path (CR3, …), or `None` for a finished-only shot. Always sidecar (never in-place).
    pub raw: Option<PathBuf>,
    /// The app's believed base orientation TURNS of the finished side — the in-place CAS `expected`
    /// (reconstructed to a non-mirrored EXIF value; a mirrored file won't match → sidecar fallback).
    ///
    /// v0.8.102: this is the RESIDUAL base (what `read_orientation` returns), i.e. the turns still
    /// OUTSTANDING after the platform decoder had its say — 0 for a portrait iPhone HEIC whose
    /// container rotation WIC already baked in, even though the file's own tag says 6.
    ///
    /// v0.8.103 (V1): this field is also the *only* input the FILE-ABSOLUTE → RESIDUAL translation
    /// needs. The turns the platform decoder already consumed are `turns_of(cur) - base_turns` for
    /// whatever absolute `cur` an arm composes on, so `apply_rotation` derives them instead of taking
    /// them (v0.8.102 carried a `decoder_consumed_turns` field here, filled by re-probing the decoder
    /// on the apply worker — see this struct's history in `apply_rotation`'s `residual_turns` note).
    pub base_turns: u8,
    /// The manual clockwise quarter-turn delta to bake in (1..=3).
    pub delta: u8,
}

/// The result of applying one shot: whether every attempted side succeeded, the new finished-side base
/// TURNS (for the caller's `note_base`, so the display stays a no-op under auto-orient), and per-side
/// actions for the log.
pub struct RotApplyReport {
    pub ok: bool,
    /// v0.8.102: in RESIDUAL terms — the SAME units as [`RotApplyPlan::base_turns`] and as what
    /// `read_orientation` will return on the next read, NOT the turns of the value written to disk.
    /// The contract the caller's no-op invariant rests on: `new_base_turns == (plan.base_turns +
    /// plan.delta) & 3`.
    ///
    /// v0.8.103 (V1): on every FINISHED side that contract is now arithmetically forced rather than
    /// arrived at — see `residual_turns` in [`apply_rotation`]'s body. (A RAW-ONLY shot is the one
    /// exception: it has no finished side, and its base comes from the RAW's own fresh value.)
    pub new_base_turns: u8,
    pub finished_action: SideAction,
    pub raw_action: SideAction,
}

/// Apply one shot's rotation delta to disk per the write-safety contract. JPEG finished side → in-place
/// 2-byte CAS patch, falling back to a `<name>.<ext>.xmp` sidecar on any structural anomaly; non-JPEG
/// finished side → that sidecar directly; RAW side → a `<basename>.xmp` sidecar (composed on the RAW's
/// OWN fresh embedded orientation — item 9). A side that fails leaves `ok=false` so the caller keeps the
/// delta. All file reads are bounded; the JPEG write is a verified 2-byte patch; sidecars are tmp+rename.
///
/// # THE SIDECAR CONVENTION (v0.8.102, decided by the F1/F2 RED)
///
/// **A Falcon sidecar's `tiff:Orientation` is FILE-ABSOLUTE: it means exactly what the file's own
/// embedded EXIF `Orientation` tag would mean, and it is composed on that tag.** It does NOT mean
/// "the turn still outstanding after the decoder honoured the container's `irot`".
///
/// The reason is about files Falcon did not write: every sidecar written by a pre-v0.8.102 Falcon
/// already carries the absolute value, so this needs no migration and no marker key to tell the two
/// dialects apart — a marker key would be a second thing to get wrong, and an unmarked foreign
/// sidecar would still have to be guessed at.
///
/// **v0.8.103 (V6): the fork is not interop-free either way, and this is the honest statement of it.**
/// EITHER half diverges for some foreign reader — the absolute value is DOUBLE-COUNTED by a reader
/// that honours both the container's `irot` and the sidecar (after one R + Apply on IMG_2814 Falcon
/// writes `tiff:Orientation=3` beside a file whose `irot` already delivers the first quarter-turn, so
/// such a reader renders 270° where Falcon renders 90°), and the residual value is MISREAD by a
/// reader that does not honour `irot`. Absolute is chosen because it needs no migration and no marker
/// key. (v0.8.102 gave a second reason — that Apple/Adobe-style tools restate the absolute value too
/// — which nothing in this tree establishes and which read as though the fork had no external cost.)
///
/// The internal cost is that this side of the contract and Falcon's DISPLAY base speak different
/// coordinate systems after v0.8.101's S4 (which made `read_orientation` return the RESIDUAL). That
/// gap is [`crate::decoder_consumed_turns`], and it is closed in exactly two places, asymmetrically
/// as of v0.8.103: **here**, when reporting `new_base_turns`, where the number is DERIVED from values
/// already in hand (see `residual_turns` below), and in **`read_orientation`**, when a sidecar value
/// comes back in, where it is PROBED — safely, because there the probe's degradation is in step with
/// the base it is subtracted from. Nothing else in the tree needs to know.
pub fn apply_rotation(plan: &RotApplyPlan) -> RotApplyReport {
    let delta = plan.delta & 3;
    let mut new_base_turns = plan.base_turns & 3;
    // FILE-ABSOLUTE turns → RESIDUAL turns (the units `base_turns`/`new_base_turns` are in).
    //
    // v0.8.103 (V1): `consumed` is DERIVED, never probed. `cur` is the absolute orientation the arm
    // composed on (the file's own tag, the sidecar Falcon last wrote, or the reconstruction of the
    // app's base) and `plan.base_turns` is the RESIDUAL the same read produced, so by the definition
    // of "residual":
    //
    //     consumed         = (turns_of(cur) + 4 - plan.base_turns) & 3
    //     residual(target) = (turns_of(target) + 4 - consumed) & 3
    //
    // `compose_exif_orientation` adds `delta` turns WITHIN cur's mirror class — both cycles are
    // indexed by turns, `NONMIRR_BY_TURNS` and `MIRR_BY_TURNS` — so `turns_of(target) ==
    // (turns_of(cur) + delta) & 3` for all EIGHT orientations, mirrored ones included, and the pair
    // collapses to `(plan.base_turns + delta) & 3` on every finished-side arm: exactly the contract
    // [`RotApplyReport::new_base_turns`] states. Spelled out rather than folded so both coordinate
    // systems stay visible at each call site.
    //
    // Why derived and not probed: v0.8.102 re-probed `decoder_consumed_turns` on the apply worker,
    // and for a HEIC that probe is a full COM `CreateDecoderFromFilename` over the file. A sharing
    // violation (OneDrive/AV/Photos holding it), a COM failure or a codec hiccup returned `None` and
    // the probe degraded silently to 0 — which reported a base one quarter-turn PAST what the user
    // saw, evicted every tier and re-rendered the shot wrong: the F1/F2 RED verbatim, produced by its
    // own fix. This form is pure arithmetic: no I/O, no failure mode, nothing to degrade.
    //
    // What it deliberately does NOT do is heal a STALE `plan.base_turns` (a shot no tier ever decoded
    // takes the caller's `unwrap_or(0)`): the report then re-states the caller's own arithmetic
    // rather than the file's truth. That is bounded and self-correcting — the caller invalidates its
    // orientation cache on the same drain, so the next decode re-bases from `read_orientation`, and a
    // never-decoded shot has no pixel tier to be wrong on in the meantime.
    let residual_turns = |cur: u8, target: u8| {
        let consumed = (turns_of(cur) + 4 - (plan.base_turns & 3)) & 3;
        (turns_of(target) + 4 - consumed) & 3
    };

    // ── finished side ─────────────────────────────────────────────────────────────────────
    let finished_action = match &plan.finished {
        None => SideAction::Skipped,
        Some(p) if plan.finished_is_jpeg => {
            let expected = turns_to_orientation(plan.base_turns);
            let target = compose_exif_orientation(expected, delta);
            // v0.8.103 (V4): **the CAS IS the "the decoder consumed nothing" test**, and that is why
            // the in-place path is safe rather than merely lucky. A MATCH proves the file's own
            // absolute tag equals `turns_to_orientation(plan.base_turns)` — i.e. that absolute and
            // residual coincide for this file — which is the only shape in which rewriting the tag in
            // place expresses the user's delta. A JPEG decoder never bakes a rotation in, so that is
            // every well-formed JPEG. A metadata anomaly that DID make `consumed` non-zero (EXIF
            // `PixelXDimension`/`PixelYDimension` transposed against the container header — see
            // [`crate::decoder_consumed_turns`], which is format-blind) shifts the file's tag away
            // from `expected` by exactly those turns, so the CAS cannot match and the file falls to
            // the sidecar arms below, which compose on the value that IS there and never mutate the
            // tag. That fall is the DELIBERATE route for a non-zero consumed, not a last resort.
            //
            // Structurally: `new_base_turns` is assigned inside the arms, where the CAS outcome is
            // known, instead of before the call on an assumption (v0.8.102 assigned it at the top and
            // the divergence was averted only by the CAS happening to fail).
            match patch_jpeg_orientation(p, expected, target) {
                Ok(JpegPatch::Patched) => {
                    new_base_turns = residual_turns(expected, target);
                    SideAction::Patched
                }
                Ok(JpegPatch::AlreadyTarget) => {
                    new_base_turns = residual_turns(expected, target);
                    SideAction::AlreadyTarget
                }
                // Structural anomaly (no/odd EXIF) → sidecar fallback, composed on the file's FRESH
                // current orientation (item 9). Full-name sidecar (`HWU_0141.JPG.xmp`) so it never
                // collides with a paired RAW's `HWU_0141.xmp`.
                Err(PatchErr::Locate(_)) => {
                    // item 9 + findings #1/#9: compose on the sidecar-aware current value (the fullname
                    // sidecar we last wrote WINS over the un-patchable embedded EXIF), so a repeat Apply
                    // on an anomalous JPEG advances instead of re-composing on the immutable original.
                    let cur = current_for_sidecar(p, false);
                    let target = compose_exif_orientation(cur, delta);
                    new_base_turns = residual_turns(cur, target);
                    sidecar_action(&sidecar_fullname(p), target)
                }
                Err(PatchErr::Cas { found, .. }) => {
                    // The file's fresh value matched neither belief — compose on what IS there and write
                    // a sidecar (still item-9-correct); the in-place path stays refused (no blind write).
                    // CAS reads the raw u16 TIFF value, bypassing the validated metadata reader.
                    // An invalid 262 must not become orientation 6 when narrowed to u8.
                    let cur = if (1..=8).contains(&found) { found as u8 } else { 1 };
                    let target = compose_exif_orientation(cur, delta);
                    new_base_turns = residual_turns(cur, target);
                    sidecar_action(&sidecar_fullname(p), target)
                }
                Err(e) => SideAction::Failed(format!("{e}")),
            }
        }
        Some(p) => {
            // Non-JPEG finished (PNG/TIFF/WebP/HEIC): sidecar only. findings #1/#9: the sidecar we last
            // wrote WINS over the (never-patched, immutable) embedded EXIF so a second Apply advances; a
            // tag-less PNG/WebP with no sidecar yet falls back to the app's believed base.
            //
            // v0.8.102: composing on the file's OWN tag (rather than on `plan.base_turns`) is correct
            // and deliberate — see the FILE-ABSOLUTE convention in this function's doc. The value
            // written is absolute; only the REPORT is translated back to residual terms below. For a
            // tag-less, sidecar-less file the app's base IS the absolute value (nothing was consumed),
            // so the `unwrap_or_else` fallback stays honest under both readings.
            let cur = sidecar_orientation(p, false)
                .or_else(|| exif_orientation(p).map(|o| o as u8))
                .unwrap_or_else(|| turns_to_orientation(plan.base_turns));
            let target = compose_exif_orientation(cur, delta);
            new_base_turns = residual_turns(cur, target);
            sidecar_action(&sidecar_fullname(p), target)
        }
    };

    // ── RAW side (always sidecar, basename.xmp) ───────────────────────────────────────────
    let raw_action = match &plan.raw {
        None => SideAction::Skipped,
        Some(p) => {
            // findings #1/#9: the RAW is never patched, so its embedded orientation is immutable — read
            // the basename sidecar we last wrote FIRST (it WINS), so a second Apply composes on that value
            // (e.g. 3), not back on the stale embedded 1 (which silently dropped the first rotation).
            let cur = current_for_sidecar(p, true);
            let target = compose_exif_orientation(cur, delta);
            // For a RAW-ONLY shot the RAW's target turns drive the display base. No residual
            // translation here: `read_orientation`'s RAW branch has no decoder-transform rule (rawler
            // never bakes a rotation into the develop output), and `decoder_consumed_turns` is 0 for a
            // shot with no finished side — so absolute and residual coincide on this path by both
            // routes. This is also the one finished-side-free arm, so the `(base + delta)` collapse
            // above does not apply: the RAW's own fresh sidecar/embedded value drives it, which is
            // what makes a second Apply on a RAW advance (T-n) rather than re-compose on the original.
            if plan.finished.is_none() {
                new_base_turns = turns_of(target);
            }
            sidecar_action(&sidecar_basename(p), target)
        }
    };

    let ok = finished_action.ok() && raw_action.ok();
    RotApplyReport { ok, new_base_turns, finished_action, raw_action }
}

/// Map a sidecar write result to a `SideAction`.
fn sidecar_action(path: &Path, target: u8) -> SideAction {
    match write_xmp_sidecar(path, target) {
        Ok(SidecarKind::CreatedFresh) => SideAction::SidecarCreated,
        Ok(SidecarKind::Updated) => SideAction::SidecarUpdated,
        Ok(SidecarKind::AlreadyTarget) => SideAction::AlreadyTarget,
        Err(e) => SideAction::Failed(format!("{e}")),
    }
}

/// The XMP sidecar path Falcon reads/writes for `path`, dispatched EXACTLY like [`sidecar_orientation`]:
/// a RAW takes the basename form (`HWU_0141.CR3` → `HWU_0141.xmp`), a finished file the full-name form
/// (`HWU_0141.JPG` → `HWU_0141.JPG.xmp`). Pure path arithmetic (no I/O) — used by the v0.8.13 delete path
/// to name the sidecar belonging to a file before it is sent to the Recycle Bin (naming must MIRROR the
/// write side so we never orphan a real sidecar nor touch a foreign file).
pub fn sidecar_path_for(path: &Path, is_raw: bool) -> PathBuf {
    if is_raw {
        sidecar_basename(path)
    } else {
        sidecar_fullname(path)
    }
}

/// `HWU_0141.CR3` → `HWU_0141.xmp` (Adobe/LR basename convention — the primary sidecar case, RAW).
fn sidecar_basename(path: &Path) -> PathBuf {
    path.with_extension("xmp")
}
/// `HWU_0141.JPG` → `HWU_0141.JPG.xmp` (full-name — used only for the rare finished-side sidecar, so it
/// can never collide with a paired RAW's basename `HWU_0141.xmp`).
fn sidecar_fullname(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".xmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests;
