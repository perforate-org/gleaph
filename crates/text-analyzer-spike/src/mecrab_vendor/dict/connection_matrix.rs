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

use crate::mecrab_vendor::byteimage::ByteImage;
use crate::mecrab_vendor::error::{Error, Result};
use byteorder::{ByteOrder, LittleEndian};
use std::sync::Arc;

/// Connection matrix storing transition costs
pub struct ConnectionMatrix {
    image: Arc<dyn ByteImage>,
    /// Offset of the cost array (after the 4-byte header)
    data_offset: u64,
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

#[inline]
fn read_u16_at(image: &dyn ByteImage, offset: u64) -> u16 {
    let mut b = [0u8; 2];
    image.read_exact_at(offset, &mut b);
    LittleEndian::read_u16(&b)
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

        let lsize = read_u16_at(image.as_ref(), 0) as usize;
        let rsize = read_u16_at(image.as_ref(), 2) as usize;

        let expected_size = 4 + lsize * rsize * 2;
        if len as usize != expected_size {
            return Err(Error::MatrixError(format!(
                "Matrix file size mismatch: expected {expected_size} bytes ({}x{} matrix + 4), got {len}",
                lsize, rsize
            )));
        }

        Ok(Self {
            image,
            data_offset: 4,
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

        if rc >= self.rsize || lc >= self.lsize {
            return i16::MAX;
        }

        let index = rc + self.lsize * lc;
        read_u16_at(self.image.as_ref(), self.data_offset + (index * 2) as u64) as i16
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