//! Which frames go into one request, and how often a frame may be refused.
//! Pure decisions; the I/O stays in `frames.rs`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The most refusals of one frame, sent alone, before the runner drops it.
pub(super) const MAX_REFUSALS: u32 = 3;

/// The most encoded bytes of frames in one request (the cloud's body limit
/// is 2 MiB). One frame over the cap goes alone.
pub(super) const BATCH_CAP: usize = 1_572_864;

/// How many of the first frames go into the next request. A refused (`lone`)
/// frame goes alone; the others go together until the next one would pass
/// the cap. At least one frame, if there is one.
pub(super) fn first_batch(sizes: &[usize], lone: &[bool], cap: usize) -> usize {
    if sizes.is_empty() {
        return 0;
    }
    if lone[0] {
        return 1;
    }
    let mut total = sizes[0];
    let mut n = 1;
    while n < sizes.len() && !lone[n] && total + sizes[n] <= cap {
        total += sizes[n];
        n += 1;
    }
    n
}

#[derive(Default)]
struct Entry {
    count: u32,
    lone: bool,
    last: Option<Instant>,
}

/// How often the cloud refused each frame (by frame id).
#[derive(Default)]
pub(super) struct Refusals {
    map: HashMap<String, Entry>,
}

impl Refusals {
    /// True when a request with this frame must have no other frame.
    pub(super) fn is_lone(&self, id: &str) -> bool {
        self.map.get(id).is_some_and(|e| e.lone)
    }

    /// The cloud took this frame.
    pub(super) fn forget(&mut self, id: &str) {
        self.map.remove(id);
    }

    /// A request with these frame ids was refused. Returns, for each id,
    /// whether to drop the frame. A request with no frame drops nothing. A
    /// request with many frames drops nothing and marks each frame to go
    /// alone. A frame refused alone counts one try, but only when `spacing`
    /// has passed since its last counted try; the third try drops it.
    pub(super) fn refused(&mut self, ids: &[&str], now: Instant, spacing: Duration) -> Vec<bool> {
        if ids.len() != 1 {
            for id in ids {
                self.map.entry((*id).to_string()).or_default().lone = true;
            }
            return vec![false; ids.len()];
        }
        let e = self.map.entry(ids[0].to_string()).or_default();
        e.lone = true;
        if e.last.is_none_or(|t| now.duration_since(t) >= spacing) {
            e.count += 1;
            e.last = Some(now);
        }
        if e.count >= MAX_REFUSALS {
            self.map.remove(ids[0]);
            return vec![true];
        }
        vec![false]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_rules() {
        let cap = 100;
        // (sizes, lone, want)
        let cases: Vec<(Vec<usize>, Vec<bool>, usize)> = vec![
            (vec![], vec![], 0),
            (vec![10, 10, 10], vec![false; 3], 3),
            (vec![60, 60], vec![false; 2], 1),
            (vec![500], vec![false], 1),
            (vec![500, 1], vec![false; 2], 1),
            (vec![10, 10], vec![true, false], 1),
            (vec![10, 10, 10], vec![false, true, false], 1),
            (vec![40, 40, 40], vec![false; 3], 2),
        ];
        for (sizes, lone, want) in cases {
            assert_eq!(first_batch(&sizes, &lone, cap), want, "{sizes:?} {lone:?}");
        }
    }

    #[test]
    fn many_frames_refused_drop_nothing_and_go_alone() {
        let mut r = Refusals::default();
        let t = Instant::now();
        assert_eq!(r.refused(&["a", "b", "c"], t, Duration::ZERO), [false; 3]);
        assert!(r.is_lone("a") && r.is_lone("b") && r.is_lone("c"));
        r.forget("b");
        assert!(!r.is_lone("b"));
    }

    #[test]
    fn an_empty_request_refused_drops_nothing() {
        let mut r = Refusals::default();
        assert!(r.refused(&[], Instant::now(), Duration::ZERO).is_empty());
    }

    #[test]
    fn a_lone_frame_drops_at_the_third_spaced_refusal() {
        let mut r = Refusals::default();
        let t = Instant::now();
        let s = Duration::from_secs(1);
        assert_eq!(r.refused(&["a"], t, s), [false]);
        assert_eq!(r.refused(&["a"], t + s, s), [false]);
        assert_eq!(r.refused(&["a"], t + s * 2, s), [true]);
        assert!(!r.is_lone("a"), "dropped frames are forgotten");
    }

    #[test]
    fn refusals_inside_the_spacing_are_not_counted() {
        let mut r = Refusals::default();
        let t = Instant::now();
        let s = Duration::from_secs(1);
        for i in 0..10 {
            let at = t + Duration::from_millis(i * 10);
            assert_eq!(r.refused(&["a"], at, s), [false], "try {i}");
        }
        assert_eq!(r.refused(&["a"], t + s, s), [false]);
        assert_eq!(r.refused(&["a"], t + s * 2, s), [true]);
    }
}
