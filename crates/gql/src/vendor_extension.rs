//! Compile-time metadata for vendor-qualified GQL extensions (`PREFIX.MEMBER`).
//!
//! [`GqlVendorMemberNames`] is typically populated by the `gql_extension!` proc macro.

/// Canonical member name plus optional ASCII aliases (e.g. wire names, legacy spellings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlVendorMemberNames {
    pub primary: &'static str,
    pub aliases: &'static [&'static str],
}

impl GqlVendorMemberNames {
    #[inline]
    pub const fn new(primary: &'static str, aliases: &'static [&'static str]) -> Self {
        Self { primary, aliases }
    }

    /// Returns true if `name` matches `primary` or any alias (ASCII case-insensitive).
    #[inline]
    pub fn matches_ignore_case(&self, name: &str) -> bool {
        if name.eq_ignore_ascii_case(self.primary) {
            return true;
        }
        self.aliases.iter().any(|a| name.eq_ignore_ascii_case(a))
    }
}
