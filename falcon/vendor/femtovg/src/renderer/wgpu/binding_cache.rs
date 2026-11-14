// Falcon patch, 2026-11-14. See FALCON-CHANGES.md ("keep and reuse the wgpu binding objects").
// The renderer's bookkeeping for the kept binding objects, as plain logic: no wgpu and no crate
// types, so Falcon's native test binary includes this file and runs its tests (the crate's own unit
// tests need a font that the published crate does not ship).

use std::collections::{HashMap, HashSet};

/// The distance between two fragment-uniform records in the shared buffer: one record rounded up to
/// the device's `min_uniform_buffer_offset_alignment`, because a dynamic offset must be a multiple of
/// it (256 on the devices Slint creates, whose limits are wgpu's WebGL2 defaults).
pub(super) fn uniform_stride(record_bytes: u64, alignment: u32) -> u64 {
    let alignment = u64::from(alignment.max(1));
    record_bytes.div_ceil(alignment) * alignment
}

/// The sampler slot (0 to 7) of one filter/repeat combination; one sampler per slot is kept.
pub(super) fn sampler_slot(nearest: bool, repeat_x: bool, repeat_y: bool) -> usize {
    usize::from(nearest) | (usize::from(repeat_x) << 1) | (usize::from(repeat_y) << 2)
}

/// One flush's fragment-uniform records, one per `stride` bytes, copied to the GPU after recording.
/// A record equal to the one before it reuses that slot (the same test the original made before
/// reusing its bind group: `f32` equality of every value).
pub(super) struct UniformRecords {
    bytes: Vec<u8>,
    stride: usize,
    capacity: u64,
    last: Vec<f32>,
    last_offset: Option<u32>,
}

impl UniformRecords {
    /// `capacity` is the flush's counted upper bound of records; the shared buffer holds that many.
    pub(super) fn new(record_bytes: u64, stride: u64, capacity: u64) -> Self {
        assert!(stride >= record_bytes, "a uniform record must fit its slot");
        Self {
            bytes: Vec::with_capacity((capacity * stride) as usize),
            stride: stride as usize,
            capacity,
            last: Vec::new(),
            last_offset: None,
        }
    }

