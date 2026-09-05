//! MORPHDICT1 container format (see crate docs for the byte layout).
//!
//! Structural validation only at open: magic + entry table + offset/len bounds. The
//! per-entry SHA-256 values are build-time integrity metadata (recorded by the builder,
//! re-attestable offline); open does NOT rehash the images (a full 53 MB hash per
//! rebind would defeat the collapse-to-validation goal). The container-level digest is
//! one xxh3_128 over the container bytes, owned by the index meta (computed at finalize).

use crate::byteimage::ByteImage;
use crate::error::{Error, Result};
use sha2::{Digest, Sha256};

/// Container magic: `MORPHDICT1`.
pub const MAGIC: &[u8; 10] = b"MORPHDICT1";

/// One container entry: name, byte range, and the build-time SHA-256 of the image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerEntry {
    pub name: String,
    pub offset: u64,
    pub len: u64,
    pub sha256: [u8; 32],
}

/// Maximum table size guard (fail-closed runaway bound; 64 MiB container cap ≈ 64 MiB /
/// 8-byte images ⇒ a 4 KiB table is far beyond any real entry count).
const MAX_ENTRIES: u32 = 4096;

/// Parses + structurally validates a container. Returns the entry table. Fails closed on
/// any structural violation (bad magic, truncated table, entry range outside the image,
/// entry overlap).
pub fn validate(image: &dyn ByteImage) -> Result<Vec<ContainerEntry>> {
    let len = image.len();
    if len < 14 {
        return Err(Error::InvalidDictionaryFormat(format!(
            "MORPHDICT1 container too small: {len} bytes"
        )));
    }
    let mut hdr = [0u8; 14];
    image.read_exact_at(0, &mut hdr);
    if &hdr[0..10] != MAGIC {
        return Err(Error::InvalidDictionaryFormat(
            "container magic is not MORPHDICT1".to_string(),
        ));
    }
    let count = u32::from_le_bytes(hdr[10..14].try_into().expect("fixed width"));
    if count > MAX_ENTRIES {
        return Err(Error::InvalidDictionaryFormat(format!(
            "container entry count {count} exceeds the structural bound"
        )));
    }

    let mut pos = 14u64;
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut nl = [0u8; 1];
        image.read_exact_at(pos, &mut nl);
        pos += 1;
        let name_len = nl[0] as u64;
        if pos + name_len + 48 > len {
            return Err(Error::InvalidDictionaryFormat(
                "container entry table truncated".to_string(),
            ));
        }
        let mut name_buf = vec![0u8; name_len as usize];
        image.read_exact_at(pos, &mut name_buf);
        pos += name_len;
        let mut rest = [0u8; 48];
        image.read_exact_at(pos, &mut rest);
        pos += 48;
        let name = String::from_utf8(name_buf)
            .map_err(|_| Error::InvalidDictionaryFormat("entry name not UTF-8".to_string()))?;
        let offset = u64::from_le_bytes(rest[0..8].try_into().expect("fixed width"));
        let ilen = u64::from_le_bytes(rest[8..16].try_into().expect("fixed width"));
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&rest[16..48]);
        if offset > len || ilen > len.saturating_sub(offset) {
            return Err(Error::InvalidDictionaryFormat(format!(
                "entry {name:?} range {offset}..{} exceeds the container length {len}",
                offset + ilen
            )));
        }
        entries.push(ContainerEntry {
            name,
            offset,
            len: ilen,
            sha256: sha,
        });
    }

    // Entry ranges must not overlap (structural integrity).
    let mut sorted: Vec<&ContainerEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| e.offset);
    for w in sorted.windows(2) {
        if w[0].offset + w[0].len > w[1].offset {
            return Err(Error::InvalidDictionaryFormat(format!(
                "container entries {:?} and {:?} overlap",
                w[0].name, w[1].name
            )));
        }
    }
    Ok(entries)
}

/// Looks up one entry by name (fail-closed if absent).
pub fn entry<'e>(entries: &'e [ContainerEntry], name: &str) -> Result<&'e ContainerEntry> {
    entries
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| {
            Error::InvalidDictionaryFormat(format!(
                "container entry {name:?} missing (have: {:?})",
                entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>()
            ))
        })
}

/// Builds a MORPHDICT1 container from named images (order preserved). Computes the
/// per-entry SHA-256 at build time.
pub fn build(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    // Table first: offsets need the image section start, which depends on table size —
    // compute it up front (name_len is bounded to u8, so sizes are stable).
    let mut table_size = 14u64;
    for (name, _) in &entries {
        let name = name.as_bytes();
        assert!(!name.is_empty() && name.len() <= 255, "entry name length 1..=255");
        table_size += 1 + name.len() as u64 + 48;
    }
    let mut cursor = table_size;
    for (name, image) in &entries {
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&cursor.to_le_bytes());
        out.extend_from_slice(&(image.len() as u64).to_le_bytes());
        let mut sha = Sha256::new();
        sha.update(image);
        out.extend_from_slice(&sha.finalize());
        cursor += image.len() as u64;
    }
    debug_assert_eq!(out.len() as u64, table_size);
    for (_, image) in &entries {
        out.extend_from_slice(image);
    }
    out
}

/// Recomputes the SHA-256 of an image region (attestation helper; never on the hot path).
pub fn attest(image: &dyn ByteImage, base: u64, len: u64) -> [u8; 32] {
    let mut sha = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut pos = 0u64;
    while pos < len {
        let take = ((len - pos) as usize).min(chunk.len());
        image.read_exact_at(base + pos, &mut chunk[..take]);
        sha.update(&chunk[..take]);
        pos += take as u64;
    }
    sha.finalize().into()
}