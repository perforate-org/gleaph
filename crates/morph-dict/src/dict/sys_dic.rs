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

use crate::byteimage::ByteImage;
use crate::dict::double_array_trie::DoubleArrayTrie;
use crate::dict::DictionaryEntry;
use crate::error::{Error, Result};
use byteorder::{ByteOrder, LittleEndian};
use std::sync::Arc;

/// Magic number for system dictionary validation (MeCab: 0xef718f77)
pub const DICTIONARY_MAGIC_ID: u32 = 0xef71_8f77;

/// Dictionary version (DIC_VERSION from MeCab)
pub const DIC_VERSION: u32 = 102;

/// Header size: 10 * u32 (40 bytes) + 32 bytes charset = 72 bytes
pub const HEADER_SIZE: usize = 72;

/// Maximum number of results from common prefix search

/// Feature-free lookup result: the lattice hot path never touches the feature region
/// (residency lever — features are read only for the Viterbi-optimal path at emission).
#[derive(Debug, Clone, Copy)]
pub struct DictEntryLite {
    pub length: usize,
    pub word_id: u32,
    pub left_id: u16,
    pub right_id: u16,
    pub pos_id: u16,
    pub wcost: i16,
}

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
    /// Byte image covering the WHOLE sys.dic (header/trie/token reads go through it;
    /// feature reads go through `feature_image` when the residency split is active).
    image: Arc<dyn ByteImage>,
    /// Lazy feature-region image with RELATIVE offsets (covers [feature_offset, len));
    /// `None` = whole-image access (feature reads go through `image`).
    feature_image: Option<Arc<dyn ByteImage>>,
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
            feature_image: None,
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

    /// Feature-free common prefix search (lattice hot path).
    pub fn common_prefix_search_lite(&self, key: &str) -> Vec<DictEntryLite> {
        let mut entries = Vec::new();
        self.common_prefix_search_lite_into(key, &mut entries);
        entries
    }

    /// Buffer-reusing variant (the lattice calls this per position with a scratch
    /// buffer — zero allocations in the steady state).
    pub fn common_prefix_search_lite_into(&self, key: &str, out: &mut Vec<DictEntryLite>) {
        out.clear();
        let key_bytes = key.as_bytes();
        self.trie.for_each_result(key_bytes, |result| {
            let value = result.value as u32;
            let token_start = (value >> 8) as usize;
            let token_count = (value & 0xff) as usize;
            for i in 0..token_count {
                let token_idx = token_start + i;
                if let Some(token) = self.get_token(token_idx) {
                    out.push(DictEntryLite {
                        length: result.length,
                        word_id: token_idx as u32,
                        left_id: token.left_id,
                        right_id: token.right_id,
                        pos_id: token.pos_id,
                        wcost: token.wcost,
                    });
                }
            }
        });
    }

    /// Perform common prefix search and return dictionary entries (with features).
    pub fn common_prefix_search(&self, key: &str) -> Vec<DictionaryEntry> {
        let key_bytes = key.as_bytes();
        let mut entries = Vec::new();
        self.trie.for_each_result(key_bytes, |result| {
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
        });
        entries
    }

    /// Read a token by index: one 16-byte `read_exact_at` (upstream: pointer add).
    #[inline]
    pub fn get_token(&self, index: usize) -> Option<Token> {
        if index >= self.tokens_count {
            return None;
        }
        let abs = self.token_offset + (index * Token::SIZE) as u64;
        if let Some(slice) = self.image.as_contiguous() {
            let off = abs as usize;
            return Some(Token {
                left_id: LittleEndian::read_u16(&slice[off..off + 2]),
                right_id: LittleEndian::read_u16(&slice[off + 2..off + 4]),
                pos_id: LittleEndian::read_u16(&slice[off + 4..off + 6]),
                wcost: LittleEndian::read_i16(&slice[off + 6..off + 8]),
                feature_offset: LittleEndian::read_u32(&slice[off + 8..off + 12]),
                compound: LittleEndian::read_u32(&slice[off + 12..off + 16]),
            });
        }
        let mut buf = [0u8; Token::SIZE];
        self.image.read_exact_at(abs, &mut buf);
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
        // Split residency: feature reads go to the lazy region image with RELATIVE
        // offsets (feature_offset is subtracted); whole-image mode reads absolutely
        // (with a contiguous fast path when the image is heap-resident).
        let mut pos = match &self.feature_image {
            Some(_) => offset as u64,
            None => self.feature_offset + offset as u64,
        };
        let image: &dyn ByteImage = match &self.feature_image {
            Some(img) => img.as_ref(),
            None => self.image.as_ref(),
        };
        // Contiguous fast path (whole-image mode over a heap copy): scan the NUL
        // terminator directly instead of chunked reads.
        if self.feature_image.is_none() {
            if let Some(slice) = image.as_contiguous() {
                let start = pos as usize;
                let end = (self.feature_offset as usize + self.feature_size).min(slice.len());
                if let Some(nul) = slice[start..end].iter().position(|&b| b == 0) {
                    return String::from_utf8_lossy(&slice[start..start + nul]).into_owned();
                }
            }
        }
        let mut chunk = [0u8; 256];
        loop {
            let remaining = self.feature_size - offset - out.len();
            let want = remaining.min(chunk.len());
            if want == 0 {
                break;
            }
            image.read_exact_at(pos, &mut chunk[..want]);
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

    /// Residency-split constructor: `resident` covers sys.dic `[0, feature_offset)`
    /// (header + trie + word-params, heap-resident), `feature_image` covers
    /// `[feature_offset, len)` with RELATIVE offsets (lazy). All header/trie/token
    /// reads go through `resident`; feature reads through `feature_image`.
    pub fn from_parts(
        resident: Arc<dyn ByteImage>,
        feature_image: Arc<dyn ByteImage>,
        feature_size: usize,
    ) -> Result<Self> {
        // Header parse against the resident prefix (magic validated against the FULL
        // size = feature_offset + feature_size, reconstructed after reading sizes).
        let full = Self::header_sizes(resident.as_ref())?;
        let feature_offset = full.0;
        let da_size = full.1;
        if feature_size != full.2 {
            return Err(Error::InvalidDictionaryFormat(
                "feature region size does not match the sys.dic header".to_string(),
            ));
        }
        let tokens_count = {
            let mut b = [0u8; 4];
            resident.read_exact_at(28, &mut b);
            LittleEndian::read_u32(&b) as usize / Token::SIZE
        };
        let trie = DoubleArrayTrie::from_image(
            std::sync::Arc::clone(&resident),
            HEADER_SIZE as u64,
            da_size,
        )?;
        let token_offset = (HEADER_SIZE + da_size) as u64;
        let mut charset_bytes = [0u8; 32];
        resident.read_exact_at(40, &mut charset_bytes);
        let charset_end = charset_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(charset_bytes.len());
        let charset = String::from_utf8_lossy(&charset_bytes[..charset_end]).to_string();
        let version = {
            let mut b = [0u8; 4];
            resident.read_exact_at(4, &mut b);
            LittleEndian::read_u32(&b)
        };
        let dict_type = {
            let mut b = [0u8; 4];
            resident.read_exact_at(8, &mut b);
            LittleEndian::read_u32(&b)
        };
        let lexicon_size = {
            let mut b = [0u8; 4];
            resident.read_exact_at(12, &mut b);
            LittleEndian::read_u32(&b)
        };
        let left_size = {
            let mut b = [0u8; 4];
            resident.read_exact_at(16, &mut b);
            LittleEndian::read_u32(&b)
        };
        let right_size = {
            let mut b = [0u8; 4];
            resident.read_exact_at(20, &mut b);
            LittleEndian::read_u32(&b)
        };
        if version != DIC_VERSION {
            return Err(Error::InvalidDictionaryFormat(format!(
                "Incompatible dictionary version: expected {DIC_VERSION}, got {version}"
            )));
        }
        let feature_offset_abs = {
            // absolute feature offset inside the WHOLE sys.dic = feature_offset (the
            // resident prefix is exactly [0, feature_offset))
            feature_offset
        };
        Ok(Self {
            image: std::sync::Arc::clone(&resident),
            feature_image: Some(feature_image),
            trie_offset: HEADER_SIZE as u64,
            trie,
            token_offset,
            tokens_count,
            feature_offset: feature_offset_abs,
            feature_size,
            version,
            dict_type,
            lexicon_size,
            left_size,
            right_size,
            charset,
        })
    }

    /// Byte offset of the trie (exposed for the access-statistics accounting).
    pub fn trie_offset(&self) -> u64 {
        self.trie_offset
    }

    /// Byte offset of the feature-string region (end of header+trie+word-params): the
    /// residency split boundary for [`crate::byteimage::SplitImage`].
    pub fn feature_offset(&self) -> u64 {
        self.feature_offset
    }

    /// Header-only parse returning (feature_offset, da_size, feature_size). Magic is
    /// validated against feature_offset + feature_size (the resident-prefix caller
    /// knows only the parts, not the whole-image length).
    fn header_sizes(
        image: &dyn crate::byteimage::ByteImage,
    ) -> crate::error::Result<(u64, usize, usize)> {
        let len = image.len();
        if len < HEADER_SIZE as u64 {
            return Err(Error::CorruptedDictionary(format!(
                "Dictionary file too small: {len} bytes (minimum {HEADER_SIZE} bytes)"
            )));
        }
        let da_size = read_u32_at(image, 24) as usize;
        let token_size = read_u32_at(image, 28) as usize;
        let feature_size = read_u32_at(image, 32) as usize;
        let feature_offset = (HEADER_SIZE + da_size + token_size) as u64;
        let magic = read_u32_at(image, 0);
        if magic ^ DICTIONARY_MAGIC_ID != (feature_offset as usize + feature_size) as u32 {
            return Err(Error::InvalidDictionaryFormat(
                "magic/size mismatch in dictionary header".to_string(),
            ));
        }
        if read_u32_at(image, 4) != DIC_VERSION {
            return Err(Error::InvalidDictionaryFormat(
                "incompatible dictionary version".to_string(),
            ));
        }
        Ok((feature_offset, da_size, feature_size))
    }

    /// Header-only parse of the feature-region offset (validates magic + version +
    /// section sizes; used to size the resident prefix BEFORE building the full
    /// structure, e.g. directly over a container-backed image). `image` covers the
    /// WHOLE dictionary; for a resident-prefix image use
    /// [`SysDic::header_feature_offset_for`] with the full dictionary size.
    pub fn header_feature_offset(image: &dyn crate::byteimage::ByteImage) -> crate::error::Result<u64> {
        let len = image.len();
        Self::header_feature_offset_for(image, len)
    }

    /// Header-only parse against a KNOWN full dictionary size (the resident-prefix
    /// image is shorter than the encoded magic size).
    pub fn header_feature_offset_for(
        image: &dyn crate::byteimage::ByteImage,
        full_len: u64,
    ) -> crate::error::Result<u64> {
        let len = image.len();
        if len < HEADER_SIZE as u64 {
            return Err(Error::CorruptedDictionary(format!(
                "Dictionary file too small: {len} bytes (minimum {HEADER_SIZE} bytes)"
            )));
        }
        let magic = read_u32_at(image, 0);
        if magic ^ DICTIONARY_MAGIC_ID != full_len as u32 {
            return Err(Error::InvalidDictionaryFormat(
                "magic/size mismatch in dictionary header".to_string(),
            ));
        }
        if read_u32_at(image, 4) != DIC_VERSION {
            return Err(Error::InvalidDictionaryFormat(
                "incompatible dictionary version".to_string(),
            ));
        }
        let da_size = read_u32_at(image, 24) as usize;
        let token_size = read_u32_at(image, 28) as usize;
        let feature_size = read_u32_at(image, 32) as usize;
        let total = HEADER_SIZE + da_size + token_size + feature_size;
        if (full_len as usize) < total || (len as usize) < HEADER_SIZE + da_size + token_size {
            return Err(Error::CorruptedDictionary(format!(
                "Dictionary file truncated: expected {total} bytes, got {len}"
            )));
        }
        Ok((HEADER_SIZE + da_size + token_size) as u64)
    }
}