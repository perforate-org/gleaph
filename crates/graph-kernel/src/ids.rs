use std::fmt;
use std::ops::AddAssign;

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NodeId([u8; 5]);

pub type EdgeId = u64;
pub type LabelId = u16;

impl NodeId {
    pub const MAX: u64 = (1u64 << 40) - 1;

    pub const fn new(bytes: [u8; 5]) -> Self {
        Self(bytes)
    }

    pub fn to_u64(self) -> u64 {
        let [b0, b1, b2, b3, b4] = self.0;
        u64::from_be_bytes([0, 0, 0, b0, b1, b2, b3, b4])
    }

    pub fn checked_next(self) -> Option<Self> {
        self.to_u64()
            .checked_add(1)
            .and_then(|value| Self::try_from(value).ok())
    }

    pub fn as_bytes(self) -> [u8; 5] {
        self.0
    }

    pub fn to_be_bytes(self) -> [u8; 5] {
        self.0
    }
}

impl TryFrom<u64> for NodeId {
    type Error = NodeIdOverflow;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value > Self::MAX {
            return Err(NodeIdOverflow(value));
        }
        let bytes = value.to_be_bytes();
        Ok(Self([bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]))
    }
}

impl From<NodeId> for u64 {
    fn from(value: NodeId) -> Self {
        value.to_u64()
    }
}

impl From<u8> for NodeId {
    fn from(value: u8) -> Self {
        Self::try_from(value as u64).expect("u8 always fits in NodeId")
    }
}

impl From<u16> for NodeId {
    fn from(value: u16) -> Self {
        Self::try_from(value as u64).expect("u16 always fits in NodeId")
    }
}

impl From<u32> for NodeId {
    fn from(value: u32) -> Self {
        Self::try_from(value as u64).expect("u32 always fits in NodeId")
    }
}

impl AddAssign<u64> for NodeId {
    fn add_assign(&mut self, rhs: u64) {
        *self = Self::try_from(self.to_u64().checked_add(rhs).expect("NodeId overflow"))
            .expect("NodeId overflow");
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.to_u64(), f)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.to_u64(), f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeIdOverflow(pub u64);

const _: () = assert!(core::mem::size_of::<NodeId>() == 5);
