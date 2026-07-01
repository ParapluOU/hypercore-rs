//! Per-writer storage: each writer core owns its own byte-store.

use autobase::WriterKey;
use storage::{MemoryStore, Store};

/// Yields an isolated byte-[`Store`] per writer core.
///
/// A [`Room`](crate::Room) is backed by one hypercore per writer, and each
/// hypercore owns its **full** `u64` keyspace (blocks at `0..len`, plus reserved
/// metadata at the top of the range). Two writers therefore cannot share a single
/// store — the factory hands each writer its own. On the node this maps to a file
/// (or DB namespace) per writer; in tests / the browser it is a fresh in-memory
/// map per writer.
pub trait StoreFactory {
    type Store: Store;

    /// Open (create or load) the byte-store backing `writer`'s core.
    fn open(&mut self, writer: WriterKey) -> Self::Store;
}

/// A fresh [`MemoryStore`] per writer — the in-memory backend for tests and
/// browser in-mem rooms.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemStoreFactory;

impl StoreFactory for MemStoreFactory {
    type Store = MemoryStore;

    fn open(&mut self, _writer: WriterKey) -> MemoryStore {
        MemoryStore::new()
    }
}
