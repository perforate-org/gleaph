//! System dictionary (sys.dic) parser
//!
//! Copyright 2026 COOLJAPAN OU (Team KitaSan)
//! Vendored (plan 0333) from github.com/cool-japan/mecrab @ 85444b5
//! (mecrab/src/dict/sys_dic.rs), MIT OR Apache-2.0.
//!
//! Plan 0333 refactor (the two contiguous-slice assumption removal points in the whole
//! vendor):
//! 1. `tokens_ptr: *const Token` / `features_ptr: *const u8` raw pointers over the mmap
//!    slice are replaced by byte offsets + per-access `read_exact_at` on `Arc<dyn
//!    ByteImage>`;
//! 2. `get_feature` returns an owned `String` (upstream returned a `&str` borrowed from
//!    the mmap via a pointer-walking NUL scan) — a per-access stable reader cannot hand
//!    out a borrowed slice, so the NUL scan is re-expressed as bounded chunked reads.

use crate::mecrab_vendor::byteimage::ByteImage;
use crate::mecrab_vendor::dict::double_array_trie::{DartsResult, DoubleArrayTrie};
use crate::mecrab_vendor::dict::DictionaryEntry;
use crate::mecrab_vendor::error::{Error, Result};
use byteorder::{ByteOrder, LittleEndian};
use std::sync::Arc;

/// Magic number for system dictionary validation (MeCab: 0xef718f77)
pub const DICTIONARY_MAGIC_ID: u32 = 0xef71_8f77;

/// Dictionary version (DIC_VERSION from MeCab)
pub const DIC_VERSION: u32 = 102;

/// Header size: 10 * u32 (40 bytes) + 32 bytes charset = 72 bytes
pub const HEADER_SIZE: usize = 72;

/// Maximum number of results from common prefix search
const MAX_RESULTS: usize = 512;

/// Token structure matching MeCab's Token struct (16 bytes)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Token {
    pub left_id: u16,
    pub right_id: u16,
    pub pos_id: u16,
    pub wcost: i16,
    pub feature_offset: u32,
    pub compound: u32,
}

impl Token {
    pub const SIZE: usize = 16;
}

/// System dictionary containing trie, tokens, and features
pub struct SysDic {
    /// Byte image (upstream: `_mmap: Arc<Mmap>`)
    image: Arc<dyn ByteImage>,
    /// Offset of the trie inside the image (upstream: trie over `&data[HEADER_SIZE..]`)
    trie_offset: u64,
    trie: DoubleArrayTrie,
    /// Offset of the token array (upstream: `tokens_ptr: *const Token`)
    token_offset: u64,
    tokens_count: usize,
    /// Offset of the feature strings (upstream: `features_ptr: *const u8`)
    feature_offset: u64,
    feature_size: usize,
    version: u32,
    dict_type: u32,
    lexicon_size: u32,
    left_size: u32,
    right_size: u32,
    charset: String,
}

impl std::fmt::Debug for SysDic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SysDic")
            .field("version", &self.version)
            .field("dict_type", &self.dict_type)
            .field("lexicon_size", &self.lexicon_size)
            .field("left_size", &self.left_size)
            .field("right_size", &self.right_size)
            .field("trie_size", &self.trie.size())
            .field("tokens_count", &self.tokens_count)
            .finish()
    }
}

/// Read a little-endian u32 at `offset` via the image.
#[inline]
fn read_u32_at(image: &dyn ByteImage, offset: u64) -> u32 {
    let mut b = [0u8; 4];
    image.read_exact_at(offset, &mut b);
    LittleEndian::read_u32(&b)
}

impl SysDic {
    /// Load system dictionary from a byte image (upstream: `from_mmap(Arc<Mmap>)`).
    pub fn from_image(image: Arc<dyn ByteImage>) -> Result<Self> {
        let len = image.len();
        if len < HEADER_SIZE as u64 {
            return Err(Error::CorruptedDictionary(format!(
                "Dictionary file too small: {len} bytes (minimum {HEADER_SIZE} bytes)"
            )));
        }

        // magic ^ DictionaryMagicID == filesize
        let magic = read_u32_at(image.as_ref(), 0);
        let expected_size = magic ^ DICTIONARY_MAGIC_ID;
        if expected_size != len as u32 {
            return Err(Error::InvalidDictionaryFormat(format!(
                "Magic number mismatch: expected file size {expected_size}, got {len}"
            )));
        }

        let version = read_u32_at(image.as_ref(), 4);
        if version != DIC_VERSION {
            return Err(Error::InvalidDictionaryFormat(format!(
                "Incompatible dictionary version: expected {DIC_VERSION}, got {version}"
            )));
        }

        let dict_type = read_u32_at(image.as_ref(), 8);
        let lexicon_size = read_u32_at(image.as_ref(), 12);
        let left_size = read_u32_at(image.as_ref(), 16);
        let right_size = read_u32_at(image.as_ref(), 20);
        let da_size = read_u32_at(image.as_ref(), 24) as usize;
        let token_size = read_u32_at(image.as_ref(), 28) as usize;
        let feature_size = read_u32_at(image.as_ref(), 32) as usize;

        // charset (32 bytes at 40, NUL-terminated)
        let mut charset_bytes = [0u8; 32];
        image.read_exact_at(40, &mut charset_bytes);
        let charset_end = charset_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(charset_bytes.len());
        let charset = String::from_utf8_lossy(&charset_bytes[..charset_end]).to_string();

        let expected_total = HEADER_SIZE + da_size + token_size + feature_size;
        if (len as usize) < expected_total {
            return Err(Error::CorruptedDictionary(format!(
                "Dictionary file truncated: expected {expected_total} bytes, got {len}"
            )));
        }

        let trie_offset = HEADER_SIZE as u64;
        let trie = DoubleArrayTrie::from_image(
            std::sync::Arc::clone(&image),
            trie_offset,
            da_size,
        )?;
        let token_offset = (HEADER_SIZE + da_size) as u64;
        let tokens_count = token_size / Token::SIZE;
        let feature_offset = (HEADER_SIZE + da_size + token_size) as u64;

        Ok(Self {
            image,
            trie_offset,
            trie,
            token_offset,
            tokens_count,
            feature_offset,
            feature_size,
            version,
            dict_type,
            lexicon_size,
            left_size,
            right_size,
            charset,
        })
    }

