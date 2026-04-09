use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::marker::PhantomData;

/// A contract that arena items must expose their insertion name.
///
/// Implementors must return the exact string the item was inserted under.
/// The [`Arena`] serde implementation uses this to rebuild its name index
/// from the `items` vector on deserialization — **violating this contract
/// silently produces an `Arena` whose `lookup()` disagrees with its stored
/// data**, which is a correctness hazard that no compile-time check can
/// catch. If an item type cannot reliably expose its insertion name, it
/// should not be stored in an `Arena`.
pub trait Named {
    fn name(&self) -> &str;
}

/// Typed index into an [`Arena`].
///
/// Cheap to copy; safe to round-trip via `serde` (transparently as a `u32`).
/// `PhantomData<fn() -> T>` is used so `Handle<T>` is `Send + Sync` regardless
/// of `T`.
pub struct Handle<T> {
    index: u32,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
    pub fn new(index: u32) -> Self {
        Self {
            index,
            _marker: PhantomData,
        }
    }

    pub fn index(self) -> u32 {
        self.index
    }

    pub fn as_usize(self) -> usize {
        self.index as usize
    }
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl<T> Eq for Handle<T> {}

impl<T> PartialOrd for Handle<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Ordering is by arena index, which equals insertion order because arenas
/// are push-only. `BTreeSet<Handle<_>>` iterates in the order items were
/// defined.
impl<T> Ord for Handle<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.index.cmp(&other.index)
    }
}

impl<T> std::hash::Hash for Handle<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl<T> std::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Handle<{}>({})", std::any::type_name::<T>(), self.index)
    }
}

impl<T> Serialize for Handle<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.index.serialize(s)
    }
}

impl<'de, T> Deserialize<'de> for Handle<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        u32::deserialize(d).map(Handle::new)
    }
}

/// Push-only typed storage with a side `BTreeMap` name index.
///
/// The name index is not serialized; it is rebuilt on deserialization from
/// `T::name()` so JSON round-trips preserve lookups while keeping the
/// serialized output compact and free of desync risk.
#[derive(Debug, Clone)]
pub struct Arena<T> {
    items: Vec<T>,
    name_index: BTreeMap<String, Handle<T>>,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            name_index: BTreeMap::new(),
        }
    }
}

impl<T> Arena<T> {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            name_index: BTreeMap::new(),
        }
    }

    /// Insert a named item into the arena.
    ///
    /// Returns `(handle, newly_inserted)`. If `name` is already present, the
    /// existing handle is returned with `newly_inserted = false` and `value` is
    /// discarded. The caller is responsible for deciding whether a duplicate
    /// definition is a validation error (typically it is, but callers may want
    /// to report all duplicates at once rather than failing on the first).
    pub fn insert(&mut self, name: String, value: T) -> (Handle<T>, bool) {
        if let Some(h) = self.name_index.get(&name) {
            return (*h, false);
        }
        let h = Handle::new(self.items.len() as u32);
        self.items.push(value);
        self.name_index.insert(name, h);
        (h, true)
    }

    pub fn get(&self, h: Handle<T>) -> Option<&T> {
        self.items.get(h.as_usize())
    }

    pub fn get_mut(&mut self, h: Handle<T>) -> Option<&mut T> {
        self.items.get_mut(h.as_usize())
    }

    pub fn lookup(&self, name: &str) -> Option<Handle<T>> {
        self.name_index.get(name).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (Handle<T>, &T)> {
        self.items
            .iter()
            .enumerate()
            .map(|(i, v)| (Handle::new(i as u32), v))
    }

    pub fn names(&self) -> impl Iterator<Item = (&str, Handle<T>)> {
        self.name_index.iter().map(|(k, v)| (k.as_str(), *v))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl<T: Serialize> Serialize for Arena<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // Only serialize items. The name_index is a derived view.
        self.items.serialize(s)
    }
}

impl<'de, T> Deserialize<'de> for Arena<T>
where
    T: Deserialize<'de> + Named,
{
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let items: Vec<T> = Vec::deserialize(d)?;
        let mut name_index = BTreeMap::new();
        for (i, item) in items.iter().enumerate() {
            let name = item.name().to_string();
            if name_index.contains_key(&name) {
                return Err(D::Error::custom(format!(
                    "arena deserialization: duplicate item name {name:?} at index {i}"
                )));
            }
            name_index.insert(name, Handle::new(i as u32));
        }
        Ok(Arena { items, name_index })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Item;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct NamedItem {
        name: String,
        value: String,
    }

    impl Named for NamedItem {
        fn name(&self) -> &str {
            &self.name
        }
    }

    #[test]
    fn handle_copy_eq_hash() {
        let a = Handle::<Item>::new(7);
        let b = a;
        assert_eq!(a, b);
        let mut set = std::collections::HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn arena_insert_get_lookup() {
        let mut arena = Arena::<NamedItem>::new();
        let (h, inserted) = arena.insert(
            "foo".into(),
            NamedItem {
                name: "foo".into(),
                value: "first".into(),
            },
        );
        assert!(inserted);
        assert_eq!(arena.get(h).map(|i| i.value.as_str()), Some("first"));
        assert_eq!(arena.lookup("foo"), Some(h));
        assert_eq!(arena.lookup("bar"), None);
        assert_eq!(arena.len(), 1);
    }

    #[test]
    fn arena_duplicate_insert_returns_existing_handle() {
        let mut arena = Arena::<NamedItem>::new();
        let (h1, inserted1) = arena.insert(
            "foo".into(),
            NamedItem {
                name: "foo".into(),
                value: "first".into(),
            },
        );
        assert!(inserted1);
        let (h2, inserted2) = arena.insert(
            "foo".into(),
            NamedItem {
                name: "foo".into(),
                value: "second".into(),
            },
        );
        assert!(!inserted2);
        assert_eq!(h1, h2);
        // Original value preserved.
        assert_eq!(arena.get(h1).map(|i| i.value.as_str()), Some("first"));
        assert_eq!(arena.len(), 1);
    }

    #[test]
    fn arena_serde_round_trip_rebuilds_name_index() {
        let mut arena = Arena::<NamedItem>::new();
        arena.insert(
            "a".into(),
            NamedItem {
                name: "a".into(),
                value: "alpha".into(),
            },
        );
        arena.insert(
            "b".into(),
            NamedItem {
                name: "b".into(),
                value: "beta".into(),
            },
        );

        let json = serde_json::to_string(&arena).unwrap();
        // Serialized form must be a bare array of items — no name_index field.
        assert!(
            json.starts_with('['),
            "expected bare JSON array (serialized Arena should skip name_index), got: {json}"
        );

        let de: Arena<NamedItem> = serde_json::from_str(&json).unwrap();

        assert_eq!(de.len(), 2);
        let ha = de.lookup("a").expect("name index rebuilt");
        let hb = de.lookup("b").expect("name index rebuilt");
        assert_eq!(de.get(ha).map(|i| i.value.as_str()), Some("alpha"));
        assert_eq!(de.get(hb).map(|i| i.value.as_str()), Some("beta"));
    }

    #[test]
    fn arena_deserialize_rejects_duplicate_names() {
        let json = r#"[{"name":"dup","value":"first"},{"name":"dup","value":"second"}]"#;
        let result: Result<Arena<NamedItem>, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
