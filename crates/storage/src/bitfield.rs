//! Sparse, growable bitfield — a domain-agnostic local **presence map** over
//! `u64`-indexed blocks (which indices a holder currently has).
//!
//! Clean-room reimplementation of the *behaviour* of upstream hypercore's
//! `lib/bitfield.js` (ported via `reference/js/hypercore/test/bitfield.js`). We are
//! **not** disk- or wire-compatible (ADR-0001): upstream threads fixed-size pages
//! through a storage transaction (`open`/`flush`) and chunks them for peers (`want`)
//! — persistence and replication framing, both out of scope per the relevance filter
//! (`docs/UPSTREAM_TEST_MAP.md`). What is in scope, and L1, is the pure data
//! structure and its query semantics: get / set / set-range / count / find-first /
//! find-last.
//!
//! The field is conceptually an **infinite** sequence of bits, every one `false`
//! until set. Storage is **paged and sparse** — a [`BTreeMap`] from page index to a
//! fixed-size page — so a single bit set at index `8e15` costs one 4 KiB page, not a
//! petabyte. A missing page is semantically identical to an all-`false` page; we never
//! materialize a page just to clear bits in it (mirroring upstream's `if (!p && val)`).

use std::collections::BTreeMap;

/// Bits per page. `2^15`, matching upstream's page granularity so the page/segment
/// boundary behaviours line up; the exact value is otherwise an internal detail.
const BITS_PER_PAGE: u64 = 32_768;
/// 64-bit words per page (`32768 / 64 = 512`).
const WORDS_PER_PAGE: usize = (BITS_PER_PAGE / 64) as usize;

/// One page: `BITS_PER_PAGE` bits packed into [`WORDS_PER_PAGE`] little-endian words
/// (bit `b` of the page lives in word `b / 64`, position `b % 64`).
type Page = [u64; WORDS_PER_PAGE];

/// A sparse, unbounded bitfield. Default is all-`false`.
#[derive(Clone, Debug, Default)]
pub struct Bitfield {
    pages: BTreeMap<u64, Box<Page>>,
}

/// Split a global bit index into `(page, word_in_page, bit_in_word)`.
#[inline]
fn locate(index: u64) -> (u64, usize, u64) {
    let page = index / BITS_PER_PAGE;
    let bit_in_page = index % BITS_PER_PAGE;
    ((page), (bit_in_page / 64) as usize, bit_in_page % 64)
}

/// A mask selecting bits `[lo, hi)` *within a single 64-bit word* (`0 <= lo <= hi <= 64`).
#[inline]
fn word_mask(lo: u64, hi: u64) -> u64 {
    let n = hi - lo;
    if n == 0 {
        0
    } else if n == 64 {
        u64::MAX
    } else {
        ((1u64 << n) - 1) << lo
    }
}

/// Set (or clear) the page-local bit range `[lo, hi)` (`0 <= lo <= hi <= BITS_PER_PAGE`).
fn page_set_range(page: &mut Page, lo: u64, hi: u64, value: bool) {
    let mut b = lo;
    while b < hi {
        let word = (b / 64) as usize;
        let word_top = (word as u64 + 1) * 64;
        let top = hi.min(word_top);
        let mask = word_mask(b % 64, top - word as u64 * 64);
        if value {
            page[word] |= mask;
        } else {
            page[word] &= !mask;
        }
        b = top;
    }
}

/// Count set bits in the page-local range `[lo, hi)`.
fn page_count_set(page: &Page, lo: u64, hi: u64) -> u64 {
    let mut b = lo;
    let mut c = 0;
    while b < hi {
        let word = (b / 64) as usize;
        let word_top = (word as u64 + 1) * 64;
        let top = hi.min(word_top);
        let mask = word_mask(b % 64, top - word as u64 * 64);
        c += (page[word] & mask).count_ones() as u64;
        b = top;
    }
    c
}

/// First page-local offset in `[lo, BITS_PER_PAGE)` whose bit equals `value`, if any.
fn page_find_first(page: &Page, value: bool, lo: u64) -> Option<u64> {
    let mut b = lo;
    while b < BITS_PER_PAGE {
        let word = (b / 64) as usize;
        let bit = b % 64;
        let w = if value { page[word] } else { !page[word] };
        let masked = w & (u64::MAX << bit);
        if masked != 0 {
            return Some(word as u64 * 64 + masked.trailing_zeros() as u64);
        }
        b = (word as u64 + 1) * 64;
    }
    None
}

