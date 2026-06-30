//\! Flat-tree index arithmetic. Leaves at even indices (block `k` -> `2k`), parents at odd.
//\! See the mafintosh `flat-tree` algorithm.

/// Depth of a node (leaves = 0).
pub fn depth(i: u64) -> u32 {
    (i + 1).trailing_zeros()
}

/// Horizontal offset of a node within its depth.
pub fn offset(i: u64) -> u64 {
    if i & 1 == 0 {
        i / 2
    } else {
        let d = depth(i);
        (((i + 1) >> d) - 1) / 2
    }
}

/// Node index from a (depth, offset) pair.
pub fn index(depth: u32, offset: u64) -> u64 {
    (1 + 2 * offset) * (1u64 << depth) - 1
}

/// Parent of node `i`.
pub fn parent(i: u64) -> u64 {
    let d = depth(i);
    index(d + 1, offset(i) >> 1)
}

/// Sibling of node `i`.
pub fn sibling(i: u64) -> u64 {
    let d = depth(i);
    index(d, offset(i) ^ 1)
}

/// The (left, right) children of a parent node, or `None` for a leaf.
pub fn children(i: u64) -> Option<(u64, u64)> {
    if i & 1 == 0 {
        return None;
    }
    let d = depth(i);
    let off = offset(i) * 2;
    Some((index(d - 1, off), index(d - 1, off + 1)))
}

/// The half-open block range `[first, end)` that node `i` covers. A leaf
/// (`i == 2k`) covers exactly block `k`; a node at depth `d` covers `2^d`
/// contiguous blocks.
pub fn block_range(i: u64) -> (u64, u64) {
    let d = depth(i);
    let count = 1u64 << d;
    let first = offset(i) * count;
    (first, first + count)
}

/// Root indices covering a fully-rooted tree of `idx` (= `2 * block_count`)
/// tree-index units. Returns one index per complete power-of-two subtree.
pub fn full_roots(idx: u64) -> Vec<u64> {
    assert!(idx & 1 == 0, "full_roots requires an even index");
    let mut result = Vec::new();
    let mut index = idx / 2;
    let mut offset = 0u64;
    let mut factor = 1u64;
    while index > 0 {
        while factor * 2 <= index {
            factor *= 2;
        }
        result.push(offset * 2 + factor - 1);
        offset += factor;
        index -= factor;
        factor = 1;
    }
    result
}
