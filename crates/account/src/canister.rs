//! Account canister ingress handlers (ADR 0068 Slice 1).
//!
//! Plain `pub(crate)` functions with explicit caller injection so unit tests drive every
//! authorization branch without WASM.

use crate::stable::store::AccountStore;
use crate::types::{Account, AccountError, Role, generate_org_account_id};
use candid::Principal;

/// Create a Personal account owned by `caller`. Rejects anonymous and an existing same-id account.
pub(crate) fn create_account_with_caller(
    caller: Principal,
    name: String,
    store: &AccountStore,
) -> Result<Account, AccountError> {
    if caller == Principal::anonymous() {
        return Err(AccountError::AnonymousPrincipal);
    }
    let account = Account::Personal {
        name,
        principal: caller,
    };
    store.insert(account.clone())?;
    Ok(account)
}

/// Create an Org account; `caller` becomes the first owner.
pub(crate) fn create_org_account_with_caller(
    caller: Principal,
    name: String,
    now_ns: u64,
    store: &AccountStore,
) -> Result<Account, AccountError> {
    if caller == Principal::anonymous() {
        return Err(AccountError::AnonymousPrincipal);
    }
    let mut members = std::collections::BTreeMap::new();
    members.insert(caller, Role::Owner);
    let account = Account::Org {
        name,
        account_id: generate_org_account_id(&caller, now_ns),
        members,
    };
    store.insert(account.clone())?;
    Ok(account)
}

/// Read an account. Any member may read.
pub(crate) fn get_account_with_caller(
    caller: Principal,
    account_id: &str,
    store: &AccountStore,
) -> Result<Account, AccountError> {
    let account = store.get(account_id).ok_or(AccountError::NotFound)?;
    if !account.is_member(&caller) {
        return Err(AccountError::NotAuthorized);
    }
    Ok(account)
}

/// Delete an account: owner (Org) or self (Personal).
pub(crate) fn delete_account_with_caller(
    caller: Principal,
    account_id: &str,
    store: &AccountStore,
) -> Result<(), AccountError> {
    let account = store.get(account_id).ok_or(AccountError::NotFound)?;
    if !account.is_owner(&caller) {
        return Err(AccountError::NotAuthorized);
    }
    store.remove(account_id);
    Ok(())
}

/// Add or change a member of an Org. Owner-only.
pub(crate) fn add_member_with_caller(
    caller: Principal,
    account_id: &str,
    target: Principal,
    role: Role,
    store: &AccountStore,
) -> Result<(), AccountError> {
    if target == Principal::anonymous() {
        return Err(AccountError::AnonymousPrincipal);
    }
    store.upsert_member(account_id, caller, target, role)
}

/// Remove a member of an Org. Owner-only.
pub(crate) fn remove_member_with_caller(
    caller: Principal,
    account_id: &str,
    target: Principal,
    store: &AccountStore,
) -> Result<(), AccountError> {
    store.remove_member(account_id, caller, target)
}

/// List the caller's own account ids (Personal plus any Org memberships).
pub(crate) fn resolve_my_accounts_with_caller(
    caller: Principal,
    store: &AccountStore,
) -> Vec<String> {
    store.accounts_of(caller)
}

#[cfg(test)]
mod tests {
    use super::{
        add_member_with_caller, create_account_with_caller, create_org_account_with_caller,
        resolve_my_accounts_with_caller,
    };
    use crate::stable::memory;
    use crate::stable::store::AccountStore;
    use crate::types::{Account, AccountError, Role};
    use candid::Principal;

    fn p(b: u8) -> Principal {
        Principal::from_slice(&[b; 29])
    }

    /// The one check that fails if the RBAC discriminator logic breaks.
    #[test]
    fn account_enum_discriminates_and_rbac_holds() {
        let alice = p(1);
        let bob = p(2);
        let org = Account::Org {
            name: "team".into(),
            account_id: "org-x".into(),
            members: [(alice, Role::Owner), (bob, Role::Member)].into(),
        };

        assert!(org.is_owner(&alice));
        assert!(!org.is_owner(&bob));
        assert!(org.is_owner_or_admin(&alice));
        assert!(!org.is_owner_or_admin(&bob));
        assert!(org.is_member(&bob));
        assert!(!org.is_member(&p(3)));

        let personal = Account::Personal {
            name: "me".into(),
            principal: alice,
        };
        assert!(personal.is_owner(&alice));
        assert!(!personal.is_member(&bob));
    }

    #[test]
    fn lifecycle_handlers_enforce_owner() {
        memory::reset_all();
        let store = AccountStore::new();
        let alice = p(1);
        let bob = p(2);

        assert_eq!(
            create_account_with_caller(Principal::anonymous(), "a".into(), &store),
            Err(AccountError::AnonymousPrincipal)
        );
        let personal = create_account_with_caller(alice, "a".into(), &store).unwrap();
        assert_eq!(personal.id(), alice.to_text());
        assert_eq!(
            create_account_with_caller(alice, "a".into(), &store),
            Err(AccountError::AlreadyExists)
        );

        let org = create_org_account_with_caller(alice, "team".into(), 0, &store).unwrap();
        let org_id = org.id();

        assert_eq!(
            add_member_with_caller(bob, &org_id, bob, Role::Admin, &store),
            Err(AccountError::NotAuthorized)
        );
        add_member_with_caller(alice, &org_id, bob, Role::Admin, &store).unwrap();
        assert!(store.get(&org_id).unwrap().is_owner_or_admin(&bob));

        let mut mine = resolve_my_accounts_with_caller(alice, &store);
        mine.sort();
        let mut expect = vec![alice.to_text(), org_id];
        expect.sort();
        assert_eq!(mine, expect);
    }
}
