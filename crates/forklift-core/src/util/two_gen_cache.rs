use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A bounded, `Arc`-sharing, two-generation approximate-LRU cache keyed by `String`.
///
/// The shape shared by `file_utils`'s raw-bytes read cache and `object_utils`'s parsed-tree
/// cache — factored out here so their eviction and bounding logic can't silently drift apart.
/// Each caller has its own instance (its own budget, its own per-entry ceiling, its own values),
/// so this type carries no domain knowledge: it does not know what a "warehouse" or a "tree" is,
/// only how to bound a map of `Arc`-shared values by an explicit weight.
///
/// Bounding: once the live generation's total weight reaches `budget`, it is retired to the old
/// generation wholesale (O(1), no per-entry eviction) and a fresh live generation starts — an
/// approximate LRU that never exceeds ~2× `budget`. A hit in the old generation is promoted into
/// the live one, so it survives the next retire. An entry heavier than `max_entry` is never
/// cached at all, so one huge entry can never evict the whole working set of smaller entries a
/// caller actually reuses.
///
/// Weight is supplied by the caller at insertion rather than derived from the value: the two
/// current callers weigh differently (a byte cache's own length vs. a parsed tree's *source*
/// byte length as a proxy for its live memory), and a generic `V` has no canonical notion of
/// "size" to derive one from.
pub(crate) struct TwoGenCache<V> {
    state: Mutex<TwoGenCacheState<V>>,
    budget: usize,
    max_entry: usize,
}

struct TwoGenCacheState<V> {
    live: HashMap<String, (Arc<V>, usize)>,
    old: HashMap<String, (Arc<V>, usize)>,
    live_weight: usize,
}

impl<V> TwoGenCache<V> {
    /// Create a new cache bounded to `budget` (approximately — up to ~2×, see the type docs),
    /// refusing to cache any single entry heavier than `max_entry`.
    pub(crate) fn new(budget: usize, max_entry: usize) -> Self {
        Self {
            state: Mutex::new(TwoGenCacheState {
                live: HashMap::new(),
                old: HashMap::new(),
                live_weight: 0,
            }),
            budget,
            max_entry,
        }
    }

    /// Look up `key`. A hit is a pointer clone under the lock, not a copy of the value.
    pub(crate) fn get(&self, key: &str) -> Option<Arc<V>> {
        let mut state = self.state.lock().expect("the cache lock is poisoned");

        if let Some((value, _)) = state.live.get(key) {
            return Some(Arc::clone(value));
        }

        // A hit in the older generation is promoted to the live one (so it survives the next
        // retire).
        if let Some((value, weight)) = state.old.remove(key) {
            let out = Arc::clone(&value);
            state.live_weight += weight;
            state.live.insert(key.to_string(), (value, weight));
            Self::retire_if_full(&mut state, self.budget);
            return Some(out);
        }

        None
    }

    /// Insert `value` under `key` with the given weight — unless it exceeds `max_entry`, or
    /// `key` is already live (first writer wins; a losing race just discards the redundant
    /// insert rather than double-counting its weight).
    pub(crate) fn put(&self, key: &str, value: Arc<V>, weight: usize) {
        if weight > self.max_entry {
            return;
        }

        let mut state = self.state.lock().expect("the cache lock is poisoned");

        if state.live.contains_key(key) {
            return;
        }

        state.live_weight += weight;
        state.live.insert(key.to_string(), (value, weight));
        Self::retire_if_full(&mut state, self.budget);
    }

    /// Drop every entry, in both generations, whose key starts with `prefix`.
    pub(crate) fn remove_prefixed(&self, prefix: &str) {
        let mut state = self.state.lock().expect("the cache lock is poisoned");
        let mut removed = 0;
        state.live.retain(|key, (_, weight)| {
            let keep = !key.starts_with(prefix);
            if !keep {
                removed += *weight;
            }
            keep
        });
        state.live_weight -= removed;
        state.old.retain(|key, _| !key.starts_with(prefix));
    }

    /// Retire the live generation to the old one (dropping the previous old generation) once it
    /// fills — bounding the cache to ~2× `budget` with O(1) eviction.
    fn retire_if_full(state: &mut TwoGenCacheState<V>, budget: usize) {
        if state.live_weight >= budget {
            state.old = std::mem::take(&mut state.live);
            state.live_weight = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `remove_prefixed` drops matching keys from both generations, keeps the rest, and leaves the
    /// live weight counting only what is left, so the next retire still happens on budget.
    #[test]
    fn remove_prefixed_drops_only_matching_keys_from_both_generations() {
        let cache = TwoGenCache::new(10, 10);
        cache.put("a\u{0}old", Arc::new(1), 6);
        cache.put("b\u{0}old", Arc::new(2), 4); // live weight 10 → retired to the old generation
        cache.put("a\u{0}live", Arc::new(3), 3);
        cache.put("b\u{0}live", Arc::new(4), 3);

        cache.remove_prefixed("a\u{0}");

        assert!(cache.get("a\u{0}old").is_none() && cache.get("a\u{0}live").is_none());
        assert_eq!(cache.get("b\u{0}live").as_deref(), Some(&4));
        assert_eq!(cache.get("b\u{0}old").as_deref(), Some(&2)); // promoted: live weight 3 + 4

        let state = cache.state.lock().unwrap();
        assert_eq!(state.live_weight, 7);
    }
}
