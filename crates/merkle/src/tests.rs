use super::*;

fn tree(n: usize) -> (MerkleTree, Vec<Vec<u8>>) {
    let mut t = MerkleTree::new();
    let mut blocks = Vec::new();
    for i in 0..n {
        let b = format!("block-{i}").into_bytes();
        t.append(&b);
        blocks.push(b);
    }
    (t, blocks)
}

fn tree_from(blocks: &[Vec<u8>]) -> MerkleTree {
    let mut t = MerkleTree::new();
    for b in blocks {
        t.append(b);
    }
    t
}

// flat-tree shape — ports "get roots" structure from merkle-tree.js.
#[test]
fn roots_shape() {
    assert_eq!(flat::full_roots(2), vec![0]); // 1 block  -> 1 root
    assert_eq!(flat::full_roots(8), vec![3]); // 4 blocks -> 1 root
    assert_eq!(flat::full_roots(10), vec![3, 8]); // 5 blocks -> 2 roots
    assert_eq!(flat::full_roots(14), vec![3, 9, 12]); // 7 blocks -> 3 roots
}

// Every block in a range of tree sizes proves & verifies — ports
// "proof only block" + "verify proof".
#[test]
fn proof_roundtrip_all_sizes() {
    for n in 1..=33usize {
        let (t, blocks) = tree(n);
        let root = t.root_hash();
        for b in 0..n as u64 {
            let proof = t.proof(b).expect("proof exists");
            assert!(
                proof.verify(&blocks[b as usize], &root),
                "honest proof must verify (n={n}, block={b})"
            );
        }
        assert!(t.proof(n as u64).is_none(), "out-of-range proof is None");
    }
}

// A non-edge block's proof carries sibling + sub-root hashes (not empty).
#[test]
fn proof_carries_siblings() {
    let (t, _) = tree(8);
    let proof = t.proof(3).unwrap();
    assert!(!proof.siblings.is_empty(), "interior block needs siblings");
}

// Determinism — ports "tree hash determinism".
#[test]
fn determinism() {
    let (a, _) = tree(6);
    let (b, _) = tree(6);
    assert_eq!(a.root_hash(), b.root_hash(), "same blocks => same root");

    let mut c = MerkleTree::new();
    for i in 0..6 {
        c.append(format!("other-{i}").as_bytes());
    }
    assert_ne!(a.root_hash(), c.root_hash(), "different blocks => different root");
}

// Tamper-rejection — the property the DoD requires.
#[test]
fn rejects_tampering() {
    let (t, blocks) = tree(7);
    let root = t.root_hash();
    let proof = t.proof(4).unwrap();

    // honest baseline
    assert!(proof.verify(&blocks[4], &root));

    // wrong data
    assert!(!proof.verify(b"forged-block", &root));
    // right length, wrong bytes
    let mut same_len = blocks[4].clone();
    same_len[0] ^= 0xff;
    assert!(!proof.verify(&same_len, &root));

    // tampered sibling
    let mut bad = proof.clone();
    bad.siblings[0].hash[0] ^= 0xff;
    assert!(!bad.verify(&blocks[4], &root));

    // tampered root entry
    let mut bad_root = proof.clone();
    bad_root.roots[0].hash[0] ^= 0xff;
    assert!(!bad_root.verify(&blocks[4], &root));

    // honest proof, wrong expected root
    let mut wrong = root;
    wrong[0] ^= 0xff;
    assert!(!proof.verify(&blocks[4], &wrong));
}

// Every contiguous sub-range of a range of tree sizes proves & verifies —
// the multi-block generalization of `proof_roundtrip_all_sizes`.
#[test]
fn range_proof_roundtrip_all_sizes() {
    for n in 1..=20usize {
        let (t, blocks) = tree(n);
        let root = t.root_hash();
        for start in 0..n as u64 {
            for end in (start + 1)..=n as u64 {
                let rp = t
                    .range_proof(start, end)
                    .expect("in-range proof exists");
                let span: &[Vec<u8>] = &blocks[start as usize..end as usize];
                assert!(
                    rp.verify(span, &root),
                    "honest range proof must verify (n={n}, [{start},{end}))"
                );
            }
        }
    }
}

// The whole-tree range recomputes every root: the strongest check.
#[test]
fn range_proof_full_tree() {
    for n in 1..=17usize {
        let (t, blocks) = tree(n);
        let rp = t.range_proof(0, n as u64).unwrap();
        assert!(rp.verify(&blocks, &t.root_hash()));
        // A full-tree range needs no off-range boundary nodes.
        assert!(rp.nodes.is_empty(), "full-tree range needs no boundary nodes (n={n})");
    }
}

// A range spanning multiple roots carries boundary nodes and still verifies.
#[test]
fn range_proof_spans_multiple_roots() {
    let (t, blocks) = tree(7); // 3 roots: indices 3, 9, 12
    let root = t.root_hash();
    let rp = t.range_proof(2, 5).unwrap(); // blocks 2,3,4 -> leaves 4,6,8
    assert!(!rp.nodes.is_empty(), "interior range needs boundary nodes");
    assert!(rp.verify(&blocks[2..5], &root));
}

// A single-block range carries exactly the same boundary set as the
// single-block inclusion proof's siblings.
#[test]
fn range_proof_single_block_matches_inclusion() {
    let (t, blocks) = tree(13);
    let root = t.root_hash();
    for b in 0..13u64 {
        let rp = t.range_proof(b, b + 1).unwrap();
        let p = t.proof(b).unwrap();
        let mut rp_idx: Vec<u64> = rp.nodes.iter().map(|n| n.index).collect();
        let mut p_idx: Vec<u64> = p.siblings.iter().map(|n| n.index).collect();
        rp_idx.sort_unstable();
        p_idx.sort_unstable();
        assert_eq!(rp_idx, p_idx, "single-block range == inclusion siblings (b={b})");
        assert!(rp.verify(std::slice::from_ref(&blocks[b as usize]), &root));
    }
}

// Out-of-range / empty ranges produce no proof.
#[test]
fn range_proof_out_of_range() {
    let (t, _) = tree(8);
    assert!(t.range_proof(0, 9).is_none(), "end past length");
    assert!(t.range_proof(8, 9).is_none(), "start past length");
    assert!(t.range_proof(3, 3).is_none(), "empty range");
    assert!(t.range_proof(5, 2).is_none(), "inverted range");
}

