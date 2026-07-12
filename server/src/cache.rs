use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Single-value TTL memo for expensive, user-independent reads (e.g. the
/// anonymous homepage feed). Readers get a shared `Arc` snapshot; on expiry
/// the next request recomputes and `put`s. Concurrent recomputes on expiry
/// are tolerated — the queries this guards are cheap, we only want to shed
/// the per-request steady-state load.
pub struct TtlCell<T> {
    ttl: Duration,
    slot: RwLock<Option<(Instant, Arc<T>)>>,
}

impl<T> TtlCell<T> {
    /// A zero TTL disables caching: `get` always misses.
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            slot: RwLock::new(None),
        }
    }

    pub fn get(&self) -> Option<Arc<T>> {
        if self.ttl.is_zero() {
            return None;
        }
        let slot = self.slot.read().unwrap_or_else(|e| e.into_inner());
        match &*slot {
            Some((stored_at, value)) if stored_at.elapsed() < self.ttl => Some(Arc::clone(value)),
            _ => None,
        }
    }

    pub fn put(&self, value: T) -> Arc<T> {
        let value = Arc::new(value);
        if !self.ttl.is_zero() {
            let mut slot = self.slot.write().unwrap_or_else(|e| e.into_inner());
            *slot = Some((Instant::now(), Arc::clone(&value)));
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_fresh_value_within_ttl() {
        let cell = TtlCell::new(Duration::from_secs(60));
        assert!(cell.get().is_none());
        cell.put(7);
        assert_eq!(cell.get().as_deref(), Some(&7));
    }

    #[test]
    fn zero_ttl_disables_caching() {
        let cell = TtlCell::new(Duration::ZERO);
        cell.put(7);
        assert!(cell.get().is_none());
    }

    #[test]
    fn expired_value_misses() {
        let cell = TtlCell::new(Duration::from_nanos(1));
        cell.put(7);
        std::thread::sleep(Duration::from_millis(1));
        assert!(cell.get().is_none());
    }
}
