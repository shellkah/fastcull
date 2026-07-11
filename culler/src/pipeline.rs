//! Decode pipeline: scheduling, caching, worker threads. Wired into the event loop by main (Task 11).
#![allow(dead_code)] // TODO(Task 11): remove once main wires the event loop

/// A decode request tagged with the generation that was current when it was stamped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request {
    pub index: usize,
    pub generation: u64,
}

/// Pure latest-wins scheduler. `generation` bumps once per navigation event.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scheduler {
    pub generation: u64,
}

impl Scheduler {
    pub fn new() -> Self {
        Self { generation: 0 }
    }

    /// Advance to a new generation (call once per navigation) and return it.
    pub fn advance(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Stamp a request for `index` with generation `gen` (typically `self.generation`).
    pub fn request(&self, index: usize, r#gen: u64) -> Request {
        Request {
            index,
            generation: r#gen,
        }
    }

    /// True if a newer generation has been issued since this request was stamped.
    /// Checked at dequeue and at delivery.
    pub fn is_stale(req: &Request, current_gen: u64) -> bool {
        req.generation < current_gen
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;

    #[test]
    fn a_request_is_fresh_until_a_newer_generation_is_issued() {
        let mut sch = Scheduler::new();
        let g = sch.advance();
        let r = sch.request(5, g);
        assert!(!Scheduler::is_stale(&r, sch.generation));
        let g2 = sch.advance();
        assert_eq!(g2, g + 1);
        assert!(Scheduler::is_stale(&r, sch.generation)); // superseded
    }

    #[test]
    fn a_batch_shares_one_generation_and_goes_stale_together() {
        let mut sch = Scheduler::new();
        let g = sch.advance();
        let a = sch.request(10, g);
        let b = sch.request(11, g);
        assert!(!Scheduler::is_stale(&a, sch.generation));
        assert!(!Scheduler::is_stale(&b, sch.generation));
        sch.advance();
        assert!(Scheduler::is_stale(&a, sch.generation));
        assert!(Scheduler::is_stale(&b, sch.generation));
    }

    #[test]
    fn generation_starts_at_zero() {
        let sch = Scheduler::new();
        assert_eq!(sch.generation, 0);
    }
}