// Tamper-rejection across the whole span — the DoD property for range proofs.
#[test]
fn range_proof_rejects_tampering() {
    let (t, blocks) = tree(11);
    let root = t.root_hash();
    let rp = t.range_proof(3, 8).unwrap(); // blocks 3..8
    let span: Vec<Vec<u8>> = blocks[3..8].to_vec();

    // honest baseline
    assert!(rp.verify(&span, &root));

    // a single mutated block anywhere in the span is caught
    for i in 0..span.len() {
        let mut bad = span.clone();
        bad[i][0] ^= 0xff;
        assert!(!rp.verify(&bad, &root), "mutated block {i} must reject");
    }

    // reordering two blocks (positions matter — leaves are positional)
    let mut swapped = span.clone();
    swapped.swap(0, 1);
    assert!(!rp.verify(&swapped, &root), "reordered span must reject");

    // wrong block count
    assert!(!rp.verify(&span[..span.len() - 1], &root), "short span rejects");

    // tampered boundary node
    let mut bad_node = rp.clone();
    bad_node.nodes[0].hash[0] ^= 0xff;
    assert!(!bad_node.verify(&span, &root), "tampered boundary node rejects");

    // tampered *untouched* root entry. Range [3,8) lives under root 7 only, so
    // the other roots (17, 20) are passed through unchanged and bound by
    // tree_hash — tampering one must reject. (A *touched* root is recomputed
    // from the block data and would be substituted over, by design.)
    let mut bad_root = rp.clone();
    let last = bad_root.roots.len() - 1;
    assert!(last > 0, "range must leave some roots untouched");
    bad_root.roots[last].hash[0] ^= 0xff;
    assert!(!bad_root.verify(&span, &root), "tampered untouched root rejects");

    // honest proof, wrong expected root
    let mut wrong = root;
    wrong[0] ^= 0xff;
    assert!(!rp.verify(&span, &wrong), "wrong expected root rejects");

    // dropping a needed boundary node makes the proof insufficient
    let mut missing = rp.clone();
    missing.nodes.pop();
    assert!(!missing.verify(&span, &root), "missing boundary node rejects");
}

// Every (old < new) length pair produces an upgrade proof that an honest
// verifier (holding the genuine old roots) accepts — the length-extension
// round-trip across the whole shape space.
#[test]
fn upgrade_proof_roundtrip_all_sizes() {
    for new in 1..=20u64 {
        let (t, blocks) = tree(new as usize);
        let new_root = t.root_hash();
        for old in 1..new {
            let old_roots = tree_from(&blocks[..old as usize]).roots();
            let up = t
                .upgrade_proof(old, new)
                .expect("1 <= old < new <= len has a proof");
            assert!(
                up.verify(&old_roots, &new_root),
                "honest upgrade must verify (old={old}, new={new})"
            );
        }
    }
}

// Extend by exactly one block — the smallest, most common upgrade.
#[test]
fn upgrade_proof_single_step() {
    for new in 2..=18u64 {
        let (t, blocks) = tree(new as usize);
        let old = new - 1;
        let old_roots = tree_from(&blocks[..old as usize]).roots();
        let up = t.upgrade_proof(old, new).unwrap();
        assert!(up.verify(&old_roots, &t.root_hash()), "single-step upgrade (new={new})");
    }
}

// The proof carries only *fully-new* subtree nodes (every covered block is
// `>= old`); it never ships old data. This is what the anti-fork soundness
// argument rests on.
#[test]
fn upgrade_proof_supplies_only_fully_new_nodes() {
    for new in 1..=20u64 {
        let (t, _) = tree(new as usize);
        for old in 1..new {
            let up = t.upgrade_proof(old, new).unwrap();
            for n in &up.nodes {
                let (first, end) = flat::block_range(n.index);
                assert!(
                    first >= old,
                    "supplied node {} covers old data [{first},{end}) (old={old}, new={new})",
                    n.index
                );
                assert!(end <= new, "supplied node must stay within the new tree");
            }
        }
    }
}

// Anti-fork across lengths: a verifier holding the *honest* prefix rejects a
// longer head that rewrote an old block — even though the proof is internally
// well-formed for the forked tree.
#[test]
fn upgrade_proof_detects_old_rewrite() {
    let (honest, blocks) = tree(8);
    let old = 5u64;
    let honest_old_roots = tree_from(&blocks[..old as usize]).roots();

    // Sanity: the honest extension verifies under the honest old roots.
    let honest_up = honest.upgrade_proof(old, 8).unwrap();
    assert!(honest_up.verify(&honest_old_roots, &honest.root_hash()));

    // Fork: identical except block 2 (which is < old) is rewritten.
    let mut forked_blocks = blocks.clone();
    forked_blocks[2] = b"rewritten".to_vec();
    let forked = tree_from(&forked_blocks);
    assert_ne!(forked.root_hash(), honest.root_hash());

    let forked_up = forked.upgrade_proof(old, 8).unwrap();
    // The forked proof is self-consistent against the *forked* old roots...
    let forked_old_roots = tree_from(&forked_blocks[..old as usize]).roots();
    assert!(forked_up.verify(&forked_old_roots, &forked.root_hash()));
    // ...but a verifier trusting the honest prefix must reject it.
    assert!(
        !forked_up.verify(&honest_old_roots, &forked.root_hash()),
        "honest old prefix must reject a forked extension"
    );
}

// Tamper-rejection across every input the verifier trusts.
#[test]
fn upgrade_proof_rejects_tampering() {
    let (t, blocks) = tree(13);
    let new_root = t.root_hash();
    let old = 6u64;
    let old_roots = tree_from(&blocks[..old as usize]).roots();
    let up = t.upgrade_proof(old, 13).unwrap();
    assert!(!up.nodes.is_empty(), "this upgrade needs new nodes");
    assert!(up.verify(&old_roots, &new_root)); // honest baseline

    // tampered supplied node
    let mut bad_node = up.clone();
    bad_node.nodes[0].hash[0] ^= 0xff;
    assert!(!bad_node.verify(&old_roots, &new_root), "tampered new node rejects");

    // wrong expected new head
    let mut wrong = new_root;
    wrong[0] ^= 0xff;
    assert!(!up.verify(&old_roots, &wrong), "wrong new head rejects");

    // dropping a needed node makes the proof insufficient
    let mut missing = up.clone();
    missing.nodes.pop();
    assert!(!missing.verify(&old_roots, &new_root), "missing node rejects");

    // tampered old root (the verifier's own trusted state, mutated)
    let mut bad_old = old_roots.clone();
    bad_old[0].hash[0] ^= 0xff;
    assert!(!up.verify(&bad_old, &new_root), "tampered old root rejects");

    // old roots of the wrong length (shape mismatch with the proof's old_len)
    let wrong_len_old = tree_from(&blocks[..(old as usize - 1)]).roots();
    assert!(!up.verify(&wrong_len_old, &new_root), "mismatched old shape rejects");

    // injecting a fully-old node (a fork attempt: stand in for the verifier's
    // own old data) is refused — supplied nodes must be fully new. Leaf 0 =
    // block 0, which lies in the trusted old prefix.
    let mut injected = up.clone();
    injected.nodes.insert(
        0,
        Node { index: 0, hash: leaf_hash(&blocks[0]), size: blocks[0].len() as u64 },
    );
    assert!(!injected.verify(&old_roots, &new_root), "injected old-region node rejects");
}

