//! Damage-region geometry: an axis-aligned rectangle type and the
//! arithmetic the damage-tracking pipeline needs on top of it.
//!
//! Origin: fork-authored code in the moerketh/lamco-rdp-server fork
//! (`pipeline_decisions.rs` / `damage` module), absent from upstream. The
//! [`Region`] type is crate-local; the fork adapts via `From`/`Into`.
//! See `PROVENANCE.md` for the audit trail.

/// An axis-aligned rectangular region of a frame, in exclusive-LTRB form
/// (right/bottom are one past the last covered pixel).
///
/// Mirrors the semantics of the fork's `DamageRegion` without sharing its
/// identity: the fork converts at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Region {
    /// X coordinate of the region (pixels from left).
    pub x: u32,
    /// Y coordinate of the region (pixels from top).
    pub y: u32,
    /// Width of the region in pixels.
    pub width: u32,
    /// Height of the region in pixels.
    pub height: u32,
}

impl Region {
    /// Construct from position and size.
    #[inline]
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// A region covering the whole frame of the given size.
    #[inline]
    pub fn full_frame(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// Area in pixels.
    #[inline]
    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Whether this region and `other` share at least one pixel.
    pub fn overlaps(&self, other: &Region) -> bool {
        let self_right = self.x + self.width;
        let self_bottom = self.y + self.height;
        let other_right = other.x + other.width;
        let other_bottom = other.y + other.height;

        self.x < other_right
            && self_right > other.x
            && self.y < other_bottom
            && self_bottom > other.y
    }

    /// Bounding box of this region and `other`.
    pub fn union(&self, other: &Region) -> Region {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);

        Region {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }

    /// Exact LTRB bounds of this region as a 4-tuple.
    #[inline]
    pub fn ltrb(&self) -> (u32, u32, u32, u32) {
        (self.x, self.y, self.x + self.width, self.y + self.height)
    }
}

/// Fraction of the frame area covered by `regions` (0.0 when nothing changed).
///
/// Sums region areas without de-duplication: overlapping inputs inflate the
/// ratio above true coverage (and possibly above 1.0). Feed from a merged
/// set (see [`merge_regions`]) when the ratio drives decisions.
pub fn damage_ratio(regions: &[Region], frame_width: u32, frame_height: u32) -> f32 {
    if regions.is_empty() {
        return 0.0;
    }
    let frame_area = u64::from(frame_width) * u64::from(frame_height);
    if frame_area == 0 {
        return 0.0;
    }
    let damage_area: u64 = regions.iter().map(Region::area).sum();
    damage_area as f32 / frame_area as f32
}

/// Subtract the coverage of `covered` from `regions`, returning the parts of
/// `regions` NOT covered by `covered`.
///
/// Used by the damage-calibration probe: when the pixel-diff detector sees
/// changed pixels that the compositor's hints did not report, those missed
/// areas must still be sent to the client — the probe already advanced the
/// detector's reference frame, so anything not sent would be invisible to
/// every future diff.
///
/// Exact rectangle subtraction with axis-aligned splitting: each input
/// region is clipped against the covered area, producing up to 4 remainder
/// rects per covered intersection. Output is not merged — callers merge
/// downstream or accept the coarse fragmentation, which tile-aligned inputs
/// keep small in practice.
///
/// Worst case is multiplicative: each covering rect can split every
/// surviving piece into up to 4 bands, so a pathological `covered` set
/// produces O(4^|covered|) pieces per input region. A piece cap guards
/// this: when it binds, the region is returned un-subtracted (a
/// conservative oversend of its full area — oversending is always safe
/// here; under-sending would leave stale pixels).
pub fn subtract_regions(
    regions: &[Region],
    covered: &[Region],
    frame_width: u32,
    frame_height: u32,
) -> Vec<Region> {
    /// Per-region cap on fragmentation pieces before falling back to the
    /// un-subtracted region. Generous for real workloads (compositor hints
    /// arrive tile-aligned and coarse), while bounding the exponential.
    const MAX_PIECES: usize = 256;

    // Fast paths: nothing to subtract from / by.
    if regions.is_empty() || covered.is_empty() {
        return regions.to_vec();
    }
    // A covered region spanning the whole frame erases everything.
    let full_coverage = covered
        .iter()
        .any(|c| c.x == 0 && c.y == 0 && c.width >= frame_width && c.height >= frame_height);
    if full_coverage {
        return Vec::new();
    }

    let mut result: Vec<Region> = Vec::new();
    for r in regions {
        // Worklist of uncovered pieces of `r`.
        let mut pieces = vec![*r];
        for c in covered {
            let mut next_pieces = Vec::new();
            for p in pieces {
                // Intersection of p and c (empty when disjoint).
                let ix = p.x.max(c.x);
                let iy = p.y.max(c.y);
                let ix2 = (p.x + p.width).min(c.x + c.width);
                let iy2 = (p.y + p.height).min(c.y + c.height);
                if ix >= ix2 || iy >= iy2 {
                    // Disjoint: p survives untouched.
                    next_pieces.push(p);
                    continue;
                }
                // Clip p against the intersection, emitting the 4 side bands.
                // Left band.
                if ix > p.x {
                    next_pieces.push(Region::new(p.x, p.y, ix - p.x, p.height));
                }
                // Right band.
                let p_x2 = p.x + p.width;
                if ix2 < p_x2 {
                    next_pieces.push(Region::new(ix2, p.y, p_x2 - ix2, p.height));
                }
                // Top band (between left/right clip).
                if iy > p.y {
                    next_pieces.push(Region::new(ix, p.y, ix2 - ix, iy - p.y));
                }
                // Bottom band (between left/right clip).
                let p_y2 = p.y + p.height;
                if iy2 < p_y2 {
                    next_pieces.push(Region::new(ix, iy2, ix2 - ix, p_y2 - iy2));
                }
            }
            pieces = next_pieces;
            if pieces.is_empty() {
                break;
            }
            if pieces.len() > MAX_PIECES {
                // Fragmentation cap: return the region un-subtracted rather
                // than let the worklist multiply further. Oversending is
                // always safe (probe-union semantics); under-sending would
                // leave stale pixels.
                pieces = vec![*r];
                break;
            }
        }
        result.extend(pieces);
    }
    result
}

/// Merge overlapping or touching regions (within `merge_distance` pixels)
/// until a fixed point. O(n²) per pass; the damage pipeline's inputs are
/// tile-aligned and small, and each pass strictly decreases the count.
///
/// The fork's pipeline calls this with `merge_distance = 0`.
pub fn merge_regions(mut regions: Vec<Region>, merge_distance: u32) -> Vec<Region> {
    if regions.len() <= 1 {
        return regions;
    }

    let mut changed = true;
    while changed {
        changed = false;
        let mut merged = Vec::with_capacity(regions.len());
        let mut used = vec![false; regions.len()];

        for (i, current_init) in regions.iter().enumerate() {
            if used[i] {
                continue;
            }

            let mut current = *current_init;
            used[i] = true;

            for j in (i + 1)..regions.len() {
                if used[j] {
                    continue;
                }

                if is_adjacent(&current, &regions[j], merge_distance) {
                    current = current.union(&regions[j]);
                    used[j] = true;
                    changed = true;
                }
            }

            merged.push(current);
        }

        regions = merged;
    }

    regions
}

/// Whether two regions overlap or touch within `merge_distance` pixels
/// along any axis (with the other axis overlapping).
fn is_adjacent(a: &Region, b: &Region, merge_distance: u32) -> bool {
    let a_right = a.x + a.width;
    let a_bottom = a.y + a.height;
    let b_right = b.x + b.width;
    let b_bottom = b.y + b.height;

    let x_close = a.x.saturating_sub(b_right) <= merge_distance
        && b.x.saturating_sub(a_right) <= merge_distance;
    let y_close = a.y.saturating_sub(b_bottom) <= merge_distance
        && b.y.saturating_sub(a_bottom) <= merge_distance;
    x_close && y_close
}

/// Tracks damage regions from consumed-but-unsent frames ("debt") so no
/// region is ever lost when the latency governor skips a frame or a send
/// fails.
///
/// Compositor damage hints are one-shot: a skipped frame's regions must be
/// re-sent with a later one or the client keeps stale pixels forever.
///
/// Invariants:
/// - [`absorb`](Self::absorb) REPLACES the debt (it does not extend it):
///   the incoming set already contains the prior debt, because the
///   pipeline prepends debt to each frame's fresh regions before the
///   governor runs. Extending here would double the region count on every
///   consecutive skip (2ⁿ growth across a sub-threshold skip streak).
/// - The stored set is always merged ([`merge_regions`]) and hard-capped,
///   so the debt cannot grow without bound and
///   [`damage_ratio`](damage_ratio()) over [`take`](Self::take)-output
///   cannot double-count area.
#[derive(Debug)]
pub struct DebtAccumulator {
    regions: Vec<Region>,
    cap: usize,
}

impl DebtAccumulator {
    /// Maximum number of rects retained as debt. The cap is never silently
    /// exceeded: when it binds, the whole debt is replaced by its bounding
    /// union — oversending the safe superset rather than dropping updates.
    pub const DEFAULT_CAP: usize = 1024;