/// Largest page-local offset in `[0, hi]` whose bit equals `value`, if any.
fn page_find_last(page: &Page, value: bool, hi: u64) -> Option<u64> {
    let mut word = (hi / 64) as i64;
    while word >= 0 {
        let w = if value { page[word as usize] } else { !page[word as usize] };
        // Restrict to bits at or below `hi` when `hi` falls inside this word.
        let masked = if word as u64 * 64 + 63 <= hi {
            w
        } else {
            w & word_mask(0, hi % 64 + 1)
        };
        if masked != 0 {
            return Some(word as u64 * 64 + (63 - masked.leading_zeros() as u64));
        }
        word -= 1;
    }
    None
}

impl Bitfield {
    /// A fresh, all-`false` bitfield.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether bit `index` is set. Unset (and never-touched) indices read `false`.
    pub fn get(&self, index: u64) -> bool {
        let (page, word, bit) = locate(index);
        self.pages
            .get(&page)
            .is_some_and(|p| (p[word] >> bit) & 1 == 1)
    }

    /// Set bit `index` to `value`. Setting `true` grows storage on demand; setting
    /// `false` never allocates a page (a missing page is already all-`false`).
    pub fn set(&mut self, index: u64, value: bool) {
        let (page, word, bit) = locate(index);
        if value {
            let p = self.pages.entry(page).or_insert_with(|| Box::new([0; WORDS_PER_PAGE]));
            p[word] |= 1u64 << bit;
        } else if let Some(p) = self.pages.get_mut(&page) {
            p[word] &= !(1u64 << bit);
        }
    }

    /// Set every bit in the half-open range `[start, end)` to `value`. `end <= start`
    /// is a no-op. Clearing a range that spans not-yet-existing pages is fine (and
    /// allocates nothing), mirroring upstream's "set false range on a page that does
    /// not yet exist".
    pub fn set_range(&mut self, start: u64, end: u64, value: bool) {
        let mut i = start;
        while i < end {
            let page = i / BITS_PER_PAGE;
            let page_base = page * BITS_PER_PAGE;
            let chunk_end = end.min(page_base + BITS_PER_PAGE);
            if value {
                let p = self.pages.entry(page).or_insert_with(|| Box::new([0; WORDS_PER_PAGE]));
                page_set_range(p, i - page_base, chunk_end - page_base, true);
            } else if let Some(p) = self.pages.get_mut(&page) {
                page_set_range(p, i - page_base, chunk_end - page_base, false);
            }
            i = chunk_end;
        }
    }

    /// Number of bits equal to `value` in the range `[start, start + length)`.
    ///
    /// Note the upstream signature: the second argument is a **length**, not an end
    /// (`count(3, 18, true)` counts over `[3, 21)`).
    pub fn count(&self, start: u64, length: u64, value: bool) -> u64 {
        let end = start + length;
        let mut i = start;
        let mut c = 0;
        while i < end {
            let page = i / BITS_PER_PAGE;
            let page_base = page * BITS_PER_PAGE;
            let chunk_end = end.min(page_base + BITS_PER_PAGE);
            let range = chunk_end - i;
            let set = match self.pages.get(&page) {
                Some(p) => page_count_set(p, i - page_base, chunk_end - page_base),
                None => 0,
            };
            c += if value { set } else { range - set };
            i = chunk_end;
        }
        c
    }

    /// The smallest index `>= position` whose bit equals `value`, or `None`.
    ///
    /// Because the field is infinite zeros beyond what is set, `find_first(false, ..)`
    /// always returns `Some`; `find_first(true, ..)` returns `None` when no bit at or
    /// after `position` is set.
    pub fn find_first(&self, value: bool, position: u64) -> Option<u64> {
        let start_page = position / BITS_PER_PAGE;
        if value {
            for (&page, p) in self.pages.range(start_page..) {
                let lo = if page == start_page { position % BITS_PER_PAGE } else { 0 };
                if let Some(off) = page_find_first(p, true, lo) {
                    return Some(page * BITS_PER_PAGE + off);
                }
            }
            None
        } else {
            // First unset bit. A missing page is all zeros, so the search terminates at
            // the first missing or partially-set page (present, fully-set pages are
            // finite). Hence always `Some`.
            let mut page = start_page;
            loop {
                let lo = if page == start_page { position % BITS_PER_PAGE } else { 0 };
                match self.pages.get(&page) {
                    Some(p) => match page_find_first(p, false, lo) {
                        Some(off) => return Some(page * BITS_PER_PAGE + off),
                        None => page += 1,
                    },
                    None => return Some(page * BITS_PER_PAGE + lo),
                }
            }
        }
    }