// Out-of-range / degenerate requests produce no proof.
#[test]
fn upgrade_proof_out_of_range() {
    let (t, _) = tree(8);
    assert!(t.upgrade_proof(0, 8).is_none(), "old=0 has no anchor");
    assert!(t.upgrade_proof(8, 8).is_none(), "old==new is not an extension");
    assert!(t.upgrade_proof(5, 3).is_none(), "old>new inverted");
    assert!(t.upgrade_proof(3, 9).is_none(), "new past length");
}

// The upgrade proof and a range proof compose: confirm the extension is an
// honest append, then verify the new blocks themselves against the same head.
#[test]
fn upgrade_proof_composes_with_range_proof() {
    let (t, blocks) = tree(14);
    let new_root = t.root_hash();
    let old = 5u64;
    let old_roots = tree_from(&blocks[..old as usize]).roots();

    // 1. append-only / anti-fork across lengths (no data)
    assert!(t.upgrade_proof(old, 14).unwrap().verify(&old_roots, &new_root));
    // 2. the new blocks [old, 14) verify against the same (now trusted) head
    let rp = t.range_proof(old, 14).unwrap();
    assert!(rp.verify(&blocks[old as usize..], &new_root));
}

// A tree with varied (cycling 1..=5) block sizes so byte seeks are non-trivial.
fn varied_tree(n: usize) -> (MerkleTree, Vec<Vec<u8>>) {
    let mut t = MerkleTree::new();
    let mut blocks = Vec::new();
    for i in 0..n {
        let b = vec![b'a' + (i % 26) as u8; (i % 5) + 1];
        t.append(&b);
        blocks.push(b);
    }
    (t, blocks)
}

// The naive linear reference for a byte seek (sum leaf sizes left-to-right).
fn linear_seek(blocks: &[Vec<u8>], bytes: u64) -> (u64, u64) {
    let mut remaining = bytes;
    for (i, b) in blocks.iter().enumerate() {
        if b.len() as u64 > remaining {
            return (i as u64, remaining);
        }
        remaining -= b.len() as u64;
    }
    (blocks.len() as u64, remaining)
}

// The tree-accelerated seek agrees with the linear reference for every byte
// offset — ports `merkle-tree.js` "basic tree seeks". Also checks past-the-end.
#[test]
fn seek_matches_linear_all_sizes() {
    for n in 1..=20usize {
        let (t, blocks) = varied_tree(n);
        let total: u64 = blocks.iter().map(|b| b.len() as u64).sum();
        for bytes in 0..total {
            assert_eq!(
                t.seek(bytes),
                linear_seek(&blocks, bytes),
                "tree seek must match linear seek (n={n}, bytes={bytes})"
            );
        }
        // Past-the-end: exactly at total lands on (len, 0); beyond carries over.
        assert_eq!(t.seek(total), (n as u64, 0));
        assert_eq!(t.seek(total + 3), (n as u64, 3));
    }
}

// Every in-range byte offset has a seek proof that verifies against the signed
// root and returns the same `(block, offset)` as the local seek.
#[test]
fn seek_proof_roundtrip_all_sizes() {
    for n in 1..=20usize {
        let (t, blocks) = varied_tree(n);
        let root = t.root_hash();
        let total: u64 = blocks.iter().map(|b| b.len() as u64).sum();
        for bytes in 0..total {
            let sp = t.seek_proof(bytes).expect("in-range seek has a proof");
            assert_eq!(
                sp.verify(&root),
                Some(t.seek(bytes)),
                "seek proof must authenticate the local seek (n={n}, bytes={bytes})"
            );
        }
    }
}

// Hand-checked block boundaries: a byte exactly on a block start belongs to
// that block at offset 0; the byte before it is the last byte of the previous.
#[test]
fn seek_proof_pins_block_at_boundaries() {
    // sizes 1,2,3,4,5 -> cumulative starts 0,1,3,6,10, total 15
    let (t, _) = varied_tree(5);
    let root = t.root_hash();
    let starts = [0u64, 1, 3, 6, 10];
    for (block, &start) in starts.iter().enumerate() {
        let size = (block % 5) as u64 + 1;
        // first byte of the block
        assert_eq!(t.seek_proof(start).unwrap().verify(&root), Some((block as u64, 0)));
        // last byte of the block
        let last = start + size - 1;
        assert_eq!(
            t.seek_proof(last).unwrap().verify(&root),
            Some((block as u64, size - 1))
        );
    }
}

// A seek at or past the end of the log has no block to locate.
#[test]
fn seek_proof_past_end_is_none() {
    let (t, blocks) = varied_tree(7);
    let total: u64 = blocks.iter().map(|b| b.len() as u64).sum();
    assert!(t.seek_proof(total).is_none(), "byte == total is past the last block");
    assert!(t.seek_proof(total + 5).is_none(), "byte past total is unlocatable");
    assert!(MerkleTree::new().seek_proof(0).is_none(), "empty tree has no blocks");
}

