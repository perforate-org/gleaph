//! Iterator adapters for the common traversal visitor contract.
//!
//! These helpers deliberately do not decode stable-memory rows. They consume an iterator supplied
//! by a storage owner such as `lara::edge::iter` and preserve `ControlFlow` without materializing
//! the remaining traversal.

use std::ops::ControlFlow;

/// Visits every item from an iterator until the visitor requests an early break.
pub fn visit<I, B>(iter: I, mut visitor: impl FnMut(I::Item) -> ControlFlow<B>) -> ControlFlow<B>
where
    I: IntoIterator,
{
    for item in iter {
        if let ControlFlow::Break(value) = visitor(item) {
            return ControlFlow::Break(value);
        }
    }
    ControlFlow::Continue(())
}

/// Visits a fallible iterator, stopping on the first storage error or visitor break.
pub fn try_visit<I, T, E, B>(
    iter: I,
    mut visitor: impl FnMut(T) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, E>
where
    I: IntoIterator<Item = Result<T, E>>,
{
    for item in iter {
        let item = item?;
        if let ControlFlow::Break(value) = visitor(item) {
            return Ok(ControlFlow::Break(value));
        }
    }
    Ok(ControlFlow::Continue(()))
}

/// Visits slot-and-edge pairs from a fallible storage iterator.
pub fn try_visit_indexed<I, S, T, E, B>(
    iter: I,
    mut visitor: impl FnMut(S, T) -> ControlFlow<B>,
) -> Result<ControlFlow<B>, E>
where
    I: IntoIterator<Item = Result<(S, T), E>>,
{
    try_visit(iter, |(slot, edge)| visitor(slot, edge))
}

/// Visits slot-and-edge pairs while preserving the storage iterator's order.
pub fn visit_indexed<I, S, E, B>(
    iter: I,
    mut visitor: impl FnMut(S, E) -> ControlFlow<B>,
) -> ControlFlow<B>
where
    I: IntoIterator<Item = (S, E)>,
{
    visit(iter, |(slot, edge)| visitor(slot, edge))
}

/// Returns the first item that satisfies a predicate, stopping the source iterator immediately.
pub fn find<I, F>(iter: I, mut predicate: F) -> Option<I::Item>
where
    I: IntoIterator,
    F: FnMut(&I::Item) -> bool,
{
    let mut found = None;
    let _ = visit(iter, |item| {
        if predicate(&item) {
            found = Some(item);
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visit_propagates_break_without_consuming_the_tail() {
        let mut iter = 0..4;
        let flow = visit(&mut iter, |value| {
            if value == 1 {
                ControlFlow::Break(value)
            } else {
                ControlFlow::Continue(())
            }
        });

        assert_eq!(flow, ControlFlow::Break(1));
        assert_eq!(iter.next(), Some(2));
    }

    #[test]
    fn visit_indexed_preserves_slots() {
        let mut seen = Vec::new();
        let flow = visit_indexed([(3, 'c'), (1, 'a')], |slot, edge| {
            seen.push((slot, edge));
            ControlFlow::<()>::Continue(())
        });

        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(seen, vec![(3, 'c'), (1, 'a')]);
    }

    #[test]
    fn find_stops_at_first_match() {
        assert_eq!(find(0..5, |value| *value == 2), Some(2));
    }

    #[test]
    fn try_visit_propagates_storage_errors() {
        let result = try_visit([Ok::<_, &'static str>(1), Err("broken"), Ok(3)], |_| {
            ControlFlow::<()>::Continue(())
        });
        assert_eq!(result, Err("broken"));
    }
}
