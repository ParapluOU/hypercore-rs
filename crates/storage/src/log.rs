//! Log-structured `u64`-keyed store over a synchronous random-access file.
//!
//! Each mutation **appends** a record `[key: u64 LE][kind: u8][len: u32 LE][value]`
//! to the end of the file — O(1) amortized, vs the naive whole-file rewrite. An
//! in-memory index maps `key → (value_offset, len)`; a read seeks and reads the
//! value; a delete appends a tombstone. When dead bytes exceed half the file,
//! [`compact`](LogStore::compact) rewrites it with only the live records.
//!
//! The file is abstracted as [`SyncFile`] so the log-structured logic is tested
//! **natively** against [`MemFile`]; the browser's OPFS sync access handle is just
//! another [`SyncFile`] impl (see `opfs`).

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt::Debug;

use crate::Store;

/// A synchronous random-access byte file. The minimal interface the log needs.
pub trait SyncFile {
    type Error: Debug;
    fn size(&self) -> Result<u64, Self::Error>;
    /// Read `buf.len()` bytes starting at `offset` (always within the file in use).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Self::Error>;
    fn truncate(&mut self, size: u64) -> Result<(), Self::Error>;
    fn flush(&mut self) -> Result<(), Self::Error>;
}

const HDR: usize = 13; // key(8) + kind(1) + len(4)
const KIND_PUT: u8 = 0;
const KIND_DEL: u8 = 1;
const MIN_COMPACT: u64 = 4096;

fn encode_record(key: u64, kind: u8, value: &[u8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(HDR + value.len());
    r.extend_from_slice(&key.to_le_bytes());
    r.push(kind);
    r.extend_from_slice(&(value.len() as u32).to_le_bytes());
    r.extend_from_slice(value);
    r
}

/// A log-structured store over a [`SyncFile`].
pub struct LogStore<F: SyncFile> {
    file: F,
    index: BTreeMap<u64, (u64, u32)>, // key -> (value_offset, len)
    end: u64,
    dead: u64,
}

impl<F: SyncFile> LogStore<F> {
    /// Open over `file`, replaying its records to rebuild the index. A partial
    /// trailing record (e.g. from an interrupted write) is dropped and the file is
    /// truncated to the last complete record.
    pub fn open(mut file: F) -> Result<Self, F::Error> {
        let size = file.size()?;
        let mut buf = vec![0u8; size as usize];
        if size > 0 {
            file.read_at(0, &mut buf)?;
        }

        let mut index: BTreeMap<u64, (u64, u32)> = BTreeMap::new();
        let mut dead = 0u64;
        let mut pos = 0usize;
        while pos + HDR <= buf.len() {
            let key = u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap());
            let kind = buf[pos + 8];
            let len = u32::from_le_bytes(buf[pos + 9..pos + 13].try_into().unwrap()) as usize;
            if pos + HDR + len > buf.len() {
                break; // truncated tail
            }
            let value_off = (pos + HDR) as u64;
            match kind {
                KIND_PUT => {
                    if let Some((_, old)) = index.insert(key, (value_off, len as u32)) {
                        dead += HDR as u64 + old as u64; // superseded record
                    }
                }
                KIND_DEL => {
                    if let Some((_, old)) = index.remove(&key) {
                        dead += HDR as u64 + old as u64;
                    }
                    dead += HDR as u64; // the tombstone is reclaimable too
                }
                _ => break, // corrupt
            }
            pos += HDR + len;
        }

        let end = pos as u64;
        if end < size {
            file.truncate(end)?; // drop a partial/garbage tail
        }
        Ok(Self { file, index, end, dead })
    }

    /// Current backing-file size in bytes.
    pub fn file_len(&self) -> u64 {
        self.end
    }

    /// Rewrite the file with only the live records, reclaiming dead space.
    pub fn compact(&mut self) -> Result<(), F::Error> {
        // Read live values from their current offsets *before* overwriting.
        let mut live: Vec<(u64, Vec<u8>)> = Vec::with_capacity(self.index.len());
        for (&k, &(off, len)) in &self.index {
            let mut v = vec![0u8; len as usize];
            if len > 0 {
                self.file.read_at(off, &mut v)?;
            }
            live.push((k, v));
        }

        let mut fresh = Vec::new();
        let mut new_index = BTreeMap::new();
        for (k, v) in &live {
            let value_off = fresh.len() as u64 + HDR as u64;
            fresh.extend_from_slice(&encode_record(*k, KIND_PUT, v));
            new_index.insert(*k, (value_off, v.len() as u32));
        }

        self.file.truncate(0)?;
        if !fresh.is_empty() {
            self.file.write_at(0, &fresh)?;
        }
        self.file.flush()?;
        self.index = new_index;
        self.end = fresh.len() as u64;
        self.dead = 0;
        Ok(())
    }

    fn maybe_compact(&mut self) -> Result<(), F::Error> {
        if self.end > MIN_COMPACT && self.dead.saturating_mul(2) > self.end {
            self.compact()?;
        }
        Ok(())
    }

    /// Consume the store, returning the backing file (for tests / handoff).
    pub fn into_file(self) -> F {
        self.file
    }
}

