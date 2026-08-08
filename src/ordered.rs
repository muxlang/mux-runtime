//! Insertion-ordered hash map and set backing Mux's `map` and `set`.
//!
//! These were `BTreeMap`/`BTreeSet`, which cost O(log n) on every operation.
//! A plain hash table would be O(1) but iterate in an order that shifts as the
//! table grows, and `map.to_string()` is user-visible output - a program's
//! printed result must not depend on how the table happened to rehash.
//!
//! So: a hash table for lookup, plus a doubly-linked list threaded through the
//! entries for order. That is the design behind Java's `LinkedHashMap`, and it
//! gives O(1) insert, lookup and removal while iterating in insertion order,
//! which is also what Python dicts and JavaScript `Map` guarantee.
//!
//! The links are `usize` indices into a slab rather than pointers, so the whole
//! structure is safe Rust and the entries live in one allocation instead of one
//! per key. `hashbrown::HashTable` stores those indices and takes the hash from
//! the caller, which is also the hook a user-defined `hash` will need.

use hashbrown::HashTable;
use std::borrow::Borrow;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};

/// One slab position: a live entry with its neighbours, or a hole waiting to be
/// reused. `next_free` chains the holes so an insert after a removal is O(1).
#[derive(Clone, Debug)]
enum Slot<K, V> {
    Occupied {
        key: K,
        value: V,
        prev: Option<usize>,
        next: Option<usize>,
    },
    Free {
        next_free: Option<usize>,
    },
}

/// A map that iterates in insertion order with O(1) operations.
#[derive(Clone)]
pub struct OrderedMap<K, V, S = RandomState> {
    table: HashTable<usize>,
    slab: Vec<Slot<K, V>>,
    head: Option<usize>,
    tail: Option<usize>,
    free_head: Option<usize>,
    len: usize,
    hasher: S,
}

impl<K: Hash + Eq, V> Default for OrderedMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Hash + Eq, V> OrderedMap<K, V> {
    pub fn new() -> Self {
        Self {
            table: HashTable::new(),
            slab: Vec::new(),
            head: None,
            tail: None,
            free_head: None,
            len: 0,
            hasher: RandomState::new(),
        }
    }
}

