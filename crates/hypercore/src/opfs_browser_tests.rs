use super::{verify_block, Hypercore};
use codec::{Bytes, Codec};
use identity::SecretKey;
use wasm_bindgen_test::*;

// OPFS sync access handles are worker-only, so run in a dedicated worker.
wasm_bindgen_test_configure!(run_in_dedicated_worker);

#[wasm_bindgen_test]
async fn hypercore_persists_and_reopens_through_opfs() {
    let seed = [80u8; 32];
    let name = "hc-opfs-core";

    // A fresh writer over OPFS-backed storage.
    let store = storage::opfs::open(name).await.expect("open opfs store");
    let mut core = Hypercore::<Vec<u8>, _, _>::new(SecretKey::from_seed(&seed), Bytes, store);
    let values: [&[u8]; 4] = [b"alpha", b"beta", b"gamma", b"delta"];
    for v in values {
        core.append(&v.to_vec()).expect("append");
    }
    core.clear(1, 2).expect("clear"); // drop block 1's bytes → sparse
    core.persist().expect("persist");

    let len = core.len();
    let root = core.head().unwrap().root;
    drop(core); // closes the OPFS sync access handle

    // What a fresh page load does: reopen the OPFS file, reconstitute the core.
    let store2 = storage::opfs::open(name).await.expect("reopen opfs store");
    let reopened =
        Hypercore::<Vec<u8>, _, _>::open(SecretKey::from_seed(&seed), Bytes, store2)
            .expect("reopen core");

    assert_eq!(reopened.len(), len, "length survives the reload");
    assert_eq!(reopened.head().unwrap().root, root, "head root survives");
    assert!(reopened.verify_head(), "reopened head verifies under the key");
    assert!(!reopened.has(1), "the cleared block is still cleared");
    assert_eq!(reopened.get(1).unwrap(), None);
    assert_eq!(reopened.get(0).unwrap(), Some(b"alpha".to_vec()));
    assert_eq!(reopened.get(3).unwrap(), Some(b"delta".to_vec()));

    // The cleared block's bytes are gone, yet it stays authenticated.
    let pk = SecretKey::from_seed(&seed).public();
    let proof = reopened.proof(1).expect("cleared block still has a proof");
    let encoded_beta = Bytes.encode(&b"beta".to_vec());
    assert!(
        verify_block(&pk, reopened.head().unwrap(), 1, &encoded_beta, &proof),
        "an absent block is still authenticated after a browser reload"
    );
}
