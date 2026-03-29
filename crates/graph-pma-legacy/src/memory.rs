pub trait Memory {
    fn len(&self) -> usize;
    fn read(&self, offset: usize, buf: &mut [u8]);
    fn write(&mut self, offset: usize, data: &[u8]);
    fn resize(&mut self, new_len: usize);
}

#[derive(Clone, Debug, Default)]
pub struct VecMemory {
    bytes: Vec<u8>,
}

impl VecMemory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Memory for VecMemory {
    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn read(&self, offset: usize, buf: &mut [u8]) {
        let end = offset + buf.len();
        buf.copy_from_slice(&self.bytes[offset..end]);
    }

    fn write(&mut self, offset: usize, data: &[u8]) {
        let end = offset + data.len();
        if end > self.bytes.len() {
            self.bytes.resize(end, 0);
        }
        self.bytes[offset..end].copy_from_slice(data);
    }

    fn resize(&mut self, new_len: usize) {
        self.bytes.resize(new_len, 0);
    }
}
