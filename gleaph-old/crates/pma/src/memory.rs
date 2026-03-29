use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
/// Errors produced by the abstract memory backend.
pub enum MemoryError {
    #[error("out of bounds read/write at offset {offset} len {len}")]
    OutOfBounds { offset: u64, len: usize },
    #[error("grow overflow")]
    GrowOverflow,
}

/// Minimal byte-addressable memory interface used by the PMA implementation.
pub trait Memory {
    fn size_bytes(&self) -> u64;
    fn grow(&mut self, additional_bytes: u64) -> Result<(), MemoryError>;
    fn read(&self, offset: u64, dst: &mut [u8]);
    fn write(&mut self, offset: u64, src: &[u8]);
}

#[derive(Debug, Default, Clone)]
/// In-memory `Memory` implementation used for tests and native execution.
pub struct VecMemory {
    bytes: Vec<u8>,
}

impl VecMemory {
    /// Creates an empty memory buffer.
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Creates a zero-initialized memory buffer with the requested size.
    pub fn with_size(size: usize) -> Self {
        Self {
            bytes: vec![0; size],
        }
    }

    fn check_range(&self, offset: u64, len: usize) {
        let start = offset as usize;
        let end = start.checked_add(len).expect("range overflow");
        assert!(end <= self.bytes.len(), "out-of-bounds memory access");
    }
}

impl Memory for VecMemory {
    fn size_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn grow(&mut self, additional_bytes: u64) -> Result<(), MemoryError> {
        let add = usize::try_from(additional_bytes).map_err(|_| MemoryError::GrowOverflow)?;
        let new_len = self
            .bytes
            .len()
            .checked_add(add)
            .ok_or(MemoryError::GrowOverflow)?;
        self.bytes.resize(new_len, 0);
        Ok(())
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        self.check_range(offset, dst.len());
        let start = offset as usize;
        let end = start + dst.len();
        dst.copy_from_slice(&self.bytes[start..end]);
    }

    fn write(&mut self, offset: u64, src: &[u8]) {
        self.check_range(offset, src.len());
        let start = offset as usize;
        let end = start + src.len();
        self.bytes[start..end].copy_from_slice(src);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_memory_grow_read_write() {
        let mut mem = VecMemory::with_size(8);
        mem.write(2, &[1, 2, 3]);
        let mut buf = [0u8; 3];
        mem.read(2, &mut buf);
        assert_eq!(buf, [1, 2, 3]);
        mem.grow(4).unwrap();
        assert_eq!(mem.size_bytes(), 12);
    }

    #[test]
    #[should_panic]
    fn vec_memory_panics_on_oob() {
        let mut mem = VecMemory::with_size(2);
        mem.write(1, &[1, 2]);
    }
}