    /// A collector with the default cap.
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
            cap: Self::DEFAULT_CAP,
        }
    }

    /// Replace the debt with `send_set` (prior debt ∪ fresh regions),
    /// merged. Called on the skip/wait paths where the frame was consumed
    /// but will not be encoded.
    pub fn absorb(&mut self, send_set: Vec<Region>) {
        self.regions = Self::normalize(send_set, self.cap);
    }

    /// Take the entire debt, leaving the accumulator empty. The pipeline
    /// prepends the returned regions to the next encoded frame's set.
    pub fn take(&mut self) -> Vec<Region> {
        std::mem::take(&mut self.regions)
    }

    /// Drop all debt (e.g. on reconnect/resize where the coordinate space
    /// changed or the client will be fully re-initialized anyway).
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    /// Whether the debt is empty.
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// Number of rects currently held as debt.
    pub fn len(&self) -> usize {
        self.regions.len()
    }

    /// Merge overlapping/adjacent regions; if the merged set still exceeds
    /// the cap, collapse it to a single bounding union.
    fn normalize(mut regions: Vec<Region>, cap: usize) -> Vec<Region> {
        if regions.len() <= 1 {
            return regions;
        }
        regions = merge_regions(regions, 0);
        if regions.len() > cap
            && let Some(first) = regions.first()
        {
            let union = regions.iter().skip(1).fold(*first, |acc, r| acc.union(r));
            return vec![union];
        }
        regions
    }
}

