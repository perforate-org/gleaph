//! Account canister ingress handlers (ADR 0068 Slice 1).
//!
//! Plain `pub(crate)` functions with explicit caller injection so unit tests drive every
//! authorization branch without WASM.

use crate::stable::store::AccountStore;
use crate::types::{Account, AccountError, Role, RouterEntry, generate_org_account_id};
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
        routers: Default::default(),
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
        routers: Default::default(),
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

/// Register a Router under an account. Owner/admin only.
pub(crate) fn register_router_with_caller(
    caller: Principal,
    account_id: &str,
    router: RouterEntry,
    store: &AccountStore,
) -> Result<(), AccountError> {
    store.register_router(account_id, caller, router)
}

/// Unregister a Router. Owner only.
pub(crate) fn unregister_router_with_caller(
    caller: Principal,
    account_id: &str,
    router_id: &str,
    store: &AccountStore,
) -> Result<(), AccountError> {
    store.unregister_router(account_id, caller, router_id)
}

/// List the account's Routers. Any member.
pub(crate) fn list_routers_with_caller(
    caller: Principal,
    account_id: &str,
    store: &AccountStore,
) -> Result<Vec<RouterEntry>, AccountError> {
    store.list_routers(account_id, caller)
}

/// Resolve a Router's canister id. Any member. NotFound if not issued.
pub(crate) fn resolve_router_with_caller(
    caller: Principal,
    account_id: &str,
    router_id: &str,
    store: &AccountStore,
) -> Result<Principal, AccountError> {
    store.resolve_router(account_id, caller, router_id)
}

/// Authorize the first-Router issuance: the Account (as bootstrap trust subject) sends a
/// `ProvisionRequest` to the Provision canister's `accept_envelope`. Owner/admin only.
///
/// `provision_canister` is the platform-fixed Provision canister id. `deployment_id` is the
/// account id (Personal principal or Org generated id). The Router callback is the Account
/// itself, so the first-issuance result returns here for `register_router`.
pub(crate) async fn authorize_router_issuance_with_caller(
    caller: Principal,
    account_id: &str,
    router_id: &str,
    provision_canister: Principal,
    store: &AccountStore,
) -> Result<gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse, AccountError> {
    let account = store.get(account_id).ok_or(AccountError::NotFound)?;
    if !account.is_owner_or_admin(&caller) {
        return Err(AccountError::NotAuthorized);
    }
    send_issuance_request(caller, account_id, router_id, provision_canister).await
}

/// The cross-canister `accept_envelope` send. Split out so the owner/admin authorization check
/// is unit-testable without a WASM runtime.
async fn send_issuance_request(
    caller: Principal,
    account_id: &str,
    router_id: &str,
    provision_canister: Principal,
) -> Result<gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse, AccountError> {
    use gleaph_graph_kernel::provisioning::wire::{
        ProvisionIngressResult, ProvisionRequest, ProvisionableResource,
    };
    use gleaph_graph_kernel::provisioning::{ProvisionableResourceKind, ProvisioningIntentKey};
    use ic_cdk::call::Call;

    let request = ProvisionRequest {
        deployment_id: account_id.to_owned(),
        request_id: format!("{router_id}-{}", caller.to_text()),
        request_fingerprint: format!("{router_id}-{}", caller.to_text()),
        intent_key: ProvisioningIntentKey::new(
            account_id,
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        ),
        reserved_graph_id: None,
        graph_name: "default".to_owned(),
        requested_resources: vec![ProvisionableResource {
            kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-0".to_owned(),
        }],
        authorized_caller: caller,
        release_id: "default".to_owned(),
        router_callback_principal: ic_cdk::api::canister_self(),
    };

    let result: ProvisionIngressResult =
        Call::unbounded_wait(provision_canister, "accept_envelope")
            .with_args(&(request,))
            .await
            .map_err(|e| AccountError::Message(format!("provision accept_envelope: {e}")))?
            .candid()
            .map_err(|e| AccountError::Message(format!("provision decode: {e}")))?;
    match result {
        ProvisionIngressResult::Ok(response) => Ok(response),
        ProvisionIngressResult::Err(e) => Err(AccountError::Message(format!("provision: {e:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        add_member_with_caller, create_account_with_caller, create_org_account_with_caller,
        register_router_with_caller, resolve_my_accounts_with_caller, resolve_router_with_caller,
    };
    use crate::stable::memory;
    use crate::stable::store::AccountStore;
    use crate::types::{Account, AccountError, Role, RouterEntry};
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
            routers: Default::default(),
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
            routers: Default::default(),
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

    #[test]
    fn router_mapping_enforces_rbac_and_resolves() {
        memory::reset_all();
        let store = AccountStore::new();
        let alice = p(1);
        let bob = p(2);
        let router = RouterEntry {
            router_id: "default".into(),
            router_canister: p(9),
        };

        let org = create_org_account_with_caller(alice, "team".into(), 0, &store).unwrap();
        let org_id = org.id();

        // bob (not a member) cannot register.
        assert_eq!(
            register_router_with_caller(bob, &org_id, router.clone(), &store),
            Err(AccountError::NotAuthorized)
        );
        // alice (owner) registers.
        register_router_with_caller(alice, &org_id, router.clone(), &store).unwrap();
        // duplicate router_id rejected.
        assert_eq!(
            register_router_with_caller(alice, &org_id, router.clone(), &store),
            Err(AccountError::AlreadyExists)
        );

        // member resolves; non-member denied.
        add_member_with_caller(alice, &org_id, bob, Role::Member, &store).unwrap();
        assert_eq!(
            resolve_router_with_caller(bob, &org_id, "default", &store),
            Ok(p(9))
        );
        assert_eq!(
            resolve_router_with_caller(p(3), &org_id, "default", &store),
            Err(AccountError::NotAuthorized)
        );
        // unknown router_id -> NotFound.
        assert_eq!(
            resolve_router_with_caller(bob, &org_id, "nope", &store),
            Err(AccountError::NotFound)
        );
    }

    #[test]
    fn authorize_router_issuance_rejects_non_owner_before_cross_canister() {
        memory::reset_all();
        let store = AccountStore::new();
        let alice = p(1);
        let bob = p(2);
        let org = create_org_account_with_caller(alice, "team".into(), 0, &store).unwrap();
        let org_id = org.id();

        // bob is not a member -> NotAuthorized, before any Provision call.
        let result = crate::canister::authorize_router_issuance_with_caller(
            bob,
            &org_id,
            "default",
            p(9),
            &store,
        );
        // The async fn returns a future; drive it to completion. Non-owner short-circuits
        // before the cross-canister call, so this is safe off-wasm.
        let result = futures::executor::block_on(result);
        assert_eq!(result, Err(AccountError::NotAuthorized));
    }
}
