//! Bounded, owner-bound records that can be consumed once. The caller chooses its token type;
//! security-sensitive callers must use an unpredictable token and an immutable request binding.
use std::collections::VecDeque;

pub struct OneTimeRegistry<K, B, V> {
    capacity: usize,
    entries: VecDeque<(K, B, V)>,
}

impl<K, B, V> Default for OneTimeRegistry<K, B, V> {
    fn default() -> Self {
        Self { capacity: 32, entries: VecDeque::new() }
    }
}

impl<K: PartialEq, B: PartialEq, V> OneTimeRegistry<K, B, V> {
    pub fn insert(&mut self, key: K, binding: B, value: V) {
        self.entries.retain(|(old, _, _)| old != &key);
        self.entries.push_back((key, binding, value));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    pub fn get(&self, key: &K, binding: &B) -> Option<&V> {
        self.entries.iter().find(|(k, b, _)| k == key && b == binding).map(|(_, _, value)| value)
    }
    pub fn contains(&self, key: &K, binding: &B) -> bool {
        self.entries.iter().any(|(k, b, _)| k == key && b == binding)
    }

    pub fn take(&mut self, key: &K, binding: &B) -> Option<V> {
        let index = self.entries.iter().position(|(k, b, _)| k == key && b == binding)?;
        self.entries.remove(index).map(|(_, _, value)| value)
    }
}

#[cfg(test)]
mod tests {
    use super::OneTimeRegistry;

    #[test]
    fn binding_mismatch_does_not_consume_and_records_are_bounded_and_once_only() {
        let mut registry = OneTimeRegistry::default();
        registry.insert(1, "owner/turn/request/snapshot", "approved");
        assert_eq!(registry.take(&1, &"other request"), None);
        assert_eq!(registry.take(&1, &"owner/turn/request/snapshot"), Some("approved"));
        assert_eq!(registry.take(&1, &"owner/turn/request/snapshot"), None);
        // bounded: the 33rd record evicts the oldest
        let mut bounded = OneTimeRegistry::default();
        for key in 0..33 {
            bounded.insert(key, "owner", key);
        }
        assert_eq!(bounded.take(&0, &"owner"), None);
        assert_eq!(bounded.take(&32, &"owner"), Some(32));
    }
}
