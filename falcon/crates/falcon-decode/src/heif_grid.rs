//! v0.8.144 (E1) — the from-scratch ISO-BMFF HEIF **grid** parse: the container stage of the
//! hardware-decode epic (July 2026).
//!
//! # What this is, and what it deliberately is not
//!
//! Every iPhone HEIC the project has ever measured is a GRID: 45–54 independently-decodable HEVC
//! tiles plus a `grid` derived item that says how they compose. WIC hides all of that behind one
//! `GetSize`/`CopyPixels` pair, and pays for it with a pure-CPU decode (measured 712–994 ms at
//! 48 MP). The hardware routes cannot be fed at all without the tile map — a decoder session takes
//! *bitstreams*, not files — so before any of E2/E3 can exist, something has to read the container.
//! That is this module, and ONLY this module: it produces a **decode plan** and decodes nothing.
//!
//! It is also INERT. Nothing on the shipping decode path calls it unless `FALCON_HW_HEIC_PARSE=1`
//! is set, and even then all that happens is one diagnostic line per file through the existing
//! [`crate::note_once`] channel. `FALCON_CLASSIC_HEIC` is untouched and orthogonal.
//!
//! # Reuse, not a second dialect
//!
//! Box walking is [`crate::bmff_children`] — the same overflow-free walker the v0.8.105 preview
//! probe and the v0.8.106 (L28) crafted-`largesize` row already harden — and property association
//! is [`crate::bmff_ipma_entries`], which v0.8.144 EXTRACTED out of `heic_preview_colr` so the two
//! readers of `ipma` are one reader. The only new primitive here is [`Cur`], a checked byte cursor
//! for the *leaf* boxes (`iloc`, `iinf`, `ispe`, the grid payload) that `bmff_children` does not
//! model — it reads fields, not boxes, so it is not a competing box parser.
//!
//! # Colour is not decided here
//!
//! The gamut answer routes through the EXISTING oracle ([`crate::heic_color_tag`] →
//! `falcon_color::resolve_source_gamut`, i.e. the v0.8.141 two-pass `colr` rule) and this module
//! never re-implements one byte of that policy. The structural parser ([`parse_heif_grid`]) has no
//! colour answer AT ALL — that is why the plan splits in two: [`HeifGridPlan`] is what the bytes
//! say about geometry, and [`HeifDecodePlan`] is that plus the oracle's colour verdict, obtainable
//! only from the path door [`heif_decode_plan`]. The primary item's own `colr` body is carried
//! along as raw DATA (`primary_colr`) for E2/E3 to hand to the oracle — never as a second verdict.
//! L42 in the design ledger says never decide from a name when the answering bytes are in hand;
//! the twin of that rule is never grow a second decider, and this split is that twin made structural.
//!
//! # Fail closed, always
//!
//! Every entry point returns `Result` and every internal read is checked. A truncated `iloc`, an
//! absent `pitm`, an `ipma` index past the end of `ipco`, a `largesize` chosen to wrap the cursor —
//! all of them are `Err`, none of them panics. `heif_grid_fuzz_survives_byte_flips` hammers 5,000
//! single-byte mutations of a real container's head through [`parse_heif_grid`] and asserts the
//! same.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::{bmff_children, bmff_ipma_entries};

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The plan
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// A rectangle in the composed mosaic's coordinate space, before any rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeifRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// One contiguous run of file bytes, resolved to ABSOLUTE file offsets whatever construction
/// method `iloc` used to express it. E3 hands these straight to a decoder session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeifExtent {
    pub offset: u64,
    pub len: u64,
}

impl HeifExtent {
    /// The exclusive end offset. Saturating: an extent this module emits is already bounds-checked
    /// against the file length, so the saturation can only ever be reached by a caller who built
    /// one by hand.
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.len)
    }
}

/// How `iloc` expressed an item's location. Recorded rather than erased because "which construction
/// method did this file use" is the first question when a HEIC ever fails on a tester's machine —
/// and because [`HeifConstruction::Item`] is a shape we deliberately refuse rather than guess at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeifConstruction {
    /// `construction_method == 0`: offsets are absolute file offsets. Every Apple file measured.
    File,
    /// `construction_method == 1`: offsets are relative to the `idat` box's payload. Apple puts the
    /// tiny `grid` payloads here.
    Idat,
    /// `construction_method == 2`: the data lives inside ANOTHER item. Legal, never seen, and not
    /// resolved — an item carrying it is reported, never silently mis-located.
    Item,
}

/// One tile of the primary grid, in `dimg` order (which IS raster order: row 0 left→right first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeifTile {
    pub item_id: u32,
    /// Raster position in the mosaic.
    pub row: u32,
    pub col: u32,
    /// The tile's HEVC payload, as absolute file byte ranges in order. Normally exactly one.
    pub extents: Vec<HeifExtent>,
    pub construction: HeifConstruction,
    /// The 1-based `ipco` index of THIS tile's `hvcC`. Every file measured so far points all its
    /// tiles at one index — but SP2's own qualification is that the hvcC BYTES vary between files,
    /// so nothing here assumes a constant, and the per-tile index is kept so a file that ever
    /// splits its parameter sets is REPRESENTED rather than mis-decoded.
    pub hvcc_prop: Option<usize>,
    /// The tile's own `ispe`. The spec requires every tile of a grid to be the same size; this is
    /// what [`parse_heif_grid`] checks that against.
    pub ispe: Option<(u32, u32)>,
}

/// `clap` — a clean-aperture crop property, carried whole. No file in the corpus has one (the only
/// crop Apple uses is the grid's own output trim), so this is representation, not exercised policy:
/// [`parse_heif_grid`] applies it when it reduces to an exact integral in-bounds rect and REFUSES
/// the file otherwise, because a crop we cannot express exactly would silently change dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeifClap {
    pub width_n: i32,
    pub width_d: i32,
    pub height_n: i32,
    pub height_d: i32,
    pub horiz_off_n: i32,
    pub horiz_off_d: i32,
    pub vert_off_n: i32,
    pub vert_off_d: i32,
}

/// `imir` — mirroring about a geometric axis. The bit describes the flip DIRECTION:
/// 0 flips top/bottom (a horizontal axis), 1 flips left/right (a vertical axis).
/// Neither changes the dimensions, which is why
/// [`HeifGridPlan::display`] does not consult it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeifMirror {
    /// Mirrored left/right (`imir` bit 1).
    Vertical,
    /// Mirrored top/bottom (`imir` bit 0).
    Horizontal,
}

/// Canonical rotation followed by a mirror. Compose in property-association order:
/// a mirror before a quarter-turn changes its axis in the resulting display space.
#[derive(Default, Clone, Copy)]
struct ItemTransform {
    rotation: u16,
    mirror: Option<HeifMirror>,
}

impl ItemTransform {
    fn rotate(&mut self, ccw: u16) {
        self.rotation = (self.rotation + ccw) % 360;
        if !ccw.is_multiple_of(180) {
            self.mirror = self.mirror.map(|m| match m {
                HeifMirror::Vertical => HeifMirror::Horizontal,
                HeifMirror::Horizontal => HeifMirror::Vertical,
            });
        }
    }

    fn reflect(&mut self, axis: HeifMirror) {
        if let Some(previous) = self.mirror.take() {
            if previous != axis {
                self.rotation = (self.rotation + 180) % 360;
            }
        } else {
            self.mirror = Some(axis);
        }
    }

    #[cfg(any(windows, test))]
    fn exif(self) -> u32 {
        let r = (self.rotation / 90) as usize;
        match self.mirror {
            None => [1, 8, 3, 6][r],
            Some(HeifMirror::Vertical) => [2, 7, 4, 5][r],
            Some(HeifMirror::Horizontal) => [4, 5, 2, 7][r],
        }
    }
}

/// Metadata-only orientation probe; no tile payloads or codec required. A complete meta box
/// must fit in the bounded prefix. Use the same association parser/order as the hardware lane.
#[cfg(windows)]
pub(crate) fn heif_primary_orientation(path: &Path) -> Option<u32> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(256 * 1024).read_to_end(&mut bytes).ok()?;
    primary_orientation_from_bytes(&bytes)
}

#[cfg(any(windows, test))]
fn primary_orientation_from_bytes(bytes: &[u8]) -> Option<u32> {
    let (_, s, e) = bmff_children(bytes, 0, bytes.len()).into_iter()
        .find(|(t, s, e)| t == b"meta" && e.saturating_sub(*s) >= 4)?;
    let meta = read_meta(bytes, s + 4, e);
    let primary = meta.primary?;
    let props = &meta.ipma.iter().find(|(id, _)| *id == primary)?.1;
    let mut transform = ItemTransform::default();
    for &ix in props {
        if ix == 0 { continue; }
        let &(t, s, e) = meta.ipco.get(ix - 1)?;
        match &t {
            b"irot" => transform.rotate(read_irot(bytes, s, e)?),
            b"imir" => transform.reflect(read_imir(bytes, s, e)?),
            _ => {}
        }
    }
    Some(transform.exif())
}

/// Why the parser walked PAST an item instead of putting it in the plan. The epic consumes exactly
/// one image per file; everything else in a 129-item iPhone container is inventory, and E2/E3 (and
/// any future gain-map work) need to know what was there rather than that something was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeifSkip {
    /// An `auxl` reference names it as auxiliary to another item — Apple's gain map, the portrait
    /// and skin mattes, the style delta.
    Auxiliary,
    /// A `thmb` reference names it as a thumbnail.
    Thumbnail,
    /// `Exif`, `mime`, `uri ` — metadata items, not pictures.
    Metadata,
    /// A `dimg` child of some grid that is not the primary (the gain-map and matte grids' tiles).
    TileOfAnother,
    /// Another derived item — a second `grid`, a `tmap` tone map, an `iovl` overlay.
    Derived,
    /// Nothing in the file refers to it and it is not the primary.
    Unreferenced,
}

/// One skipped item: what it is, why it was skipped, and the little that is cheap to say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeifAuxItem {
    pub item_id: u32,
    pub item_type: [u8; 4],
    pub why: HeifSkip,
    /// `(rows, cols, output_w, output_h)` when the skipped item is itself a grid — the gain map and
    /// the linear-thumb/style-delta grid, whose shapes matter the day the aux chain is consumed.
    pub grid: Option<(u32, u32, u32, u32)>,
    /// The `auxC` URN, when the item carries one (`…:auxiliary:hdrgainmap`,
    /// `…:portraiteffectsmatte`, `…:semanticskinmatte`, …).
    pub aux_type: Option<String>,
    /// First channel's `pixi` bit depth. The gain-map grid is Main10 and the style-delta grid is
    /// RExt monochrome; the investigation's standing instruction is that E1 must WALK PAST both
    /// without choking, and this field is the proof it read them rather than skipped blind.
    pub bit_depth: Option<u8>,
}