    /// The largest index `<= position` whose bit equals `value`, or `None`.
    ///
    /// Unlike [`find_first`](Self::find_first) there is no infinite tail below 0, so
    /// `find_last(false, ..)` returns `None` when every bit in `[0, position]` is set,
    /// and `find_last(true, ..)` returns `None` when none of them is.
    pub fn find_last(&self, value: bool, position: u64) -> Option<u64> {
        let start_page = position / BITS_PER_PAGE;
        if value {
            for (&page, p) in self.pages.range(..=start_page).rev() {
                let hi = if page == start_page {
                    position % BITS_PER_PAGE
                } else {
                    BITS_PER_PAGE - 1
                };
                if let Some(off) = page_find_last(p, true, hi) {
                    return Some(page * BITS_PER_PAGE + off);
                }
            }
            None
        } else {
            let mut page = start_page as i64;
            loop {
                if page < 0 {
                    return None;
                }
                let pageu = page as u64;
                let hi = if pageu == start_page {
                    position % BITS_PER_PAGE
                } else {
                    BITS_PER_PAGE - 1
                };
                match self.pages.get(&pageu) {
                    Some(p) => match page_find_last(p, false, hi) {
                        Some(off) => return Some(pageu * BITS_PER_PAGE + off),
                        None => page -= 1,
                    },
                    None => return Some(pageu * BITS_PER_PAGE + hi),
                }
            }
        }
    }

    /// First set index `>= position` (`find_first(true, ..)`).
    pub fn first_set(&self, position: u64) -> Option<u64> {
        self.find_first(true, position)
    }

    /// First unset index `>= position` (`find_first(false, ..)`); always `Some`.
    pub fn first_unset(&self, position: u64) -> Option<u64> {
        self.find_first(false, position)
    }

    /// Last set index `<= position` (`find_last(true, ..)`).
    pub fn last_set(&self, position: u64) -> Option<u64> {
        self.find_last(true, position)
    }

    /// Last unset index `<= position` (`find_last(false, ..)`).
    pub fn last_unset(&self, position: u64) -> Option<u64> {
        self.find_last(false, position)
    }

