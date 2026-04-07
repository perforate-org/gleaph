use ic_stable_structures::storable::{Bound, Storable};

pub(crate) struct Bounds {
    pub max_size: u32,
    pub is_fixed_size: bool,
}

pub(crate) const fn bounds<A: Storable>() -> Bounds {
    if let Bound::Bounded {
        max_size,
        is_fixed_size,
    } = A::BOUND
    {
        Bounds {
            max_size,
            is_fixed_size,
        }
    } else {
        panic!("Cannot get bounds of unbounded type.");
    }
}

pub(crate) const fn bytes_to_store_size_bounded(bounds: &Bounds) -> u32 {
    if bounds.is_fixed_size {
        0
    } else {
        bytes_to_store_size(bounds.max_size as usize) as u32
    }
}

const fn bytes_to_store_size(bytes_size: usize) -> usize {
    if bytes_size <= u8::MAX as usize {
        1
    } else if bytes_size <= u16::MAX as usize {
        2
    } else {
        4
    }
}

/// Bytes after the 5-byte cell prefix (`tag` + `generation`), large enough for `T`'s encoding and
/// at least 4 bytes so a vacant cell can store `next_free: u32`.
pub(crate) const fn slot_payload_bytes<T: Storable>() -> u32 {
    let b = bounds::<T>();
    let sz = b.max_size.saturating_add(bytes_to_store_size_bounded(&b));
    if sz < 4 { 4 } else { sz }
}
