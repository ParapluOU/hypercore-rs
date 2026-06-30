//! OPFS browser backend — synchronous, persistent storage in the browser.
//!
//! Uses the Origin Private File System's `FileSystemSyncAccessHandle`, whose
//! read/write/getSize/truncate/flush are **synchronous**, so this fits the sync
//! [`Store`] trait with no async plumbing — exactly what the local-first,
//! browser-is-the-writer model needs (this is what SQLite-WASM uses).
//!
//! Constraints:
//! - Worker-only: `createSyncAccessHandle()` is unavailable on the main thread.
//! - Requires `RUSTFLAGS=--cfg=web_sys_unstable_apis` and the `opfs` feature.
//!
//! v1 design: the store is held in memory and mirrored to a single OPFS file —
//! every mutation re-serializes the map and rewrites the file (O(n) per write).
//! Simple and correct; a log-structured/append layout is a follow-up.

use std::collections::BTreeMap;

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetFileOptions,
    FileSystemSyncAccessHandle, WorkerGlobalScope,
};

use crate::Store;

/// An OPFS operation failure (a stringified `JsValue` or a context message).
#[derive(Debug, Clone)]
pub struct OpfsError(pub String);

impl From<JsValue> for OpfsError {
    fn from(v: JsValue) -> Self {
        OpfsError(format!("{v:?}"))
    }
}

/// A `u64`-keyed [`Store`] persisted to one OPFS file.
pub struct OpfsStore {
    map: BTreeMap<u64, Vec<u8>>,
    handle: FileSystemSyncAccessHandle,
}

impl OpfsStore {
    /// Open (creating if absent) the OPFS file `name` and load its contents.
    /// Must be called from a Web Worker.
    pub async fn open(name: &str) -> Result<Self, OpfsError> {
        let scope: WorkerGlobalScope = js_sys::global()
            .dyn_into()
            .map_err(|_| OpfsError("OPFS requires a Web Worker scope".into()))?;
        let storage = scope.navigator().storage();

        let dir: FileSystemDirectoryHandle = JsFuture::from(storage.get_directory())
            .await?
            .dyn_into()
            .map_err(|_| OpfsError("getDirectory did not return a directory handle".into()))?;

        let opts = FileSystemGetFileOptions::new();
        opts.set_create(true);
        let file: FileSystemFileHandle =
            JsFuture::from(dir.get_file_handle_with_options(name, &opts))
                .await?
                .dyn_into()
                .map_err(|_| OpfsError("getFileHandle did not return a file handle".into()))?;

        let handle: FileSystemSyncAccessHandle = JsFuture::from(file.create_sync_access_handle())
            .await?
            .dyn_into()
            .map_err(|_| OpfsError("createSyncAccessHandle failed".into()))?;

        // Load existing contents synchronously.
        let size = handle.get_size()? as usize;
        let mut buf = vec![0u8; size];
        if size > 0 {
            handle.read_with_u8_array(&mut buf)?;
        }
        Ok(Self {
            map: decode_map(&buf),
            handle,
        })
    }

    /// Re-serialize the map and rewrite the OPFS file.
    fn persist(&self) -> Result<(), OpfsError> {
        let buf = encode_map(&self.map);
        self.handle.truncate_with_f64(0.0)?;
        self.handle.write_with_u8_array(&buf)?;
        self.handle.flush()?;
        Ok(())
    }
}

impl Store for OpfsStore {
    type Error = OpfsError;

    fn put(&mut self, key: u64, value: &[u8]) -> Result<(), OpfsError> {
        self.map.insert(key, value.to_vec());
        self.persist()
    }

    fn get(&self, key: u64) -> Result<Option<Vec<u8>>, OpfsError> {
        Ok(self.map.get(&key).cloned())
    }

    fn delete(&mut self, key: u64) -> Result<(), OpfsError> {
        self.map.remove(&key);
        self.persist()
    }

    fn len(&self) -> Result<u64, OpfsError> {
        Ok(self.map.len() as u64)
    }
}

impl Drop for OpfsStore {
    fn drop(&mut self) {
        // Release the exclusive sync access handle so the file can be reopened.
        self.handle.close();
    }
}

#[cfg(test)]
mod browser_tests {
    use super::*;
    use wasm_bindgen_test::*;

    // OPFS sync access handles are worker-only, so run the tests in a worker.
    wasm_bindgen_test_configure!(run_in_dedicated_worker);

    #[wasm_bindgen_test]
    async fn opfs_store_roundtrip_and_persists_across_reopen() {
        let name = "hc-opfs-roundtrip";
        let mut s = OpfsStore::open(name).await.expect("open");

        // Reset the keys this test uses (the OPFS file persists across runs).
        for k in [1u64, 2, 7] {
            s.delete(k).unwrap();
        }
        assert_eq!(s.get(1).unwrap(), None);

        s.put(1, b"one").unwrap();
        s.put(2, b"two").unwrap();
        assert_eq!(s.get(1).unwrap().as_deref(), Some(&b"one"[..]));
        s.put(1, b"ONE").unwrap(); // overwrite
        assert_eq!(s.get(1).unwrap().as_deref(), Some(&b"ONE"[..]));
        s.delete(2).unwrap();
        assert_eq!(s.get(2).unwrap(), None);

        // Close (Drop) then reopen: the value must survive — real persistence.
        drop(s);
        let mut s2 = OpfsStore::open(name).await.expect("reopen");
        assert_eq!(
            s2.get(1).unwrap().as_deref(),
            Some(&b"ONE"[..]),
            "value persisted across a close+reopen via OPFS"
        );
        s2.delete(1).unwrap(); // leave it clean
    }
}

/// `[key: u64 LE][len: u32 LE][bytes]` per entry, in key order.
fn encode_map(map: &BTreeMap<u64, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in map {
        out.extend_from_slice(&k.to_le_bytes());
        out.extend_from_slice(&(v.len() as u32).to_le_bytes());
        out.extend_from_slice(v);
    }
    out
}

fn decode_map(mut b: &[u8]) -> BTreeMap<u64, Vec<u8>> {
    let mut map = BTreeMap::new();
    while b.len() >= 12 {
        let k = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let len = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        b = &b[12..];
        if b.len() < len {
            break;
        }
        map.insert(k, b[..len].to_vec());
        b = &b[len..];
    }
    map
}
