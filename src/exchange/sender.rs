//! The state of the sender: at most one request that carries results is in
//! flight. Results that finish meanwhile wait in the queue and go together
//! in the next request. Pure; the I/O stays in `frames.rs` and `run_op.rs`.

#[derive(Default)]
pub(super) struct Gate {
    busy: bool,
}

impl Gate {
    /// Takes the slot when it is free and there is something to send.
    pub(super) fn try_start(&mut self, queued: bool) -> bool {
        if self.busy || !queued {
            return false;
        }
        self.busy = true;
        true
    }

    /// A request ended. True: results wait, send again (the slot stays
    /// taken). False: the slot is free.
    pub(super) fn next(&mut self, queued: bool) -> bool {
        if !queued {
            self.busy = false;
        }
        queued
    }

    /// The sender ends without a next request.
    pub(super) fn stop(&mut self) {
        self.busy = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_sends_at_once() {
        let mut g = Gate::default();
        assert!(g.try_start(true));
    }

    #[test]
    fn nothing_queued_takes_no_slot() {
        let mut g = Gate::default();
        assert!(!g.try_start(false));
        assert!(g.try_start(true));
    }

    #[test]
    fn busy_slot_refuses_a_second_request() {
        let mut g = Gate::default();
        assert!(g.try_start(true));
        for _ in 0..10 {
            assert!(!g.try_start(true));
        }
    }

    #[test]
    fn done_with_waiting_results_goes_again_and_keeps_the_slot() {
        let mut g = Gate::default();
        assert!(g.try_start(true));
        assert!(g.next(true));
        assert!(!g.try_start(true), "still busy");
        assert!(!g.next(false));
        assert!(g.try_start(true), "free after an empty queue");
    }

    #[test]
    fn stop_frees_the_slot() {
        let mut g = Gate::default();
        assert!(g.try_start(true));
        g.stop();
        assert!(g.try_start(true));
    }
}