/// The structural decode plan for a HEIF container's PRIMARY item. Geometry and byte ranges only —
/// no colour verdict lives here by construction (see the module header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeifGridPlan {
    pub primary_item_id: u32,
    pub primary_item_type: [u8; 4],
    pub major_brand: [u8; 4],
    /// `ftyp`'s compatible brands as a SET — sorted and deduped, because Stage 0 measured the ORDER
    /// varying between files from the same camera. Anything that reads this as a sequence is wrong.
    pub compatible_brands: Vec<[u8; 4]>,
    pub rows: u32,
    pub cols: u32,
    pub tile_w: u32,
    pub tile_h: u32,
    /// `cols × tile_w` by `rows × tile_h` — the extent the tiles compose to before any trim.
    pub mosaic: (u32, u32),
    /// The `grid` payload's own `output_width`/`output_height`: the trim of the mosaic. On a 48 MP
    /// iPhone file the mosaic is 8064×6144 and this is 8064×6048 — the bottom 96 px of the last
    /// tile row are padding the grid itself declares away.
    pub grid_output: (u32, u32),
    /// The primary item's `ispe`. Redundant with `grid_output` on every real file, and the parser
    /// says so out loud: they disagreeing is an `Err`, not a preference.
    pub ispe: Option<(u32, u32)>,
    pub clap: Option<HeifClap>,
    /// The rect of the mosaic that survives (grid trim, then `clap` if any) — pre-rotation.
    pub crop: HeifRect,
    /// Post-crop, POST-rotation dimensions: the number the shipping WIC path answers with, and the
    /// one the parity battery pins.
    pub display: (u32, u32),
    /// `irot` in degrees counter-clockwise: 0, 90, 180 or 270.
    pub irot: u16,
    pub imir: Option<HeifMirror>,
    /// The `pixi` bits-per-channel list, whole.
    pub pixi: Option<Vec<u8>>,
    /// First channel's bit depth — 8 on every base image measured, in a corpus whose AUX chain is
    /// 10-bit, which is exactly why it is read rather than assumed.
    pub bit_depth: Option<u8>,
    pub tiles: Vec<HeifTile>,
    /// The one `hvcC` every tile shares — `Some` **only** when they really do all name the same
    /// `ipco` index. A file that splits them yields `None` here and the per-tile
    /// [`HeifTile::hvcc_prop`] with `hvcc_by_prop` still fully populated.
    pub hvcc: Option<Vec<u8>>,
    /// Every distinct `hvcC` the primary's tiles reference, by 1-based `ipco` index.
    pub hvcc_by_prop: BTreeMap<usize, Vec<u8>>,
    /// The primary item's own `colr` box BODY (from the `colour_type` fourcc onward), raw. DATA for
    /// the oracle to answer from — never an answer.
    pub primary_colr: Option<Vec<u8>>,
    /// Everything the parser walked past, with a reason each.
    pub aux: Vec<HeifAuxItem>,
    pub file_len: u64,
    /// How many items `iinf` declared, in total.
    pub item_count: usize,
}

/// [`HeifGridPlan`] plus the colour answer the EXISTING oracle gives for the same file. Only the
/// path door [`heif_decode_plan`] can build one, because the oracle reads a path.
#[derive(Debug, Clone, PartialEq)]
pub struct HeifDecodePlan {
    pub grid: HeifGridPlan,
    /// `falcon_color::resolve_source_gamut` on `heic_color_tag`'s verdict — byte-for-byte the
    /// answer `shot_source_gamut` gives for the same file. Not re-derived here; delegated.
    pub gamut: falcon_color::Gamut,
    /// The colour-space name the same oracle reports, when it has one.
    pub color_desc: Option<String>,
}

/// Every way the parse can decline. All of them are declines: none is a panic, and none is a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeifParseError {
    /// The file could not be opened or read.
    Io(String),
    /// Larger than [`MAX_CONTAINER_BYTES`] — refused before allocating.
    TooLarge(u64),
    /// No `meta` box, or one too short to hold its own version/flags.
    NoMeta,
    /// No `pitm`, or a `pitm` naming an item `iinf` does not declare.
    NoPrimaryItem,
    /// The primary is not a shape this stage plans for (E1 handles `grid` and a bare coded image).
    PrimaryNotGrid([u8; 4]),
    /// The `grid` payload could not be read or is too short to carry its own header.
    GridPayloadUnreadable,
    /// `dimg` named a different number of tiles than `rows × cols`.
    TileCountMismatch { want: usize, got: usize },
    /// A tile has no `ispe`, or the tiles disagree about their size.
    TileDimensions,
    /// `iloc` places item data outside the file.
    ExtentOutOfBounds { item: u32 },
    /// An item's data lives in an external file (`data_reference_index != 0`) or in another item.
    UnresolvableLocation { item: u32 },
    /// The container is malformed in a way with a name; the string is that name.
    Malformed(&'static str),
}

impl std::fmt::Display for HeifParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeifParseError::Io(e) => write!(f, "io: {e}"),
            HeifParseError::TooLarge(n) => write!(f, "container too large ({n} bytes)"),
            HeifParseError::NoMeta => write!(f, "no usable meta box"),
            HeifParseError::NoPrimaryItem => write!(f, "no primary item"),
            HeifParseError::PrimaryNotGrid(t) => {
                write!(f, "primary item is '{}', not a grid", fourcc(t))
            }
            HeifParseError::GridPayloadUnreadable => write!(f, "grid payload unreadable"),
            HeifParseError::TileCountMismatch { want, got } => {
                write!(f, "grid wants {want} tiles, dimg names {got}")
            }
            HeifParseError::TileDimensions => write!(f, "tiles have no or inconsistent ispe"),
            HeifParseError::ExtentOutOfBounds { item } => {
                write!(f, "item {item} extends past the end of the file")
            }
            HeifParseError::UnresolvableLocation { item } => {
                write!(f, "item {item} data is not in this file")
            }
            HeifParseError::Malformed(w) => write!(f, "malformed: {w}"),
        }
    }
}

/// Refuse anything bigger than this outright. A 48 MP HEIC is ~11 MB; 256 MB is four hundred times
/// the largest file the project has ever seen and still small enough that the refusal is the safe
/// answer rather than the surprising one.
pub const MAX_CONTAINER_BYTES: u64 = 256 * 1024 * 1024;

/// A `grid`/`tmap` payload is a handful of bytes. Anything claiming megabytes is not one, and this
/// module will not allocate on an attacker's say-so to find out.
const MAX_DERIVED_PAYLOAD: u64 = 64 * 1024;

/// A fourcc as a printable string, non-ASCII bytes shown as `.` — for messages only.
fn fourcc(t: &[u8; 4]) -> String {
    t.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect()
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The checked cursor — fields, not boxes
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// A bounds-checked big-endian reader over a byte span. Every read returns `Option`, so "the box
/// ended early" is expressed by `?` at the call site and never by a panic. This is deliberately NOT
/// a box parser: box structure is [`bmff_children`]'s job and this only reads the *contents* of a
/// leaf box, which `bmff_children` does not model.
struct Cur<'a> {
    b: &'a [u8],
    p: usize,
    end: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8], start: usize, end: usize) -> Self {
        let end = end.min(b.len());
        Cur { b, p: start.min(end), end }
    }
    fn left(&self) -> usize {
        self.end.saturating_sub(self.p)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.left() {
            return None;
        }
        let s = self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_be_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }
    fn fourcc(&mut self) -> Option<[u8; 4]> {
        self.take(4).map(|s| [s[0], s[1], s[2], s[3]])
    }
    /// An unsigned big-endian integer of `n` bytes, `n` in 0..=8 (ISO-BMFF's `{offset,length,
    /// base_offset,index}_size` fields are exactly this, and `0` legitimately means "the value is 0").
    fn uint(&mut self, n: usize) -> Option<u64> {
        if n == 0 {
            return Some(0);
        }
        if n > 8 {
            return None;
        }
        let s = self.take(n)?;
        Some(s.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64))
    }
    /// A NUL-terminated UTF-8 string. An unterminated run to the end of the span is NOT a string —
    /// it is a truncated box, and returns `None` so the caller declines rather than inventing one.
    fn cstr(&mut self) -> Option<String> {
        let rest = self.b.get(self.p..self.end)?;
        let n = rest.iter().position(|&b| b == 0)?;
        let s = String::from_utf8_lossy(&rest[..n]).into_owned();
        self.p += n + 1;
        Some(s)
    }
    /// The version byte and 24-bit flags of a FullBox.
    fn full_header(&mut self) -> Option<(u8, u32)> {
        let v = self.u8()?;
        let f = self.take(3)?;
        Some((v, u32::from_be_bytes([0, f[0], f[1], f[2]])))
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Intermediates
// ─────────────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ItemInfo {
    item_type: [u8; 4],
}

#[derive(Debug, Clone)]
struct ItemLoc {
    construction: HeifConstruction,
    data_reference_index: u16,
    base_offset: u64,
    /// `(offset, length)` pairs exactly as `iloc` wrote them, before any base is applied.
    extents: Vec<(u64, u64)>,
}

