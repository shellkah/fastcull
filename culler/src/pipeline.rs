//! Decode pipeline: scheduling, caching, worker threads. Wired into the event loop by main (Task 11).

/// A decode request tagged with the generation that was current when it was stamped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request {
    pub index: usize,
    pub generation: u64,
}

/// Pure latest-wins scheduler. `generation` bumps once per navigation event.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scheduler {
    // Exercised by `scheduler_tests` below; production staleness checks route
    // through `Pipeline`'s own `AtomicU64` generation counter instead (see
    // `Pipeline::spawn`/`bump`/`enqueue`), so this field is unread outside tests.
    #[allow(dead_code)]
    pub generation: u64,
}

impl Scheduler {
    // Test-only convenience constructor for `scheduler_tests`; `Pipeline`
    // tracks its own generation via `AtomicU64` rather than a `Scheduler`.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self { generation: 0 }
    }

    /// Advance to a new generation (call once per navigation) and return it.
    // Test-only; see the `generation` field's doc comment above.
    #[allow(dead_code)]
    pub fn advance(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Stamp a request for `index` with generation `gen` (typically `self.generation`).
    // Test-only; production stamps `Request` directly in `Pipeline::enqueue`.
    #[allow(dead_code)]
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

use culler_core::decode::{DecodedImage, TargetSize, decode, embedded_thumbnail};
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

    // Test-only convenience (cache_tests below): production code checks
    // `contains`/`get` directly and never needs the aggregate counts.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    #[allow(dead_code)]
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

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};

/// A job for a worker. `req` carries the generation for latest-wins dropping.
pub struct DecodeRequest {
    pub req: Request,
    pub path: PathBuf,
    pub target: TargetSize,
    pub thumb_first: bool, // filmstrip: try embedded EXIF thumbnail for instant first paint
}

/// A decoded result handed back to the event loop.
pub struct DecodeResult {
    pub req: Request,
    pub target: TargetSize,
    pub image: Arc<DecodedImage>,
}

/// Marshal straight-RGBA8 into a `slint::Image` (the ONLY such conversion in the app).
pub fn to_slint_image(img: &DecodedImage) -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(img.w, img.h);
    let bytes = buf.make_mut_bytes();
    let n = bytes.len().min(img.rgba.len());
    bytes[..n].copy_from_slice(&img.rgba[..n]);
    Image::from_rgba8(buf)
}

/// Worker pool + shared generation counter. Requests stamp the current generation;
/// stale ones are dropped at dequeue here and at delivery in `on_ready`.
pub struct Pipeline {
    tx: Sender<DecodeRequest>,
    pub generation: Arc<AtomicU64>,
}

impl Pipeline {
    /// Spawn `workers` decode threads. `on_ready` runs on a worker thread; it should
    /// re-check staleness and marshal onto the event loop via `invoke_from_event_loop`.
    pub fn spawn<F>(workers: usize, on_ready: F) -> Self
    where
        F: Fn(DecodeResult) + Send + Sync + 'static,
    {
        let (tx, rx) = channel::<DecodeRequest>();
        let rx = Arc::new(Mutex::new(rx));
        let generation = Arc::new(AtomicU64::new(0));
        let on_ready = Arc::new(on_ready);
        for _ in 0..workers.max(1) {
            let rx = rx.clone();
            let generation = generation.clone();
            let on_ready = on_ready.clone();
            std::thread::spawn(move || {
                loop {
                    let job = {
                        let guard = rx.lock().unwrap();
                        guard.recv()
                    };
                    let Ok(job) = job else { break }; // channel closed -> exit
                    // DROP AT DEQUEUE
                    if Scheduler::is_stale(&job.req, generation.load(Ordering::SeqCst)) {
                        continue;
                    }
                    // Filmstrip fast path: embedded EXIF thumbnail first, refined later.
                    if job.thumb_first
                        && let Some(t) = embedded_thumbnail(&job.path)
                    {
                        on_ready(DecodeResult {
                            req: job.req,
                            target: job.target,
                            image: Arc::new(t),
                        });
                    }
                    match decode(&job.path, job.target) {
                        Ok(img) => on_ready(DecodeResult {
                            req: job.req,
                            target: job.target,
                            image: Arc::new(img),
                        }),
                        Err(e) => eprintln!("decode {:?} failed: {:?}", job.path, e),
                    }
                }
            });
        }
        Pipeline { tx, generation }
    }

    /// Bump the shared generation (call once per navigation) and return the new value.
    pub fn bump(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Enqueue a request stamped with the current generation.
    pub fn enqueue(&self, index: usize, path: PathBuf, target: TargetSize, thumb_first: bool) {
        let r#gen = self.generation.load(Ordering::SeqCst);
        let _ = self.tx.send(DecodeRequest {
            req: Request {
                index,
                generation: r#gen,
            },
            path,
            target,
            thumb_first,
        });
    }
}

/// Single dedicated slot for the current 1:1/Full frame, kept OUT of the LRU so a
/// ~180 MB RGBA decode never evicts prefetched fit-size neighbors (§12).
#[derive(Default)]
pub struct FullSlot {
    pub index: Option<usize>,
    pub image: Option<Arc<DecodedImage>>,
}

impl FullSlot {
    /// Store the full decode for `index`, replacing any prior one.
    pub fn set(&mut self, index: usize, image: Arc<DecodedImage>) {
        self.index = Some(index);
        self.image = Some(image);
    }

    /// The stored full image iff it is for `index`.
    pub fn get(&self, index: usize) -> Option<Arc<DecodedImage>> {
        if self.index == Some(index) {
            self.image.clone()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod marshal_tests {
    use super::*;
    use culler_core::decode::DecodedImage;

    #[test]
    fn to_slint_image_preserves_dimensions() {
        let d = DecodedImage {
            w: 4,
            h: 3,
            rgba: vec![0u8; 4 * 3 * 4],
        };
        let img = to_slint_image(&d);
        assert_eq!(img.size().width, 4);
        assert_eq!(img.size().height, 3);
    }
}

/// A 1x1 grey placeholder image for filmstrip tiles not yet decoded.
pub fn grey_thumb() -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(1, 1);
    buf.make_mut_bytes().copy_from_slice(&[128, 128, 128, 255]);
    Image::from_rgba8(buf)
}