impl<K, V, S> OrderedMap<K, V, S>
where
    K: Hash + Eq,
    S: BuildHasher,
{
    fn hash_of<Q>(&self, key: &Q) -> u64
    where
        Q: Hash + ?Sized,
    {
        self.hasher.hash_one(key)
    }

    fn key_at(&self, index: usize) -> &K {
        match &self.slab[index] {
            Slot::Occupied { key, .. } => key,
            Slot::Free { .. } => unreachable!("table index points at a free slot"),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Find the slab index for a key, if present.
    fn index_of<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.index_of_hashed(self.hash_of(key), key)
    }

    /// `index_of` for a caller that has already hashed the key, so an insert
    /// does not hash it once to look for it and again to store it.
    fn index_of_hashed<Q>(&self, hash: u64, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.table
            .find(hash, |&i| self.key_at(i).borrow() == key)
            .copied()
    }

    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let index = self.index_of(key)?;
        match &self.slab[index] {
            Slot::Occupied { value, .. } => Some(value),
            Slot::Free { .. } => None,
        }
    }

    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let index = self.index_of(key)?;
        match &mut self.slab[index] {
            Slot::Occupied { value, .. } => Some(value),
            Slot::Free { .. } => None,
        }
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.index_of(key).is_some()
    }

    /// Take a slot for a new entry, reusing a hole when one exists.
    fn alloc_slot(&mut self, slot: Slot<K, V>) -> usize {
        match self.free_head {
            Some(index) => {
                let next_free = match self.slab[index] {
                    Slot::Free { next_free } => next_free,
                    Slot::Occupied { .. } => unreachable!("free list points at a live slot"),
                };
                self.free_head = next_free;
                self.slab[index] = slot;
                index
            }
            None => {
                self.slab.push(slot);
                self.slab.len() - 1
            }
        }
    }

    /// Insert, replacing any existing value. An existing key keeps its position
    /// in the order, matching Python and JavaScript: re-assigning does not move
    /// the entry to the end.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let hash = self.hash_of(&key);
        if let Some(index) = self.index_of_hashed(hash, &key) {
            let Slot::Occupied { value: slot, .. } = &mut self.slab[index] else {
                unreachable!("table index points at a free slot");
            };
            return Some(std::mem::replace(slot, value));
        }

        let index = self.alloc_slot(Slot::Occupied {
            key,
            value,
            prev: self.tail,
            next: None,
        });

        match self.tail {
            Some(previous) => {
                if let Slot::Occupied { next, .. } = &mut self.slab[previous] {
                    *next = Some(index);
                }
            }
            None => self.head = Some(index),
        }
        self.tail = Some(index);

        // The rehash closure needs each stored index's own hash, which means
        // reading its key back out of the slab.
        let slab = &self.slab;
        let hasher = &self.hasher;
        self.table.insert_unique(hash, index, |&i| match &slab[i] {
            Slot::Occupied { key, .. } => hasher.hash_one(key),
            Slot::Free { .. } => unreachable!("table index points at a free slot"),
        });
        self.len += 1;
        None
    }

    /// Remove a key in O(1), leaving the order of every other entry untouched.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.hash_of(key);
        let index = *self.table.find(hash, |&i| self.key_at(i).borrow() == key)?;
        self.table
            .find_entry(hash, |&i| i == index)
            .ok()
            .map(|entry| entry.remove());

        let Slot::Occupied {
            value, prev, next, ..
        } = std::mem::replace(
            &mut self.slab[index],
            Slot::Free {
                next_free: self.free_head,
            },
        )
        else {
            unreachable!("table index points at a free slot");
        };
        self.free_head = Some(index);

        match prev {
            Some(p) => {
                if let Slot::Occupied { next: n, .. } = &mut self.slab[p] {
                    *n = next;
                }
            }
            None => self.head = next,
        }
        match next {
            Some(n) => {
                if let Slot::Occupied { prev: p, .. } = &mut self.slab[n] {
                    *p = prev;
                }
            }
            None => self.tail = prev,
        }

        self.len -= 1;
        Some(value)
    }

    pub fn clear(&mut self) {
        self.table.clear();
        self.slab.clear();
        self.head = None;
        self.tail = None;
        self.free_head = None;
        self.len = 0;
    }

    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            slab: &self.slab,
            next: self.head,
            remaining: self.len,
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }
}

/// Walks the linked list, so iteration is insertion order rather than slab or
/// bucket order.
pub struct Iter<'a, K, V> {
    slab: &'a [Slot<K, V>],
    next: Option<usize>,
    remaining: usize,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        let index = self.next?;
        match &self.slab[index] {
            Slot::Occupied {
                key, value, next, ..
            } => {
                self.next = *next;
                self.remaining -= 1;
                Some((key, value))
            }
            // Loud rather than silent: the link chain only reaches live slots,
            // and a map quietly losing entries is worse than a panic. Every
            // other impossible-state path here does the same.
            Slot::Free { .. } => unreachable!("the link chain reached a free slot"),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'a, K: Hash + Eq, V, S: BuildHasher> IntoIterator for &'a OrderedMap<K, V, S> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K: Hash + Eq, V> FromIterator<(K, V)> for OrderedMap<K, V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let mut map = Self::new();
        for (k, v) in iter {
            map.insert(k, v);
        }
        map
    }
}

impl<K: Hash + Eq + PartialEq, V: PartialEq, S: BuildHasher> PartialEq for OrderedMap<K, V, S> {
    /// Order-insensitive, like every other language's map equality: two maps
    /// with the same pairs are equal however they were built.
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len
            && self
                .iter()
                .all(|(k, v)| other.get(k).is_some_and(|ov| ov == v))
    }
}

