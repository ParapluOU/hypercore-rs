//! OPFS browser backend — a [`SyncFile`] over the Origin Private File System's
//! synchronous access handle, composed with [`LogStore`] for a persistent,
//! log-structured `u64`-keyed [`Store`](crate::Store).
//!
//! `FileSystemSyncAccessHandle` read/write/getSize/truncate/flush are
//! **synchronous**, so it slots straight into [`SyncFile`] — the local-first,
//! browser-is-the-writer primitive (what SQLite-WASM uses). The log-structured
//! KV + compaction logic lives in [`crate::log`] and is tested natively; this
//! module is only the thin OPFS file binding.
//!
//! Constraints: worker-only (`createSyncAccessHandle()` is unavailable on the main
//! thread), and requires `RUSTFLAGS=--cfg=web_sys_unstable_apis` + the `opfs` feature.

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetFileOptions,
    FileSystemReadWriteOptions, FileSystemSyncAccessHandle, WorkerGlobalScope,
};

use crate::log::{LogStore, SyncFile};

/// An OPFS operation failure (a stringified `JsValue` or a context message).
#[derive(Debug, Clone)]
pub struct OpfsError(pub String);

impl From<JsValue> for OpfsError {
    fn from(v: JsValue) -> Self {
        OpfsError(format!("{v:?}"))
    }
}

/// An OPFS file presented as a synchronous [`SyncFile`].
pub struct OpfsFile {
    handle: FileSystemSyncAccessHandle,
}

impl OpfsFile {
    /// Open (creating if absent) the OPFS file `name`. Must run in a Web Worker.
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

        Ok(Self { handle })
    }
}

impl SyncFile for OpfsFile {
    type Error = OpfsError;

    fn size(&self) -> Result<u64, OpfsError> {
        Ok(self.handle.get_size()? as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), OpfsError> {
        let opts = FileSystemReadWriteOptions::new();
        opts.set_at(offset as f64);
        self.handle.read_with_u8_array_and_options(buf, &opts)?;
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), OpfsError> {
        let opts = FileSystemReadWriteOptions::new();
        opts.set_at(offset as f64);
        self.handle.write_with_u8_array_and_options(buf, &opts)?;
        Ok(())
    }

    fn truncate(&mut self, size: u64) -> Result<(), OpfsError> {
        self.handle.truncate_with_f64(size as f64)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), OpfsError> {
        self.handle.flush()?;
        Ok(())
    }
}

impl Drop for OpfsFile {
    fn drop(&mut self) {
        // Release the exclusive sync access handle so the file can be reopened.
        self.handle.close();
    }
}

/// A persistent, log-structured browser store backed by OPFS.
pub type OpfsStore = LogStore<OpfsFile>;

/// Open (creating if absent) an OPFS-backed store named `name`, replaying its log
/// to rebuild the index. Worker-only.
pub async fn open(name: &str) -> Result<OpfsStore, OpfsError> {
    LogStore::open(OpfsFile::open(name).await?)
}

#[cfg(test)]
mod browser_tests {
    use super::*;
    use crate::Store;
    use wasm_bindgen_test::*;

    // OPFS sync access handles are worker-only, so run the tests in a worker.
    wasm_bindgen_test_configure!(run_in_dedicated_worker);

    #[wasm_bindgen_test]
    async fn opfs_log_store_roundtrip_and_persists_across_reopen() {
        let name = "hc-opfs-log";
        let mut s = open(name).await.expect("open");

        // Reset the keys this test uses (the OPFS file persists across runs).
        for k in [1u64, 2, 7] {
            s.delete(k).unwrap();
        }
        s.put(1, b"one").unwrap();
        s.put(2, b"two").unwrap();
        s.put(1, b"ONE").unwrap(); // overwrite
        s.delete(2).unwrap();
        assert_eq!(s.get(1).unwrap().as_deref(), Some(&b"ONE"[..]));
        assert_eq!(s.get(2).unwrap(), None);

        // Close (Drop) then reopen: the log replays and the value survives.
        drop(s);
        let mut s2 = open(name).await.expect("reopen");
        assert_eq!(
            s2.get(1).unwrap().as_deref(),
            Some(&b"ONE"[..]),
            "value persisted across close+reopen via the OPFS log"
        );
        s2.delete(1).unwrap();
    }
}
