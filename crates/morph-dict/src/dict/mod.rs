//! Dictionary module for MeCrab
//!
//! Copyright 2026 COOLJAPAN OU (Team KitaSan)
//! Vendored (plan 0333) from github.com/cool-japan/mecrab @ 85444b5
//! (mecrab/src/dict/mod.rs), MIT OR Apache-2.0.
//!
//! Plan 0333 refactor: the four MeCab-format images (sys.dic, unk.dic, matrix.bin,
//! char.bin) are parameterized as `Arc<dyn ByteImage>` instead of `Arc<Mmap>`;
//! `user_dict` and `semantic_pool` are EXCLUDED from the vendored runtime path
//! (recorded as deletions). The native loader reads files into `HeapImage` with
//! plain `std::fs::read` — no memmap2 anywhere in the vendor tree.

mod char_def;
mod connection_matrix;
pub mod double_array_trie;
mod feature;
pub mod sys_dic;
mod unknown;

pub use char_def::{CharCategory, CharDef, CharInfo};
pub use connection_matrix::ConnectionMatrix;
pub use feature::FeatureTable;
pub use sys_dic::{DictEntryLite, SysDic, Token};
pub use unknown::UnknownDictionary;

use crate::byteimage::{ByteImage, HeapImage};
use crate::error::{Error, Result};
use std::path::Path;
use std::sync::Arc;

/// Dictionary file names (MeCab/IPADIC format)
pub const SYS_DIC_FILE: &str = "sys.dic";
pub const UNK_DIC_FILE: &str = "unk.dic";
pub const MATRIX_FILE: &str = "matrix.bin";
pub const CHAR_BIN_FILE: &str = "char.bin";

pub const MECAB_SYS_DIC: u32 = 0;
pub const MECAB_USR_DIC: u32 = 1;
pub const MECAB_UNK_DIC: u32 = 2;

/// The main dictionary structure containing all loaded dictionary data
pub struct Dictionary {
    pub sys_dic: SysDic,
    pub unknown: UnknownDictionary,
    pub matrix: ConnectionMatrix,
    pub char_def: CharDef,
}

impl std::fmt::Debug for Dictionary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dictionary")
            .field("sys_dic", &self.sys_dic)
            .field("matrix", &self.matrix)
            .field("char_def", &self.char_def)
            .finish()
    }
}

impl Dictionary {
    /// Assemble with the standard residency split: sys.dic already built as
    /// resident-prefix + lazy-feature (`SysDic::from_parts`), the other three images
    /// fully resident.
    pub fn from_parts(
        sys_dic: crate::dict::sys_dic::SysDic,
        unk: Arc<dyn ByteImage>,
        matrix: Arc<dyn ByteImage>,
        char_bin: Arc<dyn ByteImage>,
    ) -> Result<Self> {
        let unknown = UnknownDictionary::from_image(unk)?;
        let matrix = ConnectionMatrix::from_image(matrix)?;
        let char_def = CharDef::from_image(char_bin)?;
        Ok(Self {
            sys_dic,
            unknown,
            matrix,
            char_def,
        })
    }

    /// Load the four images from a directory (native only; `std::fs::read` into
    /// `HeapImage` — no mmap).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(Error::DictionaryNotFound(path.to_path_buf()));
        }
        let read = |name: &str| -> Result<Vec<u8>> {
            std::fs::read(path.join(name))
                .map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("{name}: {e}"))))
        };
        Self::from_images(
            Arc::new(HeapImage::from_vec(read(SYS_DIC_FILE)?)),
            Arc::new(HeapImage::from_vec(read(UNK_DIC_FILE)?)),
            Arc::new(HeapImage::from_vec(read(MATRIX_FILE)?)),
            Arc::new(HeapImage::from_vec(read(CHAR_BIN_FILE)?)),
        )
    }

    /// Assemble a dictionary from four in-memory images (each image arrives from
    /// stable memory behind a `ByteImage` impl). sys.dic is taken WHOLE (residency
    /// decided by the caller); prefer [`Dictionary::from_parts`] for the standard
    /// resident-prefix/lazy-feature split.
    #[allow(dead_code)]
    pub fn from_images(
        sys: Arc<dyn ByteImage>,
        unk: Arc<dyn ByteImage>,
        matrix: Arc<dyn ByteImage>,
        char_bin: Arc<dyn ByteImage>,
    ) -> Result<Self> {
        let sys_dic = SysDic::from_image(sys)?;
        let unknown = UnknownDictionary::from_image(unk)?;
        let matrix = ConnectionMatrix::from_image(matrix)?;
        let char_def = CharDef::from_image(char_bin)?;
        Ok(Self {
            sys_dic,
            unknown,
            matrix,
            char_def,
        })
    }

    /// Common prefix lookup over the system dictionary (with features).
    pub fn lookup(&self, key: &str) -> Vec<DictionaryEntry> {
        self.sys_dic.common_prefix_search(key)
    }

    /// Feature-free common prefix lookup (lattice hot path).
    pub fn lookup_lite(&self, key: &str) -> Vec<DictEntryLite> {
        self.sys_dic.common_prefix_search_lite(key)
    }

    /// Buffer-reusing variant of [`Dictionary::lookup_lite`].
    pub fn lookup_lite_into(&self, key: &str, out: &mut Vec<DictEntryLite>) {
        self.sys_dic.common_prefix_search_lite_into(key, out)
    }

    #[inline]
    pub fn connection_cost(&self, right_id: u16, left_id: u16) -> i16 {
        self.matrix.cost(right_id, left_id)
    }

    pub fn get_feature(&self, token: &Token) -> String {
        self.sys_dic.get_feature(token)
    }

    pub fn char_info(&self, c: char) -> CharInfo {
        self.char_def.get_char_info(c)
    }

    pub fn char_category(&self, c: char) -> CharCategory {
        self.char_def.get_char_info(c).category()
    }

    pub fn charset(&self) -> &str {
        self.sys_dic.charset()
    }

    pub fn size(&self) -> usize {
        self.sys_dic.lexicon_size()
    }
}

/// A dictionary entry returned from lookup
#[derive(Debug, Clone)]
pub struct DictionaryEntry {
    pub length: usize,
    pub word_id: u32,
    pub left_id: u16,
    pub right_id: u16,
    pub pos_id: u16,
    pub wcost: i16,
    pub feature: String,
}
