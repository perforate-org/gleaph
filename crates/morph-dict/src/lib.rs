//! morph-dict — MeCab-format morphological dictionary engine over byte images.
//!
//! Morphological analysis (tokenization + Viterbi) over MeCab-format dictionaries that
//! are READ as byte images through offset accessors instead of being materialized into
//! heap structures (the MeCab 2001 mmap design; carried into pure Rust from MeCrab
//! github.com/cool-japan/mecrab @ 85444b5, MIT OR Apache-2.0 — see LICENSE-MECRAB and
//! the per-file copyright headers; mecab-ipadic BSD acknowledgment in LICENSE-IPADIC).
//!
//! The dictionary parameter is the [`byteimage::ByteImage`] trait (`len` +
//! `read_exact_at`), so the same accessors run over a heap copy ([`byteimage::HeapImage`]),
//! a resident-prefix/lazy-suffix split ([`byteimage::SplitImage`]), an offset view
//! ([`byteimage::OffsetImage`]), or a stable-memory-backed image from the companion
//! `ic-morph-dict` adapter. Nothing in this crate assumes IC — it is std-only.
//!
//! # Container format (`MPD`)
//!
//! A container bundles the four MeCab-format images (sys.dic, unk.dic, matrix.bin,
//! char.bin) into ONE addressable byte string so a single stable-memory region can back
//! the whole dictionary and open-time validation is structural. The magic identifies the
//! format FAMILY (stable across revisions); the layout version byte identifies the layout
//! revision — readers fail closed on an unknown version (the fix is "rebuild the container
//! and re-upload": dictionaries are re-uploadable, no migration exists or is planned).
//!
//! ```text
//! offset  size  field
//! 0       3     magic b"MPD"
//! 3       1     layout_version: u8 (current: 1)
//! 4       4     entry_count: u32 LE
//! 8       ...   entry table, entry_count times:
//!                 1    name_len: u8
//!                 n    name: UTF-8 bytes
//!                 8    offset: u64 LE (from container start)
//!                 8    len: u64 LE
//!                 32   sha256: image digest (structural integrity metadata)
//! ...     ...   concatenated image bytes
//! ```
//!
//! Digests: the per-entry SHA-256 values are recorded at build time for integrity
//! attestation; open-time validation checks magic/table/offset/len structure only (a
//! full-image rehash at open would defeat the rebind-collapses-to-validation goal; the
//! container-level digest lives with the index meta, computed at finalize).
//!
//! # DictionaryProfile
//!
//! Unit-emission rules (which feature column is the lemma, which categories are dropped)
//! are parameterized by [`profile::DictionaryProfile`]; the shipped instance is the
//! Japanese ipadic profile (品詞1 column 0 drop set, 基本形 column 6 lemma). Korean
//! (mecab-ko-dic) and Chinese (jieba-converted) dictionaries plug in with a new profile
//! and NO structural changes.

pub mod analyzer;
pub mod byteimage;
pub mod container;
pub mod dict;
pub mod error;
pub mod lattice;
pub mod profile;
pub mod viterbi;

#[allow(dead_code)]
pub mod normalize;

pub use analyzer::Analyzer;
pub use byteimage::{AccessStats, ByteImage, HeapImage, OffsetImage, SplitImage};
pub use container::{ContainerEntry, MAGIC};
pub use error::{Error, Result};
pub use profile::DictionaryProfile;
