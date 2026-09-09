//! Connection matrix for transition costs between context IDs
//!
//! Copyright 2026 COOLJAPAN OU (Team KitaSan)
//! Vendored (plan 0333) from github.com/cool-japan/mecrab @ 85444b5
//! (mecrab/src/dict/connection_matrix.rs), MIT OR Apache-2.0.
//!
//! Plan 0333 refactor: `matrix_ptr: *const i16` over the mmap slice replaced by
//! `Arc<dyn ByteImage>`; every cost lookup is a 2-byte `read_exact_at`.
//!
//! Binary format: u16 lsize, u16 rsize, then lsize * rsize i16 costs.
//! Index formula: matrix[rcAttr + lsize * lcAttr]

use crate::byteimage::ByteImage;
use crate::error::{Error, Result};
use byteorder::{ByteOrder, LittleEndian};
use std::sync::Arc;

/// Connection matrix storing transition costs. Fully RESIDENT at load (the whole
/// matrix is a hot random-access region — 2-byte reads through a lazy image would be
/// syscall-hostile), materialized into a flat `Vec<i16>`.
pub struct ConnectionMatrix {
    costs: Vec<i16>,
    lsize: usize,
    rsize: usize,
}

impl std::fmt::Debug for ConnectionMatrix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionMatrix")
            .field("lsize", &self.lsize)
            .field("rsize", &self.rsize)
            .finish()
    }
}

impl ConnectionMatrix {
    /// Load connection matrix from a byte image (upstream: `from_mmap(Arc<Mmap>)`).
    pub fn from_image(image: Arc<dyn ByteImage>) -> Result<Self> {
        let len = image.len();
        if len < 4 {
            return Err(Error::MatrixError(
                "Matrix file too small for header".to_string(),
            ));
        }

        let mut hdr = [0u8; 4];
        image.read_exact_at(0, &mut hdr);
        let lsize = LittleEndian::read_u16(&hdr[0..2]) as usize;
        let rsize = LittleEndian::read_u16(&hdr[2..4]) as usize;

        let expected_size = 4 + lsize * rsize * 2;
        if len as usize != expected_size {
            return Err(Error::MatrixError(format!(
                "Matrix file size mismatch: expected {expected_size} bytes ({}x{} matrix + 4), got {len}",
                lsize, rsize
            )));
        }

        // Materialize the resident matrix (memcpy from the image).
        let mut costs = vec![0i16; lsize * rsize];
        let mut bytes = vec![0u8; lsize * rsize * 2];
        image.read_exact_at(4, &mut bytes);
        for (i, cell) in costs.iter_mut().enumerate() {
            *cell = LittleEndian::read_i16(&bytes[i * 2..i * 2 + 2]);
        }

        Ok(Self {
            costs,
            lsize,
            rsize,
        })
    }

    /// Get the connection cost between right and left context IDs
    /// (MeCab: `matrix_[rcAttr + lsize_ * lcAttr]`)
    #[inline]
    pub fn cost(&self, right_id: u16, left_id: u16) -> i16 {
        let rc = right_id as usize;
        let lc = left_id as usize;

        // Bounds follow the index formula (MeCab `matrix_[rcAttr + lsize_ * lcAttr]`):
        // rc ranges over the stride dimension (lsize), lc over rsize. (The swapped
        // form was invisible for square ipadic 1316x1316; ko-dic 3822x2693 exposed
        // it — valid right ids up to 3813 were rejected as i16::MAX.)
        if rc >= self.lsize || lc >= self.rsize {
            return i16::MAX;
        }

        let index = rc + self.lsize * lc;
        self.costs[index]
    }

    /// Get the number of left context IDs
    #[inline]
    pub fn left_size(&self) -> usize {
        self.lsize
    }

    /// Get the number of right context IDs
    #[inline]
    pub fn right_size(&self) -> usize {
        self.rsize
    }

    /// Get total number of entries
    #[inline]
    pub fn size(&self) -> usize {
        self.lsize * self.rsize
    }
}
