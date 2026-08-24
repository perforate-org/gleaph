//! Delta + LEB128 varint postings — the uncompressed-size baseline encoding.
//!
//! Byte layout (no magic or version yet; stable-store headers embed those later):
//!
//! ```text
//! count:        u32 LE   // number of docids
//! first_docid:  LEB128 u32
//! gaps:         LEB128 u32 × (count - 1)  // strictly positive
//! ```
//!
//! Corruption policy: buffers are produced only by [`encode_varint`]; any truncated or
//! malformed input panics with a `corrupt postings:` message at the first offending read.
//! `advance(target)` is a linear walk: this encoding has no random-access structure.

/// Encodes strictly increasing, non-empty docids as delta + LEB128 varints.
///
/// # Panics
/// Panics when `docs` is empty or not strictly increasing.
pub fn encode_varint(docs: &[u32]) -> Vec<u8> {
    assert_strictly_increasing(docs);
    let mut out = Vec::with_capacity(docs.len() * 5 + 4);
    out.extend_from_slice(&(docs.len() as u32).to_le_bytes());
    let mut prev: u32 = 0;
    for (i, &doc) in docs.iter().enumerate() {
        let delta = if i == 0 { doc } else { doc - prev };
        write_u32(&mut out, delta);
        prev = doc;
    }
    out
}

/// Cursor over a delta + LEB128 varint posting list.
pub struct VarintReader<'a> {
    data: &'a [u8],
    count: u32,
    /// Bytes consumed from the delta stream (after the header).
    byte_pos: usize,
    pos: u32,
    /// Last decoded absolute docid (accumulated), valid once pos >= 1.
    prev: u32,
    current: Option<u32>,
}

impl<'a> VarintReader<'a> {
    /// Parses the header; payload decoding is lazy.
    ///
    /// # Panics
    /// Panics when the header itself is truncated.
    pub fn new(data: &'a [u8]) -> Self {
        if data.len() < 4 {
            panic!("corrupt postings: varint header truncated");
        }
        let count = u32::from_le_bytes(data[..4].try_into().expect("fixed-size header"));
        Self {
            data,
            count,
            byte_pos: 4,
            pos: 0,
            prev: 0,
            current: None,
        }
    }

    fn decode_delta(&mut self) -> u32 {
        let mut result: u32 = 0;
        let mut shift: u32 = 0;
        loop {
            let Some(&byte) = self.data.get(self.byte_pos) else {
                panic!("corrupt postings: varint stream truncated");
            };
            self.byte_pos += 1;
            if shift == 28 && byte > 0x0F {
                panic!("corrupt postings: varint exceeds 32 bits");
            }
            result |= u32::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return result;
            }
            shift += 7;
            if shift >= 32 {
                panic!("corrupt postings: varint exceeds 32 bits");
            }
        }
    }

    fn fill_current(&mut self) {
        if self.current.is_none() && self.pos < self.count {
            let delta = self.decode_delta();
            let value = if self.pos == 0 {
                delta
            } else {
                match self.prev.checked_add(delta) {
                    Some(v) => v,
                    None => panic!("corrupt postings: varint delta overflow"),
                }
            };
            self.prev = value;
            self.current = Some(value);
        }
    }
}

impl<'a> super::PostingReader for VarintReader<'a> {
    fn len(&self) -> u32 {
        self.count
    }

    fn pos(&self) -> u32 {
        self.pos
    }

    fn peek(&mut self) -> Option<u32> {
        self.fill_current();
        self.current
    }

    fn next(&mut self) -> Option<u32> {
        let value = self.peek();
        if value.is_some() {
            self.current = None;
            self.pos += 1;
        }
        value
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        loop {
            match self.peek() {
                None => return None,
                Some(v) if v >= target => return Some(v),
                Some(_) => {
                    self.next();
                }
            }
        }
    }
}

/// Shared encoder-side precondition for every posting codec in this crate.
pub(crate) fn assert_strictly_increasing(docs: &[u32]) {
    assert!(!docs.is_empty(), "postings must be non-empty");
    assert!(
        docs.windows(2).all(|w| w[0] < w[1]),
        "postings must be strictly increasing"
    );
}

/// Appends one LEB128-encoded u32 — the shared wire primitive for delta-based codecs
/// (used directly by the freq-varint encoder).
pub(crate) fn write_u32(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let group = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(group);
            return;
        }
        out.push(group | 0x80);
    }
}
