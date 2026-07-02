//! Per-writer storage: each writer core owns its own byte-store.

use autobase::WriterKey;
use storage::{MemoryStore, Store};

/// Yields an isolated, possibly-durable byte-[`Store`] per writer core.
///
/// A [`Room`](crate::Room) is backed by one hypercore per writer, and each
/// hypercore owns its **full** `u64` keyspace (blocks at `0..len` plus reserved
/// metadata at the top). Two writers therefore cannot share a store — the factory
/// hands each writer its own. On the node this is a file per writer
/// ([`DiskStoreFactory`]) so rooms survive a restart; in tests it is a fresh
/// in-memory map ([`MemStoreFactory`]).
///
/// [`known_writers`](Self::known_writers) lets [`Room::open`](crate::Room::open)
/// enumerate the writers a durable factory has persisted, so it can reopen every
/// writer's core from local storage — the resume path — without any peer.
pub trait StoreFactory {
    type Store: Store;
    /// Error from opening a writer's store (e.g. disk I/O). `Infallible` in-memory.
    type OpenError: core::fmt::Debug;

    /// Open (create or load) the byte-store backing `writer`'s core.
    fn open(&mut self, writer: WriterKey) -> Result<Self::Store, Self::OpenError>;

    /// The writers this factory has durably persisted (empty for an ephemeral one).
    fn known_writers(&self) -> Vec<WriterKey>;
}

/// A fresh [`MemoryStore`] per writer — ephemeral (nothing survives a drop), for
/// tests and browser in-mem rooms. `known_writers` is always empty, so a `Room`
/// over this factory always starts fresh.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemStoreFactory;

impl StoreFactory for MemStoreFactory {
    type Store = MemoryStore;
    type OpenError = core::convert::Infallible;

    fn open(&mut self, _writer: WriterKey) -> Result<MemoryStore, Self::OpenError> {
        Ok(MemoryStore::new())
    }

    fn known_writers(&self) -> Vec<WriterKey> {
        Vec::new()
    }
}

/// A durable, on-disk factory: each writer's log is a `LogStore<StdFile>` file
/// named `hex(writer)` under a directory. This is what makes a room **resume from
/// disk** after a restart — no peers required. Unix-only (the node's target).
#[cfg(unix)]
pub struct DiskStoreFactory {
    dir: std::path::PathBuf,
}

#[cfg(unix)]
impl DiskStoreFactory {
    /// Create/open a factory rooted at `dir` (created if absent). Use a distinct
    /// directory per room.
    pub fn new(dir: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, writer: &WriterKey) -> std::path::PathBuf {
        self.dir.join(hex(writer))
    }
}

#[cfg(unix)]
impl StoreFactory for DiskStoreFactory {
    type Store = storage::LogStore<storage::StdFile>;
    type OpenError = std::io::Error;

    fn open(&mut self, writer: WriterKey) -> Result<Self::Store, std::io::Error> {
        storage::LogStore::open(storage::StdFile::open(self.path(&writer))?)
    }

    fn known_writers(&self) -> Vec<WriterKey> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                if let Some(w) = e.file_name().to_str().and_then(unhex) {
                    out.push(w);
                }
            }
        }
        out
    }
}

#[cfg(unix)]
fn hex(bytes: &WriterKey) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(unix)]
fn unhex(s: &str) -> Option<WriterKey> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}
