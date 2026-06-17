use std::cell::RefCell;

use gleaph_graph_kernel::federation::ElementIdEncodingKey;

thread_local! {
    static EXECUTION_ELEMENT_ID_KEY: RefCell<Option<ElementIdEncodingKey>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MissingElementIdEncodingKey;

pub(crate) fn set_execution_element_id_key(key: Option<ElementIdEncodingKey>) {
    EXECUTION_ELEMENT_ID_KEY.with_borrow_mut(|slot| {
        *slot = key;
    });
}

pub(crate) fn clear_execution_element_id_key() {
    set_execution_element_id_key(None);
}

pub(crate) fn require_execution_element_id_key()
-> Result<ElementIdEncodingKey, MissingElementIdEncodingKey> {
    EXECUTION_ELEMENT_ID_KEY.with_borrow(|slot| slot.ok_or(MissingElementIdEncodingKey))
}

/// Host tests and canbench may fall back to [`ElementIdEncodingKey::host_test_fixture`]; production paths
/// must set the router-issued key before plan execution (ADR 0019).
pub(crate) fn execution_element_id_key() -> ElementIdEncodingKey {
    require_execution_element_id_key().unwrap_or_else(|_| {
        #[cfg(any(test, feature = "canbench"))]
        {
            ElementIdEncodingKey::host_test_fixture()
        }
        #[cfg(not(any(test, feature = "canbench")))]
        {
            panic!("element id encoding key must be set before ELEMENT_ID or path encoding");
        }
    })
}
