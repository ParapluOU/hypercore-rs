//! Toy projections + op encoders — a reference domain for examples and tests.
//!
//! Deliberately domain-agnostic and tiny: a key/value map folded from `PUT`/`DEL`
//! ops, plus a mutation counter. Compiled into the normal build so both the unit
//! tests and the external integration tests can share one example projection.

use std::collections::BTreeMap;

use autobase::NodeId;
use codec::varint;

use crate::projection::Projection;

const OP_PUT: u8 = 0;
const OP_DEL: u8 = 1;

/// Encode a `PUT key value` op.
pub fn put(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = vec![OP_PUT];
    varint::write(&mut out, key.len() as u64);
    out.extend_from_slice(key);
    out.extend_from_slice(value);
    out
}

/// Encode a `DEL key` op.
pub fn del(key: &[u8]) -> Vec<u8> {
    let mut out = vec![OP_DEL];
    out.extend_from_slice(key);
    out
}

/// A key/value map, last-write-by-autobase-order wins.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KvProjection {
    state: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl KvProjection {
    pub fn new() -> Self {
        Self::default()
    }
}

/// A malformed op payload.
#[derive(Debug)]
pub struct KvError;

impl Projection for KvProjection {
    type State = BTreeMap<Vec<u8>, Vec<u8>>;
    type Error = KvError;

    fn apply(&mut self, _node: NodeId, payload: &[u8]) -> Result<(), KvError> {
        let (&tag, mut rest) = payload.split_first().ok_or(KvError)?;
        match tag {
            OP_PUT => {
                let klen = varint::read(&mut rest).map_err(|_| KvError)? as usize;
                if rest.len() < klen {
                    return Err(KvError);
                }
                let (key, value) = rest.split_at(klen);
                self.state.insert(key.to_vec(), value.to_vec());
            }
            OP_DEL => {
                self.state.remove(rest);
            }
            _ => return Err(KvError),
        }
        Ok(())
    }

    fn snapshot(&self) -> &Self::State {
        &self.state
    }

    fn reset_to(&mut self, checkpoint: &Self::State) {
        self.state = checkpoint.clone();
    }
}

/// Counts applied mutations (an order-insensitive sanity projection).
#[derive(Clone, Debug, Default)]
pub struct CounterProjection {
    n: u64,
}

/// This projection never fails.
#[derive(Debug)]
pub enum Never {}

impl Projection for CounterProjection {
    type State = u64;
    type Error = Never;

    fn apply(&mut self, _node: NodeId, _payload: &[u8]) -> Result<(), Never> {
        self.n += 1;
        Ok(())
    }

    fn snapshot(&self) -> &u64 {
        &self.n
    }

    fn reset_to(&mut self, checkpoint: &u64) {
        self.n = *checkpoint;
    }
}