// Tamper-rejection across every input the verifier trusts.
#[test]
fn seek_proof_rejects_tampering() {
    let (t, blocks) = varied_tree(11);
    let root = t.root_hash();
    // byte offset inside block 4 (an interior block, so the proof has siblings
    // and the climb crosses at least one root boundary).
    let cum4: u64 = blocks[..4].iter().map(|b| b.len() as u64).sum();
    let bytes = cum4 + 0; // first byte of block 4
    let sp = t.seek_proof(bytes).unwrap();
    assert!(!sp.siblings.is_empty(), "interior block needs siblings");
    assert_eq!(sp.verify(&root), Some((4, 0))); // honest baseline

    // tampered leaf hash
    let mut bad = sp.clone();
    bad.leaf.hash[0] ^= 0xff;
    assert!(bad.verify(&root).is_none(), "tampered leaf hash rejects");

    // tampered leaf size (would shift the bracket; the climb also diverges)
    let mut bad = sp.clone();
    bad.leaf.size += 1;
    assert!(bad.verify(&root).is_none(), "tampered leaf size rejects");

    // tampered sibling
    let mut bad = sp.clone();
    bad.siblings[0].hash[0] ^= 0xff;
    assert!(bad.verify(&root).is_none(), "tampered sibling rejects");

    // tampered untouched root entry (the containing root is substituted over,
    // so mutate a different one — bound by tree_hash).
    let root_indices: Vec<u64> = sp.roots.iter().map(|n| n.index).collect();
    let leaf_root = {
        // find which root the leaf climbs to, to mutate a *different* one
        let mut node = sp.leaf.index;
        while !root_indices.contains(&node) {
            node = flat::parent(node);
        }
        node
    };
    let other = sp.roots.iter().position(|r| r.index != leaf_root);
    assert!(other.is_some(), "an 11-block tree has multiple roots");
    let mut bad = sp.clone();
    bad.roots[other.unwrap()].hash[0] ^= 0xff;
    assert!(bad.verify(&root).is_none(), "tampered untouched root rejects");

    // wrong expected root
    let mut wrong = root;
    wrong[0] ^= 0xff;
    assert!(sp.verify(&wrong).is_none(), "wrong expected root rejects");

    // dropped sibling -> climb cannot reach a real root
    let mut bad = sp.clone();
    bad.siblings.pop();
    assert!(bad.verify(&root).is_none(), "dropped sibling rejects");

    // tampered `bytes` to a value in a *different* block: the proof genuinely
    // proves block 4's interval, which no longer brackets the byte -> None.
    let mut bad = sp.clone();
    bad.bytes = cum4 + (blocks[4].len() as u64); // first byte of block 5
    assert!(bad.verify(&root).is_none(), "bytes outside the proven block rejects");
}

// A power-of-two tree has a single root; seeks/proofs still hold.
#[test]
fn seek_proof_single_root() {
    let (t, blocks) = varied_tree(8); // one root (index 7)
    assert_eq!(t.roots().len(), 1, "8 blocks => single root");
    let root = t.root_hash();
    let total: u64 = blocks.iter().map(|b| b.len() as u64).sum();
    for bytes in 0..total {
        assert_eq!(t.seek_proof(bytes).unwrap().verify(&root), Some(t.seek(bytes)));
    }
}

// recovery: a tree with its tree nodes deleted still reports its length and
// does not panic — ports `merkle-tree-recovery.js` "core can still ready" +
// "still has length".
#[test]
fn recovery_corrupt_tree_keeps_length() {
    let (mut t, _) = tree(30); // 4 roots
    let roots: Vec<u64> = t.roots().iter().map(|n| n.index).collect();
    assert!(roots.len() > 1, "30 blocks has multiple roots");

    for r in &roots {
        assert!(t.remove_node(*r), "deleting a present root");
    }

    // Length survives; no panic querying it.
    assert_eq!(t.len(), 30);
    assert!(!t.is_intact(), "missing roots => repair mode");
    assert_eq!(t.try_root_hash(), None, "cannot build a root hash with roots gone");

    // The missing set is exactly the deleted roots — nothing else was touched.
    let mut missing = t.missing_nodes();
    missing.sort_unstable();
    let mut expect = roots.clone();
    expect.sort_unstable();
    assert_eq!(missing, expect);
}

// recovery: a deleted *root* is restored from a remote proof verified against
// the signed root — ports "fix via fully remote proof".
#[test]
fn recovery_root_via_remote_proof() {
    let (healthy, _) = tree(30);
    let root_hash = healthy.root_hash();
    let root_index = healthy.roots()[0].index; // first root, covers [0,16)
    let proof = healthy.node_proof(root_index).expect("healthy can prove its root");

    let mut corrupt = healthy.clone();
    assert!(corrupt.remove_node(root_index));
    assert_eq!(corrupt.len(), 30, "length survives corruption");
    assert!(!corrupt.is_intact());
    assert_eq!(corrupt.try_root_hash(), None, "cannot create tree hash with a root gone");
    assert!(corrupt.node_proof(root_index).is_none(), "corrupt source cannot prove the lost node");

    assert!(corrupt.recover_node(&proof, &root_hash), "honest remote proof recovers the node");
    assert!(corrupt.has_node(root_index));
    assert!(corrupt.is_intact(), "recovered tree is whole again");
    assert_eq!(corrupt.try_root_hash(), Some(root_hash), "root hash reconstructed exactly");
}

// recovery: a deleted *interior sub-root* is restored from a remote proof; the
// (still-present) root hash is unaffected by the gap, but the node itself is
// gone until recovered — ports "fix via fully remote proof" for a sub root.
#[test]
fn recovery_subroot_via_remote_proof() {
    let (healthy, _) = tree(64); // single root 63
    let root_hash = healthy.root_hash();
    let subroot = 15u64; // covers blocks [0,16): root 63 -> 31 -> 15
    assert!(healthy.has_node(subroot));
    let proof = healthy.node_proof(subroot).expect("prove the sub-root");
    let original = proof.node;

    let mut corrupt = healthy.clone();
    assert!(corrupt.remove_node(subroot));
    assert!(!corrupt.is_intact(), "a missing sub-root is repair mode");
    // A sub-root gap does not prevent the still-present root hash...
    assert_eq!(corrupt.try_root_hash(), Some(root_hash));
    // ...but the node itself is gone and cannot be re-proven locally.
    assert!(corrupt.node_proof(subroot).is_none());

    assert!(corrupt.recover_node(&proof, &root_hash));
    assert!(corrupt.is_intact());
    // The recovered node is exactly the original (hash + size reconstructed),
    // and it is provable again against the signed root.
    let reproof = corrupt.node_proof(subroot).expect("provable again");
    assert_eq!(reproof.node, original);
    assert_eq!(reproof.verify(&root_hash), Some(original));
}

