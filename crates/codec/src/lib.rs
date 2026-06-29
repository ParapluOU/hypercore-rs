//! `codec` — typed payload <-> bytes: versioned and tolerant.
//!
//! A `Codec<T>` turns a typed payload into bytes for the log and back. The log
//! is immutable history *forever* and entries are content-addressed, so the wire
//! format is a permanent ABI: the codec must be **versioned** (a format tag can
//! evolve) and **tolerant** (an older reader skips fields/variants a newer writer
//! added; trailing bytes are ignored, not an error).
//!
//! Content-blind: nothing here inspects domain meaning — it moves bytes.
//!
//! Dependency-free (LEB128 varints + length/version framing) so it builds for
//! every target including `wasm32`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Ran out of bytes mid-value.
    Eof,
    /// A varint did not terminate within 64 bits.
    BadVarint,
    /// Decoder-specific failure.
    Invalid(&'static str),
}

/// LEB128 unsigned varints.
pub mod varint {
    use super::Error;

    pub fn write(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                break;
            }
        }
    }

    /// Read a varint, advancing `bytes` past it.
    pub fn read(bytes: &mut &[u8]) -> Result<u64, Error> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            if shift >= 64 {
                return Err(Error::BadVarint);
            }
            let (&b, rest) = bytes.split_first().ok_or(Error::Eof)?;
            *bytes = rest;
            result |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        Ok(result)
    }
}

/// Prepend a format-version varint to a payload. The decoder reads the version
/// and may branch / default missing fields, so the format can evolve.
pub fn frame(version: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 2);
    varint::write(&mut out, version);
    out.extend_from_slice(payload);
    out
}

/// Split a framed buffer into `(version, payload)`.
pub fn unframe(bytes: &[u8]) -> Result<(u64, &[u8]), Error> {
    let mut b = bytes;
    let version = varint::read(&mut b)?;
    Ok((version, b))
}

/// Write a self-describing `(tag, len, body)` record. Because the length is
/// explicit, a reader can **skip a variant it does not recognise**.
pub fn write_tagged(out: &mut Vec<u8>, tag: u64, body: &[u8]) {
    varint::write(out, tag);
    varint::write(out, body.len() as u64);
    out.extend_from_slice(body);
}

/// Read one `(tag, body)` record, advancing `bytes`. Unknown tags are handled by
/// the caller; the body length is always known, so parsing never gets lost.
pub fn read_tagged<'a>(bytes: &mut &'a [u8]) -> Result<(u64, &'a [u8]), Error> {
    let tag = varint::read(bytes)?;
    let len = varint::read(bytes)? as usize;
    if bytes.len() < len {
        return Err(Error::Eof);
    }
    let (body, rest) = bytes.split_at(len);
    *bytes = rest;
    Ok((tag, body))
}

/// A codec for payloads of type `T`. Separated from `T` so the same type can use
/// different wire formats, and so ordering/storage stay content-blind.
pub trait Codec<T> {
    fn encode_into(&self, value: &T, out: &mut Vec<u8>);

    fn decode(&self, bytes: &[u8]) -> Result<T, Error>;

    fn encode(&self, value: &T) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(value, &mut out);
        out
    }
}

/// `u64` as a varint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct U64;
impl Codec<u64> for U64 {
    fn encode_into(&self, value: &u64, out: &mut Vec<u8>) {
        varint::write(out, *value);
    }
    fn decode(&self, bytes: &[u8]) -> Result<u64, Error> {
        let mut b = bytes;
        varint::read(&mut b)
    }
}

/// Length-delimited bytes. Decoding **ignores trailing bytes** past the declared
/// length — an older reader tolerates a newer writer's extra data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bytes;
impl Codec<Vec<u8>> for Bytes {
    fn encode_into(&self, value: &Vec<u8>, out: &mut Vec<u8>) {
        varint::write(out, value.len() as u64);
        out.extend_from_slice(value);
    }
    fn decode(&self, bytes: &[u8]) -> Result<Vec<u8>, Error> {
        let mut b = bytes;
        let len = varint::read(&mut b)? as usize;
        if b.len() < len {
            return Err(Error::Eof);
        }
        Ok(b[..len].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
            let mut out = Vec::new();
            varint::write(&mut out, v);
            let mut b = &out[..];
            assert_eq!(varint::read(&mut b).unwrap(), v);
            assert!(b.is_empty(), "varint consumed exactly its bytes");
        }
    }

    #[test]
    fn varint_truncated_is_eof() {
        let mut b: &[u8] = &[0x80]; // continuation bit set, no next byte
        assert_eq!(varint::read(&mut b), Err(Error::Eof));
    }

    #[test]
    fn builtin_codecs_roundtrip() {
        assert_eq!(U64.decode(&U64.encode(&42)).unwrap(), 42);
        let data = b"hello world".to_vec();
        assert_eq!(Bytes.decode(&Bytes.encode(&data)).unwrap(), data);
    }

    #[test]
    fn bytes_tolerates_trailing() {
        // a newer writer appended bytes the older reader doesn't understand
        let mut buf = Bytes.encode(&b"abc".to_vec());
        buf.extend_from_slice(b"future-fields");
        assert_eq!(Bytes.decode(&buf).unwrap(), b"abc".to_vec());
    }

    #[test]
    fn framing_preserves_version_and_payload() {
        let f = frame(7, b"body");
        let (v, payload) = unframe(&f).unwrap();
        assert_eq!(v, 7);
        assert_eq!(payload, b"body");
    }

    #[test]
    fn tagged_skips_unknown_variants() {
        // stream: known(1), unknown(99), known(2)
        let mut out = Vec::new();
        write_tagged(&mut out, 1, b"a");
        write_tagged(&mut out, 99, b"some-unknown-body");
        write_tagged(&mut out, 2, b"cc");

        let mut b = &out[..];
        let (t1, body1) = read_tagged(&mut b).unwrap();
        let (t2, _skip) = read_tagged(&mut b).unwrap(); // unknown, but length-skippable
        let (t3, body3) = read_tagged(&mut b).unwrap();
        assert_eq!((t1, body1), (1, &b"a"[..]));
        assert_eq!(t2, 99);
        assert_eq!((t3, body3), (2, &b"cc"[..]));
        assert!(b.is_empty());
    }

    // Schema evolution in both directions, the DoD property.
    // v1 record = one field; v2 record = two fields. Encoded behind a version frame.
    fn decode_record(bytes: &[u8]) -> (u64, u64) {
        let (version, mut body) = {
            let (v, p) = unframe(bytes).unwrap();
            (v, p)
        };
        let a = varint::read(&mut body).unwrap();
        // v2 added field `b`; v1 readers default it; v1 bytes give a v2 reader EOF -> default.
        let b = if version >= 2 {
            varint::read(&mut body).unwrap_or(0)
        } else {
            0
        };
        (a, b)
    }

    #[test]
    fn old_bytes_decode_under_newer_schema() {
        // v1 writer (one field); read by the current (v2-aware) decoder.
        let mut body = Vec::new();
        varint::write(&mut body, 10);
        let v1 = frame(1, &body);
        assert_eq!(decode_record(&v1), (10, 0)); // missing field defaulted
    }

    #[test]
    fn newer_bytes_tolerated_by_reader() {
        // v2 writer (two fields).
        let mut body = Vec::new();
        varint::write(&mut body, 10);
        varint::write(&mut body, 20);
        let v2 = frame(2, &body);
        assert_eq!(decode_record(&v2), (10, 20));
    }
}