impl<F: SyncFile> Store for LogStore<F> {
    type Error = F::Error;

    fn put(&mut self, key: u64, value: &[u8]) -> Result<(), F::Error> {
        let rec = encode_record(key, KIND_PUT, value);
        self.file.write_at(self.end, &rec)?;
        let value_off = self.end + HDR as u64;
        if let Some((_, old)) = self.index.insert(key, (value_off, value.len() as u32)) {
            self.dead += HDR as u64 + old as u64;
        }
        self.end += rec.len() as u64;
        self.file.flush()?;
        self.maybe_compact()
    }

    fn get(&self, key: u64) -> Result<Option<Vec<u8>>, F::Error> {
        match self.index.get(&key) {
            None => Ok(None),
            Some(&(off, len)) => {
                let mut v = vec![0u8; len as usize];
                if len > 0 {
                    self.file.read_at(off, &mut v)?;
                }
                Ok(Some(v))
            }
        }
    }

    fn delete(&mut self, key: u64) -> Result<(), F::Error> {
        if let Some((_, old)) = self.index.remove(&key) {
            let rec = encode_record(key, KIND_DEL, &[]);
            self.file.write_at(self.end, &rec)?;
            self.end += rec.len() as u64;
            self.dead += HDR as u64 + old as u64 + HDR as u64;
            self.file.flush()?;
            self.maybe_compact()?;
        }
        Ok(())
    }

    fn len(&self) -> Result<u64, F::Error> {
        Ok(self.index.len() as u64)
    }
}

/// In-memory [`SyncFile`] — the native test backend and a reference impl.
#[derive(Clone, Debug, Default)]
pub struct MemFile {
    data: Vec<u8>,
}

impl MemFile {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self { data }
    }
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }
}