/// Everything `meta` declared, once, so the derivation below reads like the question it answers.
struct Meta {
    primary: Option<u32>,
    items: BTreeMap<u32, ItemInfo>,
    /// `(reference_type, from_item, to_items)`.
    refs: Vec<([u8; 4], u32, Vec<u32>)>,
    /// The `ipco` children — 1-based property indices index THIS, offset by one.
    ipco: Vec<([u8; 4], usize, usize)>,
    /// `(item_id, property indices)`.
    ipma: Vec<(u32, Vec<usize>)>,
    locs: BTreeMap<u32, ItemLoc>,
    /// The `idat` payload's absolute file span, when the container has one.
    idat: Option<(usize, usize)>,
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The public doors
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Parse a whole HEIF container's bytes into the primary item's structural decode plan.
///
/// `bytes` must be the WHOLE file: `iloc` addresses tile payloads by absolute file offset, and the
/// bounds check that keeps every emitted extent honest has nothing to check against otherwise. That
/// is a real cost for a 48 MP file (~11 MB) and it is the right one for the consumer this exists
/// for — E3 must read essentially all of those bytes anyway to feed a decoder session. (When E3
/// lands, the natural refinement is a memory map, which changes the caller and not this function.)
pub fn parse_heif_grid(bytes: &[u8]) -> Result<HeifGridPlan, HeifParseError> {
    let file_len = bytes.len() as u64;
    if file_len > MAX_CONTAINER_BYTES {
        return Err(HeifParseError::TooLarge(file_len));
    }
    let top = bmff_children(bytes, 0, bytes.len());

    // ── ftyp: the brand SET (Stage 0 measured the ORDER varying between files) ──
    let mut major_brand = [0u8; 4];
    let mut brand_set: BTreeSet<[u8; 4]> = BTreeSet::new();
    if let Some((_, s, e)) = top.iter().find(|(t, _, _)| t == b"ftyp") {
        let mut c = Cur::new(bytes, *s, *e);
        if let Some(m) = c.fourcc() {
            major_brand = m;
        }
        let _minor = c.u32();
        while let Some(b) = c.fourcc() {
            brand_set.insert(b);
        }
    }

    // ── meta: a FullBox, so its children start 4 bytes in ──
    let (meta_s, meta_e) = top
        .iter()
        .find(|(t, s, e)| t == b"meta" && e.saturating_sub(*s) >= 4)
        .map(|(_, s, e)| (*s + 4, *e))
        .ok_or(HeifParseError::NoMeta)?;
    let meta = read_meta(bytes, meta_s, meta_e);

    let primary = meta.primary.ok_or(HeifParseError::NoPrimaryItem)?;
    let primary_info = meta.items.get(&primary).ok_or(HeifParseError::NoPrimaryItem)?.clone();

    // ── property lookup, once ──
    //
    // v0.8.152 (R3-M3): ONE map, built here, instead of a linear scan of `meta.ipma` per lookup.
    // The old `props_of` was `meta.ipma.iter().find(..)` and it ran once per tile — and BOTH counts
    // are attacker-chosen. `want` is `rows × cols` from `parse_image_grid`'s two `u8 + 1` fields, so
    // legitimately up to 65 536; `bmff_ipma_entries` reads its entry count from a `u32` and is
    // bounded only by the box's own byte span at ≥3 bytes per entry inside a 256 MB container. That
    // is 10⁹–10¹¹ comparisons a crafted few-MB HEIC could buy — and this parse is on the SHIPPING
    // hardware lane (`falcon_hwdec::tile_source` calls it, `hwheic.rs` calls that per decode
    // attempt), so the cost is a wedged fast-pool worker, per attempt, per visit.
    //
    // v0.8.153 (skeptic A / O3) — THERE ARE TWO MAPS, BECAUSE THE TWO READERS NEVER AGREED.
    //
    // v0.8.152 built one map and gave it the UNION on a repeated item id, on the reasoning that
    // "the union is the one that cannot hide an association". That is true of the INVENTORY, which
    // reports what it can see and whose two questions (`auxC`, `pixi`) are last-write-wins over a
    // list. It is false — and a REGRESSION — for `props_of`, whose readers are `irot`, `imir`,
    // `clap`, `ispe`, `pixi`, `colr` and each tile's `hvcC`: those are `match` arms that ASSIGN, so
    // concatenating a second entry's associations lets the LAST one win. A file with two legal
    // `ipma` boxes for the primary would decode with a different rotation, a different crop, or a
    // different parameter set than v0.8.151 gave it — which is the wrong-parameter-set class this
    // very epic's `tail_verified` gate exists to close.
    //
    // So `props_of` is FIRST-WINS — `entry().or_insert_with()`, exactly the old
    // `meta.ipma.iter().find(..)` — and `inventory` keeps the union it always had. Every real file
    // has exactly one entry per item, so on everything that is not crafted the two maps are equal.
    let ipma_first: BTreeMap<u32, &[usize]> = {
        let mut m: BTreeMap<u32, &[usize]> = BTreeMap::new();
        for (id, props) in &meta.ipma {
            m.entry(*id).or_insert_with(|| props.as_slice());
        }
        m
    };
    let ipma_by_item: BTreeMap<u32, Vec<usize>> = {
        let mut m: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        for (id, props) in &meta.ipma {
            m.entry(*id).or_default().extend_from_slice(props);
        }
        m
    };
    let props_of = |id: u32| -> &[usize] { ipma_first.get(&id).copied().unwrap_or(&[][..]) };
    // 1-based, per the spec; index 0 means "no property" and an index past `ipco` is an `Err`
    // wherever it is load-bearing (see `prop_or_err`).
    let prop = |ix: usize| -> Option<([u8; 4], usize, usize)> {
        meta.ipco.get(ix.checked_sub(1)?).copied()
    };

    // ── the primary's own transform/format properties ──
    let mut ispe: Option<(u32, u32)> = None;
    let mut transform = ItemTransform::default();
    let mut clap: Option<HeifClap> = None;
    let mut pixi: Option<Vec<u8>> = None;
    let mut primary_colr: Option<Vec<u8>> = None;
    let primary_props = props_of(primary);
    for &ix in primary_props {
        // Index 0 is the spec's "no property" — legal, and not a defect.
        if ix == 0 {
            continue;
        }
        // v0.8.144: an `ipma` index past the end of `ipco` IS a defect, and a loud one. Skipping it
        // silently would let a crafted file erase an `irot` and hand back landscape dimensions for a
        // portrait photo — a wrong answer dressed as a successful parse, which is the failure mode
        // this whole stage exists to make impossible.
        let (t, s, e) = prop(ix).ok_or(HeifParseError::Malformed("ipma index past ipco"))?;
        match &t {
            b"ispe" => ispe = read_ispe(bytes, s, e),
            b"irot" => {
                transform.rotate(read_irot(bytes, s, e).ok_or(HeifParseError::Malformed("short irot"))?);
            }
            b"imir" => {
                transform.reflect(read_imir(bytes, s, e).ok_or(HeifParseError::Malformed("short imir"))?);
            }
            b"clap" => {
                // The assembler crops before transforming. A crop expressed after a transform
                // needs different coordinates; let the platform decoder handle that ordering.
                if transform.rotation != 0 || transform.mirror.is_some() || clap.is_some() {
                    return Err(HeifParseError::Malformed("crop after transform or repeated crop"));
                }
                clap = Some(read_clap(bytes, s, e).ok_or(HeifParseError::Malformed("short clap"))?)
            }
            b"pixi" => pixi = read_pixi(bytes, s, e),
            // FIRST only: an item may associate several `colr` boxes and the ORDER is the file's
            // statement of precedence — the same order the v0.8.141 two-pass oracle walks. Taking
            // the last would hand a later stage a different box from the one the oracle answered on.
            b"colr" if primary_colr.is_none() => primary_colr = bytes.get(s..e).map(|b| b.to_vec()),
            _ => {}
        }
    }

    let irot = transform.rotation;
    let imir = transform.mirror;

    // ── the grid itself ──
    let (rows, cols, grid_output, tile_ids) = match &primary_info.item_type {
        b"grid" => {
            let payload = read_derived_payload(bytes, &meta, primary, file_len)?;
            let (rows, cols, ow, oh) =
                parse_image_grid(&payload).ok_or(HeifParseError::GridPayloadUnreadable)?;
            let tiles = dimg_children(&meta, primary);
            (rows, cols, (ow, oh), tiles)
        }
        // A container whose primary is a plain coded image is legal HEIF and, per the Stage 0
        // carried-forward list, the one shape the corpus has NO sample of. Modelling it as a 1×1
        // grid is the honest generalisation — it needs no new machinery and it means a non-Apple
        // file does not fall over on its way to the fallback. Its "tile" is the item itself.
        b"hvc1" | b"hev1" | b"av01" | b"jpeg" => {
            let d = ispe.ok_or(HeifParseError::TileDimensions)?;
            (1, 1, d, vec![primary])
        }
        other => return Err(HeifParseError::PrimaryNotGrid(*other)),
    };

    let want = (rows as usize).checked_mul(cols as usize).ok_or(HeifParseError::Malformed("grid overflow"))?;
    if tile_ids.len() != want {
        return Err(HeifParseError::TileCountMismatch { want, got: tile_ids.len() });
    }

    // ── the tiles ──
    let mut tiles: Vec<HeifTile> = Vec::with_capacity(want);
    let mut tile_dims: Option<(u32, u32)> = None;
    let mut hvcc_by_prop: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    for (n, &id) in tile_ids.iter().enumerate() {
        let (extents, construction) = resolve_extents(&meta, id, file_len)?;
        let mut hvcc_prop = None;
        let mut t_ispe = None;
        for &ix in props_of(id) {
            if ix == 0 {
                continue;
            }
            let Some((t, s, e)) = prop(ix) else {
                return Err(HeifParseError::Malformed("ipma index past ipco"));
            };
            match &t {
                b"hvcC" => {
                    hvcc_prop = Some(ix);
                    if let Some(b) = bytes.get(s..e) {
                        hvcc_by_prop.entry(ix).or_insert_with(|| b.to_vec());
                    }
                }
                b"ispe" => t_ispe = read_ispe(bytes, s, e),
                _ => {}
            }
        }
        let d = t_ispe.ok_or(HeifParseError::TileDimensions)?;
        match tile_dims {
            None => tile_dims = Some(d),
            // The spec requires every input image of a grid to be the same size. A file that says
            // otherwise cannot have a tile map, so it is refused rather than averaged.
            Some(prev) if prev != d => return Err(HeifParseError::TileDimensions),
            _ => {}
        }
        tiles.push(HeifTile {
            item_id: id,
            row: (n as u32) / cols,
            col: (n as u32) % cols,
            extents,
            construction,
            hvcc_prop,
            ispe: t_ispe,
        });
    }
    let (tile_w, tile_h) = tile_dims.ok_or(HeifParseError::TileDimensions)?;
    if tile_w == 0 || tile_h == 0 {
        return Err(HeifParseError::TileDimensions);
    }

    // ── geometry ──
    let mosaic = (
        cols.checked_mul(tile_w).ok_or(HeifParseError::Malformed("mosaic overflow"))?,
        rows.checked_mul(tile_h).ok_or(HeifParseError::Malformed("mosaic overflow"))?,
    );
    if grid_output.0 == 0
        || grid_output.1 == 0
        || grid_output.0 > mosaic.0
        || grid_output.1 > mosaic.1
    {
        return Err(HeifParseError::Malformed("grid output does not fit its tiles"));
    }
    // The grid's declared output and the item's `ispe` are two statements of the same fact. Every
    // real file agrees; a file that does not is not one this stage can plan, because the two
    // answers would give two different canvases.
    if let Some(d) = ispe {
        if d != grid_output {
            return Err(HeifParseError::Malformed("ispe disagrees with the grid output"));
        }
    }
    let crop = apply_clap(grid_output, clap.as_ref())?;
    let display = match irot {
        90 | 270 => (crop.h, crop.w),
        _ => (crop.w, crop.h),
    };

    // ── the shared hvcC, if and only if it really is shared ──
    let shared_ix = tiles.first().and_then(|t| t.hvcc_prop);
    let hvcc = shared_ix
        .filter(|ix| tiles.iter().all(|t| t.hvcc_prop == Some(*ix)))
        .and_then(|ix| hvcc_by_prop.get(&ix).cloned());

    // ── the inventory ──
    let aux = inventory(bytes, &meta, &ipma_by_item, primary, &tile_ids, file_len);

    Ok(HeifGridPlan {
        primary_item_id: primary,
        primary_item_type: primary_info.item_type,
        major_brand,
        compatible_brands: brand_set.into_iter().collect(),
        rows,
        cols,
        tile_w,
        tile_h,
        mosaic,
        grid_output,
        ispe,
        clap,
        crop,
        display,
        irot,
        imir,
        bit_depth: pixi.as_ref().and_then(|p| p.first().copied()),
        pixi,
        tiles,
        hvcc,
        hvcc_by_prop,
        primary_colr,
        aux,
        file_len,
        item_count: meta.items.len(),
    })
}

/// Where a HEIF file's `Exif` item lies, as [`heif_exif_item`] answers it from the file's first bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeifExifLookup {
    /// The item's bytes as ABSOLUTE file extents, in order, each non-empty and inside the file. An
    /// item stored in `idat` (construction method 1) resolves to the `idat` bytes' own file offsets,
    /// which lie inside the prefix the caller already holds.
    Found(Vec<HeifExtent>),
    /// The top-level `meta` box starts in the prefix and ends at this file offset, past it: the
    /// caller may read up to it and ask again.
    MetaEndsAt(u64),
    /// No `meta` in the prefix, no `Exif` item, or an index this module's readers decline.
    NotFound,
}