// recovery security/atomicity: a mangled remote proof is rejected and the tree
// is left unchanged (node stays missing) — ports "atomically updates storage".
#[test]
fn recovery_rejects_tampered_proof_atomically() {
    let (healthy, _) = tree(64); // single root 63
    let root_hash = healthy.root_hash();
    let target = 15u64; // interior node: its proof carries siblings [47, 95]
    let proof = healthy.node_proof(target).unwrap();
    assert!(!proof.siblings.is_empty(), "interior node needs siblings");

    let assert_untouched = |c: &MerkleTree| {
        assert!(!c.has_node(target), "tampered recovery must not store the node");
        assert!(!c.is_intact(), "still in repair mode");
    };

    // mangled node size (upstream mangles the proven node's size)
    let mut bad = proof.clone();
    bad.node.size += 1;
    let mut corrupt = healthy.clone();
    corrupt.remove_node(target);
    assert!(!corrupt.recover_node(&bad, &root_hash), "mangled size rejected");
    assert_untouched(&corrupt);

    // mangled node hash
    let mut bad = proof.clone();
    bad.node.hash[0] ^= 0xff;
    let mut corrupt = healthy.clone();
    corrupt.remove_node(target);
    assert!(!corrupt.recover_node(&bad, &root_hash), "mangled hash rejected");
    assert_untouched(&corrupt);

    // tampered sibling
    let mut bad = proof.clone();
    bad.siblings[0].hash[0] ^= 0xff;
    let mut corrupt = healthy.clone();
    corrupt.remove_node(target);
    assert!(!corrupt.recover_node(&bad, &root_hash), "tampered sibling rejected");
    assert_untouched(&corrupt);

    // dropped sibling -> climb cannot reach a real root
    let mut bad = proof.clone();
    bad.siblings.pop();
    let mut corrupt = healthy.clone();
    corrupt.remove_node(target);
    assert!(!corrupt.recover_node(&bad, &root_hash), "dropped sibling rejected");
    assert_untouched(&corrupt);

    // honest proof, wrong expected root
    let mut wrong = root_hash;
    wrong[0] ^= 0xff;
    let mut corrupt = healthy.clone();
    corrupt.remove_node(target);
    assert!(!corrupt.recover_node(&proof, &wrong), "wrong expected root rejected");
    assert_untouched(&corrupt);

    // finally, the honest proof recovers cleanly after the failed attempts
    assert!(corrupt.recover_node(&proof, &root_hash));
    assert!(corrupt.is_intact());
}

// recovery: appends are refused while in repair mode, and resume after the
// missing node is recovered — ports "fail appends … when in repair mode".
#[test]
fn recovery_append_refused_in_repair_mode() {
    let (mut t, _) = tree(30);
    let root_hash = t.root_hash();
    let root_index = t.roots()[0].index;
    let proof = t.node_proof(root_index).unwrap(); // capture while healthy

    assert!(t.remove_node(root_index));
    assert!(!t.is_intact());
    assert_eq!(t.try_append(b"nope"), Err(InRepairMode), "cannot extend in repair mode");
    assert_eq!(t.len(), 30, "the refused append did not change the length");

    // Recover, then appending works again and the tree grows.
    assert!(t.recover_node(&proof, &root_hash));
    assert!(t.is_intact());
    assert_eq!(t.try_append(b"now ok").expect("append after recovery"), 30);
    assert_eq!(t.len(), 31);
}

// recovery round-trip: every stored node (leaf, interior, root) over a range
// of tree sizes proves & verifies against the signed root, and recovers a copy
// that had exactly that node deleted back to intact.
#[test]
fn node_proof_roundtrip_all_nodes() {
    for n in 1..=16u64 {
        let (t, _) = tree(n as usize);
        let root = t.root_hash();
        for i in 0..(2 * n) {
            let (_, end) = flat::block_range(i);
            if end > n {
                continue; // not a complete subtree of this tree
            }
            let proof = t.node_proof(i).expect("every stored node is provable");
            assert_eq!(proof.verify(&root), Some(proof.node), "node proof must verify (n={n}, i={i})");

            let mut corrupt = t.clone();
            assert!(corrupt.remove_node(i), "node {i} was present");
            assert!(!corrupt.is_intact(), "deleting node {i} => repair mode (n={n})");
            assert!(corrupt.recover_node(&proof, &root), "honest proof recovers node {i} (n={n})");
            assert!(corrupt.is_intact(), "recovered tree intact (n={n}, i={i})");
        }
    }
}

// truncate: rewinding to `new_len` leaves a tree node-for-node identical to
// a fresh tree of the first `new_len` blocks — for every (new_len < n) over a
// range of sizes. The root hash, node set, byte length, and proofs all match.
#[test]
fn truncate_equals_fresh_prefix_all_sizes() {
    for n in 1..=20u64 {
        for new_len in 0..n {
            let (mut t, blocks) = tree(n as usize);
            assert!(t.truncate(new_len), "truncate {n}->{new_len} changes the tree");
            assert_eq!(t.len(), new_len);

            let fresh = tree_from(&blocks[..new_len as usize]);
            assert_eq!(t.len(), fresh.len());
            assert_eq!(t.root_hash(), fresh.root_hash(), "root == prefix root ({n}->{new_len})");
            assert_eq!(t.byte_length(), fresh.byte_length(), "byte_length == prefix");
            assert_eq!(t.roots(), fresh.roots(), "root nodes identical");
            // The node maps coincide exactly (no stale nodes left behind).
            let live: Vec<u64> = t.missing_nodes();
            assert!(live.is_empty(), "truncated tree is intact ({n}->{new_len})");
            // Every surviving block still proves against the truncated root.
            for b in 0..new_len {
                let p = t.proof(b).expect("surviving block proves");
                assert!(p.verify(&blocks[b as usize], &t.root_hash()), "block {b} proves");
            }
            // A block past the new length is gone.
            assert!(t.proof(new_len).is_none(), "truncated block has no proof");
        }
    }
}

// truncate byte_length tracks the live prefix byte size exactly.
#[test]
fn truncate_byte_length() {
    let mut t = MerkleTree::new();
    for b in [&b"hello"[..], b"world", b"fo", b"ooo"] {
        t.append(b);
    }
    assert_eq!(t.byte_length(), 15); // 5+5+2+3
    assert!(t.truncate(3));
    assert_eq!(t.byte_length(), 12); // 5+5+2
    assert!(t.truncate(2));
    assert_eq!(t.byte_length(), 10); // 5+5
    assert!(t.truncate(0));
    assert_eq!(t.byte_length(), 0);
    assert!(t.is_empty());
    assert_eq!(t.root_hash(), MerkleTree::new().root_hash(), "empty == fresh empty");
}

// truncate is a no-op (returns false, no change) when new_len >= len, and a
// truncated tree can be appended to again, re-deriving the discarded indices.
#[test]
fn truncate_noop_and_reappend() {
    let (mut t, _) = tree(5);
    let root5 = t.root_hash();
    assert!(!t.truncate(5), "truncate to current length is a no-op");
    assert!(!t.truncate(9), "truncate beyond length is a no-op");
    assert_eq!(t.root_hash(), root5, "no-op truncate left the tree unchanged");

    assert!(t.truncate(3));
    // Re-append two blocks; the result equals a fresh 5-block tree of the new
    // content (the reused indices are overwritten cleanly).
    t.append(b"new-3");
    t.append(b"new-4");
    let mut fresh = tree_from(&[b"block-0".to_vec(), b"block-1".to_vec(), b"block-2".to_vec()]);
    fresh.append(b"new-3");
    fresh.append(b"new-4");
    assert_eq!(t.root_hash(), fresh.root_hash(), "re-append after truncate is clean");
    assert!(t.is_intact());
}

