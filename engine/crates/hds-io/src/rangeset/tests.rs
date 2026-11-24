use super::*;
use crate::testing::XorShift;

/// Naive model: one flag per offset of a small universe.
struct Model {
    bits: Vec<bool>,
}

impl Model {
    fn new(size: usize) -> Model {
        Model {
            bits: vec![false; size],
        }
    }

    fn clip(&self, r: &Range<u64>) -> Range<usize> {
        let n = self.bits.len() as u64;
        let (s, e) = (r.start.min(n) as usize, r.end.min(n) as usize);
        if s >= e {
            0..0
        } else {
            s..e
        }
    }

    fn insert(&mut self, r: Range<u64>) -> u64 {
        let mut added = 0;
        for i in self.clip(&r) {
            if !self.bits[i] {
                self.bits[i] = true;
                added += 1;
            }
        }
        added
    }

    fn remove(&mut self, r: Range<u64>) -> u64 {
        let mut removed = 0;
        for i in self.clip(&r) {
            if self.bits[i] {
                self.bits[i] = false;
                removed += 1;
            }
        }
        removed
    }

    /// Maximal runs of `want` inside `r`.
    fn runs(&self, r: Range<u64>, want: bool) -> Vec<Range<u64>> {
        let mut out = Vec::new();
        let mut run_start = None;
        let clip = self.clip(&r);
        for i in clip.clone() {
            match (self.bits[i] == want, run_start) {
                (true, None) => run_start = Some(i as u64),
                (false, Some(s)) => {
                    out.push(s..i as u64);
                    run_start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = run_start {
            out.push(s..clip.end as u64);
        }
        out
    }

    fn total(&self) -> u64 {
        self.bits.iter().filter(|&&b| b).count() as u64
    }
}

fn assert_canonical(set: &RangeSet) {
    let mut prev_end: Option<u64> = None;
    let mut total = 0u64;
    for r in set.iter() {
        assert!(r.start < r.end, "empty range {r:?} stored");
        if let Some(p) = prev_end {
            assert!(p < r.start, "ranges touch or overlap at {p} / {r:?}");
        }
        prev_end = Some(r.end);
        total += r.end - r.start;
    }
    assert_eq!(total, set.total_len(), "cached total is stale");
    assert_eq!(set.range_count(), set.iter().count());
    assert_eq!(set.is_empty(), set.range_count() == 0);
}

fn random_range(rng: &mut XorShift, universe: u64) -> Range<u64> {
    let start = rng.below(universe + 8);
    let len = match rng.below(10) {
        0 => 0,
        1 => rng.below(universe),
        _ => rng.below(24) + 1,
    };
    if rng.below(20) == 0 {
        // inverted on purpose
        start + len..start
    } else {
        start..start + len
    }
}

#[test]
fn matches_naive_model_under_random_operations() {
    const UNIVERSE: u64 = 300;
    for seed in 1..=40u64 {
        let mut rng = XorShift::new(seed);
        let mut set = RangeSet::new();
        let mut model = Model::new(UNIVERSE as usize);
        for step in 0..600 {
            let r = random_range(&mut rng, UNIVERSE);
            // The model only knows offsets below UNIVERSE; keep the set inside it too.
            let r = r.start.min(UNIVERSE)..r.end.min(UNIVERSE);
            if rng.below(3) == 0 {
                assert_eq!(
                    set.remove(r.clone()),
                    model.remove(r.clone()),
                    "remove {r:?} (seed {seed}, step {step})"
                );
            } else {
                assert_eq!(
                    set.insert(r.clone()),
                    model.insert(r.clone()),
                    "insert {r:?} (seed {seed}, step {step})"
                );
            }
            assert_canonical(&set);
            assert_eq!(set.total_len(), model.total());

            if step % 7 == 0 {
                assert_eq!(
                    set.iter().collect::<Vec<_>>(),
                    model.runs(0..UNIVERSE, true)
                );
                for offset in 0..UNIVERSE + 4 {
                    let expect = model.bits.get(offset as usize).copied().unwrap_or(false);
                    assert_eq!(set.contains(offset), expect, "contains({offset})");
                }
            }
            let q = random_range(&mut rng, UNIVERSE);
            let inside = model.runs(q.clone(), true);
            let outside = model.runs(q.clone(), false);
            let q_len = q.end.saturating_sub(q.start);
            let q_len_in_universe = q.end.min(UNIVERSE).saturating_sub(q.start.min(UNIVERSE));
            let covered: u64 = inside.iter().map(|r| r.end - r.start).sum();
            assert_eq!(
                set.intersection(q.clone()).collect::<Vec<_>>(),
                inside,
                "intersection({q:?})"
            );
            assert_eq!(
                set.intersects(q.clone()),
                !inside.is_empty(),
                "intersects({q:?})"
            );
            assert_eq!(set.covered_len(q.clone()), covered, "covered_len({q:?})");
            assert_eq!(
                set.contains_range(q.clone()),
                q_len == 0 || (covered == q_len && q_len == q_len_in_universe),
                "contains_range({q:?})"
            );
            // Gaps beyond the model's universe are one uncovered tail.
            let mut expect_gaps = outside;
            if q.start < q.end && q.end > UNIVERSE {
                let tail_start = q.start.max(UNIVERSE);
                match expect_gaps.last_mut() {
                    Some(last) if last.end == tail_start => last.end = q.end,
                    _ => expect_gaps.push(tail_start..q.end),
                }
            }
            assert_eq!(
                set.gaps(q.clone()).collect::<Vec<_>>(),
                expect_gaps,
                "gaps({q:?})"
            );
        }
    }
}

#[test]
fn insert_merges_touching_and_overlapping_ranges() {
    let mut set = RangeSet::new();
    assert_eq!(set.insert(10..20), 10);
    assert_eq!(set.insert(30..40), 10);
    assert_eq!(set.range_count(), 2);
    assert_eq!(set.insert(20..30), 10, "exactly fills the gap");
    assert_eq!(set.iter().collect::<Vec<_>>(), vec![10..40]);
    assert_eq!(set.insert(15..35), 0, "already covered");
    assert_eq!(set.insert(5..12), 5);
    assert_eq!(set.insert(38..50), 10);
    assert_eq!(set.iter().collect::<Vec<_>>(), vec![5..50]);
    assert_eq!(set.insert(60..70), 10);
    assert_eq!(set.insert(80..90), 10);
    assert_eq!(set.insert(0..100), 100 - 45 - 20, "swallows three ranges");
    assert_eq!(set.iter().collect::<Vec<_>>(), vec![0..100]);
    assert_eq!(set.total_len(), 100);
}

#[test]
fn remove_splits_and_trims() {
    let mut set: RangeSet = std::iter::once(0..100u64).collect();
    assert_eq!(set.remove(40..60), 20);
    assert_eq!(set.iter().collect::<Vec<_>>(), vec![0..40, 60..100]);
    assert_eq!(set.remove(30..70), 20);
    assert_eq!(set.iter().collect::<Vec<_>>(), vec![0..30, 70..100]);
    assert_eq!(set.remove(0..30), 30);
    assert_eq!(set.remove(95..200), 5);
    assert_eq!(set.iter().collect::<Vec<_>>(), vec![70..95]);
    assert_eq!(set.remove(0..70), 0);
    assert_eq!(set.remove(95..1000), 0);
    assert_eq!(set.remove(0..u64::MAX), 25);
    assert!(set.is_empty());
    assert_eq!(set.total_len(), 0);
}

#[test]
fn empty_and_inverted_ranges_are_the_empty_set() {
    let mut set = RangeSet::new();
    assert_eq!(set.insert(5..5), 0);
    #[allow(clippy::reversed_empty_ranges)]
    let inverted = 9..3u64;
    assert_eq!(set.insert(inverted.clone()), 0);
    assert!(set.is_empty());
    set.insert(0..10);
    assert_eq!(set.remove(inverted.clone()), 0);
    assert_eq!(set.remove(4..4), 0);
    assert!(!set.intersects(inverted.clone()));
    assert!(!set.intersects(4..4));
    assert!(set.contains_range(inverted.clone()));
    assert!(set.contains_range(20..20));
    assert_eq!(set.intersection(inverted.clone()).count(), 0);
    assert_eq!(set.gaps(inverted).count(), 0);
    assert_eq!(set.gaps(4..4).count(), 0);
    assert_eq!(set.total_len(), 10);
}

#[test]
fn works_at_the_ends_of_the_u64_space() {
    let mut set = RangeSet::new();
    assert_eq!(set.insert(0..1), 1);
    assert_eq!(set.insert(u64::MAX - 10..u64::MAX), 10);
    assert!(set.contains(0));
    assert!(set.contains(u64::MAX - 1));
    assert!(!set.contains(u64::MAX));
    assert_eq!(
        set.gaps(0..u64::MAX).collect::<Vec<_>>(),
        vec![1..u64::MAX - 10]
    );
    assert_eq!(set.insert(0..u64::MAX), u64::MAX - 11);
    assert_eq!(set.total_len(), u64::MAX);
    assert_eq!(set.range_count(), 1);
    assert_eq!(set.remove(1..u64::MAX - 1), u64::MAX - 2);
    assert_eq!(
        set.iter().collect::<Vec<_>>(),
        vec![0..1, u64::MAX - 1..u64::MAX]
    );
    assert_eq!(set.span(), Some(0..u64::MAX));
}

#[test]
fn queries_report_first_last_and_span() {
    let mut set = RangeSet::new();
    assert_eq!((set.first(), set.last(), set.span()), (None, None, None));
    set.extend([100..200u64, 10..20, 300..301]);
    assert_eq!(set.first(), Some(10..20));
    assert_eq!(set.last(), Some(300..301));
    assert_eq!(set.span(), Some(10..301));
    assert_eq!(
        set.iter().rev().collect::<Vec<_>>(),
        vec![300..301, 100..200, 10..20]
    );
    assert_eq!((&set).into_iter().len(), 3);
    let mut other = RangeSet::new();
    other.insert(15..120);
    other.union_with(&set);
    assert_eq!(other.iter().collect::<Vec<_>>(), vec![10..200, 300..301]);
    set.clear();
    assert!(set.is_empty());
    assert_eq!(set.total_len(), 0);
    assert_eq!(format!("{other:?}"), "{10..200, 300..301}");
}

#[test]
fn serializes_as_ascending_pairs_and_round_trips() {
    let set: RangeSet = [4096..8192u64, 0..512, 1 << 40..(1 << 40) + 512]
        .into_iter()
        .collect();
    let json = serde_json::to_string(&set).unwrap();
    assert_eq!(json, "[[0,512],[4096,8192],[1099511627776,1099511628288]]");
    let back: RangeSet = serde_json::from_str(&json).unwrap();
    assert_eq!(back, set);
    assert_eq!(back.total_len(), set.total_len());
    assert_eq!(serde_json::to_string(&RangeSet::new()).unwrap(), "[]");
}

#[test]
fn deserialization_normalizes_but_rejects_inverted_pairs() {
    // Unsorted, overlapping and touching input is merged into canonical form.
    let set: RangeSet = serde_json::from_str("[[100,200],[0,50],[150,300],[50,60]]").unwrap();
    assert_eq!(set.iter().collect::<Vec<_>>(), vec![0..60, 100..300]);
    assert_eq!(set.total_len(), 260);
    assert_canonical(&set);
    for bad in [
        "[[5,5]]",
        "[[9,3]]",
        "[[0,10],[20,19]]",
        "[[1]]",
        "[1,2]",
        "{}",
        "[[-1,2]]",
    ] {
        assert!(
            serde_json::from_str::<RangeSet>(bad).is_err(),
            "{bad} must be rejected"
        );
    }
}

#[test]
fn a_million_ranges_stay_fast() {
    // Inserted in a scattered order, then queried and half removed again. With a
    // vector-backed set this takes minutes; the tree keeps it well below a second
    // in release builds.
    const N: u64 = 1_000_000;
    let mut set = RangeSet::new();
    // 0, 7919, 15838, ... mod N visits every index once (7919 is coprime to N).
    let mut i = 0u64;
    for _ in 0..N {
        set.insert(i * 16..i * 16 + 8);
        i = (i + 7919) % N;
    }
    assert_eq!(set.range_count(), N as usize);
    assert_eq!(set.total_len(), N * 8);
    assert!(set.contains(16 * 123_456 + 7));
    assert!(!set.contains(16 * 123_456 + 8));
    assert_eq!(set.gaps(0..N * 16).count(), N as usize);
    assert_eq!(set.covered_len(1600..3200), 800);
    for i in (0..N).step_by(2) {
        set.remove(i * 16..i * 16 + 8);
    }
    assert_eq!(set.range_count(), (N / 2) as usize);
    // Filling every gap collapses the set into one range.
    set.insert(0..N * 16);
    assert_eq!(set.range_count(), 1);
    assert_eq!(set.total_len(), N * 16);
}
