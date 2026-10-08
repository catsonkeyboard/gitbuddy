//! Bounded, weighted LRU storage. Limits apply to retained cache values, not
//! temporary worker allocations or the currently displayed document.
use std::{cell::Cell, collections::HashMap, hash::Hash};

pub trait Weight {
    fn bytes(&self) -> usize;
}
#[derive(Clone)]
struct Entry<V> {
    value: V,
    used: Cell<u64>,
}
#[derive(Clone)]
pub struct Cache<K, V> {
    values: HashMap<K, Entry<V>>,
    clock: Cell<u64>,
    max_entries: usize,
    max_bytes: usize,
}
impl<K: Eq + Hash + Clone, V: Weight> Cache<K, V> {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            values: HashMap::new(),
            clock: Cell::new(0),
            max_entries,
            max_bytes,
        }
    }
    fn tick(&self) -> u64 {
        let next = self.clock.get().saturating_add(1);
        self.clock.set(next);
        next
    }
    pub fn get(&self, key: &K) -> Option<&V> {
        let entry = self.values.get(key)?;
        entry.used.set(self.tick());
        Some(&entry.value)
    }
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let used = self.tick();
        let entry = self.values.get_mut(key)?;
        entry.used.set(used);
        Some(&mut entry.value)
    }
    pub fn contains_key(&self, key: &K) -> bool {
        self.values.contains_key(key)
    }
    pub fn insert(&mut self, key: K, value: V) {
        if value.bytes() > self.max_bytes || self.max_entries == 0 {
            self.values.remove(&key);
            return;
        }
        let used = self.tick();
        self.values.insert(
            key,
            Entry {
                value,
                used: Cell::new(used),
            },
        );
        self.trim();
    }
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        self.values.retain(|k, e| keep(k, &mut e.value));
        self.trim();
    }
    pub fn clear(&mut self) {
        self.values.clear();
    }
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.values.iter().map(|(k, e)| (k, &e.value))
    }
    pub fn len(&self) -> usize {
        self.values.len()
    }
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    pub fn bytes(&self) -> usize {
        self.values.values().map(|e| e.value.bytes()).sum()
    }
    pub fn set_limits(&mut self, entries: usize, bytes: usize) {
        self.max_entries = entries;
        self.max_bytes = bytes;
        self.trim();
    }
    fn trim(&mut self) {
        let mut bytes = self.bytes();
        while self.values.len() > self.max_entries || bytes > self.max_bytes {
            let Some(key) = self
                .values
                .iter()
                .min_by_key(|(_, e)| e.used.get())
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(entry) = self.values.remove(&key) {
                bytes = bytes.saturating_sub(entry.value.bytes());
            }
        }
    }
}
impl Weight for crate::git::CommitDetail {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.id.capacity()
            + self.subject.capacity()
            + self.body.capacity()
            + self.author.capacity()
            + self.date.capacity()
            + self.files.capacity() * std::mem::size_of::<crate::git::CommitFile>()
            + self
                .files
                .iter()
                .map(|f| {
                    f.path.as_os_str().len()
                        + f.original.as_ref().map_or(0, |p| p.as_os_str().len())
                })
                .sum::<usize>()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    impl Weight for String {
        fn bytes(&self) -> usize {
            self.len()
        }
    }
    #[test]
    fn reads_update_recency_and_both_limits_apply() {
        let mut cache: Cache<i32, String> = Cache::new(2, 6);
        cache.insert(1, "abc".into());
        cache.insert(2, "de".into());
        assert!(cache.get(&1).is_some());
        cache.insert(3, "f".into());
        assert!(cache.get(&2).is_none());
        cache.insert(4, "1234567".into());
        assert!(cache.get(&4).is_none());
        assert_eq!(cache.len(), 2);
    }
    #[test]
    fn replacement_and_lowered_limits_release_old_values() {
        let mut cache: Cache<i32, String> = Cache::new(10, 100);
        cache.insert(1, "a".into());
        cache.insert(1, "bbb".into());
        assert_eq!(cache.bytes(), 3);
        assert_eq!(cache.len(), 1);
        cache.insert(2, "cc".into());
        cache.set_limits(1, 2);
        assert_eq!(cache.get(&2).unwrap(), "cc");
        cache.get_mut(&2).unwrap().push('d');
        cache.retain(|_, _| true);
        assert!(cache.is_empty());
    }
}