// After a reorg, `local` must be byte-identical to `remote`: same length,
// same roots, same root hash, intact, and every block proves.
fn assert_followed(local: &MerkleTree, remote: &MerkleTree, blocks: &[Vec<u8>]) {
    assert_eq!(local.len(), remote.len(), "reorg adopts remote's length");
    assert_eq!(local.roots(), remote.roots(), "reorg adopts remote's roots");
    assert_eq!(local.root_hash(), remote.root_hash(), "byte-identical after reorg");
    assert_eq!(local.byte_length(), remote.byte_length(), "byte_length follows remote");
    assert!(local.is_intact(), "reorged tree is intact");
    let root = remote.root_hash();
    for b in 0..remote.len() {
        let p = local.proof(b).expect("every adopted block proves");
        assert!(p.verify(&blocks[b as usize], &root), "block {b} proves after reorg");
    }
}

// Two trees built from identical content where one is a strict prefix of the
// other: LCA is the shorter length, and the shorter reorgs up to the longer
// (and vice versa) byte-identically. Ports merkle-tree.js "lowest common
// ancestor - small gap / bigger gap / remote is shorter than local".
#[test]
fn lca_prefix_gaps() {
    for &(remote_n, local_n, expect) in &[(10u64, 8u64, 8u64), (20, 1, 1), (5, 10, 5)] {
        let (remote, rblocks) = tree(remote_n as usize);
        let (mut local, _) = tree(local_n as usize);
        assert_eq!(
            local.lowest_common_ancestor(&remote),
            expect,
            "LCA(remote={remote_n}, local={local_n})"
        );
        // Reorg always makes `local` follow `remote` (up or down to its length).
        let ancestors = local.reorg(&remote);
        assert_eq!(ancestors, expect, "reorg returns the LCA");
        assert_followed(&local, &remote, &rblocks);
    }
}

// Both trees share a prefix then diverge at one block. LCA is the shared
// length; the local follows the remote onto its fork. Ports merkle-tree.js
// "lowest common ancestor - simple fork".
#[test]
fn lca_simple_fork() {
    let shared: Vec<Vec<u8>> = (0..5).map(|i| format!("block-{i}").into_bytes()).collect();
    let mut remote = tree_from(&shared);
    remote.append(b"fork #1");
    let mut local = tree_from(&shared);
    local.append(b"fork #2");

    assert_eq!(local.lowest_common_ancestor(&remote), 5, "diverge at block 5");
    let mut rblocks = shared.clone();
    rblocks.push(b"fork #1".to_vec());

    let ancestors = local.reorg(&remote);
    assert_eq!(ancestors, 5);
    assert_followed(&local, &remote, &rblocks);
}

// Diverge at block 5, then each side appends 100 more blocks (a long fork).
// LCA is still the shared prefix; the local fully adopts the remote's fork.
// Ports merkle-tree.js "lowest common ancestor - long fork".
#[test]
fn lca_long_fork() {
    let shared: Vec<Vec<u8>> = (0..5).map(|i| format!("block-{i}").into_bytes()).collect();
    let mut rblocks = shared.clone();
    rblocks.push(b"fork #1".to_vec());
    let mut lblocks = shared.clone();
    lblocks.push(b"fork #2".to_vec());
    for i in 0..100u64 {
        rblocks.push(format!("r#{i}").into_bytes());
        lblocks.push(format!("l#{i}").into_bytes());
    }
    let remote = tree_from(&rblocks);
    let mut local = tree_from(&lblocks);

    assert_eq!(local.lowest_common_ancestor(&remote), 5, "LCA is the shared prefix");
    let ancestors = local.reorg(&remote);
    assert_eq!(ancestors, 5);
    assert_followed(&local, &remote, &rblocks);
}

// Property: for every shared-prefix length `k` and every divergence shape,
// the LCA is exactly `k`. Covers prefix-only (no divergence ⇒ LCA = min len),
// divergence at `k`, and identical trees (LCA = full length, reorg is a no-op).
#[test]
fn lca_all_divergence_points() {
    for total in 1..=16u64 {
        for k in 0..=total {
            // Two trees agreeing on `[0, k)`, then differing from block `k`.
            let mut ablocks: Vec<Vec<u8>> = Vec::new();
            let mut bblocks: Vec<Vec<u8>> = Vec::new();
            for i in 0..total {
                let shared = format!("s-{i}").into_bytes();
                if i < k {
                    ablocks.push(shared.clone());
                    bblocks.push(shared);
                } else {
                    ablocks.push(format!("a-{i}").into_bytes());
                    bblocks.push(format!("b-{i}").into_bytes());
                }
            }
            let a = tree_from(&ablocks);
            let mut b = tree_from(&bblocks);
            // When k == total the trees are identical ⇒ LCA = total.
            assert_eq!(b.lowest_common_ancestor(&a), k, "LCA(total={total}, k={k})");
            assert_eq!(a.lowest_common_ancestor(&b), k, "LCA is symmetric");

            let was_noop = b.root_hash() == a.root_hash();
            b.reorg(&a);
            assert_followed(&b, &a, &ablocks);
            if was_noop {
                // Identical trees: reorg changes nothing.
                assert_eq!(b.len(), total);
            }
        }
    }
}

// Reorg keeps the shared prefix rather than rebuilding it: the surviving
// prefix nodes are exactly the ones the common ancestor already held (same
// hashes), so a block in `[0, ancestors)` proves under the *pre-reorg* root
// too — the prefix was never rewritten.
#[test]
fn reorg_preserves_shared_prefix() {
    let shared: Vec<Vec<u8>> = (0..6).map(|i| format!("block-{i}").into_bytes()).collect();
    let mut remote = tree_from(&shared);
    remote.append(b"R");
    let mut local = tree_from(&shared);
    local.append(b"L");

    // The shared prefix's root hash before the reorg.
    let prefix_root = {
        let mut p = local.clone();
        p.truncate(6);
        p.root_hash()
    };
    let ancestors = local.reorg(&remote);
    assert_eq!(ancestors, 6);
    // After the reorg, truncating back to the ancestor reproduces the very
    // same prefix root — the common prefix was preserved, not re-derived.
    let mut back = local.clone();
    back.truncate(ancestors);
    assert_eq!(back.root_hash(), prefix_root, "shared prefix preserved across reorg");
}

