//! Cheap planning for the experimental zero-run and centered-patch delta candidates.
//!
//! These are heuristics about final Zstd sizes, not proofs. The existing candidate menu remains
//! intact. Collect statistics in physical column order, using the same restart boundaries as the
//! encoders. Nothing here is persisted in an archive or catalog.

use super::{Stats, candidates_from_stats, packed_len, scan_observed};
use crate::delta_blocks::ROWS;

#[derive(Clone, Copy, Debug)]
pub(crate) struct DeltaCandidateSelection {
    pub zero_run: bool,
    pub center_patched: bool,
    /// Exact adaptive zero-run payload length, including tag, anchors and offset directory.
    /// This excludes the outer MixedInteger flag/bitmap and Zstd framing.
    pub zero_run_bytes: usize,
    /// Four-bit payload length assuming no exceptions. This is a lower bound BEFORE Zstd only.
    pub center_patched_min_bytes: usize,
    pub plain_bytes: usize,
    pub delta_count: usize,
    pub zero_delta_count: usize,
    pub zero_run_count: usize,
    /// Run lengths 1 through 7, then a conservative catch-all for runs of 8 or more.
    pub zero_run_buckets: [usize; 8],
}

pub(crate) struct CandidateAnalysis<'a> {
    values: &'a [i64],
    stats: Option<Stats>,
    delta: DeltaCandidateSelection,
}

impl CandidateAnalysis<'_> {
    /// Reuse the basic statistics from the detailed pass; do not scan the column a second time.
    pub(crate) fn candidates(&self) -> impl Iterator<Item = Vec<u8>> + '_ {
        candidates_from_stats(self.values, self.stats.as_ref())
    }

    pub(crate) fn delta_candidates(&self) -> DeltaCandidateSelection {
        self.delta
    }
}

/// Analyze the optional new codecs while collecting the ordinary candidates' statistics once.
pub(crate) fn analyze_candidates(values: &[i64]) -> CandidateAnalysis<'_> {
    let mut deltas = DeltaSizes::default();
    let stats = (!values.is_empty()).then(|| {
        scan_observed(values, |row, previous, value| {
            deltas.observe(row, previous, value);
        })
    });
    let mut delta = deltas.finish(values.len());
    if stats.as_ref().is_some_and(|stats| stats.min != stats.max) {
        // Small nonzero varints are useful even when zero runs are absent. Do not gate solely
        // on the zero count. Raw-size rejection is deliberately loose; Zstd still picks the
        // winner among all surviving candidates and the unchanged ordinary menu.
        delta.zero_run = delta.zero_run_bytes <= delta.plain_bytes;
        // Regular run lengths can make the four-bit layout exceptionally Zstd-friendly even
        // when its raw payload is larger. Retain it when one run-length bucket dominates;
        // grouping every length >=8 together deliberately errs on the side of retaining it.
        let diffuse_runs = delta.zero_run_count != 0
            && delta
                .zero_run_buckets
                .iter()
                .all(|&count| count < delta.zero_run_count.div_ceil(2));
        // A factor of two leaves ambiguous cases in the shortlist. There is no need to find
        // block medians or allocate packed/exception buffers merely to reject this candidate.
        delta.center_patched = !(delta.zero_run
            && diffuse_runs
            && delta.zero_run_bytes <= delta.center_patched_min_bytes / 2);
    }
    CandidateAnalysis {
        values,
        stats,
        delta,
    }
}

#[derive(Default)]
struct DeltaSizes {
    tail_bytes: usize,
    pending_zeros: usize,
    delta_count: usize,
    zero_delta_count: usize,
    zero_run_count: usize,
    zero_run_buckets: [usize; 8],
}

fn varint_len(value: u64) -> usize {
    (64 - value.leading_zeros()).max(1).div_ceil(7) as usize
}

impl DeltaSizes {
    fn flush_zeros(&mut self) {
        if self.pending_zeros != 0 {
            self.tail_bytes += 1 + varint_len(self.pending_zeros as u64);
            self.zero_run_count += 1;
            self.zero_run_buckets[(self.pending_zeros - 1).min(7)] += 1;
            self.pending_zeros = 0;
        }
    }

    fn observe(&mut self, row: usize, previous: i64, value: i64) {
        if row % ROWS == 0 {
            // The new block starts with an absolute anchor, not a difference from the old block.
            self.flush_zeros();
            return;
        }
        self.delta_count += 1;
        let delta = value.wrapping_sub(previous);
        if delta == 0 {
            self.pending_zeros += 1;
            self.zero_delta_count += 1;
        } else {
            self.flush_zeros();
            let zigzag = ((delta as u64) << 1) ^ ((delta >> 63) as u64);
            self.tail_bytes += varint_len(zigzag);
        }
    }

