//! `storage` — pluggable byte storage for logs, keyed by `u64`.
//!
//! A [`Store`] is where a log's bytes live: blocks and tree nodes addressed by a
//! `u64` key. Backends are swappable — an in-memory map here, native disk and a
//! browser `localStorage`/IndexedDB backend later — so a user's hypercores can
//! persist locally with no server.
//!
//! Content-blind: it stores opaque bytes, never the typed payload. The
//! [`contract`] module is the behaviour every backend must satisfy.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt::Debug;

/// A `u64`-keyed byte store.
pub trait Store {
    type Error: Debug;

    /// Insert or overwrite the value at `key`.
    fn put(&mut self, key: u64, value: &[u8]) -> Result<(), Self::Error>;

    /// Fetch the value at `key`, or `None`.
    fn get(&self, key: u64) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Remove `key` if present.
    fn delete(&mut self, key: u64) -> Result<(), Self::Error>;

    /// Number of stored entries.
    fn len(&self) -> Result<u64, Self::Error>;

    fn contains(&self, key: u64) -> Result<bool, Self::Error> {
        Ok(self.get(key)?.is_some())
    }

    fn is_empty(&self) -> Result<bool, Self::Error> {
        Ok(self.len()? == 0)
    }
}

/// In-memory backend. Never fails (`Error = Infallible`).
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    map: BTreeMap<u64, Vec<u8>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Store for MemoryStore {
    type Error = Infallible;

    fn put(&mut self, key: u64, value: &[u8]) -> Result<(), Infallible> {
        self.map.insert(key, value.to_vec());
        Ok(())
    }

    fn get(&self, key: u64) -> Result<Option<Vec<u8>>, Infallible> {
        Ok(self.map.get(&key).cloned())
    }

    fn delete(&mut self, key: u64) -> Result<(), Infallible> {
        self.map.remove(&key);
        Ok(())
    }

    fn len(&self) -> Result<u64, Infallible> {
        Ok(self.map.len() as u64)
    }
}

/// The behavioural contract every [`Store`] backend must satisfy. The in-memory
/// test and (later) the IndexedDB browser test both call this, so backends are
/// proven against one spec. Panics on any violation.
pub mod contract {
    use super::Store;

    pub fn run<S: Store>(s: &mut S) {
        assert!(s.is_empty().unwrap(), "fresh store is empty");

        s.put(0, b"a").unwrap();
        s.put(1, b"bb").unwrap();
        assert_eq!(s.len().unwrap(), 2);
        assert_eq!(s.get(0).unwrap().as_deref(), Some(&b"a"[..]));
        assert!(s.contains(1).unwrap());

        // missing keys
        assert!(!s.contains(2).unwrap());
        assert_eq!(s.get(2).unwrap(), None);

        // overwrite keeps count, replaces value
        s.put(0, b"A").unwrap();
        assert_eq!(s.get(0).unwrap().as_deref(), Some(&b"A"[..]));
        assert_eq!(s.len().unwrap(), 2);

        // delete
        s.delete(0).unwrap();
        assert_eq!(s.get(0).unwrap(), None);
        assert_eq!(s.len().unwrap(), 1);
        s.delete(0).unwrap(); // deleting a missing key is fine

        // large keys + arbitrary binary
        let blob = vec![0u8, 159, 146, 150, 255];
        s.put(u64::MAX, &blob).unwrap();
        assert_eq!(s.get(u64::MAX).unwrap().unwrap(), blob);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_backend_upholds_contract() {
        contract::run(&mut MemoryStore::new());
    }

    #[test]
    fn empty_and_missing() {
        let s = MemoryStore::new();
        assert!(s.is_empty().unwrap());
        assert_eq!(s.get(5).unwrap(), None);
    }
}