impl<K: Hash + Eq, V: Eq, S: BuildHasher> Eq for OrderedMap<K, V, S> {}

impl<K: Hash + Eq + std::fmt::Debug, V: std::fmt::Debug, S: BuildHasher> std::fmt::Debug
    for OrderedMap<K, V, S>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// A set with the same guarantees, built on the map so the ordering and slab
/// logic exist in one place.
#[derive(Clone)]
pub struct OrderedSet<T, S = RandomState> {
    map: OrderedMap<T, (), S>,
}

impl<T: Hash + Eq> Default for OrderedSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Hash + Eq> OrderedSet<T> {
    pub fn new() -> Self {
        Self {
            map: OrderedMap::new(),
        }
    }
}

impl<T, S> OrderedSet<T, S>
where
    T: Hash + Eq,
    S: BuildHasher,
{
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn insert(&mut self, value: T) -> bool {
        self.map.insert(value, ()).is_none()
    }

    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(value)
    }

    pub fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.remove(value).is_some()
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn iter(&self) -> SetIter<'_, T> {
        SetIter {
            inner: self.map.iter(),
        }
    }
}

/// Named so `&OrderedSet` can implement `IntoIterator` without boxing the
/// iterator, which cost a heap allocation on every traversal.
pub struct SetIter<'a, T> {
    inner: Iter<'a, T, ()>,
}

impl<'a, T> Iterator for SetIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(value, ())| value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a, T: Hash + Eq, S: BuildHasher> IntoIterator for &'a OrderedSet<T, S> {
    type Item = &'a T;
    type IntoIter = SetIter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T: Hash + Eq> FromIterator<T> for OrderedSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut set = Self::new();
        for value in iter {
            set.insert(value);
        }
        set
    }
}

impl<T: Hash + Eq, S: BuildHasher> PartialEq for OrderedSet<T, S> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|v| other.contains(v))
    }
}

impl<T: Hash + Eq, S: BuildHasher> Eq for OrderedSet<T, S> {}

impl<T: Hash + Eq + std::fmt::Debug, S: BuildHasher> std::fmt::Debug for OrderedSet<T, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<K: Hash + Eq + Ord, V: Ord, S: BuildHasher> Ord for OrderedMap<K, V, S> {
    /// Compares contents, not insertion order.
    ///
    /// `Ord` has to agree with `Eq`, and `Eq` here is order-insensitive - two
    /// maps with the same pairs are equal however they were built. So ordering
    /// sorts the entries first; comparing them in iteration order would report
    /// two equal maps as unequal.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let mut mine: Vec<_> = self.iter().collect();
        let mut theirs: Vec<_> = other.iter().collect();
        mine.sort_by(|a, b| a.0.cmp(b.0));
        theirs.sort_by(|a, b| a.0.cmp(b.0));
        mine.cmp(&theirs)
    }
}

impl<K: Hash + Eq + Ord, V: Ord, S: BuildHasher> PartialOrd for OrderedMap<K, V, S> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<K: Hash + Eq, V: Hash, S: BuildHasher> Hash for OrderedMap<K, V, S> {
    /// Combines entry hashes commutatively, so that - like `Eq` - the result
    /// does not depend on insertion order.
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut combined = 0u64;
        for (key, value) in self.iter() {
            // Deliberately NOT `self.hasher`: each map builds its own
            // `RandomState`, so using it would give two equal maps different
            // hashes and break a map used as a key. `DefaultHasher` has fixed
            // keys, so the same contents always produce the same value.
            let mut entry = std::collections::hash_map::DefaultHasher::new();
            key.hash(&mut entry);
            value.hash(&mut entry);
            combined = combined.wrapping_add(entry.finish());
        }
        self.len.hash(state);
        combined.hash(state);
    }
}