// --- audit regression tests (post-iteration-21) ---

// P0 soundness: a seek target must be a real block leaf. Passing the root node
// (odd index) authenticates against the real root and its aggregate subtree size
// brackets any offset; without the evenness guard, `verify` returned a bogus
// `index / 2` block. (Upstream's ByteSeeker guards `(index & 1) === 0`.)
#[test]
fn seek_rejects_non_leaf_target() {
    let (t, _) = tree(4);
    let root = t.root_hash();
    let root_node = t.roots()[0]; // index 3 (odd) for 4 blocks
    assert_eq!(root_node.index & 1, 1, "the 4-block root is an interior (odd) node");

    let forged = SeekProof {
        bytes: 0,
        leaf: root_node,
        siblings: vec![],
        roots: t.roots(),
    };
    assert!(
        forged.verify(&root).is_none(),
        "an interior node must not be accepted as a seek leaf"
    );
}

// P1 defense-in-depth: a proof sibling must be the path node's actual sibling.
// `parent_hash` binds child hash+size but NOT index, so a falsified same-side
// sibling index leaves the climb hash unchanged — only the structural guard
// rejects it.
#[test]
fn proof_rejects_falsified_sibling_index() {
    let (t, blocks) = tree(4);
    let root = t.root_hash();
    let mut proof = t.proof(0).unwrap();
    assert!(proof.verify(&blocks[0], &root), "honest proof verifies");

    // Real sibling of leaf 0 is index 2; forge the index to another same-side leaf.
    proof.siblings[0].index = 6;
    assert!(
        !proof.verify(&blocks[0], &root),
        "a sibling at the wrong index must be rejected structurally"
    );
}

// --- audit follow-up: reorg / LCA adversarial + seek zero-size (iter 25) ---

// Two length-`len` trees sharing blocks `[0, share)` then diverging.
fn forked_pair(share: u64, len: u64) -> (MerkleTree, MerkleTree, Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut ablocks: Vec<Vec<u8>> = Vec::new();
    let mut bblocks: Vec<Vec<u8>> = Vec::new();
    for i in 0..len {
        if i < share {
            let s = format!("s-{i}").into_bytes();
            ablocks.push(s.clone());
            bblocks.push(s);
        } else {
            ablocks.push(format!("a-{i}").into_bytes());
            bblocks.push(format!("b-{i}").into_bytes());
        }
    }
    (tree_from(&ablocks), tree_from(&bblocks), ablocks, bblocks)
}

// `lowest_common_ancestor` is content-blind and depends on both trees being
// intact; a missing node reads conservatively as disagreement. The invariant
// the binary search keeps — `agree(lo)` is always true, and `agree(a)` is true
// only when *both* trees produce equal prefix-root-hashes at `a` — means a
// corrupt input can only *shrink* the LCA, never over-claim. Whatever it
// returns is a genuine shared prefix (a real ancestor), with no panic.
#[test]
fn lca_conservative_under_corruption() {
    // self = a (intact); other = b. They genuinely share [0, 6) of length 8.
    let (a, b, _ablocks, _bblocks) = forked_pair(6, 8);
    let intact = a.lowest_common_ancestor(&b);
    assert_eq!(intact, 6, "intact LCA is the true shared prefix");

    // --- corrupt `other`: remove node 9 (root of blocks [4,6)), which the
    // length-6 prefix needs. The gap reads as disagreement, so the LCA falls
    // back to a genuine shorter shared prefix — never larger than the intact LCA.
    let mut b_corrupt = b.clone();
    assert!(b_corrupt.remove_node(9));
    assert_eq!(b_corrupt.prefix_root_hash(6), None, "length-6 prefix now unavailable");
    let lca = a.lowest_common_ancestor(&b_corrupt);
    assert!(lca <= intact, "corruption can only shrink the LCA, never grow it");
    assert!(b_corrupt.prefix_root_hash(lca).is_some(), "returned LCA is computable");
    assert_eq!(
        a.prefix_root_hash(lca),
        b_corrupt.prefix_root_hash(lca),
        "the returned LCA is a genuine shared prefix, not a forged one"
    );

    // --- monotonicity-precondition violation: removing node 8 (block-4 leaf)
    // makes the `agree` predicate FALSE at length 5 (node 8 gone) yet TRUE at
    // length 6 (nodes 3, 9 present, content shared) — non-monotone. The binary
    // search must still land on a length where the prefixes genuinely match.
    let mut b_holey = b.clone();
    assert!(b_holey.remove_node(8));
    assert_eq!(b_holey.prefix_root_hash(5), None, "length-5 prefix unavailable (node 8 gone)");
    assert_eq!(
        b_holey.prefix_root_hash(6),
        a.prefix_root_hash(6),
        "yet length-6 prefix is intact and matches — agreement is non-monotone"
    );
    let lca = a.lowest_common_ancestor(&b_holey);
    assert!(lca <= intact && lca > 0, "still a conservative, non-empty ancestor");
    assert_eq!(
        a.prefix_root_hash(lca),
        b_holey.prefix_root_hash(lca),
        "non-monotone agreement still yields a genuine ancestor"
    );

    // --- gapped `self`: corruption is symmetric and equally conservative.
    let mut a_corrupt = a.clone();
    assert!(a_corrupt.remove_node(9));
    let lca = a_corrupt.lowest_common_ancestor(&b);
    assert!(lca <= intact, "a gap in self also only shrinks the LCA");
    assert_eq!(
        a_corrupt.prefix_root_hash(lca),
        b.prefix_root_hash(lca),
        "gapped self still returns a genuine shared prefix"
    );
}

// The precondition the LCA binary search relies on: for two INTACT trees,
// prefix agreement is monotone — agreeing on `[0, a)` implies agreeing on every
// shorter prefix — so the search is exact (no over- or under-shoot).
#[test]
fn lca_intact_agreement_is_monotone() {
    let (a, b, _, _) = forked_pair(6, 9);
    let max = a.len().min(b.len());
    let agree: Vec<bool> = (0..=max)
        .map(|k| a.prefix_root_hash(k) == b.prefix_root_hash(k))
        .collect();
    // No agreement reappears after the first disagreement.
    if let Some(f) = agree.iter().position(|&x| !x) {
        assert!(agree[f..].iter().all(|&x| !x), "intact agreement is monotone");
    }
    // Diverging at block 6, [0,6) is shared so length-6 prefix agrees; length 7
    // (which covers block 6) does not.
    assert!(agree[6] && !agree[7], "boundary is exactly at the divergence");
    assert_eq!(a.lowest_common_ancestor(&b), 6, "binary search is exact for intact inputs");
}

