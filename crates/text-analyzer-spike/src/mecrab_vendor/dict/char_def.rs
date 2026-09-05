//! Character category definitions for unknown word handling
//!
//! Copyright 2026 COOLJAPAN OU (Team KitaSan)
//! Vendored (plan 0333) from github.com/cool-japan/mecrab @ 85444b5
//! (mecrab/src/dict/char_def.rs), MIT OR Apache-2.0.
//!
//! Plan 0333 refactor: `map_ptr: *const CharInfo` over the mmap slice replaced by
//! `Arc<dyn ByteImage>`; every char lookup is a 4-byte `read_exact_at`.
//!
//! Binary format (char.bin): u32 csize; csize * 32-byte category names;
//! 0xffff * 4-byte packed CharInfo indexed by UCS-2 code point.

use crate::mecrab_vendor::byteimage::ByteImage;
use crate::mecrab_vendor::error::{Error, Result};
use byteorder::{ByteOrder, LittleEndian};
use std::sync::Arc;

/// Character category names (matching IPADIC)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum CharCategory {
    #[default]
    Default = 0,
    Space = 1,
    Kanji = 2,
    Symbol = 3,
    Numeric = 4,
    Alpha = 5,
    Hiragana = 6,
    Katakana = 7,
    Kanjinumeric = 8,
    Greek = 9,
    Cyrillic = 10,
}

impl From<u8> for CharCategory {
    fn from(value: u8) -> Self {
        match value {
            1 => Self::Space,
            2 => Self::Kanji,
            3 => Self::Symbol,
            4 => Self::Numeric,
            5 => Self::Alpha,
            6 => Self::Hiragana,
            7 => Self::Katakana,
            8 => Self::Kanjinumeric,
            9 => Self::Greek,
            10 => Self::Cyrillic,
            _ => Self::Default,
        }
    }
}

/// Character information packed in 4 bytes
/// Bit layout: type 18 | default_type 8 | length 4 | group 1 | invoke 1
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct CharInfo {
    packed: u32,
}

impl CharInfo {
    pub const SIZE: usize = 4;

    #[inline]
    pub fn type_mask(&self) -> u32 {
        self.packed & 0x3FFFF
    }

    #[inline]
    pub fn default_type(&self) -> u8 {
        ((self.packed >> 18) & 0xFF) as u8
    }

    #[inline]
    pub fn length(&self) -> u8 {
        ((self.packed >> 26) & 0xF) as u8
    }

    #[inline]
    pub fn group(&self) -> bool {
        ((self.packed >> 30) & 1) != 0
    }

    #[inline]
    pub fn invoke(&self) -> bool {
        ((self.packed >> 31) & 1) != 0
    }

    #[inline]
    pub fn category(&self) -> CharCategory {
        CharCategory::from(self.default_type())
    }

    #[inline]
    pub fn is_kind_of(&self, other: CharInfo) -> bool {
        (self.type_mask() & other.type_mask()) != 0
    }
}

/// Character definition table
pub struct CharDef {
    image: Arc<dyn ByteImage>,
    /// Offset of the CharInfo table (4 + csize * 32)
    map_offset: u64,
    categories: Vec<String>,
}

// The table offsets are plain integers; the image is an immutable shared parameter.
unsafe impl Send for CharDef {}
unsafe impl Sync for CharDef {}

impl std::fmt::Debug for CharDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CharDef")
            .field("categories", &self.categories)
            .finish()
    }
}

impl CharDef {
    pub const TABLE_SIZE: usize = 0xFFFF;

    /// Load character definitions from a byte image (upstream: `from_mmap(Arc<Mmap>)`).
    pub fn from_image(image: Arc<dyn ByteImage>) -> Result<Self> {
        let len = image.len();
        if len < 4 {
            return Err(Error::CharDefError(
                "Character definition file too small".to_string(),
            ));
        }

        let mut b4 = [0u8; 4];
        image.read_exact_at(0, &mut b4);
        let csize = LittleEndian::read_u32(&b4) as usize;

        let expected_size = 4 + (csize * 32) + (Self::TABLE_SIZE * CharInfo::SIZE);
        if len as usize != expected_size {
            return Err(Error::CharDefError(format!(
                "Character definition file size mismatch: expected {expected_size}, got {len}"
            )));
        }

        let mut categories = Vec::with_capacity(csize);
        for i in 0..csize {
            let mut name_bytes = [0u8; 32];
            image.read_exact_at((4 + i * 32) as u64, &mut name_bytes);
            let name_end = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_bytes.len());
            categories.push(String::from_utf8_lossy(&name_bytes[..name_end]).to_string());
        }

        Ok(Self {
            map_offset: (4 + csize * 32) as u64,
            categories,
            image,
        })
    }

    /// Get CharInfo for a Unicode code point (one 4-byte read)
    #[inline]
    pub fn get_char_info(&self, c: char) -> CharInfo {
        let code = c as u32;
        if code < Self::TABLE_SIZE as u32 {
            let mut b = [0u8; 4];
            self.image
                .read_exact_at(self.map_offset + (code as usize * CharInfo::SIZE) as u64, &mut b);
            CharInfo {
                packed: LittleEndian::read_u32(&b),
            }
        } else {
            CharInfo::default()
        }
    }

    /// Get CharInfo for a byte sequence (handles UTF-8)
    pub fn get_char_info_from_bytes(&self, bytes: &[u8]) -> (CharInfo, usize) {
        if bytes.is_empty() {
            return (CharInfo::default(), 0);
        }
        let s = match std::str::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return (CharInfo::default(), 1),
        };
        if let Some(c) = s.chars().next() {
            let len = c.len_utf8();
            (self.get_char_info(c), len)
        } else {
            (CharInfo::default(), 0)
        }
    }

    pub fn category_name(&self, id: usize) -> Option<&str> {
        self.categories.get(id).map(String::as_str)
    }

    pub fn category_count(&self) -> usize {
        self.categories.len()
    }

    pub fn category_id(&self, name: &str) -> Option<usize> {
        self.categories.iter().position(|n| n == name)
    }

    pub fn should_group(&self, category: CharCategory) -> bool {
        let sample_char = match category {
            CharCategory::Default => ' ',
            CharCategory::Space => ' ',
            CharCategory::Kanji => '漢',
            CharCategory::Symbol => '!',
            CharCategory::Numeric => '0',
            CharCategory::Alpha => 'A',
            CharCategory::Hiragana => 'あ',
            CharCategory::Katakana => 'ア',
            CharCategory::Kanjinumeric => '一',
            CharCategory::Greek => 'Α',
            CharCategory::Cyrillic => 'А',
        };
        self.get_char_info(sample_char).group()
    }
}