impl<T: Hash + Eq + Ord, S: BuildHasher> Ord for OrderedSet<T, S> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let mut mine: Vec<_> = self.iter().collect();
        let mut theirs: Vec<_> = other.iter().collect();
        mine.sort();
        theirs.sort();
        mine.cmp(&theirs)
    }
}

impl<T: Hash + Eq + Ord, S: BuildHasher> PartialOrd for OrderedSet<T, S> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Hash + Eq, S: BuildHasher> Hash for OrderedSet<T, S> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.map.hash(state);
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> Extend<(K, V)> for OrderedMap<K, V, S> {
    fn extend<T: IntoIterator<Item = (K, V)>>(&mut self, iter: T) {
        for (key, value) in iter {
            self.insert(key, value);
        }
    }
}

/// Consuming iteration, still in insertion order. Takes each entry out of the
/// slab as it walks, so it allocates nothing beyond the map it consumes.
pub struct IntoIter<K, V> {
    slab: Vec<Slot<K, V>>,
    next: Option<usize>,
    remaining: usize,
}

impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);

    fn next(&mut self) -> Option<Self::Item> {
        let index = self.next?;
        match std::mem::replace(&mut self.slab[index], Slot::Free { next_free: None }) {
            Slot::Occupied {
                key,
                value,
                next: following,
                ..
            } => {
                self.next = following;
                self.remaining -= 1;
                Some((key, value))
            }
            Slot::Free { .. } => unreachable!("the link chain reached a free slot"),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> IntoIterator for OrderedMap<K, V, S> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            slab: self.slab,
            next: self.head,
            remaining: self.len,
        }
    }
}

impl<T: Hash + Eq, S: BuildHasher> Extend<T> for OrderedSet<T, S> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for value in iter {
            self.insert(value);
        }
    }
}

impl<T: Hash + Eq, S: BuildHasher> IntoIterator for OrderedSet<T, S> {
    type Item = T;
    type IntoIter = SetIntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        SetIntoIter {
            inner: self.map.into_iter(),
        }
    }
}

/// Consuming set iteration, allocation-free for the same reason as the map's.
pub struct SetIntoIter<T> {
    inner: IntoIter<T, ()>,
}