impl SyncFile for MemFile {
    type Error = Infallible;
    fn size(&self) -> Result<u64, Infallible> {
        Ok(self.data.len() as u64)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Infallible> {
        let off = offset as usize;
        let n = buf.len().min(self.data.len().saturating_sub(off));
        buf[..n].copy_from_slice(&self.data[off..off + n]);
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Infallible> {
        let off = offset as usize;
        if off + buf.len() > self.data.len() {
            self.data.resize(off + buf.len(), 0);
        }
        self.data[off..off + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn truncate(&mut self, size: u64) -> Result<(), Infallible> {
        self.data.truncate(size as usize);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
}

/// A native [`SyncFile`] over `std::fs::File` (Unix positional `pread`/`pwrite`).
///
/// The real on-disk backend for [`LogStore`] — a node's logs are written to disk
/// and survive a process restart. Unix-only (macOS/Linux, the node targets); wasm
/// uses the OPFS backend instead.
#[cfg(unix)]
#[derive(Debug)]
pub struct StdFile {
    file: std::fs::File,
}

#[cfg(unix)]
impl StdFile {
    /// Open the file at `path` for read+write, creating it if absent.
    pub fn open(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).open(path)?;
        Ok(Self { file })
    }
}

#[cfg(unix)]
impl SyncFile for StdFile {
    type Error = std::io::Error;

    fn size(&self) -> Result<u64, Self::Error> {
        Ok(self.file.metadata()?.len())
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), Self::Error> {
        use std::os::unix::fs::FileExt;
        self.file.write_all_at(buf, offset)
    }
    fn truncate(&mut self, size: u64) -> Result<(), Self::Error> {
        self.file.set_len(size)
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.file.sync_all() // fsync — durability across a crash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upholds_store_contract() {
        let mut s = LogStore::open(MemFile::new()).unwrap();
        crate::contract::run(&mut s);
    }

    #[test]
    fn persists_across_reopen() {
        let mut s = LogStore::open(MemFile::new()).unwrap();
        s.put(1, b"one").unwrap();
        s.put(2, b"two").unwrap();
        s.put(1, b"ONE").unwrap(); // overwrite
        s.delete(2).unwrap();

        // Reopen on the same bytes — replay must reconstruct the live state.
        let bytes = s.into_file().bytes().to_vec();
        let s2 = LogStore::open(MemFile::from_bytes(bytes)).unwrap();
        assert_eq!(s2.get(1).unwrap().as_deref(), Some(&b"ONE"[..]));
        assert_eq!(s2.get(2).unwrap(), None);
        assert_eq!(s2.len().unwrap(), 1);
    }

    #[test]
    fn compaction_reclaims_space_and_preserves_values() {
        let mut s = LogStore::open(MemFile::new()).unwrap();
        let big = vec![b'x'; 256];
        // Overwrite 4 keys 200× each: without compaction the file would be ~215 KB.
        for _ in 0..200 {
            for k in 0..4u64 {
                s.put(k, &big).unwrap();
            }
        }
        assert!(
            s.file_len() < 50_000,
            "auto-compaction must bound growth (got {})",
            s.file_len()
        );
        for k in 0..4u64 {
            assert_eq!(s.get(k).unwrap().unwrap(), big);
        }

        // Explicit compaction reduces to exactly the live records.
        s.compact().unwrap();
        assert_eq!(s.file_len(), 4 * (HDR as u64 + 256));
        for k in 0..4u64 {
            assert_eq!(s.get(k).unwrap().unwrap(), big);
        }
        assert_eq!(s.len().unwrap(), 4);
    }

    #[test]
    fn partial_trailing_record_is_dropped_on_open() {
        let mut s = LogStore::open(MemFile::new()).unwrap();
        s.put(1, b"good").unwrap();
        let mut bytes = s.into_file().bytes().to_vec();
        bytes.extend_from_slice(&[7, 0, 0, 0, 0, 0, 0, 0]); // < HDR: a half-written record
        let s2 = LogStore::open(MemFile::from_bytes(bytes)).unwrap();
        assert_eq!(s2.get(1).unwrap().as_deref(), Some(&b"good"[..]));
        assert_eq!(s2.len().unwrap(), 1, "partial tail ignored");
    }

    #[cfg(unix)]
    #[test]
    fn stdfile_upholds_contract_and_persists_across_reopen() {
        let path =
            std::env::temp_dir().join(format!("hcrs-stdfile-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // The full Store contract, on a real file.
        {
            let mut s = LogStore::open(StdFile::open(&path).unwrap()).unwrap();
            crate::contract::run(&mut s);
        }
        let _ = std::fs::remove_file(&path);

        // Writes survive dropping the store and reopening the file from disk.
        {
            let mut s = LogStore::open(StdFile::open(&path).unwrap()).unwrap();
            s.put(1, b"one").unwrap();
            s.put(1, b"ONE").unwrap();
            s.put(2, b"two").unwrap();
            s.delete(2).unwrap();
        }
        {
            let s = LogStore::open(StdFile::open(&path).unwrap()).unwrap();
            assert_eq!(s.get(1).unwrap().as_deref(), Some(&b"ONE"[..]), "reopened from disk");
            assert_eq!(s.get(2).unwrap(), None);
            assert_eq!(s.len().unwrap(), 1);
        }
        let _ = std::fs::remove_file(&path);
    }
}