    fn finish(mut self, rows: usize) -> DeltaCandidateSelection {
        self.flush_zeros();
        let blocks = rows.div_ceil(ROWS);
        let header = 1 + 8 * (blocks + 1);
        // Each full block has an odd number of differences: account for its padding separately.
        let packed_bytes = rows / ROWS * packed_len(ROWS - 1, 4)
            + if rows % ROWS == 0 {
                0
            } else {
                packed_len(rows % ROWS - 1, 4)
            };
        DeltaCandidateSelection {
            zero_run: false,
            center_patched: false,
            zero_run_bytes: header + blocks * 8 + self.tail_bytes,
            center_patched_min_bytes: header + blocks * 17 + packed_bytes,
            plain_bytes: 1 + rows * 8,
            delta_count: self.delta_count,
            zero_delta_count: self.zero_delta_count,
            zero_run_count: self.zero_run_count,
            zero_run_buckets: self.zero_run_buckets,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_baseline_candidates_and_empty_constant_shortlists() {
        for values in [
            vec![],
            vec![7],
            vec![9; ROWS + 1],
            vec![i64::MIN, i64::MAX, -1, 0, 0, 1],
        ] {
            let plan = analyze_candidates(&values);
            assert_eq!(
                plan.candidates().collect::<Vec<_>>(),
                super::super::candidates(&values).collect::<Vec<_>>()
            );
            if values.iter().all(|&value| Some(&value) == values.first()) {
                assert!(!plan.delta_candidates().zero_run);
                assert!(!plan.delta_candidates().center_patched);
            }
        }
        assert_eq!(analyze_candidates(&[]).delta_candidates().zero_run_bytes, 9);
    }

    #[test]
    fn restart_anchors_break_runs_and_ignore_cross_block_jumps() {
        let mut values = vec![10; ROWS + 3];
        values[ROWS..].fill(i64::MIN);
        let stats = analyze_candidates(&values).delta_candidates();
        assert_eq!(stats.delta_count, values.len() - 2);
        assert_eq!(stats.zero_delta_count, stats.delta_count);
        assert_eq!(stats.zero_run_bytes, 1 + 3 * 8 + 2 * 8 + 4 + 2);
        assert_eq!(
            stats.center_patched_min_bytes,
            1 + 3 * 8 + 2 * 17 + ROWS / 2 + 1
        );
        assert_eq!(stats.zero_run_count, 2);
        assert_eq!(stats.zero_run_buckets, [0, 1, 0, 0, 0, 0, 0, 1]);
        assert!(stats.zero_run);
        assert!(stats.center_patched);
    }

    #[test]
    fn small_nonzero_deltas_stay_eligible() {
        let values: Vec<_> = (0..1000).map(|i| 123_000 + 50 * i).collect();
        let stats = analyze_candidates(&values).delta_candidates();
        assert_eq!(stats.zero_delta_count, 0);
        assert_eq!(stats.zero_run_bytes, 1 + 2 * 8 + 8 + 999);
        assert!(stats.zero_run);
        assert!(stats.center_patched);
    }

    #[test]
    fn wrapping_deltas_and_varint_boundaries_are_counted() {
        let deltas: [i64; 12] = [
            1,
            -1,
            63,
            -64,
            64,
            -65,
            8191,
            -8192,
            8192,
            -8193,
            i64::MIN,
            i64::MAX,
        ];
        let mut values = vec![i64::MAX];
        for delta in deltas {
            values.push(values.last().unwrap().wrapping_add(delta));
        }
        let stats = analyze_candidates(&values).delta_candidates();
        assert_eq!(
            stats.zero_run_bytes,
            1 + 2 * 8 + 8 + 1 + 1 + 1 + 1 + 2 + 2 + 2 + 2 + 3 + 3 + 10 + 10
        );
        assert_eq!(stats.delta_count, deltas.len());
        assert_eq!(stats.zero_delta_count, 0);
    }

    #[test]
    fn large_uncorrelated_deltas_can_reject_varints() {
        let mut state = 0x12345678abcdef01u64;
        let values: Vec<_> = (0..5000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as i64
            })
            .collect();
        let stats = analyze_candidates(&values).delta_candidates();
        assert!(stats.zero_run_bytes > stats.plain_bytes);
        assert!(!stats.zero_run);
        // With no useful alternative-size evidence, preserve the patched trial.
        assert!(stats.center_patched);
    }

    #[test]
    fn regular_runs_remain_eligible_even_with_rare_long_runs() {
        let mut values = Vec::new();
        for i in 0..5000 {
            values.extend(std::iter::repeat_n(
                i,
                if i % 1000 == 0 { 4096 } else { 16 },
            ));
        }
        let stats = analyze_candidates(&values).delta_candidates();
        assert!(stats.zero_run_bytes <= stats.center_patched_min_bytes / 2);
        assert!(stats.zero_run_buckets[7] >= stats.zero_run_count.div_ceil(2));
        assert!(stats.zero_run);
        assert!(stats.center_patched);
    }

    #[test]
    fn diffuse_runs_with_a_large_size_margin_prune_the_patched_trial() {
        let mut values = Vec::new();
        for i in 0..4000 {
            values.extend(std::iter::repeat_n(i, [2, 3, 4, 100][i as usize % 4]));
        }
        let stats = analyze_candidates(&values).delta_candidates();
        assert!(stats.zero_run_bytes <= stats.center_patched_min_bytes / 2);
        assert!(stats.zero_run);
        assert!(!stats.center_patched);
    }
}
