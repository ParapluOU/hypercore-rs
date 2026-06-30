use codec::Codec;
use storage::Store;

use crate::*;

impl<T, C: Codec<T>, S: Store> Iterator for ReadStream<'_, T, C, S> {
    type Item = Result<T, Error<S::Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.lo < self.hi {
            let i = if self.reverse {
                self.hi -= 1;
                self.hi
            } else {
                let i = self.lo;
                self.lo += 1;
                i
            };
            match self.core.get(i) {
                Ok(Some(v)) => return Some(Ok(v)),
                Ok(None) => continue, // absent block: no-wait skip
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }
}

impl<T, C: Codec<T>, S: Store> Iterator for ByteStream<'_, T, C, S> {
    type Item = Result<Vec<u8>, Error<S::Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.budget > 0 && self.next < self.core.len() {
            let i = self.next;
            self.next += 1;
            match self.core.block(i) {
                Ok(Some(bytes)) => {
                    self.budget = self.budget.saturating_sub(bytes.len() as u64);
                    return Some(Ok(bytes));
                }
                Ok(None) => continue, // absent block: no-wait skip
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }
}

