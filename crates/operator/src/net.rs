//! Re-export of the shared network endpoint resolution ([`gleaph_ingress_client::net`]).
//!
//! Conventions (endpoint selection, root-key fetch policy) are intentionally identical to the
//! dev CLI's `RemoteTransport::connect` (`crates/cli/src/remote.rs`). The implementation now
//! lives next to the transport that consumes it ([`crate::transport`]).

pub use gleaph_ingress_client::net::{DEFAULT_IC_URL, DEFAULT_LOCAL_URL, resolve_network};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn re_exports_resolve_with_the_dev_cli_conventions() {
        let (url, fetch) = resolve_network("local").expect("local");
        assert_eq!(url, DEFAULT_LOCAL_URL);
        assert!(fetch, "local replicas always need the root key");
    }
}