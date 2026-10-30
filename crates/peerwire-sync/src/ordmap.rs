//! A map that iterates in insertion order, as a JS `Map` or `Set` does. Where doc.js's behaviour hangs on that
//! order (which idle writer a full context forgets first, the order of an ask's writers or of a copy's entries),
//! the port comes out the same, frame for frame.

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub(crate) struct OrdMap<K, V> {
    next: u64,
    map: BTreeMap<K, (u64, V)>,
    order: BTreeMap<u64, K>,
}

impl<K, V> Default for OrdMap<K, V> {
    fn default() -> Self {
        OrdMap { next: 0, map: BTreeMap::new(), order: BTreeMap::new() }
    }
}

impl<K: Ord + Clone, V> OrdMap<K, V> {
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub(crate) fn contains_key(&self, k: &K) -> bool {
        self.map.contains_key(k)
    }

    pub(crate) fn get(&self, k: &K) -> Option<&V> {
        self.map.get(k).map(|(_, v)| v)
    }

    pub(crate) fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        self.map.get_mut(k).map(|(_, v)| v)
    }

    /// `Map.set`: a new key goes last, a key held keeps its place.
    pub(crate) fn insert(&mut self, k: K, v: V) {
        if let Some((_, old)) = self.map.get_mut(&k) {
            *old = v;
            return;
        }
        self.next += 1;
        self.order.insert(self.next, k.clone());
        self.map.insert(k, (self.next, v));
    }

    pub(crate) fn get_or_insert_with(&mut self, k: &K, f: impl FnOnce() -> V) -> &mut V {
        if !self.map.contains_key(k) {
            self.insert(k.clone(), f());
        }
        self.get_mut(k).expect("just inserted")
    }

    pub(crate) fn remove(&mut self, k: &K) -> Option<V> {
        let (at, v) = self.map.remove(k)?;
        self.order.remove(&at);
        Some(v)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.order.values().map(move |k| (k, &self.map[k].1))
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.order.values()
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    pub(crate) fn into_vec(mut self) -> Vec<(K, V)> {
        let order = std::mem::take(&mut self.order);
        order
            .into_values()
            .map(|k| {
                let (_, v) = self.map.remove(&k).expect("in order");
                (k, v)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterates_as_a_js_map_does() {
        let mut m = OrdMap::default();
        for k in [5, 1, 3] {
            m.insert(k, k * 10);
        }
        m.insert(1, 11); // keeps its place
        m.remove(&5);
        m.insert(5, 50); // goes last
        assert_eq!(m.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>(), [(1, 11), (3, 30), (5, 50)]);
        *m.get_or_insert_with(&7, || 70) += 1;
        assert_eq!(m.into_vec(), [(1, 11), (3, 30), (5, 50), (7, 71)]);
    }
}