    /// Appends `record` (or reuses the previous slot when equal) and returns its dynamic offset.
    pub(super) fn push(&mut self, record: &[f32]) -> u32 {
        if let Some(offset) = self.last_offset {
            if self.last.as_slice() == record {
                return offset;
            }
        }
        let offset = self.bytes.len();
        assert!(
            ((offset / self.stride) as u64) < self.capacity,
            "femtovg wgpu: more uniform records than counted for this flush"
        );
        for value in record {
            self.bytes.extend_from_slice(&value.to_ne_bytes());
        }
        debug_assert!(self.bytes.len() <= offset + self.stride);
        self.bytes.resize(offset + self.stride, 0);
        self.last.clear();
        self.last.extend_from_slice(record);
        self.last_offset = Some(offset as u32);
        offset as u32
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Values (bind groups) kept per pair (main image uid, glyph image uid); uid 0 means "no image".
/// An entry goes when either of its images goes. Stored by main image, so dropping an image's
/// entries costs a lookup; a glyph image (a text atlas, a handful at a time) is looked for in every
/// entry only when it really is the glyph half of some pair.
#[derive(Debug)]
pub(super) struct PairCache<V> {
    by_main: HashMap<u64, Vec<(u64, V)>>,
    glyph_uids: HashSet<u64>,
}

impl<V> Default for PairCache<V> {
    fn default() -> Self {
        Self { by_main: HashMap::new(), glyph_uids: HashSet::new() }
    }
}

impl<V> PairCache<V> {
    pub(super) fn get(&self, main: u64, glyph: u64) -> Option<&V> {
        self.by_main.get(&main)?.iter().find(|(g, _)| *g == glyph).map(|(_, value)| value)
    }

    /// Adds an entry for a pair that has none (the caller looked first).
    pub(super) fn insert(&mut self, main: u64, glyph: u64, value: V) {
        debug_assert!(self.get(main, glyph).is_none());
        self.by_main.entry(main).or_default().push((glyph, value));
        if glyph != 0 {
            self.glyph_uids.insert(glyph);
        }
    }

    /// Drops every entry that has `uid` as either image.
    pub(super) fn evict(&mut self, uid: u64) {
        self.by_main.remove(&uid);
        if self.glyph_uids.remove(&uid) {
            self.by_main.retain(|_, pairs| {
                pairs.retain(|(glyph, _)| *glyph != uid);
                !pairs.is_empty()
            });
        }
    }

    pub(super) fn clear(&mut self) {
        self.by_main.clear();
        self.glyph_uids.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_main.values().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::{sampler_slot, uniform_stride, PairCache, UniformRecords};

    /// FALSIFIER: return `record_bytes` unrounded and the 64 and 256 rows fail (a dynamic offset
    /// that is not a multiple of the alignment is a wgpu validation error at draw time).
    #[test]
    fn uniform_stride_rounds_one_record_up_to_the_offset_alignment() {
        for (alignment, stride) in [(0, 224), (1, 224), (16, 224), (64, 256), (256, 256)] {
            assert_eq!(uniform_stride(224, alignment), stride, "alignment {alignment}");
            assert_eq!(stride % u64::from(alignment.max(1)), 0);
        }
    }

    /// FALSIFIER: drop the equality test in `push` and the second push takes a new slot; compare
    /// only the record's first value and the third push (same first value) wrongly shares slot 0.
    #[test]
    fn consecutive_equal_records_share_a_slot_and_unequal_ones_do_not() {
        let a = [1.0f32, 2.0, 3.0, 4.0];
        let b = [1.0f32, 2.0, 3.0, 5.0];
        let mut records = UniformRecords::new(16, 32, 8);
        assert_eq!(records.push(&a), 0);
        assert_eq!(records.push(&a), 0, "an equal record right after reuses its slot");
        assert_eq!(records.push(&b), 32, "an unequal record takes the next slot");
        assert_eq!(records.push(&a), 64, "only the record just before is shared");
        let bytes = records.bytes();
        assert_eq!(bytes.len(), 96, "one stride per slot");
        let slot = |n: usize| &bytes[n * 32..n * 32 + 16];
        let le = |r: &[f32]| r.iter().flat_map(|v| v.to_ne_bytes()).collect::<Vec<u8>>();
        assert_eq!(slot(0), le(&a).as_slice());
        assert_eq!(slot(1), le(&b).as_slice());
        assert_eq!(slot(2), le(&a).as_slice());
        assert!(bytes[16..32].iter().all(|&byte| byte == 0), "the padding is zero");
    }

    /// FALSIFIER: remove the capacity assert in `push` and this writes past the counted records
    /// (on the GPU: a dynamic offset beyond the buffer, a validation error) instead of stopping.
    #[test]
    #[should_panic(expected = "more uniform records than counted")]
    fn more_records_than_counted_stop_at_once() {
        let mut records = UniformRecords::new(4, 4, 2);
        records.push(&[1.0]);
        records.push(&[2.0]);
        records.push(&[3.0]);
    }

    /// FALSIFIER: make `evict` only remove the main-image entries (drop the glyph branch) and the
    /// pairs whose glyph image went stay (they would keep the dead atlas texture alive); make it
    /// remove everything and the "not before" checks fail.
    #[test]
    fn a_pair_goes_when_either_of_its_images_goes_and_not_before() {
        let mut cache = PairCache::default();
        for (main, glyph) in [(5, 0), (0, 9), (5, 9), (6, 0), (7, 9), (0, 0)] {
            cache.insert(main, glyph, (main, glyph));
        }
        assert_eq!(cache.len(), 6);
        cache.evict(8);
        assert_eq!(cache.len(), 6, "an image with no entries changes nothing");
        cache.evict(5);
        assert_eq!(cache.get(5, 0), None);
        assert_eq!(cache.get(5, 9), None);
        assert_eq!(cache.len(), 4, "only the pairs with image 5 went");
        assert_eq!(cache.get(0, 9), Some(&(0, 9)));
        cache.evict(9);
        assert_eq!(cache.get(0, 9), None, "a glyph image takes its pairs with it");
        assert_eq!(cache.get(7, 9), None);
        assert_eq!(cache.get(6, 0), Some(&(6, 0)));
        assert_eq!(cache.get(0, 0), Some(&(0, 0)), "no image, no glyph: kept");
        assert_eq!(cache.len(), 2);
        cache.insert(7, 11, (7, 11));
        cache.evict(7);
        assert_eq!(cache.get(7, 11), None);
        cache.clear();
        assert_eq!(cache.len(), 0);
    }

    /// FALSIFIER: leave out any one flag (for example `repeat_x`) and two combinations share a
    /// slot, so an image would be drawn with another combination's sampler.
    #[test]
    fn the_sampler_slot_covers_every_flag_combination() {
        let mut seen = [false; 8];
        for bits in 0..8u8 {
            let slot = sampler_slot(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
            assert!(slot < 8);
            assert!(!seen[slot], "slot {slot} given twice");
            seen[slot] = true;
        }
    }
}
