//! An LRU bounded by entry count and by summed weight (bytes).

use lru::LruCache;
use std::sync::Mutex;

pub struct Lru<V> {
    inner: Mutex<Inner<V>>,
    max_entries: usize,
    max_bytes: u64,
}

struct Inner<V> {
    map: LruCache<String, (V, u64)>,
    bytes: u64,
}

impl<V: Clone> Lru<V> {
    pub fn new(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: LruCache::unbounded(),
                bytes: 0,
            }),
            max_entries,
            max_bytes,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<V>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn get(&self, key: &str) -> Option<V> {
        self.lock().map.get(key).map(|(v, _)| v.clone())
    }

    /// Inserts `value`, evicting the least recently used entries until both bounds hold. A value
    /// heavier than the whole cache is not kept.
    pub fn put(&self, key: String, value: V, weight: u64) {
        if self.max_entries == 0 || weight > self.max_bytes {
            return;
        }
        let mut inner = self.lock();
        if let Some((_, old)) = inner.map.put(key, (value, weight)) {
            inner.bytes -= old;
        }
        inner.bytes += weight;
        while inner.bytes > self.max_bytes || inner.map.len() > self.max_entries {
            match inner.map.pop_lru() {
                Some((_, (_, w))) => inner.bytes -= w,
                None => break,
            }
        }
    }

    /// Entries and summed weight.
    pub fn size(&self) -> (usize, u64) {
        let inner = self.lock();
        (inner.map.len(), inner.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_by_weight_and_count() {
        let cache = Lru::new(3, 10);
        cache.put("a".into(), 1, 4);
        cache.put("b".into(), 2, 4);
        assert_eq!(cache.get("a"), Some(1));
        cache.put("c".into(), 3, 4);
        assert_eq!(cache.get("b"), None, "least recently used goes first");
        assert_eq!(cache.size(), (2, 8));
        cache.put("big".into(), 4, 11);
        assert_eq!(cache.get("big"), None);
        cache.put("d".into(), 5, 1);
        cache.put("e".into(), 6, 1);
        assert_eq!(cache.size(), (3, 6));
    }
}