    /// Perform common prefix search and return dictionary entries
    pub fn common_prefix_search(&self, key: &str) -> Vec<DictionaryEntry> {
        let key_bytes = key.as_bytes();
        let mut results = [DartsResult::default(); MAX_RESULTS];
        let num_results = self.trie.common_prefix_search(key_bytes, &mut results);

        let mut entries = Vec::new();
        for result in results.iter().take(num_results) {
            let value = result.value as u32;
            let token_start = (value >> 8) as usize;
            let token_count = (value & 0xff) as usize;
            for i in 0..token_count {
                let token_idx = token_start + i;
                if let Some(token) = self.get_token(token_idx) {
                    let feature = self.get_feature(&token);
                    entries.push(DictionaryEntry {
                        length: result.length,
                        word_id: token_idx as u32,
                        left_id: token.left_id,
                        right_id: token.right_id,
                        pos_id: token.pos_id,
                        wcost: token.wcost,
                        feature,
                    });
                }
            }
        }
        entries
    }

    /// Read a token by index: one 16-byte `read_exact_at` (upstream: pointer add).
    #[inline]
    pub fn get_token(&self, index: usize) -> Option<Token> {
        if index >= self.tokens_count {
            return None;
        }
        let mut buf = [0u8; Token::SIZE];
        self.image
            .read_exact_at(self.token_offset + (index * Token::SIZE) as u64, &mut buf);
        Some(Token {
            left_id: LittleEndian::read_u16(&buf[0..2]),
            right_id: LittleEndian::read_u16(&buf[2..4]),
            pos_id: LittleEndian::read_u16(&buf[4..6]),
            wcost: LittleEndian::read_i16(&buf[6..8]),
            feature_offset: LittleEndian::read_u32(&buf[8..12]),
            compound: LittleEndian::read_u32(&buf[12..16]),
        })
    }

    /// Feature string for a token (owned — see module doc, refactor point 2). The NUL
    /// scan is done in 256-byte chunked reads.
    pub fn get_feature(&self, token: &Token) -> String {
        let offset = token.feature_offset as usize;
        if offset >= self.feature_size {
            return String::new();
        }
        let mut out = Vec::with_capacity(64);
        let mut pos = self.feature_offset + offset as u64;
        let mut chunk = [0u8; 256];
        loop {
            let remaining = self.feature_size - offset - out.len();
            let want = remaining.min(chunk.len());
            if want == 0 {
                break;
            }
            self.image.read_exact_at(pos, &mut chunk[..want]);
            if let Some(nul) = chunk[..want].iter().position(|&b| b == 0) {
                out.extend_from_slice(&chunk[..nul]);
                break;
            }
            out.extend_from_slice(&chunk[..want]);
            pos += want as u64;
        }
        String::from_utf8(out).unwrap_or_default()
    }

    pub fn charset(&self) -> &str {
        &self.charset
    }

    pub fn lexicon_size(&self) -> usize {
        self.lexicon_size as usize
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn token_count(&self) -> usize {
        self.tokens_count
    }

    pub fn token_at(&self, index: usize) -> Option<Token> {
        self.get_token(index)
    }

    pub fn dict_type(&self) -> u32 {
        self.dict_type
    }

    pub fn left_size(&self) -> usize {
        self.left_size as usize
    }

    pub fn right_size(&self) -> usize {
        self.right_size as usize
    }

    pub fn trie_size(&self) -> usize {
        self.trie.size()
    }

    /// Byte offset of the trie (exposed for the access-statistics accounting).
    pub fn trie_offset(&self) -> u64 {
        self.trie_offset
    }
}