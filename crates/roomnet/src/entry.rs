//! The content of one log entry: a domain op plus its causal links.

use autobase::NodeId;
use codec::{varint, Codec, Error as CodecError};

/// The **content** of one log entry — the bytes of a single hypercore block
/// (stored + Merkle-hashed).
///
/// - `heads`: the autobase causal deps this op references (the frontier it saw);
///   fed to the [`Linearizer`](autobase::Linearizer).
/// - `payload`: the opaque domain op; fed to the [`Projection`](crate::Projection).
///
/// L1 (hypercore/merkle) never inspects this — it is opaque bytes below the codec.
/// Only roomnet decodes it. This is exactly what rides inside
/// [`SyncMessage::Block`](crate::sync::SyncMessage)'s `bytes`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Entry {
    pub heads: Vec<NodeId>,
    pub payload: Vec<u8>,
}

impl Entry {
    pub fn new(heads: Vec<NodeId>, payload: Vec<u8>) -> Self {
        Self { heads, payload }
    }
}

/// Versioned, tolerant codec for [`Entry`] (LEB128 framing; forward-compatible).
#[derive(Clone, Copy, Debug, Default)]
pub struct EntryCodec;

/// Wire format version for [`Entry`] (a permanent ABI — see [`codec`]).
const ENTRY_V1: u64 = 1;

impl Codec<Entry> for EntryCodec {
    fn encode_into(&self, value: &Entry, out: &mut Vec<u8>) {
        varint::write(out, ENTRY_V1);
        varint::write(out, value.heads.len() as u64);
        for h in &value.heads {
            out.extend_from_slice(&h.key);
            varint::write(out, h.seq);
        }
        varint::write(out, value.payload.len() as u64);
        out.extend_from_slice(&value.payload);
    }

    fn decode(&self, bytes: &[u8]) -> Result<Entry, CodecError> {
        let mut b = bytes;
        let _version = varint::read(&mut b)?; // tolerant: unknown future fields ignored
        let n = varint::read(&mut b)? as usize;
        let mut heads = Vec::with_capacity(n);
        for _ in 0..n {
            if b.len() < 32 {
                return Err(CodecError::Eof);
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&b[..32]);
            b = &b[32..];
            let seq = varint::read(&mut b)?;
            heads.push(NodeId::new(key, seq));
        }
        let len = varint::read(&mut b)? as usize;
        if b.len() < len {
            return Err(CodecError::Eof);
        }
        Ok(Entry { heads, payload: b[..len].to_vec() })
    }
}