// `reorg` adopts every node `other` holds, so an intact `other` is the
// precondition for a clean follow: following a CORRUPT `other` faithfully
// copies its gaps (self ends non-intact), while an intact `other` HEALS a
// gapped `self` by overwriting the gap with the complete node set.
#[test]
fn reorg_precondition_on_intact_other() {
    // Corrupt `other`: removing a suffix node (block-6 leaf = index 12) is
    // copied into `self`, leaving it in repair mode.
    let (a, mut b, _ablocks, _bblocks) = forked_pair(4, 8);
    let mut a_corrupt = a.clone();
    assert!(a_corrupt.remove_node(12));
    let _ = b.reorg(&a_corrupt);
    assert_eq!(b.len(), 8, "reorg adopts other's length");
    assert!(!b.has_node(12), "the gap in other is copied verbatim");
    assert!(!b.is_intact(), "reorg copies other's corruption — intact-other is required");

    // Intact `other` heals a gapped `self`: remove a shared-region node
    // (node 3 = root of [0,4)) from self, then follow the intact other. Adopting
    // other's full node set overwrites the gap, so self ends intact + identical.
    let (a2, b2, ablocks2, _) = forked_pair(4, 8);
    let mut b2_holey = b2.clone();
    assert!(b2_holey.remove_node(3));
    assert!(!b2_holey.is_intact(), "self starts gapped");
    b2_holey.reorg(&a2);
    assert!(b2_holey.is_intact(), "intact other heals self's gap");
    assert_eq!(b2_holey.root_hash(), a2.root_hash(), "byte-identical follow");
    let root = a2.root_hash();
    for blk in 0..a2.len() {
        let p = b2_holey.proof(blk).expect("every adopted block proves");
        assert!(p.verify(&ablocks2[blk as usize], &root), "block {blk} proves after reorg");
    }
}

// Zero-size (empty) blocks are legitimate L1 payloads. A zero-size block
// occupies an empty byte interval, so no byte offset lands in it — the seek
// skips it to the next non-empty block — and the tree seek still agrees with a
// linear scan, with seek proofs authenticating the same mapping.
#[test]
fn seek_handles_zero_size_blocks() {
    // Leading, interior, consecutive, and trailing empties.
    let sizes = [0usize, 2, 0, 0, 3, 1, 0];
    let mut t = MerkleTree::new();
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    for (i, &s) in sizes.iter().enumerate() {
        let b = vec![b'a' + i as u8; s];
        t.append(&b);
        blocks.push(b);
    }
    let total: u64 = sizes.iter().map(|&s| s as u64).sum();
    assert!(total > 0, "the tree has some bytes despite the empties");
    let root = t.root_hash();

    for bytes in 0..total {
        let located = t.seek(bytes);
        assert_eq!(located, linear_seek(&blocks, bytes), "tree seek == linear (bytes={bytes})");
        let (block, _off) = located;
        assert!(
            block < t.len() && !blocks[block as usize].is_empty(),
            "a byte never resolves to an empty block (bytes={bytes})"
        );
        let sp = t.seek_proof(bytes).expect("in-range byte has a seek proof");
        assert_eq!(
            sp.verify(&root),
            Some(located),
            "seek proof authenticates the located block (bytes={bytes})"
        );
    }
    // At/past the end there is no block to locate.
    assert_eq!(t.seek(total), (t.len(), 0), "byte == total is past the last block");
    assert!(t.seek_proof(total).is_none());

    // An all-empty tree has zero bytes: every offset is past the (zero) end.
    let mut empties = MerkleTree::new();
    for _ in 0..4 {
        empties.append(b"");
    }
    assert_eq!(empties.seek(0), (4, 0), "all-empty tree: byte 0 is past the end");
    assert!(empties.seek_proof(0).is_none(), "no block to locate in an all-empty tree");
}

#[test]
fn serialize_round_trips_root_proofs_and_byte_length() {
    for n in [0usize, 1, 2, 3, 5, 8, 13, 25] {
        let (t, blocks) = tree(n);
        let restored = MerkleTree::deserialize(&t.serialize())
            .unwrap_or_else(|| panic!("round-trips at n={n}"));

        assert_eq!(restored.len(), t.len(), "length n={n}");
        assert_eq!(restored.root_hash(), t.root_hash(), "root_hash n={n}");
        assert_eq!(restored.byte_length(), t.byte_length(), "byte_length n={n}");

        // every per-block proof from the restored tree verifies against the
        // (identical) root — the authenticated structure survived the round-trip.
        let root = restored.root_hash();
        for (i, b) in blocks.iter().enumerate() {
            let p = restored.proof(i as u64).expect("proof");
            assert!(p.verify(b, &root), "restored proof block {i} (n={n})");
        }
    }
}

#[test]
fn serialize_round_trips_a_sparse_tree_missing_block_bytes() {
    // A sparse core keeps its tree nodes after the block bytes are dropped;
    // serialization must carry those nodes so absent blocks stay authenticated.
    let (mut t, blocks) = tree(8);
    let root = t.root_hash();
    let restored = MerkleTree::deserialize(&t.serialize()).unwrap();
    // the tree itself doesn't hold bytes, but its node set must be intact:
    assert!(restored.is_intact());
    for (i, b) in blocks.iter().enumerate() {
        let p = restored.proof(i as u64).expect("proof from sparse tree");
        assert!(p.verify(b, &root), "block {i} still authenticated after round-trip");
    }
    // truncation still works post-round-trip (length is a pure prefix function)
    t.truncate(3);
    let restored3 = MerkleTree::deserialize(&t.serialize()).unwrap();
    assert_eq!(restored3.root_hash(), t.root_hash());
}

#[test]
fn deserialize_rejects_malformed_buffers() {
    assert!(MerkleTree::deserialize(&[]).is_none());
    assert!(MerkleTree::deserialize(&[0u8; 8]).is_none()); // header too short
    let (t, _) = tree(5);
    let mut bytes = t.serialize();
    bytes.truncate(bytes.len() - 1); // chop the last node short
    assert!(MerkleTree::deserialize(&bytes).is_none());
    let mut trailing = t.serialize();
    trailing.push(0); // trailing garbage
    assert!(MerkleTree::deserialize(&trailing).is_none());
}
