//! A write-through store combinator: mirror every write to a secondary "cache".

use crate::Store;

/// Writes go to **both** the `primary` and the `cache`; reads come from the
/// `primary`, falling back to the `cache` when the primary lacks the key.
///
/// The intended use: the `primary` is a consumer's real persistence (a database,
/// a remote store, …) and the `cache` is an always-present *local* copy (e.g.
/// on-disk). A fresh container that still has the local cache can recover a log
/// even if the primary is unavailable or empty — while the consumer's chosen
/// backend remains the source of truth. `put`/`delete` keep the two in lockstep,
/// so under normal operation they hold the same data.
pub struct TeeStore<P, C> {
    primary: P,
    cache: C,
}

impl<P, C> TeeStore<P, C> {
    pub fn new(primary: P, cache: C) -> Self {
        Self { primary, cache }
    }
    pub fn primary(&self) -> &P {
        &self.primary
    }
    pub fn cache(&self) -> &C {
        &self.cache
    }
}

/// Which side of a [`TeeStore`] produced an error.
#[derive(Debug, PartialEq, Eq)]
pub enum TeeError<PE, CE> {
    Primary(PE),
    Cache(CE),
}

impl<P: Store, C: Store> Store for TeeStore<P, C> {
    type Error = TeeError<P::Error, C::Error>;

    fn put(&mut self, key: u64, value: &[u8]) -> Result<(), Self::Error> {
        self.primary.put(key, value).map_err(TeeError::Primary)?;
        self.cache.put(key, value).map_err(TeeError::Cache)?;
        Ok(())
    }

    fn get(&self, key: u64) -> Result<Option<Vec<u8>>, Self::Error> {
        match self.primary.get(key).map_err(TeeError::Primary)? {
            Some(v) => Ok(Some(v)),
            None => self.cache.get(key).map_err(TeeError::Cache),
        }
    }

    fn delete(&mut self, key: u64) -> Result<(), Self::Error> {
        self.primary.delete(key).map_err(TeeError::Primary)?;
        self.cache.delete(key).map_err(TeeError::Cache)?;
        Ok(())
    }

    fn len(&self) -> Result<u64, Self::Error> {
        // Normally in sync; if the primary is empty (fresh container) report the
        // cache's count so a recovering reader sees the local copy.
        let p = self.primary.len().map_err(TeeError::Primary)?;
        if p > 0 {
            Ok(p)
        } else {
            self.cache.len().map_err(TeeError::Cache)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    #[test]
    fn upholds_store_contract() {
        crate::contract::run(&mut TeeStore::new(MemoryStore::new(), MemoryStore::new()));
    }

    #[test]
    fn writes_mirror_to_both_sides() {
        let mut tee = TeeStore::new(MemoryStore::new(), MemoryStore::new());
        tee.put(1, b"x").unwrap();
        tee.put(2, b"y").unwrap();
        tee.delete(2).unwrap();
        assert_eq!(tee.primary().get(1).unwrap().as_deref(), Some(&b"x"[..]));
        assert_eq!(tee.cache().get(1).unwrap().as_deref(), Some(&b"x"[..]), "mirrored to cache");
        assert_eq!(tee.cache().get(2).unwrap(), None, "delete mirrored to cache");
    }

    #[test]
    fn recovers_from_cache_when_primary_is_empty() {
        // Simulate a container restart: the primary is fresh/empty, but the local
        // cache still holds data from a previous run.
        let mut cache = MemoryStore::new();
        cache.put(5, b"survived").unwrap();
        let tee = TeeStore::new(MemoryStore::new(), cache);
        assert_eq!(
            tee.get(5).unwrap().as_deref(),
            Some(&b"survived"[..]),
            "read falls back to the local cache"
        );
    }
}