/// 14 November 2026 (the date-read fix): where the `Exif` item of a HEIF file is, from its first
/// bytes alone. `prefix` must be the file's bytes from offset 0 (so a prefix index IS a file offset,
/// the same identity [`parse_heif_grid`] relies on for a whole file); `file_len` is the file's
/// length, against which every extent is checked.
///
/// The date read needs this because an iPhone writes the Exif item wherever its `iloc` says, measured
/// at 3.5 to 94.6 KiB from the start while the index (`meta`) itself ends by 36.2 KiB: a fixed-size
/// head misses a third of a real library's dates. Nothing new is parsed here: `meta` is
/// [`read_meta`] (so `iinf`/`infe`, `iref` and `iloc` are this module's own readers, with their field
/// widths, versions and construction methods), and the extents are [`resolve_extents`] (the
/// `data_reference_index` refusal, the overflow-free bounds, the `idat` containment). The one extra
/// is [`top_box`], a single top-level box HEADER, because [`bmff_children`] stops at a box that runs
/// past the bytes in hand, and "where does `meta` end" is exactly the question then.
///
/// Which item: the `Exif` item that describes (`cdsc`) the primary picture; else the last one by
/// item ID, the one kamadak-exif (the details panel's reader) takes, as every file seen declares
/// items in ID order. Every file measured has exactly one.
pub(crate) fn heif_exif_item(prefix: &[u8], file_len: u64) -> HeifExifLookup {
    let mut at = 0usize;
    loop {
        let Some((typ, hdr, end)) = top_box(prefix, at, file_len) else { return HeifExifLookup::NotFound };
        if end > prefix.len() as u64 {
            // Only `meta` is worth reading on for; a box before it that runs past the prefix (an
            // `mdat` first) means the index is not near the start, and the caller keeps its old path.
            return if &typ == b"meta" { HeifExifLookup::MetaEndsAt(end) } else { HeifExifLookup::NotFound };
        }
        let end = end as usize; // <= prefix.len(), so it fits
        if &typ == b"meta" {
            // A FullBox: its children start 4 bytes in, exactly as `parse_heif_grid` reads it.
            let body = at + hdr;
            if end.saturating_sub(body) < 4 {
                return HeifExifLookup::NotFound;
            }
            return exif_extents(&read_meta(prefix, body + 4, end), file_len);
        }
        at = end;
    }
}

/// One top-level box header at `at`: `(type, header length, the box's END as a file offset)`. The
/// sizes follow [`bmff_children`]'s rules (`1` = a 64-bit `largesize`, a size below the header is
/// refused), except that `0` ("to the end") means the end of the FILE, which is what it says at the
/// top level and which the prefix alone cannot know. Every sum is checked; an end past the file is
/// `None`.
fn top_box(buf: &[u8], at: usize, file_len: u64) -> Option<([u8; 4], usize, u64)> {
    let mut c = Cur::new(buf, at, buf.len());
    let size = c.u32()?;
    let typ = c.fourcc()?;
    let (hdr, size) = match size {
        0 => (8usize, file_len.checked_sub(at as u64)?),
        1 => (16, c.uint(8)?),
        n => (8, u64::from(n)),
    };
    if size < hdr as u64 {
        return None;
    }
    let end = (at as u64).checked_add(size)?;
    (end <= file_len).then_some((typ, hdr, end))
}

/// The `Exif` item of `meta`, resolved (see [`heif_exif_item`] for which one).
fn exif_extents(meta: &Meta, file_len: u64) -> HeifExifLookup {
    let ids: Vec<u32> =
        meta.items.iter().filter(|(_, info)| &info.item_type == b"Exif").map(|(id, _)| *id).collect();
    let describes_primary = |id: &u32| {
        meta.primary.is_some_and(|p| {
            meta.refs.iter().any(|(t, from, to)| t == b"cdsc" && from == id && to.contains(&p))
        })
    };
    let Some(id) = ids.iter().copied().find(describes_primary).or_else(|| ids.last().copied()) else {
        return HeifExifLookup::NotFound;
    };
    match resolve_extents(meta, id, file_len) {
        // A zero length means "to the end of the data" in ISO-BMFF; no Exif item is written that
        // way, and following it would read a whole file, so it is declined.
        Ok((extents, _)) if extents.iter().all(|e| e.len > 0) => HeifExifLookup::Found(extents),
        _ => HeifExifLookup::NotFound,
    }
}

/// The path door: [`parse_heif_grid`] plus the colour answer from the EXISTING oracle.
///
/// The colour half is one delegation and nothing else — `heic_color_tag` is the v0.8.141 two-pass
/// `colr` rule and `falcon_color::resolve_source_gamut` is the τ-bounded colorimetry match, exactly
/// as `shot_source_gamut` calls them for the shipping path. This function must never grow a rule of
/// its own: if it did, a HEIC would have two colour answers, and the round that taught this project
/// what that costs is only three versions old.
pub fn heif_decode_plan(path: &Path) -> Result<HeifDecodePlan, HeifParseError> {
    let len = std::fs::metadata(path).map_err(|e| HeifParseError::Io(e.to_string()))?.len();
    if len > MAX_CONTAINER_BYTES {
        return Err(HeifParseError::TooLarge(len));
    }
    let bytes = std::fs::read(path).map_err(|e| HeifParseError::Io(e.to_string()))?;
    let grid = parse_heif_grid(&bytes)?;
    let tag = crate::heic_color_tag(path);
    let gamut = falcon_color::resolve_source_gamut(tag.icc.as_deref(), tag.desc.as_deref()).gamut;
    Ok(HeifDecodePlan { grid, gamut, color_desc: tag.desc })
}

impl HeifGridPlan {
    /// Where tile `t` lands in the mosaic, before the crop. Saturating by construction — `row`/`col`
    /// come from the tile's own index in a `rows × cols` list, so the products cannot exceed the
    /// mosaic this plan already validated.
    pub fn tile_rect(&self, t: &HeifTile) -> HeifRect {
        HeifRect {
            x: t.col.saturating_mul(self.tile_w),
            y: t.row.saturating_mul(self.tile_h),
            w: self.tile_w,
            h: self.tile_h,
        }
    }

    /// The first pair of tiles whose file byte ranges OVERLAP, if any. Two tiles sharing bytes means
    /// the map is wrong, and a decoder fed a wrong map produces plausible garbage rather than an
    /// error — so this is a property worth being able to ask cheaply, both from the parity battery
    /// and from E3 before it submits.
    pub fn overlapping_tile_extents(&self) -> Option<(u32, u32)> {
        let mut spans: Vec<(u64, u64, u32)> = Vec::new();
        for t in &self.tiles {
            for e in &t.extents {
                if e.len > 0 {
                    spans.push((e.offset, e.end(), t.item_id));
                }
            }
        }
        spans.sort_unstable();
        for w in spans.windows(2) {
            if w[1].0 < w[0].1 {
                return Some((w[0].2, w[1].2));
            }
        }
        None
    }

    /// The total payload bytes the tiles occupy.
    pub fn tile_bytes(&self) -> u64 {
        self.tiles.iter().flat_map(|t| &t.extents).map(|e| e.len).sum()
    }

