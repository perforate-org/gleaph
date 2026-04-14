use std::fmt;
use std::ops::AddAssign;

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NodeId(u32);

pub type EdgeId = u64;
pub type LabelId = u16;

impl NodeId {
    pub const MAX: u64 = u32::MAX as u64;

    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn to_u32(self) -> u32 {
        self.0
    }

    pub const fn to_u64(self) -> u64 {
        self.0 as u64
    }

    pub fn checked_next(self) -> Option<Self> {
        self.to_u64()
            .checked_add(1)
            .and_then(|value| Self::try_from(value).ok())
    }

    pub const fn as_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }

    pub const fn to_be_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }

    pub const fn from_be_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_be_bytes(bytes))
    }
}

impl TryFrom<u64> for NodeId {
    type Error = NodeIdOverflow;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value > Self::MAX {
            return Err(NodeIdOverflow(value));
        }
        Ok(Self(value as u32))
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

const _: () = assert!(core::mem::size_of::<NodeId>() == 4);
