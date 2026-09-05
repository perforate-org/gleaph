//! Unknown word dictionary handling (unk.dic)
//!
//! Copyright 2026 COOLJAPAN OU (Team KitaSan)
//! Vendored (plan 0333) from github.com/cool-japan/mecrab @ 85444b5
//! (mecrab/src/dict/unknown.rs), MIT OR Apache-2.0.
//!
//! Plan 0333 refactor: `from_mmap(Arc<Mmap>)` → `from_image(Arc<dyn ByteImage>)`
//! (delegates to the refactored `SysDic`).

use crate::mecrab_vendor::byteimage::ByteImage;
use crate::mecrab_vendor::dict::sys_dic::SysDic;
use crate::mecrab_vendor::dict::DictionaryEntry;
use crate::mecrab_vendor::error::Result;
use std::sync::Arc;

/// Unknown word dictionary (same binary format as sys.dic, entries keyed by
/// character-category name)
pub struct UnknownDictionary {
    inner: SysDic,
}

impl std::fmt::Debug for UnknownDictionary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnknownDictionary")
            .field("inner", &self.inner)
            .finish()
    }
}

impl UnknownDictionary {
    /// Load unknown word dictionary from a byte image
    pub fn from_image(image: Arc<dyn ByteImage>) -> Result<Self> {
        let inner = SysDic::from_image(image)?;

        // Verify this is actually an unknown dictionary (type = 2)
        if inner.dict_type() != super::MECAB_UNK_DIC {
            return Err(crate::mecrab_vendor::error::Error::InvalidDictionaryFormat(
                format!("Expected unknown dictionary (type=2), got type={}", inner.dict_type()),
            ));
        }

        Ok(Self { inner })
    }

    /// Look up entries for a category name (e.g. "DEFAULT", "HIRAGANA")
    pub fn lookup(&self, category_name: &str) -> Vec<DictionaryEntry> {
        self.inner.common_prefix_search(category_name)
    }

    /// Get all tokens for a category (exact match)
    pub fn get_entries_for_category(&self, category_name: &str) -> Vec<DictionaryEntry> {
        self.inner
            .common_prefix_search(category_name)
            .into_iter()
            .filter(|e| e.length == category_name.len())
            .collect()
    }

    pub fn charset(&self) -> &str {
        self.inner.charset()
    }

    /// Generate entries for unknown words based on category + surface length
    pub fn generate_entries(
        &self,
        category: super::CharCategory,
        length: usize,
    ) -> Vec<DictionaryEntry> {
        let category_name = match category {
            super::CharCategory::Default => "DEFAULT",
            super::CharCategory::Space => "SPACE",
            super::CharCategory::Kanji => "KANJI",
            super::CharCategory::Symbol => "SYMBOL",
            super::CharCategory::Numeric => "NUMERIC",
            super::CharCategory::Alpha => "ALPHA",
            super::CharCategory::Hiragana => "HIRAGANA",
            super::CharCategory::Katakana => "KATAKANA",
            super::CharCategory::Kanjinumeric => "KANJINUMERIC",
            super::CharCategory::Greek => "GREEK",
            super::CharCategory::Cyrillic => "CYRILLIC",
        };

        self.get_entries_for_category(category_name)
            .into_iter()
            .map(|mut e| {
                e.length = length;
                e
            })
            .collect()
    }
}