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

use culler_core::decode::DecodedImage;
use std::collections::HashMap;
use std::sync::Arc;

struct CacheEntry {
    image: Arc<DecodedImage>,
    bytes: usize,
}

/// Memory-budgeted LRU of fit-size RGBA textures, keyed by shot index.
/// Full/1:1 decodes never enter here (see Task 7's dedicated slot).
pub struct LruCache {
    budget: usize,
    used: usize,
    order: Vec<usize>, // front = LRU, back = MRU
    map: HashMap<usize, CacheEntry>,
}

impl LruCache {
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            order: Vec::new(),
            map: HashMap::new(),
        }
    }

    fn touch(&mut self, key: usize) {
        if let Some(pos) = self.order.iter().position(|&k| k == key) {
            self.order.remove(pos);
            self.order.push(key);
        }
    }

    pub fn get(&mut self, key: usize) -> Option<Arc<DecodedImage>> {
        if self.map.contains_key(&key) {
            self.touch(key);
            self.map.get(&key).map(|e| e.image.clone())
        } else {
            None
        }
    }

    pub fn put(&mut self, key: usize, image: Arc<DecodedImage>) {
        let bytes = image.rgba.len();
        if let Some(old) = self.map.remove(&key) {
            self.used -= old.bytes;
            if let Some(pos) = self.order.iter().position(|&k| k == key) {
                self.order.remove(pos);
            }
        }
        self.used += bytes;
        self.map.insert(key, CacheEntry { image, bytes });
        self.order.push(key);
        self.evict();
    }

    fn evict(&mut self) {
        while self.used > self.budget && self.order.len() > 1 {
            let lru = self.order.remove(0);
            if let Some(e) = self.map.remove(&lru) {
                self.used -= e.bytes;
            }
        }
    }

    pub fn contains(&self, key: usize) -> bool {
        self.map.contains_key(&key)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }
}

/// Indices to prefetch around `current`: current first, then +1,-1,+2,-2,... within [0,len).
/// Forward-biased because navigation usually goes right.
pub fn prefetch_set(current: usize, n: usize, len: usize) -> Vec<usize> {
    let mut out = Vec::new();
    if len == 0 {
        return out;
    }
    let current = current.min(len - 1);
    out.push(current);
    for d in 1..=n {
        if current + d < len {
            out.push(current + d);
        }
        if current >= d {
            out.push(current - d);
        }
    }
    out
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use culler_core::decode::DecodedImage;
    use std::sync::Arc;

    fn img(bytes: usize) -> Arc<DecodedImage> {
        Arc::new(DecodedImage {
            w: 1,
            h: bytes as u32,
            rgba: vec![0u8; bytes],
        })
    }

    #[test]
    fn lru_evicts_least_recently_used_over_budget() {
        let mut c = LruCache::new(300);
        c.put(0, img(100));
        c.put(1, img(100));
        c.put(2, img(100));
        assert_eq!(c.len(), 3);
        assert!(c.get(0).is_some()); // touch 0 -> now MRU; 1 becomes LRU
        c.put(3, img(100)); // over budget -> evict LRU (1)
        assert!(c.contains(0));
        assert!(!c.contains(1));
        assert!(c.contains(2));
        assert!(c.contains(3));
        assert!(c.used_bytes() <= 300);
    }

    #[test]
    fn lru_get_absent_is_none() {
        let mut c = LruCache::new(100);
        assert!(c.get(42).is_none());
    }

    #[test]
    fn lru_put_same_key_updates_not_duplicates() {
        let mut c = LruCache::new(1000);
        c.put(7, img(100));
        c.put(7, img(200));
        assert_eq!(c.len(), 1);
        assert_eq!(c.used_bytes(), 200);
    }

    #[test]
    fn prefetch_is_forward_biased_and_clamped() {
        assert_eq!(prefetch_set(5, 2, 100), vec![5, 6, 4, 7, 3]);
        assert_eq!(prefetch_set(0, 2, 100), vec![0, 1, 2]); // clamp at start
        assert_eq!(prefetch_set(99, 2, 100), vec![99, 98, 97]); // clamp at end
        assert_eq!(prefetch_set(0, 3, 0), Vec::<usize>::new()); // empty set
    }
}