    /// The ONE diagnostic line `FALCON_HW_HEIC_PARSE=1` parks per file. Deliberately one line and
    /// deliberately grep-shaped: a field report is read with `findstr`, not parsed.
    pub fn summary(&self, name: &str) -> String {
        let hv = match (&self.hvcc, self.hvcc_by_prop.len()) {
            (Some(b), _) => format!("shared/{}B", b.len()),
            (None, n) => format!("split/{n}"),
        };
        format!(
            "hw heic parse: {name} {}x{} grid {}x{} tile {}x{} tiles={} mosaic {}x{} crop {}x{}+{}+{} \
             irot={} mirror={} depth={} hvcC={} payload={}KB items={} aux={}",
            self.display.0,
            self.display.1,
            self.cols,
            self.rows,
            self.tile_w,
            self.tile_h,
            self.tiles.len(),
            self.mosaic.0,
            self.mosaic.1,
            self.crop.w,
            self.crop.h,
            self.crop.x,
            self.crop.y,
            self.irot,
            match self.imir {
                None => "none",
                Some(HeifMirror::Vertical) => "v",
                Some(HeifMirror::Horizontal) => "h",
            },
            self.bit_depth.map(|d| d.to_string()).unwrap_or_else(|| "?".into()),
            hv,
            self.tile_bytes() / 1024,
            self.item_count,
            self.aux.len(),
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// meta
// ─────────────────────────────────────────────────────────────────────────────────────────────

fn read_meta(buf: &[u8], s: usize, e: usize) -> Meta {
    let mut m = Meta {
        primary: None,
        items: BTreeMap::new(),
        refs: Vec::new(),
        ipco: Vec::new(),
        ipma: Vec::new(),
        locs: BTreeMap::new(),
        idat: None,
    };
    for (t, cs, ce) in bmff_children(buf, s, e) {
        match &t {
            b"pitm" => m.primary = read_pitm(buf, cs, ce),
            b"iinf" => m.items = read_iinf(buf, cs, ce),
            b"iref" => m.refs = read_iref(buf, cs, ce),
            b"iloc" => m.locs = read_iloc(buf, cs, ce),
            b"idat" => m.idat = Some((cs, ce)),
            b"iprp" => {
                for (pt, ps, pe) in bmff_children(buf, cs, ce) {
                    match &pt {
                        b"ipco" => m.ipco = bmff_children(buf, ps, pe),
                        // Reused verbatim from the v0.8.105 preview walk — see
                        // `crate::bmff_ipma_entries`. Multiple `ipma` boxes are legal and simply
                        // concatenate.
                        b"ipma" if pe.saturating_sub(ps) >= 8 => {
                            m.ipma.extend(bmff_ipma_entries(buf, ps, pe))
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    m
}

fn read_pitm(buf: &[u8], s: usize, e: usize) -> Option<u32> {
    let mut c = Cur::new(buf, s, e);
    let (ver, _) = c.full_header()?;
    if ver >= 1 {
        c.u32()
    } else {
        c.u16().map(u32::from)
    }
}

fn read_iinf(buf: &[u8], s: usize, e: usize) -> BTreeMap<u32, ItemInfo> {
    let mut out = BTreeMap::new();
    let mut c = Cur::new(buf, s, e);
    let Some((ver, _)) = c.full_header() else { return out };
    // The declared count is advisory: the real bound is how many `infe` boxes the box actually
    // holds, which is what `bmff_children` reports. A crafted count of u32::MAX buys nothing.
    let _count = if ver >= 1 { c.u32() } else { c.u16().map(u32::from) };
    for (t, is, ie) in bmff_children(buf, c.p, e) {
        if &t != b"infe" {
            continue;
        }
        if let Some((id, info)) = read_infe(buf, is, ie) {
            out.insert(id, info);
        }
    }
    out
}

fn read_infe(buf: &[u8], s: usize, e: usize) -> Option<(u32, ItemInfo)> {
    let mut c = Cur::new(buf, s, e);
    let (ver, _) = c.full_header()?;
    if ver < 2 {
        // Versions 0/1 name no item TYPE at all — every item is an untyped "mdat item". Nothing in
        // the HEIF item model this stage plans for can be expressed that way, so decline rather
        // than invent a type.
        return None;
    }
    let id = if ver >= 3 { c.u32()? } else { c.u16()? as u32 };
    let _protection = c.u16()?;
    let item_type = c.fourcc()?;
    Some((id, ItemInfo { item_type }))
}

fn read_iref(buf: &[u8], s: usize, e: usize) -> Vec<([u8; 4], u32, Vec<u32>)> {
    let mut out = Vec::new();
    let mut c = Cur::new(buf, s, e);
    let Some((ver, _)) = c.full_header() else { return out };
    let wide = ver >= 1;
    for (t, rs, re) in bmff_children(buf, c.p, e) {
        let mut rc = Cur::new(buf, rs, re);
        let Some(from) = (if wide { rc.u32() } else { rc.u16().map(u32::from) }) else { continue };
        let Some(count) = rc.u16() else { continue };
        let mut to = Vec::new();
        for _ in 0..count {
            let Some(id) = (if wide { rc.u32() } else { rc.u16().map(u32::from) }) else { break };
            to.push(id);
        }
        out.push((t, from, to));
    }
    out
}

/// `iloc`, all three versions. The one box in HEIF where a lazy read turns into a wrong byte range
/// rather than a missing feature, so every field width is read from the header rather than assumed.
fn read_iloc(buf: &[u8], s: usize, e: usize) -> BTreeMap<u32, ItemLoc> {
    let mut out = BTreeMap::new();
    let mut c = Cur::new(buf, s, e);
    let Some((ver, _)) = c.full_header() else { return out };
    let Some(sizes) = c.u16() else { return out };
    let offset_size = (sizes >> 12) as usize & 0xf;
    let length_size = (sizes >> 8) as usize & 0xf;
    let base_offset_size = (sizes >> 4) as usize & 0xf;
    // The low nibble is `index_size` in versions 1 and 2, and reserved (must be 0) in version 0.
    let index_size = if ver == 1 || ver == 2 { sizes as usize & 0xf } else { 0 };
    let Some(item_count) = (if ver < 2 { c.u16().map(u32::from) } else { c.u32() }) else {
        return out;
    };
    for _ in 0..item_count {
        let Some(id) = (if ver < 2 { c.u16().map(u32::from) } else { c.u32() }) else { break };
        let construction = if ver == 1 || ver == 2 {
            // 12 reserved bits then a 4-bit construction_method.
            match c.u16() {
                Some(v) => match v & 0xf {
                    0 => HeifConstruction::File,
                    1 => HeifConstruction::Idat,
                    _ => HeifConstruction::Item,
                },
                None => break,
            }
        } else {
            HeifConstruction::File
        };
        let Some(data_reference_index) = c.u16() else { break };
        let Some(base_offset) = c.uint(base_offset_size) else { break };
        let Some(extent_count) = c.u16() else { break };
        // An extent record of ZERO bytes (index, offset and length all 0 wide) says "the whole of the
        // base": one is legal, more are no information. They cost nothing in the box and 16 bytes
        // each here, 65,535 per 6-byte item: gigabytes from a `meta` that fits the first 64 KiB, in
        // a read the Date taken sort now makes for every HEIC of a folder (14 November 2026, found
        // by the date-read review). Declined like a truncated list: keep what was read whole.
        if index_size + offset_size + length_size == 0 && extent_count > 1 {
            break;
        }
        let mut extents = Vec::new();
        let mut short = false;
        for _ in 0..extent_count {
            if (ver == 1 || ver == 2) && index_size > 0 && c.uint(index_size).is_none() {
                short = true;
                break;
            }
            let (Some(off), Some(len)) = (c.uint(offset_size), c.uint(length_size)) else {
                short = true;
                break;
            };
            extents.push((off, len));
        }
        out.insert(id, ItemLoc { construction, data_reference_index, base_offset, extents });
        if short {
            // A truncated extent list means every later item's fields are misaligned; keep what was
            // read whole and stop, exactly as `bmff_children` stops at the first bad header.
            break;
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Properties
// ─────────────────────────────────────────────────────────────────────────────────────────────

fn read_ispe(buf: &[u8], s: usize, e: usize) -> Option<(u32, u32)> {
    let mut c = Cur::new(buf, s, e);
    c.full_header()?;
    Some((c.u32()?, c.u32()?))
}

fn read_irot(buf: &[u8], s: usize, e: usize) -> Option<u16> {
    let mut c = Cur::new(buf, s, e);
    Some((c.u8()? as u16 & 0x3) * 90)
}

fn read_imir(buf: &[u8], s: usize, e: usize) -> Option<HeifMirror> {
    let mut c = Cur::new(buf, s, e);
    Some(if c.u8()? & 1 == 0 { HeifMirror::Horizontal } else { HeifMirror::Vertical })
}

fn read_clap(buf: &[u8], s: usize, e: usize) -> Option<HeifClap> {
    let mut c = Cur::new(buf, s, e);
    Some(HeifClap {
        width_n: c.i32()?,
        width_d: c.i32()?,
        height_n: c.i32()?,
        height_d: c.i32()?,
        horiz_off_n: c.i32()?,
        horiz_off_d: c.i32()?,
        vert_off_n: c.i32()?,
        vert_off_d: c.i32()?,
    })
}

fn read_pixi(buf: &[u8], s: usize, e: usize) -> Option<Vec<u8>> {
    let mut c = Cur::new(buf, s, e);
    c.full_header()?;
    let n = c.u8()? as usize;
    let mut v = Vec::with_capacity(n.min(16));
    for _ in 0..n {
        v.push(c.u8()?);
    }
    Some(v)
}

fn read_auxc(buf: &[u8], s: usize, e: usize) -> Option<String> {
    let mut c = Cur::new(buf, s, e);
    c.full_header()?;
    c.cstr()
}

/// The `clap` crop, or the whole grid output when there is none.
///
/// A clean aperture is a pair of RATIONALS and an offset from the CENTRE. Real-world `clap` boxes
/// are integral, but the box can express a rect that is not — and a crop rounded by a half pixel is
/// dimensions that silently disagree with the platform decoder's. So: exact or refuse.
fn apply_clap(output: (u32, u32), clap: Option<&HeifClap>) -> Result<HeifRect, HeifParseError> {
    let Some(c) = clap else {
        return Ok(HeifRect { x: 0, y: 0, w: output.0, h: output.1 });
    };
    let exact = |n: i32, d: i32| -> Option<i64> {
        if d == 0 {
            return None;
        }
        let (n, d) = (n as i64, d as i64);
        (n % d == 0).then_some(n / d)
    };
    let (Some(cw), Some(ch), Some(hx), Some(vy)) = (
        exact(c.width_n, c.width_d),
        exact(c.height_n, c.height_d),
        exact(c.horiz_off_n, c.horiz_off_d),
        exact(c.vert_off_n, c.vert_off_d),
    ) else {
        return Err(HeifParseError::Malformed("clap is not an exact integral crop"));
    };
    // ISO/IEC 14496-12: the centre of the clean aperture sits at ((W-1)/2 + horizOff,
    // (H-1)/2 + vertOff), so the left edge is that centre minus (cw-1)/2. Doubling throughout keeps
    // the arithmetic in integers; an odd result is a half-pixel edge, which is a refusal.
    let (w2, h2) = (output.0 as i64 * 2, output.1 as i64 * 2);
    let left2 = (w2 - 2) / 2 + hx * 2 - (cw * 2 - 2) / 2;
    let top2 = (h2 - 2) / 2 + vy * 2 - (ch * 2 - 2) / 2;
    if cw <= 0 || ch <= 0 || left2 % 2 != 0 || top2 % 2 != 0 {
        return Err(HeifParseError::Malformed("clap is not an exact integral crop"));
    }
    let (x, y) = (left2 / 2, top2 / 2);
    if x < 0 || y < 0 || x + cw > output.0 as i64 || y + ch > output.1 as i64 {
        return Err(HeifParseError::Malformed("clap falls outside the image"));
    }
    Ok(HeifRect { x: x as u32, y: y as u32, w: cw as u32, h: ch as u32 })
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Items → bytes
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// One item's data as ABSOLUTE file extents, whichever construction method `iloc` used.
fn resolve_extents(
    meta: &Meta,
    id: u32,
    file_len: u64,
) -> Result<(Vec<HeifExtent>, HeifConstruction), HeifParseError> {
    let loc = meta.locs.get(&id).ok_or(HeifParseError::UnresolvableLocation { item: id })?;
    // A non-zero data_reference_index means the bytes are in a DIFFERENT file, named by `dinf`.
    // Falcon opens one file; an item that is not in it has no plan.
    if loc.data_reference_index != 0 {
        return Err(HeifParseError::UnresolvableLocation { item: id });
    }
    let base = match loc.construction {
        HeifConstruction::File => 0u64,
        HeifConstruction::Idat => {
            let (s, _) = meta.idat.ok_or(HeifParseError::UnresolvableLocation { item: id })?;
            s as u64
        }
        HeifConstruction::Item => return Err(HeifParseError::UnresolvableLocation { item: id }),
    };
    let mut out = Vec::with_capacity(loc.extents.len());
    for (off, len) in &loc.extents {
        let start = base
            .checked_add(loc.base_offset)
            .and_then(|b| b.checked_add(*off))
            .ok_or(HeifParseError::ExtentOutOfBounds { item: id })?;
        let end = start.checked_add(*len).ok_or(HeifParseError::ExtentOutOfBounds { item: id })?;
        if end > file_len {
            return Err(HeifParseError::ExtentOutOfBounds { item: id });
        }
        // An `idat`-constructed extent must also stay inside the `idat` box itself, or it is
        // reading a neighbouring box's bytes through a legal-looking offset.
        if let (HeifConstruction::Idat, Some((_, ie))) = (loc.construction, meta.idat) {
            if end > ie as u64 {
                return Err(HeifParseError::ExtentOutOfBounds { item: id });
            }
        }
        out.push(HeifExtent { offset: start, len: *len });
    }
    if out.is_empty() {
        return Err(HeifParseError::UnresolvableLocation { item: id });
    }
    Ok((out, loc.construction))
}

/// A DERIVED item's payload (a `grid`'s 8 bytes, a `tmap`'s handful) copied out. Bounded hard —
/// this is the only place the parser allocates on a length the file chose.
fn read_derived_payload(
    buf: &[u8],
    meta: &Meta,
    id: u32,
    file_len: u64,
) -> Result<Vec<u8>, HeifParseError> {
    let (extents, _) = resolve_extents(meta, id, file_len)?;
    let total: u64 = extents.iter().map(|e| e.len).sum();
    if total == 0 || total > MAX_DERIVED_PAYLOAD {
        return Err(HeifParseError::GridPayloadUnreadable);
    }
    let mut out = Vec::with_capacity(total as usize);
    for e in &extents {
        let (s, en) = (e.offset as usize, e.end() as usize);
        out.extend_from_slice(buf.get(s..en).ok_or(HeifParseError::GridPayloadUnreadable)?);
    }
    Ok(out)
}

/// The `ImageGrid` payload: `version, flags, rows-1, cols-1` then the output size, 16- or 32-bit
/// per flags bit 0. Returns `(rows, cols, output_w, output_h)`.
fn parse_image_grid(p: &[u8]) -> Option<(u32, u32, u32, u32)> {
    let mut c = Cur::new(p, 0, p.len());
    let _version = c.u8()?;
    let flags = c.u8()?;
    let rows = c.u8()? as u32 + 1;
    let cols = c.u8()? as u32 + 1;
    let (w, h) = if flags & 1 == 1 {
        (c.u32()?, c.u32()?)
    } else {
        (c.u16()? as u32, c.u16()? as u32)
    };
    Some((rows, cols, w, h))
}

/// The `dimg` children of `id`, in declaration order — which for a grid IS raster order.
fn dimg_children(meta: &Meta, id: u32) -> Vec<u32> {
    meta.refs
        .iter()
        .filter(|(t, from, _)| t == b"dimg" && *from == id)
        .flat_map(|(_, _, to)| to.iter().copied())
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The inventory
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Every item that is neither the primary nor one of its tiles, with a REASON. Never fails: an
/// inventory that declined would tell a future stage less than one that reports what it could see.
/// `ipma_by_item` is v0.8.152 (R3-M3): the item→properties map the caller already built, rather
/// than this function's own `meta.ipma.iter().filter(..)` per item — the same quadratic the tile
/// loop had, over every declared item instead of every tile.
fn inventory(
    buf: &[u8],
    meta: &Meta,
    ipma_by_item: &BTreeMap<u32, Vec<usize>>,
    primary: u32,
    tile_ids: &[u32],
    file_len: u64,
) -> Vec<HeifAuxItem> {
    let tiles: BTreeSet<u32> = tile_ids.iter().copied().collect();
    let aux_of: BTreeSet<u32> =
        meta.refs.iter().filter(|(t, _, _)| t == b"auxl").map(|(_, f, _)| *f).collect();
    let thumb_of: BTreeSet<u32> =
        meta.refs.iter().filter(|(t, _, _)| t == b"thmb").map(|(_, f, _)| *f).collect();
    let other_grid_tiles: BTreeSet<u32> = meta
        .refs
        .iter()
        .filter(|(t, from, _)| t == b"dimg" && *from != primary)
        .flat_map(|(_, _, to)| to.iter().copied())
        .collect();
    let derived_from: BTreeSet<u32> =
        meta.refs.iter().filter(|(t, _, _)| t == b"dimg").map(|(_, f, _)| *f).collect();

    let mut out = Vec::new();
    for (&id, info) in &meta.items {
        if id == primary || tiles.contains(&id) {
            continue;
        }
        let why = if aux_of.contains(&id) {
            HeifSkip::Auxiliary
        } else if thumb_of.contains(&id) {
            HeifSkip::Thumbnail
        } else if matches!(&info.item_type, b"Exif" | b"mime" | b"uri ") {
            HeifSkip::Metadata
        } else if other_grid_tiles.contains(&id) {
            HeifSkip::TileOfAnother
        } else if derived_from.contains(&id) || matches!(&info.item_type, b"grid" | b"tmap" | b"iovl")
        {
            HeifSkip::Derived
        } else {
            HeifSkip::Unreferenced
        };
        // Only ask the cheap questions, and only of the items where the answer means something.
        let mut grid = None;
        let mut aux_type = None;
        let mut bit_depth = None;
        if &info.item_type == b"grid" {
            if let Ok(p) = read_derived_payload(buf, meta, id, file_len) {
                grid = parse_image_grid(&p);
            }
        }
        for ix in ipma_by_item.get(&id).map_or(&[][..], |v| v.as_slice()).iter().copied() {
            let Some((t, s, e)) = ix.checked_sub(1).and_then(|k| meta.ipco.get(k)).copied() else {
                continue;
            };
            match &t {
                b"auxC" => aux_type = read_auxc(buf, s, e),
                b"pixi" => bit_depth = read_pixi(buf, s, e).and_then(|p| p.first().copied()),
                _ => {}
            }
        }
        out.push(HeifAuxItem { item_id: id, item_type: info.item_type, why, grid, aux_type, bit_depth });
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The inert wiring
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// `FALCON_HW_HEIC_PARSE=1` — the field-diagnostic switch for E1. Read ONCE per process, exactly
/// like [`crate::classic_heic`]: a decode lane must not be able to change its mind mid-session.
///
/// When it is off — which is every shipping run — [`note_hw_heic_parse`] returns before touching the
/// file system, so the container parse costs one `OnceLock` read per HEIC decode and produces not
/// one log line. That is what "INERT" means here and the boot-verify is what proves it.
pub fn hw_heic_parse() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| hw_heic_parse_from_env(std::env::var("FALCON_HW_HEIC_PARSE").ok().as_deref()))
}

/// The switch's PARSE, split out so it is testable without touching the process environment — the
/// [`crate::classic_heic_from_env`] idiom, including that exactly `"1"` arms it.
#[inline]
pub fn hw_heic_parse_from_env(v: Option<&str>) -> bool {
    v == Some("1")
}

/// Park ONE line about this file, if the switch is on. Keyed on the path, so a folder of 500 HEICs
/// under five tiers writes 500 lines and not 2,500 — and a folder opened twice writes none the
/// second time.
pub(crate) fn note_hw_heic_parse(path: &Path) {
    if !hw_heic_parse() {
        return;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let line = match heif_decode_plan(path) {
        Ok(p) => format!(
            "{} gamut={:?}{}",
            p.grid.summary(&name),
            p.gamut,
            p.color_desc.map(|d| format!(" \"{d}\"")).unwrap_or_default()
        ),
        Err(e) => format!("hw heic parse: {name} DECLINED — {e}"),
    };
    crate::note_once(&format!("hwheic:{}", path.display()), line);
}

// ─────────────────────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The little ISO-BMFF box builders, the same shape the v0.8.105 preview-attribution row uses.
    fn bx(t: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(t);
        v.extend_from_slice(body);
        v
    }
    fn full(t: &[u8; 4], ver: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut b = vec![ver, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
        b.extend_from_slice(body);
        bx(t, &b)
    }

    fn infe(id: u16, typ: &[u8; 4]) -> Vec<u8> {
        let mut b = id.to_be_bytes().to_vec();
        b.extend_from_slice(&0u16.to_be_bytes()); // item_protection_index
        b.extend_from_slice(typ);
        b.push(0); // item_name = ""
        full(b"infe", 2, 0, &b)
    }

    /// The one thing wrong with this container, if anything is.
    #[derive(Default, Clone, Copy)]
    struct Opts {
        no_pitm: bool,
        short_dimg: bool,
        bad_ipma_index: bool,
        tile_without_ispe: bool,
        truncate_iloc: bool,
        overlap_tiles: bool,
        rotate: bool,
        mirror: bool,
        /// v0.8.153 (skeptic A / O3): a SECOND, legal `ipma` box that associates the primary with
        /// `irot` — an association the first box does not carry. Multiple `ipma` boxes concatenate
        /// (`read_meta`'s own arm says so), so this is the only shape where "first entry wins" and
        /// "union of every entry" give different photographs.
        second_ipma_rotates_primary: bool,
    }

    /// A minimal but REAL 2×2 grid container: `ftyp` + `meta`(pitm/iinf/iref/iprp/iloc/idat) +
    /// `mdat` holding four one-byte "tiles". Tiles are 4×4, the mosaic is 8×8 and the grid trims it
    /// to 7×6 — DELIBERATELY not square, so a rotation that failed to transpose could not hide.
    /// Every crafted row below is this container damaged in exactly one place, so what a row proves
    /// is the damage and not the fixture.
    fn build(opts: &Opts) -> Vec<u8> {
        // items: 1 = the grid, 2..=5 = its tiles, 6 = an `Exif` item nobody points at.
        let mut infes = infe(1, b"grid");
        for id in 2..=5u16 {
            infes.extend_from_slice(&infe(id, b"hvc1"));
        }
        infes.extend_from_slice(&infe(6, b"Exif"));
        let mut iinf_body = 6u16.to_be_bytes().to_vec();
        iinf_body.extend_from_slice(&infes);
        let iinf = full(b"iinf", 0, 0, &iinf_body);

        let pitm = if opts.no_pitm { Vec::new() } else { full(b"pitm", 0, 0, &1u16.to_be_bytes()) };

        // dimg: item 1 → [2,3,4,5], or one short.
        let n = if opts.short_dimg { 3u16 } else { 4u16 };
        let mut dimg = 1u16.to_be_bytes().to_vec();
        dimg.extend_from_slice(&n.to_be_bytes());
        for id in 2..2 + n {
            dimg.extend_from_slice(&id.to_be_bytes());
        }
        let iref = full(b"iref", 0, 0, &bx(b"dimg", &dimg));

        // ipco, 1-based: 1 = ispe(4×4) for tiles, 2 = hvcC, 3 = ispe(7×6) for the grid, 4 = irot 90.
        let ispe = |w: u32, h: u32| {
            let mut b = w.to_be_bytes().to_vec();
            b.extend_from_slice(&h.to_be_bytes());
            full(b"ispe", 0, 0, &b)
        };
        let ipco = bx(
            b"ipco",
            &[
                ispe(4, 4),
                bx(b"hvcC", &[0xAA, 0xBB, 0xCC]),
                ispe(7, 6),
                bx(b"irot", &[1]),
                bx(b"imir", &[1]), // direction = 1 => left/right flip about a vertical axis
            ]
            .concat(),
        );

        // ipma: the grid → [3] (+ the irot/imir when asked), each tile → [ispe, hvcC].
        let grid_props: Vec<u8> = if opts.bad_ipma_index {
            vec![99] // an index far past the end of `ipco`
        } else {
            let mut v = vec![3u8];
            if opts.rotate {
                v.push(4);
            }
            if opts.mirror {
                v.push(5);
            }
            v
        };
        let mut ipma_body = 5u32.to_be_bytes().to_vec(); // entry_count
        ipma_body.extend_from_slice(&1u16.to_be_bytes());
        ipma_body.push(grid_props.len() as u8);
        ipma_body.extend_from_slice(&grid_props);
        for id in 2..=5u16 {
            ipma_body.extend_from_slice(&id.to_be_bytes());
            if opts.tile_without_ispe && id == 3 {
                ipma_body.extend_from_slice(&[1, 2]); // hvcC only — no ispe
            } else {
                ipma_body.extend_from_slice(&[2, 1, 2]);
            }
        }
        // A SECOND `ipma` for item 1 — one entry, associating `irot` (property 4) and nothing else.
        // Legal per the spec and per `read_meta`'s own arm, and the only fixture in this module
        // where a repeated item id carries DIFFERENT associations.
        let ipma2: Vec<u8> = if opts.second_ipma_rotates_primary {
            let mut b = 1u32.to_be_bytes().to_vec(); // entry_count = 1
            b.extend_from_slice(&1u16.to_be_bytes()); // item 1, the grid
            b.push(1); // association_count = 1
            b.push(4); // → irot 90
            full(b"ipma", 0, 0, &b)
        } else {
            Vec::new()
        };
        let iprp = bx(b"iprp", &[ipco, full(b"ipma", 0, 0, &ipma_body), ipma2].concat());

        // `idat` holds the 8-byte ImageGrid payload; the four tiles live in `mdat`.
        let grid_payload: Vec<u8> = vec![0, 0, 1, 1, 0, 7, 0, 6]; // 2 rows, 2 cols, 7×6
        let idat = bx(b"idat", &grid_payload);

        // `iloc` v1: item 1 from `idat` (construction 1), items 2..=5 from the file.
        let iloc_for = |mdat_data_off: u64| -> Vec<u8> {
            // offset_size = 4, length_size = 4, base_offset_size = 0, index_size = 0.
            let mut body = 0x4400u16.to_be_bytes().to_vec();
            body.extend_from_slice(&5u16.to_be_bytes()); // item_count
            body.extend_from_slice(&1u16.to_be_bytes()); // item 1
            body.extend_from_slice(&1u16.to_be_bytes()); // construction_method = 1 (idat)
            body.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
            body.extend_from_slice(&1u16.to_be_bytes()); // extent_count
            body.extend_from_slice(&0u32.to_be_bytes()); // extent_offset (into idat)
            body.extend_from_slice(&(grid_payload.len() as u32).to_be_bytes());
            for k in 0..4u32 {
                body.extend_from_slice(&((2 + k) as u16).to_be_bytes());
                body.extend_from_slice(&0u16.to_be_bytes()); // construction_method = 0 (file)
                body.extend_from_slice(&0u16.to_be_bytes());
                body.extend_from_slice(&1u16.to_be_bytes());
                body.extend_from_slice(
                    &(mdat_data_off as u32 + if opts.overlap_tiles { 0 } else { k }).to_be_bytes(),
                );
                body.extend_from_slice(&1u32.to_be_bytes());
            }
            if opts.truncate_iloc {
                // A well-formed BOX whose CONTENTS stop mid-item: the last two items' fields are
                // simply not there. `read_iloc` must keep what it read whole and stop.
                body.truncate(body.len() - 18);
            }
            full(b"iloc", 1, 0, &body)
        };

        let assemble = |iloc: &[u8]| -> (Vec<u8>, u64) {
            let meta = full(
                b"meta",
                0,
                0,
                &[pitm.clone(), iinf.clone(), iref.clone(), iprp.clone(), iloc.to_vec(), idat.clone()]
                    .concat(),
            );
            let mut out = bx(b"ftyp", b"heic\0\0\0\0mif1heic");
            out.extend_from_slice(&meta);
            let mdat_data_off = out.len() as u64 + 8;
            out.extend_from_slice(&bx(b"mdat", &[1, 2, 3, 4]));
            (out, mdat_data_off)
        };
        // Pass 1 measures where `mdat`'s payload lands; pass 2 writes the real offsets. `iloc`'s own
        // length cannot change between the passes, and the assert is what keeps that true.
        let (_, off) = assemble(&iloc_for(0));
        let (bytes, off2) = assemble(&iloc_for(off));
        assert_eq!(off, off2, "the fixture's iloc length must not depend on the offsets it holds");
        bytes
    }

    /// ROW 0 — the fixture itself parses, and everything the plan claims about it is true. Without
    /// this row the damaged rows below prove only that damage is damage.
    #[test]
    fn the_synthetic_grid_parses_whole() {
        let p = parse_heif_grid(&build(&Opts::default())).expect("the well-formed fixture parses");
        assert_eq!((p.rows, p.cols), (2, 2));
        assert_eq!((p.tile_w, p.tile_h), (4, 4));
        assert_eq!(p.mosaic, (8, 8), "2 cols × 4 px by 2 rows × 4 px");
        assert_eq!(p.grid_output, (7, 6), "the grid trims the tile padding away");
        assert_eq!(p.crop, HeifRect { x: 0, y: 0, w: 7, h: 6 });
        assert_eq!(p.display, (7, 6));
        assert_eq!(p.tiles.len(), 4);
        assert_eq!(
            p.tiles.iter().map(|t| (t.item_id, t.row, t.col)).collect::<Vec<_>>(),
            vec![(2, 0, 0), (3, 0, 1), (4, 1, 0), (5, 1, 1)],
            "dimg order IS raster order"
        );
        assert_eq!(p.hvcc.as_deref(), Some(&[0xAAu8, 0xBB, 0xCC][..]), "one shared hvcC");
        assert_eq!(p.hvcc_by_prop.len(), 1);
        assert!(p.tiles.iter().all(|t| t.construction == HeifConstruction::File));
        assert_eq!(p.overlapping_tile_extents(), None);
        assert_eq!(p.compatible_brands, vec![*b"heic", *b"mif1"], "a SET: sorted, deduped");
        // The Exif item is inventory, with a reason.
        assert_eq!(p.aux.len(), 1);
        assert_eq!((p.aux[0].item_id, p.aux[0].why), (6, HeifSkip::Metadata));
        // Every emitted extent is inside the file.
        for t in &p.tiles {
            for e in &t.extents {
                assert!(e.end() <= p.file_len, "extent {e:?} past the {}-byte file", p.file_len);
            }
        }
    }

    /// ROW 1 — `irot` reaches the DISPLAY dimensions, which is the whole reason it is read. The
    /// fixture's 7×6 crop is deliberately not square, so the falsifier is direct: delete the
    /// `90 | 270` transpose arm and this row goes red on the very next line.
    #[test]
    fn irot_transposes_the_display_dimensions() {
        let p = parse_heif_grid(&build(&Opts { rotate: true, ..Default::default() }))
            .expect("the rotated fixture must parse");
        assert_eq!(p.irot, 90);
        assert_eq!(p.crop, HeifRect { x: 0, y: 0, w: 7, h: 6 }, "rotation does not move the crop");
        assert_eq!(p.display, (6, 7), "…it transposes what the crop is displayed as");
        assert_eq!(p.imir, None, "and rotation is not mirroring");
    }

    /// ROW 1b — `imir` is READ and does NOT touch the dimensions. No file in the corpus carries one
    /// (the investigation checked: no `imir`, no `clap`, on any of the six), so without this row the
    /// field would ship to E3 as an untested `None` that nobody had ever seen hold a value. Both
    /// halves matter: a mirror that failed to parse would silently un-flip a photograph, and a
    /// mirror that transposed the dimensions would break parity on the first file that carries one.
    #[test]
    fn imir_is_read_and_leaves_the_dimensions_alone() {
        let p = parse_heif_grid(&build(&Opts { mirror: true, ..Default::default() }))
            .expect("the mirrored fixture must parse");
        assert_eq!(p.imir, Some(HeifMirror::Vertical), "bit 1 flips left/right about a vertical axis");
        assert_eq!(p.display, (7, 6), "mirroring never changes the displayed size");
        // …and both transforms together still only transpose once.
        let both = parse_heif_grid(&build(&Opts { mirror: true, rotate: true, ..Default::default() }))
            .expect("rotated AND mirrored must parse");
        assert_eq!((both.irot, both.imir), (90, Some(HeifMirror::Vertical)));
        assert_eq!(both.display, (6, 7));
    }

    #[test]
    fn container_transform_order_and_both_mirror_directions() {
        // Explicit EXIF oracles, including IMG_3223's CCW90 + top/bottom flip = transpose (5).
        for (bit, expected) in [(0, [4, 5, 2, 7]), (1, [2, 7, 4, 5])] {
            for rot in 0..4u8 {
                let mut bytes = build(&Opts { mirror: true, rotate: true, ..Default::default() });
                let r = bytes.windows(4).position(|w| w == b"irot").unwrap() + 4;
                let m = bytes.windows(4).position(|w| w == b"imir").unwrap() + 4;
                bytes[r] = rot;
                bytes[m] = bit;
                assert_eq!(primary_orientation_from_bytes(&bytes), Some(expected[rot as usize]));
                let plan = parse_heif_grid(&bytes).unwrap();
                assert_eq!(plan.imir, Some(if bit == 0 { HeifMirror::Horizontal } else { HeifMirror::Vertical }));
                // Reverse ONLY the associated transform order, keeping properties in place.
                let associations = bytes.windows(5).position(|w| w == [0, 1, 3, 3, 4]).unwrap();
                bytes.swap(associations + 4, associations + 5);
                let reversed = parse_heif_grid(&bytes).unwrap();
                let expected_rot = (4 - rot as usize) % 4;
                assert_eq!(primary_orientation_from_bytes(&bytes), Some(expected[expected_rot]));
                assert_eq!(reversed.display, plan.display);
                if rot & 1 == 1 { assert_ne!(reversed.imir, plan.imir); }
            }
        }
    }

    #[test]
    fn explicit_half_turn_and_square_orientation_do_not_need_dimension_inference() {
        let mut bytes = build(&Opts { rotate: true, ..Default::default() });
        let r = bytes.windows(4).position(|w| w == b"irot").unwrap() + 4;
        bytes[r] = 2;
        assert_eq!(primary_orientation_from_bytes(&bytes), Some(3));
        // Header dimensions cannot affect the declared transformation, including square sources.
        let indices: Vec<_> = bytes.windows(4).enumerate().filter_map(|(i, w)| (w == b"ispe").then_some(i)).collect();
        for i in indices { bytes[i+8..i+16].copy_from_slice(&[0,0,0,8,0,0,0,8]); }
        for (rval, expected) in [(0,1),(1,8),(2,3),(3,6)] {
            bytes[r] = rval;
            assert_eq!(primary_orientation_from_bytes(&bytes), Some(expected));
        }
    }

    /// ROW 1c — v0.8.153 (skeptic A / O3): **A REPEATED `ipma` ITEM ID IS FIRST-WINS FOR THE
    /// PROPERTY LOOKUP, AND UNION ONLY FOR THE INVENTORY.**
    ///
    /// v0.8.152's R3-M3 replaced `meta.ipma.iter().find(..)` — a linear scan whose semantics were
    /// "the FIRST entry for this id" — with one map that CONCATENATED repeated ids, on the reasoning
    /// that a union cannot hide an association. For the inventory that holds. For the property
    /// lookup it inverts the answer: `irot`, `imir`, `clap`, `ispe`, `colr` and each tile's `hvcC`
    /// are `match` arms that ASSIGN, so a second entry's associations do not add to the first's,
    /// they REPLACE them.
    ///
    /// This fixture is the well-formed grid plus one extra, legal `ipma` box associating the primary
    /// with `irot` 90. Under first-wins the photo is what v0.8.151 produced: unrotated, 7×6. Under
    /// the union it silently becomes a 6×7 portrait — a wrong picture from a file that parsed
    /// cleanly, which is precisely the wrong-parameter-set class this epic exists to close.
    ///
    /// FALSIFIER (L28): spell the `ipma_first` builder `or_default().extend_from_slice(props)` — the
    /// v0.8.152 union — and both assertions below redden together.
    #[test]
    fn a_repeated_ipma_id_takes_the_first_entry_not_the_union() {
        let opts = Opts { second_ipma_rotates_primary: true, ..Default::default() };
        let p = parse_heif_grid(&build(&opts)).expect("two ipma boxes are legal — this must parse");
        assert_eq!(
            p.irot, 0,
            "the FIRST ipma entry for item 1 carries no irot; a second entry must not reach in"
        );
        assert_eq!(
            p.display,
            (7, 6),
            "…and the photograph therefore keeps the orientation v0.8.151 gave it"
        );
        // The union is still what the INVENTORY sees — it reports what it can find, and its two
        // questions (`auxC`, `pixi`) are additive rather than exclusive. Item 6 has no properties
        // in either box, so the row is unchanged and the inventory's contract is intact.
        assert_eq!(p.aux.len(), 1);
        assert_eq!((p.aux[0].item_id, p.aux[0].why), (6, HeifSkip::Metadata));
    }

    /// ROWS 2-6 — the CRAFTED damage table. Each is the well-formed fixture with exactly one thing
    /// wrong, and each must decline with a NAMED error rather than panic or improvise. The named
    /// error matters as much as the decline: "it returned Err" would pass even if every row failed
    /// for the same accidental reason.
    #[test]
    fn crafted_damage_declines_with_a_reason() {
        /// What the row is called, how the fixture is damaged, and which decline it must produce.
        type Row = (&'static str, Opts, fn(&HeifParseError) -> bool);
        let cases: Vec<Row> = vec![
            ("absent pitm", Opts { no_pitm: true, ..Default::default() }, |e| {
                matches!(e, HeifParseError::NoPrimaryItem)
            }),
            (
                "grid tile without dimensions",
                Opts { tile_without_ispe: true, ..Default::default() },
                |e| matches!(e, HeifParseError::TileDimensions),
            ),
            ("ipma pointing past ipco", Opts { bad_ipma_index: true, ..Default::default() }, |e| {
                matches!(e, HeifParseError::Malformed("ipma index past ipco"))
            }),
            ("dimg one tile short", Opts { short_dimg: true, ..Default::default() }, |e| {
                matches!(e, HeifParseError::TileCountMismatch { want: 4, got: 3 })
            }),
            ("truncated iloc", Opts { truncate_iloc: true, ..Default::default() }, |e| {
                matches!(e, HeifParseError::UnresolvableLocation { item: 4 })
            }),
        ];
        for (what, opts, ok) in cases {
            match parse_heif_grid(&build(&opts)) {
                Ok(p) => {
                    panic!("{what}: must decline, got a plan for {}×{}", p.display.0, p.display.1)
                }
                Err(e) => assert!(ok(&e), "{what}: declined with the wrong reason — {e}"),
            }
        }
    }

    /// ROW 7 — the overlap detector detects. Two tiles pointed at the SAME byte is a tile map that
    /// would feed a decoder plausible garbage rather than an error, and the parity battery asks this
    /// of every real file — so the asking itself has to be proven on a file where the answer is yes.
    #[test]
    fn overlapping_tile_extents_are_reported() {
        let p = parse_heif_grid(&build(&Opts { overlap_tiles: true, ..Default::default() }))
            .expect("overlap is not a parse failure, it is a property of the plan");
        assert!(
            p.overlapping_tile_extents().is_some(),
            "four tiles at one offset must be reported as overlapping"
        );
        let clean = parse_heif_grid(&build(&Opts::default())).unwrap();
        assert_eq!(clean.overlapping_tile_extents(), None, "…and the honest fixture must not be");
    }

    /// ROW 8 — TRUNCATION AT EVERY LENGTH. The container is cut at each of its byte lengths and the
    /// parser must decline every one of them without unwinding. The cheapest possible proof that no
    /// read in the module is unchecked, and it reaches boundary lengths a random byte-flip fuzz
    /// would need many thousands of trials to stumble onto.
    #[test]
    fn every_truncation_declines_softly() {
        let bytes = build(&Opts::default());
        for cut in 0..bytes.len() {
            assert!(
                parse_heif_grid(&bytes[..cut]).is_err(),
                "a container cut to {cut} bytes must not yield a plan"
            );
        }
        assert!(parse_heif_grid(&bytes).is_ok(), "…and the uncut one still parses");
    }

    /// ROW 8b — BYTE-FLIP FUZZ, in-tree. 5,000 single-byte mutations of the synthetic container,
    /// deterministically seeded so a failure is reproducible. Nothing about the outcome is asserted
    /// except that the parser RETURNED: a mutated container may legitimately still parse (a flipped
    /// `Exif` fourcc changes nothing structural), may decline, and must never unwind. The real
    /// containers get the same treatment in `tests/heic_grid_parity.rs`, which needs the testkit;
    /// this row runs everywhere, including a codec-less CI box.
    #[test]
    fn byte_flips_never_panic() {
        let base = build(&Opts::default());
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut parsed = 0usize;
        for _ in 0..5_000 {
            // xorshift64* — a deterministic PRNG in four lines, so no dev-dependency is needed.
            seed ^= seed >> 12;
            seed ^= seed << 25;
            seed ^= seed >> 27;
            let r = seed.wrapping_mul(0x2545F4914F6CDD1D);
            let mut m = base.clone();
            let at = (r % base.len() as u64) as usize;
            m[at] ^= 1u8 << ((r >> 32) % 8);
            if parse_heif_grid(&m).is_ok() {
                parsed += 1;
            }
        }
        // Not a tolerance — a witness that the fuzz was actually reaching the parser rather than
        // being rejected at the front door 5,000 times in a row.
        assert!(parsed > 0, "no mutated container parsed at all — the fuzz is not exercising anything");
    }

    /// ROW 9 — the env switch reads exactly `"1"`, the [`crate::classic_heic_from_env`] rule. A user
    /// who writes `FALCON_HW_HEIC_PARSE=0` means off, and an any-non-empty reading would hand them
    /// the opposite of what they asked.
    #[test]
    fn the_parse_switch_reads_only_the_exact_flag() {
        assert!(hw_heic_parse_from_env(Some("1")));
        assert!(!hw_heic_parse_from_env(Some("0")));
        assert!(!hw_heic_parse_from_env(Some("true")));
        assert!(!hw_heic_parse_from_env(Some("")));
        assert!(!hw_heic_parse_from_env(None));
    }

    /// ROW 10 — `clap` is exact or it is a refusal. No file in the corpus carries one, so this is
    /// the only place the rule is exercised; without it the "exact or refuse" claim in
    /// [`apply_clap`] would be a comment rather than a behaviour.
    #[test]
    fn clap_is_applied_exactly_or_refused() {
        let whole = apply_clap((100, 80), None).unwrap();
        assert_eq!(whole, HeifRect { x: 0, y: 0, w: 100, h: 80 });
        // A centred 50×40 crop of a 100×80 image: offsets 0, edges at 25 and 20.
        let centred = HeifClap {
            width_n: 50,
            width_d: 1,
            height_n: 40,
            height_d: 1,
            horiz_off_n: 0,
            horiz_off_d: 1,
            vert_off_n: 0,
            vert_off_d: 1,
        };
        assert_eq!(
            apply_clap((100, 80), Some(&centred)).unwrap(),
            HeifRect { x: 25, y: 20, w: 50, h: 40 }
        );
        // A width of 50/3 is not a whole number of pixels → refuse, never round.
        let ragged = HeifClap { width_d: 3, ..centred };
        assert!(apply_clap((100, 80), Some(&ragged)).is_err());
        // A zero denominator is a division that must not happen.
        let zero = HeifClap { width_d: 0, ..centred };
        assert!(apply_clap((100, 80), Some(&zero)).is_err());
        // A crop bigger than the image is outside it.
        let huge = HeifClap { width_n: 500, ..centred };
        assert!(apply_clap((100, 80), Some(&huge)).is_err());
    }

    /// A crafted `iloc` whose extent records are zero bytes wide may not buy memory: 65,535 extents
    /// per 6-byte item would be a megabyte each, gigabytes from an index that fits the date read's
    /// 64 KiB head (and the Date taken sort reads every HEIC of a folder). One zero-width extent,
    /// "the whole of the base", stays legal.
    ///
    /// FALSIFIER (L28): drop the `record == 0 && extent_count > 1` refusal in `read_iloc` and the
    /// eight items hold 524,280 extents (seen failing so on the code before 14 November 2026).
    #[test]
    fn zero_width_iloc_extents_buy_no_memory() {
        // Version 0 with every field width 0: an item is its id, data reference and extent count.
        let iloc = |items: &[(u16, u16)]| {
            let mut b = vec![0u8; 4]; // version 0, flags 0
            b.extend_from_slice(&0u16.to_be_bytes()); // offset, length and base offset sizes: 0
            b.extend_from_slice(&(items.len() as u16).to_be_bytes());
            for &(id, extent_count) in items {
                for v in [id, 0, extent_count] {
                    b.extend_from_slice(&v.to_be_bytes());
                }
            }
            b
        };
        let crafted = iloc(&(1..=8).map(|id| (id, u16::MAX)).collect::<Vec<_>>());
        let held: usize = read_iloc(&crafted, 0, crafted.len()).values().map(|l| l.extents.len()).sum();
        assert!(held <= 8, "a {}-byte iloc held {held} extents", crafted.len());
        // The legal form is still read: one zero-width extent per item.
        let legal = iloc(&[(1, 1), (2, 1)]);
        let locs = read_iloc(&legal, 0, legal.len());
        assert_eq!(locs.len(), 2);
        assert!(locs.values().all(|l| l.extents == [(0, 0)]));
    }
}
