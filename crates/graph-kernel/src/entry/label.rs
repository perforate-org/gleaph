#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LabelId(u16);

impl LabelId {
    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }
}
