//! Byte encoding for [`SyncMessage`] — what a real transport puts on the wire.
//!
//! Hand-rolled over the [`codec`] varint primitives (no serde), so the L1 types
//! stay serde-free. It serializes the signed head + Merkle proof carried by a
//! [`SyncMessage::Block`] via their public fields. Always compiled (the `iroh`
//! transport uses it; the default build tests it).

use autobase::WriterKey;
use codec::varint;
use hypercore::SignedHead;
use identity::Sig;
use merkle::{Node, Proof};

use crate::sync::SyncMessage;

/// A malformed wire buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    /// Ran out of bytes mid-value.
    Eof,
    /// A varint did not terminate.
    Varint,
    /// Unknown message tag.
    BadTag(u64),
}

const T_HEAD: u64 = 0;
const T_HAVE: u64 = 1;
const T_WANT: u64 = 2;
const T_BLOCK: u64 = 3;

/// Encode a [`SyncMessage`] to bytes.
pub fn encode(m: &SyncMessage) -> Vec<u8> {
    let mut out = Vec::new();
    match m {
        SyncMessage::Head { writer, head } => {
            varint::write(&mut out, T_HEAD);
            out.extend_from_slice(writer);
            wr_head(&mut out, head);
        }
        SyncMessage::Have { writer, length } => {
            varint::write(&mut out, T_HAVE);
            out.extend_from_slice(writer);
            varint::write(&mut out, *length);
        }
        SyncMessage::Want { writer, start, end } => {
            varint::write(&mut out, T_WANT);
            out.extend_from_slice(writer);
            varint::write(&mut out, *start);
            varint::write(&mut out, *end);
        }
        SyncMessage::Block { writer, head, index, bytes, proof } => {
            varint::write(&mut out, T_BLOCK);
            out.extend_from_slice(writer);
            wr_head(&mut out, head);
            varint::write(&mut out, *index);
            varint::write(&mut out, bytes.len() as u64);
            out.extend_from_slice(bytes);
            wr_proof(&mut out, proof);
        }
    }
    out
}

/// Decode a [`SyncMessage`] from bytes.
pub fn decode(mut b: &[u8]) -> Result<SyncMessage, WireError> {
    let tag = rd_u64(&mut b)?;
    let writer: WriterKey = rd_32(&mut b)?;
    Ok(match tag {
        T_HEAD => SyncMessage::Head { writer, head: rd_head(&mut b)? },
        T_HAVE => SyncMessage::Have { writer, length: rd_u64(&mut b)? },
        T_WANT => {
            let start = rd_u64(&mut b)?;
            let end = rd_u64(&mut b)?;
            SyncMessage::Want { writer, start, end }
        }
        T_BLOCK => {
            let head = rd_head(&mut b)?;
            let index = rd_u64(&mut b)?;
            let blen = rd_u64(&mut b)? as usize;
            let bytes = take(&mut b, blen)?.to_vec();
            let proof = rd_proof(&mut b)?;
            SyncMessage::Block { writer, head, index, bytes, proof }
        }
        other => return Err(WireError::BadTag(other)),
    })
}

// ---- field codecs --------------------------------------------------------

fn wr_head(out: &mut Vec<u8>, h: &SignedHead) {
    varint::write(out, h.fork);
    varint::write(out, h.length);
    out.extend_from_slice(&h.root);
    out.extend_from_slice(&h.sig.to_bytes());
}

fn rd_head(b: &mut &[u8]) -> Result<SignedHead, WireError> {
    let fork = rd_u64(b)?;
    let length = rd_u64(b)?;
    let root = rd_32(b)?;
    let sig = Sig::from_bytes(&rd_64(b)?);
    Ok(SignedHead { fork, length, root, sig })
}

fn wr_node(out: &mut Vec<u8>, n: &Node) {
    varint::write(out, n.index);
    out.extend_from_slice(&n.hash);
    varint::write(out, n.size);
}

fn rd_node(b: &mut &[u8]) -> Result<Node, WireError> {
    let index = rd_u64(b)?;
    let hash = rd_32(b)?;
    let size = rd_u64(b)?;
    Ok(Node { index, hash, size })
}

fn wr_nodes(out: &mut Vec<u8>, ns: &[Node]) {
    varint::write(out, ns.len() as u64);
    for n in ns {
        wr_node(out, n);
    }
}

fn rd_nodes(b: &mut &[u8]) -> Result<Vec<Node>, WireError> {
    let k = rd_u64(b)? as usize;
    let mut v = Vec::with_capacity(k);
    for _ in 0..k {
        v.push(rd_node(b)?);
    }
    Ok(v)
}

fn wr_proof(out: &mut Vec<u8>, p: &Proof) {
    varint::write(out, p.block);
    varint::write(out, p.leaf_size);
    wr_nodes(out, &p.siblings);
    wr_nodes(out, &p.roots);
}

fn rd_proof(b: &mut &[u8]) -> Result<Proof, WireError> {
    let block = rd_u64(b)?;
    let leaf_size = rd_u64(b)?;
    let siblings = rd_nodes(b)?;
    let roots = rd_nodes(b)?;
    Ok(Proof { block, leaf_size, siblings, roots })
}

// ---- primitive readers ---------------------------------------------------

fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8], WireError> {
    if b.len() < n {
        return Err(WireError::Eof);
    }
    let (head, tail) = b.split_at(n);
    *b = tail;
    Ok(head)
}

fn rd_u64(b: &mut &[u8]) -> Result<u64, WireError> {
    varint::read(b).map_err(|_| WireError::Varint)
}

fn rd_32(b: &mut &[u8]) -> Result<[u8; 32], WireError> {
    let s = take(b, 32)?;
    let mut a = [0u8; 32];
    a.copy_from_slice(s);
    Ok(a)
}

fn rd_64(b: &mut &[u8]) -> Result<[u8; 64], WireError> {
    let s = take(b, 64)?;
    let mut a = [0u8; 64];
    a.copy_from_slice(s);
    Ok(a)
}