impl Default for DebtAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtract_disjoint_returns_input_untouched() {
        let regions = [Region::new(0, 0, 10, 10)];
        let covered = [Region::new(50, 50, 5, 5)];
        let out = subtract_regions(&regions, &covered, 100, 100);
        assert_eq!(out, vec![Region::new(0, 0, 10, 10)]);
    }

    #[test]
    fn subtract_exact_cover_erases_region() {
        let regions = [Region::new(4, 4, 10, 10)];
        let covered = [Region::new(4, 4, 10, 10)];
        let out = subtract_regions(&regions, &covered, 100, 100);
        assert!(out.is_empty());
    }

    #[test]
    fn subtract_partial_cover_emits_side_bands() {
        // Cover the middle: expect left, right, top, bottom bands.
        let regions = [Region::new(0, 0, 30, 30)];
        let covered = [Region::new(10, 10, 10, 10)];
        let out = subtract_regions(&regions, &covered, 100, 100);
        // 4 bands: left(10 wide), right(10 wide), top(10x10), bottom(10x10)
        assert_eq!(out.len(), 4, "{out:?}");
        for piece in &out {
            assert!(!piece.overlaps(&covered[0]), "{piece:?}");
        }
        // Union of pieces must re-cover everything the original did minus
        // the covered square (900 - 100 = 800 px²).
        let area: u64 = out.iter().map(Region::area).sum();
        assert_eq!(area, 900 - 100);
    }

    #[test]
    fn subtract_full_frame_coverage_erases_all() {
        let regions = [Region::new(4, 4, 10, 10)];
        let covered = [Region::new(0, 0, 100, 100)];
        let out = subtract_regions(&regions, &covered, 100, 100);
        assert!(out.is_empty());
    }

    #[test]
    fn subtract_caps_fragmentation_instead_of_exploding() {
        // Pathological covered set: many thin strips crossing the region
        // would multiply pieces toward 4^|covered| without the cap.
        let region = Region::new(0, 0, 4096, 4096);
        let covered: Vec<Region> = (0..200)
            .map(|i| {
                let y = i * 20;
                // Horizontal strip crossing the full width, with a gap so
                // full_coverage doesn't trigger.
                Region::new(0, y, 4000, 10)
            })
            .collect();
        let out = subtract_regions(&[region], &covered, 4096, 4096);
        // Either the exact subtraction finished cheaply or the cap fell back
        // to the whole region — both bounded results are acceptable; the
        // exponential blowup (4^200) is not.
        assert!(
            out.len() <= 256 || (out.len() == 1 && out[0] == region),
            "bounded output expected, got {}",
            out.len()
        );
    }

    #[test]
    fn subtract_empty_inputs() {
        assert!(subtract_regions(&[], &[Region::new(0, 0, 5, 5)], 100, 100).is_empty());
        let regions = [Region::new(0, 0, 5, 5)];
        let out = subtract_regions(&regions, &[], 100, 100);
        assert_eq!(out, regions.to_vec());
    }

    #[test]
    fn ratio_over_merged_debt_cannot_double_count() {
        let mut acc = DebtAccumulator::new();
        acc.absorb(vec![
            Region::new(0, 0, 100, 100),
            Region::new(0, 0, 100, 100),
        ]);
        assert_eq!(acc.len(), 1, "overlaps must merge");
        let taken = acc.take();
        let ratio = damage_ratio(&taken, 200, 200);
        assert!((ratio - 0.25).abs() < 1e-6, "ratio {ratio}");
    }

    #[test]
    fn accumulator_absorb_replaces_instead_of_extending() {
        // Regression: extending the debt with a send set that already
        // contained the debt doubled it on every consecutive skip.
        let mut acc = DebtAccumulator::new();
        let a = Region::new(0, 0, 100, 100);
        acc.absorb(vec![a]); // frame 1 skipped: debt = {A}
        let b = Region::new(200, 200, 50, 50);
        acc.absorb(vec![a, b]); // must REPLACE → {A, B}
        let taken = acc.take();
        assert_eq!(taken.len(), 2, "no doubling: {taken:?}");
        assert!(acc.is_empty());
    }

    #[test]
    fn accumulator_survives_long_skip_streak_linearly() {
        let mut acc = DebtAccumulator::new();
        let fresh = Region::new(10, 10, 40, 40);
        let mut send_set = vec![fresh];
        for _ in 0..10 {
            acc.absorb(send_set.clone());
            let mut next = acc.take();
            next.push(fresh);
            send_set = next;
        }
        acc.absorb(send_set);
        assert!(
            acc.len() <= 2,
            "streak of 10 skips must stay bounded, got {}",
            acc.len()
        );
    }

    #[test]
    fn accumulator_cap_collapses_to_bounding_union() {
        let mut acc = DebtAccumulator::new();
        let mut set = Vec::new();
        for i in 0..(DebtAccumulator::DEFAULT_CAP + 64) {
            let x = u32::try_from(i % 64).expect("in range") * 1000;
            let y = u32::try_from(i / 64).expect("in range") * 1000;
            set.push(Region::new(x, y, 10, 10));
        }
        acc.absorb(set);
        assert!(
            acc.len() <= DebtAccumulator::DEFAULT_CAP,
            "cap must bind, got {}",
            acc.len()
        );
    }

    #[test]
    fn merge_regions_collapses_touching_rects() {
        let regions = vec![
            Region::new(0, 0, 10, 10),
            Region::new(10, 0, 10, 10), // touching on x
        ];
        let merged = merge_regions(regions, 0);
        assert_eq!(merged, vec![Region::new(0, 0, 20, 10)]);
    }

    #[test]
    fn merge_regions_respects_merge_distance() {
        let regions = vec![
            Region::new(0, 0, 10, 10),
            Region::new(15, 0, 10, 10), // 5px gap
        ];
        let merged = merge_regions(regions.clone(), 4);
        assert_eq!(merged.len(), 2, "gap > distance: no merge");
        let merged = merge_regions(regions, 5);
        assert_eq!(merged.len(), 1, "gap <= distance: merge");
    }
}