    /// Serialize the field for persistence (e.g. a hypercore's presence map; see
    /// `hypercore::Hypercore::persist`). Only **non-zero** pages are emitted — an
    /// all-`false` page is semantically identical to an absent one, so a cleared
    /// page never bloats the snapshot. Layout: `[live_page_count u64]` then, per
    /// page, `[page_index u64][WORDS_PER_PAGE × u64]`, all little-endian. Not
    /// disk-compatible with upstream (ADR-0001 / ADR-0030's deferred persistence).
    pub fn serialize(&self) -> Vec<u8> {
        let live: Vec<(&u64, &Box<Page>)> = self
            .pages
            .iter()
            .filter(|(_, p)| p.iter().any(|&w| w != 0))
            .collect();
        let mut out = Vec::with_capacity(8 + live.len() * (8 + WORDS_PER_PAGE * 8));
        out.extend_from_slice(&(live.len() as u64).to_le_bytes());
        for (idx, page) in live {
            out.extend_from_slice(&idx.to_le_bytes());
            for &word in page.iter() {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
        out
    }

    /// Reconstruct a field from [`serialize`](Self::serialize) output. Returns
    /// `None` on a malformed buffer (truncated, or trailing bytes). The result is
    /// semantically identical to the serialized field for every query, though its
    /// page map omits any all-zero pages the original happened to retain.
    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        let count = u64::from_le_bytes(bytes[0..8].try_into().ok()?) as usize;
        const PAGE_BYTES: usize = WORDS_PER_PAGE * 8;
        let mut pages = BTreeMap::new();
        let mut off = 8usize;
        for _ in 0..count {
            if off + 8 + PAGE_BYTES > bytes.len() {
                return None;
            }
            let idx = u64::from_le_bytes(bytes[off..off + 8].try_into().ok()?);
            off += 8;
            let mut page: Page = [0u64; WORDS_PER_PAGE];
            for word in page.iter_mut() {
                *word = u64::from_le_bytes(bytes[off..off + 8].try_into().ok()?);
                off += 8;
            }
            pages.insert(idx, Box::new(page));
        }
        if off != bytes.len() {
            return None; // trailing garbage — reject rather than silently ignore
        }
        Some(Bitfield { pages })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // Deterministic PRNG (SplitMix64) — no `rand`/`getrandom`, so the "random"
    // tests reproduce forever and stay wasm-safe (the convergence-sim approach).
    struct SplitMix64(u64);
    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    // upstream `bitfield - set and get`
    #[test]
    fn set_and_get() {
        let mut b = Bitfield::new();

        assert!(!b.get(42));
        b.set(42, true);
        assert!(b.get(42));

        // bigger offsets (sparse pages)
        assert!(!b.get(42_000_000));
        b.set(42_000_000, true);
        assert!(b.get(42_000_000));
        b.set(42_000_000, false);
        assert!(!b.get(42_000_000));
        // unrelated bit untouched
        assert!(b.get(42));
    }

    // upstream `bitfield - sparse array overflow`: a huge index must not panic.
    #[test]
    fn huge_sparse_index() {
        let mut b = Bitfield::new();
        b.set(7_995_511_118_690_925, true);
        assert!(b.get(7_995_511_118_690_925));
        assert!(!b.get(7_995_511_118_690_924));
        assert!(!b.get(0));
    }

    // upstream `bitfield - random set and gets`, made deterministic (seeded PRNG +
    // a reference `HashSet`): every probed index must agree with the oracle.
    #[test]
    fn random_set_and_gets_match_a_reference_set() {
        let mut b = Bitfield::new();
        let mut oracle: HashSet<u64> = HashSet::new();
        let mut rng = SplitMix64(0x5EED_1234_ABCD_0001);

        // Indices up to ~2^40 — large and sparse (many pages), but bounded so the
        // test stays fast.
        let pick = |rng: &mut SplitMix64| rng.next() % (1u64 << 40);

        for _ in 0..200 {
            let idx = pick(&mut rng);
            b.set(idx, true);
            oracle.insert(idx);
        }
        for _ in 0..500 {
            let idx = pick(&mut rng);
            assert_eq!(b.get(idx), oracle.contains(&idx), "mismatch at {idx}");
        }
        // Every set index reads back true.
        for &idx in &oracle {
            assert!(b.get(idx), "set index {idx} read back false");
        }
    }

    // upstream `bitfield - count` (note: second arg is a length, not an end).
    #[test]
    fn count_set_and_unset() {
        let mut b = Bitfield::new();
        for &(start, end) in &[(0, 2), (5, 6), (7, 9), (13, 14), (16, 19), (20, 25)] {
            b.set_range(start, end, true);
        }
        // [3, 21): set = {5,7,8,13,16,17,18,20} = 8; unset = 18 - 8 = 10.
        assert_eq!(b.count(3, 18, true), 8);
        assert_eq!(b.count(3, 18, false), 10);
        // length 0 counts nothing.
        assert_eq!(b.count(3, 0, true), 0);
    }

    // upstream `bitfield - find first, all zeroes` (page + segment boundaries).
    #[test]
    fn find_first_all_zeroes() {
        let b = Bitfield::new();
        assert_eq!(b.find_first(false, 0), Some(0));
        assert_eq!(b.find_first(true, 0), None);

        for &p in &[
            1u64 << 15,
            (1 << 15) - 1,
            (1 << 15) + 1,
            1 << 16,
            (1 << 16) - 1,
            (1 << 16) + 1,
            1 << 21,
            (1 << 21) - 1,
            (1 << 21) + 1,
            1 << 22,
            (1 << 22) - 1,
            (1 << 22) + 1,
        ] {
            assert_eq!(b.find_first(false, p), Some(p), "first unset from {p}");
        }
    }

    // upstream `bitfield - find first, all ones`.
    #[test]
    fn find_first_all_ones() {
        let mut b = Bitfield::new();
        b.set_range(0, 1 << 24, true);

        assert_eq!(b.find_first(true, 0), Some(0));
        assert_eq!(b.find_first(true, 1 << 24), None);
        assert_eq!(b.find_first(false, 0), Some(1 << 24));
        assert_eq!(b.find_first(false, 1 << 24), Some(1 << 24));

        for &p in &[
            1u64 << 15,
            (1 << 15) - 1,
            (1 << 15) + 1,
            1 << 16,
            (1 << 16) - 1,
            (1 << 16) + 1,
            1 << 21,
            (1 << 21) - 1,
            (1 << 21) + 1,
            1 << 22,
            (1 << 22) - 1,
            (1 << 22) + 1,
        ] {
            // every probed index < 2^24 is set
            assert_eq!(b.find_first(true, p), Some(p), "first set from {p}");
        }
    }

    // upstream `bitfield - find last, all zeroes`.
    #[test]
    fn find_last_all_zeroes() {
        let b = Bitfield::new();
        assert_eq!(b.find_last(false, 0), Some(0));
        assert_eq!(b.find_last(true, 0), None);

        for &p in &[
            1u64 << 15,
            (1 << 15) - 1,
            (1 << 15) + 1,
            1 << 16,
            (1 << 16) - 1,
            (1 << 16) + 1,
            1 << 21,
            (1 << 21) - 1,
            (1 << 21) + 1,
            1 << 22,
            (1 << 22) - 1,
            (1 << 22) + 1,
        ] {
            assert_eq!(b.find_last(false, p), Some(p), "last unset up to {p}");
        }
    }

    // upstream `bitfield - find last, all ones`.
    #[test]
    fn find_last_all_ones() {
        let mut b = Bitfield::new();
        b.set_range(0, 1 << 24, true);

        assert_eq!(b.find_last(false, 0), None);
        assert_eq!(b.find_last(false, 1 << 24), Some(1 << 24));
        assert_eq!(b.find_last(true, 0), Some(0));
        assert_eq!(b.find_last(true, 1 << 24), Some((1 << 24) - 1));

        for &p in &[
            1u64 << 15,
            (1 << 15) - 1,
            (1 << 15) + 1,
            1 << 16,
            (1 << 16) - 1,
            (1 << 16) + 1,
            1 << 21,
            (1 << 21) - 1,
            (1 << 21) + 1,
            1 << 22,
            (1 << 22) - 1,
            (1 << 22) + 1,
        ] {
            // every probed index < 2^24 is set, so the last set up to p is p itself.
            assert_eq!(b.find_last(true, p), Some(p), "last set up to {p}");
        }
    }

    // upstream `bitfield - find last, ones around page boundary`.
    #[test]
    fn last_unset_around_page_boundary() {
        let mut b = Bitfield::new();
        b.set(32767, true); // last bit of page 0
        b.set(32768, true); // first bit of page 1

        assert_eq!(b.last_unset(32768), Some(32766));
        assert_eq!(b.last_unset(32769), Some(32769));
    }

    // upstream `bitfield - set range on page boundary`.
    #[test]
    fn set_range_on_page_boundary() {
        let mut b = Bitfield::new();
        b.set_range(2032, 2058, true);
        assert_eq!(b.find_first(true, 2048), Some(2048));
    }

    // upstream `bitfield - set false range on page that does not yet exist`: must not
    // panic and must allocate nothing (a missing page is already all-false).
    #[test]
    fn set_false_range_on_absent_page() {
        let mut b = Bitfield::new();
        b.set_range(32769, 32780, false);
        assert!(b.pages.is_empty(), "clearing absent pages allocates nothing");
        assert!(!b.get(32770));
    }

    // upstream `set last bits in segment and findFirst`.
    #[test]
    fn last_bits_in_segment_then_find_first() {
        let mut b = Bitfield::new();
        b.set(2097150, true);
        assert_eq!(b.find_first(false, 2097150), Some(2097151));

        b.set(2097151, true);
        assert_eq!(b.find_first(false, 2097150), Some(2097152));
        assert_eq!(b.find_first(false, 2097151), Some(2097152));
    }

    // upstream `bitfield - setRange over multiple pages`.
    #[test]
    fn set_range_over_multiple_pages() {
        let mut b = Bitfield::new();
        b.set_range(32768, 32769, true);
        assert!(!b.get(0));
        assert!(b.get(32768));
        assert!(!b.get(32769));

        b.set_range(0, 32768 * 2, false);
        b.set_range(32768, 32768 * 2 + 1, true);
        assert!(!b.get(0));
        assert!(b.get(32768));
        assert!(b.get(32768 * 2));
        assert!(!b.get(32768 * 2 + 1));
    }

    // Cross-check: count over a range equals a brute-force scan, including across
    // page boundaries and over set/unset/sparse mixes (the count invariant the
    // upstream numbers are a special case of).
    #[test]
    fn count_matches_brute_force() {
        let mut b = Bitfield::new();
        b.set_range(10, 20, true);
        b.set_range(32760, 32790, true); // straddles the page-0/page-1 boundary
        b.set(40000, true);

        for &(start, len) in &[(0u64, 50), (15, 30), (32750, 60), (0, 40010), (39990, 30)] {
            let mut set = 0;
            for i in start..start + len {
                if b.get(i) {
                    set += 1;
                }
            }
            assert_eq!(b.count(start, len, true), set, "set count [{start},{len})");
            assert_eq!(
                b.count(start, len, false),
                len - set,
                "unset count [{start},{len})"
            );
        }
    }

    // Cross-check: find_first / find_last equal a brute-force scan for every value
    // and a spread of probe positions over a hand-built sparse field.
    #[test]
    fn find_matches_brute_force() {
        let mut b = Bitfield::new();
        for &i in &[3u64, 4, 5, 100, 32767, 32768, 32769, 70000] {
            b.set(i, true);
        }
        let max_probe = 70_010u64;

        let brute_first = |value: bool, pos: u64| -> Option<u64> {
            (pos..=max_probe + BITS_PER_PAGE).find(|&i| b.get(i) == value)
        };
        let brute_last = |value: bool, pos: u64| -> Option<u64> {
            (0..=pos).rev().find(|&i| b.get(i) == value)
        };

        for pos in [0u64, 2, 3, 6, 99, 101, 32766, 32767, 32768, 32770, 69999, 70000, 70001] {
            for value in [true, false] {
                assert_eq!(
                    b.find_first(value, pos),
                    brute_first(value, pos),
                    "find_first({value}, {pos})"
                );
                assert_eq!(
                    b.find_last(value, pos),
                    brute_last(value, pos),
                    "find_last({value}, {pos})"
                );
            }
        }
    }

    #[test]
    fn serialize_round_trip_preserves_every_query() {
        let mut b = Bitfield::new();
        // bits scattered across several sparse pages + a set range spanning a page edge
        for i in [0u64, 1, 63, 64, 65, 32_767, 32_768, 100_000, 8_000_000_000] {
            b.set(i, true);
        }
        b.set_range(500, 1200, true);

        let restored = Bitfield::deserialize(&b.serialize()).expect("round-trips");

        // every individual query agrees across a wide, page-boundary-crossing sweep
        for i in (0..1300u64)
            .chain([32_766, 32_767, 32_768, 32_769, 99_999, 100_000, 100_001])
            .chain([7_999_999_999, 8_000_000_000, 8_000_000_001])
        {
            assert_eq!(b.get(i), restored.get(i), "get({i})");
        }
        for pos in [0u64, 100, 500, 1199, 1200, 32_768, 100_000, 8_000_000_000] {
            for value in [true, false] {
                assert_eq!(b.find_first(value, pos), restored.find_first(value, pos));
                assert_eq!(b.find_last(value, pos), restored.find_last(value, pos));
            }
        }
        assert_eq!(b.count(0, 9_000_000_000, true), restored.count(0, 9_000_000_000, true));
    }

    #[test]
    fn serialize_skips_all_zero_pages_but_keeps_semantics() {
        let mut b = Bitfield::new();
        b.set(40_000, true); // materializes page 1
        b.set(40_000, false); // page 1 is now all-zero but still in the map
        b.set(5, true); // page 0 is genuinely live
        // page 1 contributes 8 bytes of header it would otherwise carry: assert it's dropped
        let one_live_page = 8 + (8 + WORDS_PER_PAGE * 8);
        assert_eq!(b.serialize().len(), one_live_page, "the all-zero page is not emitted");
        let restored = Bitfield::deserialize(&b.serialize()).unwrap();
        assert!(restored.get(5));
        assert!(!restored.get(40_000));
        assert_eq!(restored.count(0, 100_000, true), 1);
    }

    #[test]
    fn deserialize_rejects_malformed_buffers() {
        assert!(Bitfield::deserialize(&[]).is_none()); // no header
        assert!(Bitfield::deserialize(&[1, 0, 0, 0, 0, 0, 0, 0]).is_none()); // claims a page, has none
        let mut b = Bitfield::new();
        b.set(7, true);
        let mut bytes = b.serialize();
        bytes.push(0xff); // trailing garbage
        assert!(Bitfield::deserialize(&bytes).is_none());
    }
}