impl<T> Iterator for SetIntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(value, ())| value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn ordering_agrees_with_equality_regardless_of_insertion_order() {
        let a: OrderedMap<i32, i32> = [(1, 1), (2, 2)].into_iter().collect();
        let b: OrderedMap<i32, i32> = [(2, 2), (1, 1)].into_iter().collect();
        assert_eq!(a, b);
        assert_eq!(a.cmp(&b), std::cmp::Ordering::Equal);
    }

    #[test]
    fn hashing_agrees_with_equality_regardless_of_insertion_order() {
        use std::collections::hash_map::DefaultHasher;
        let hash_of = |m: &OrderedMap<i32, i32>| {
            let mut h = DefaultHasher::new();
            m.hash(&mut h);
            h.finish()
        };
        let a: OrderedMap<i32, i32> = [(1, 1), (2, 2)].into_iter().collect();
        let b: OrderedMap<i32, i32> = [(2, 2), (1, 1)].into_iter().collect();
        assert_eq!(hash_of(&a), hash_of(&b));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterates_in_insertion_order_not_sorted_order() {
        let mut map = OrderedMap::new();
        for key in [3, 1, 2] {
            map.insert(key, key * 10);
        }
        let seen: Vec<_> = map.keys().copied().collect();
        assert_eq!(seen, vec![3, 1, 2]);
    }

    #[test]
    fn removal_keeps_the_order_of_everything_else() {
        // The property indexmap cannot give cheaply: O(1) removal that does not
        // move an unrelated entry.
        let mut map = OrderedMap::new();
        for key in 0..6 {
            map.insert(key, key);
        }
        assert_eq!(map.remove(&2), Some(2));
        assert_eq!(map.remove(&4), Some(4));
        let seen: Vec<_> = map.keys().copied().collect();
        assert_eq!(seen, vec![0, 1, 3, 5]);
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn reinsert_after_removal_reuses_the_hole_and_appends() {
        let mut map = OrderedMap::new();
        for key in 0..3 {
            map.insert(key, key);
        }
        map.remove(&0);
        map.insert(9, 9);
        let seen: Vec<_> = map.keys().copied().collect();
        assert_eq!(seen, vec![1, 2, 9]);
    }

    #[test]
    fn reassigning_keeps_the_original_position() {
        let mut map = OrderedMap::new();
        for key in [1, 2, 3] {
            map.insert(key, key);
        }
        assert_eq!(map.insert(1, 100), Some(1));
        let seen: Vec<_> = map.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(seen, vec![(1, 100), (2, 2), (3, 3)]);
    }

    #[test]
    fn removing_the_only_entry_empties_the_list() {
        let mut map = OrderedMap::new();
        map.insert("solo", 1);
        assert_eq!(map.remove("solo"), Some(1));
        assert!(map.is_empty());
        assert_eq!(map.keys().count(), 0);
        map.insert("again", 2);
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), vec!["again"]);
    }

    #[test]
    fn removing_head_and_tail_relinks_the_ends() {
        let mut map = OrderedMap::new();
        for key in 0..4 {
            map.insert(key, key);
        }
        map.remove(&0);
        map.remove(&3);
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
        map.insert(7, 7);
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), vec![1, 2, 7]);
    }

    #[test]
    fn equality_ignores_order() {
        let a: OrderedMap<i32, i32> = [(1, 1), (2, 2)].into_iter().collect();
        let b: OrderedMap<i32, i32> = [(2, 2), (1, 1)].into_iter().collect();
        assert_eq!(a, b);
    }

    #[test]
    fn set_keeps_first_insertion_position_for_a_duplicate() {
        let mut set = OrderedSet::new();
        assert!(set.insert("a"));
        assert!(set.insert("b"));
        assert!(!set.insert("a"));
        assert_eq!(set.iter().copied().collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn consuming_iteration_yields_insertion_order_and_all_entries() {
        let mut map = OrderedMap::new();
        for key in [5, 1, 4] {
            map.insert(key, key * 2);
        }
        map.remove(&1);
        map.insert(9, 18);
        let drained: Vec<_> = map.into_iter().collect();
        assert_eq!(drained, vec![(5, 10), (4, 8), (9, 18)]);
    }

    #[test]
    fn consuming_set_iteration_yields_insertion_order() {
        let mut set = OrderedSet::new();
        for value in ["c", "a", "b"] {
            set.insert(value);
        }
        set.remove("a");
        assert_eq!(set.into_iter().collect::<Vec<_>>(), vec!["c", "b"]);
    }

    #[test]
    fn iterators_report_an_exact_size() {
        let mut map = OrderedMap::new();
        for key in 0..5 {
            map.insert(key, key);
        }
        map.remove(&2);
        assert_eq!(map.iter().size_hint(), (4, Some(4)));
        let set: OrderedSet<i32> = (0..3).collect();
        assert_eq!(set.iter().size_hint(), (3, Some(3)));
    }

    #[test]
    fn survives_many_interleaved_inserts_and_removals() {
        let mut map = OrderedMap::new();
        for key in 0..500 {
            map.insert(key, key);
        }
        for key in (0..500).step_by(3) {
            map.remove(&key);
        }
        for key in 500..700 {
            map.insert(key, key);
        }
        let expected: Vec<i32> = (0..500).filter(|k| k % 3 != 0).chain(500..700).collect();
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), expected);
        assert_eq!(map.len(), expected.len());
        for key in &expected {
            assert_eq!(map.get(key), Some(key));
        }
    }
}
