//! Provider-independent fiat order state machine (ADR 0133).
//!
//! This crate owns the business transition rules only: order identity, rate snapshots,
//! payment-event idempotency, credit/refund intents, and the launch refund policy. It has no
//! network, database, or payment-provider dependency. An external service supplies a
//! [`PaymentGateway`] implementation and a [`CreditGateway`] implementation (the Provision
//! `grant_credit` / `reverse_credit` calls), then persists the [`Order`] snapshot.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Longest accepted external order / deployment identifier.
pub const MAX_IDENTIFIER_LEN: usize = 128;

/// Opaque external order identifier. `order_id` is the idempotency key for credit grants and
/// unused-grant reversals.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OrderId(String);

impl OrderId {
    pub fn new(value: impl Into<String>) -> Result<Self, OrderError> {
        let value = value.into();
        validate_identifier("order id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque Provision deployment identifier. The adapter maps it to the canister principal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentRef(String);

impl DeploymentRef {
    pub fn new(value: impl Into<String>) -> Result<Self, OrderError> {
        let value = value.into();
        validate_identifier("deployment", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_identifier(what: &str, value: &str) -> Result<(), OrderError> {
    if value.is_empty() {
        return Err(OrderError::InvalidIdentifier(format!("{what} is empty")));
    }
    if value.len() > MAX_IDENTIFIER_LEN {
        return Err(OrderError::InvalidIdentifier(format!(
            "{what} exceeds {MAX_IDENTIFIER_LEN} bytes"
        )));
    }
    Ok(())
}

/// Currency of the fiat price snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Currency {
    /// Japanese yen, minor unit = 1 yen.
    Jpy,
}

/// A verified payment-provider event. Signature verification belongs to the adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEvent {
    /// Provider delivery/dedupe id.
    pub provider_event_id: String,
    /// Order this event settles.
    pub order_id: OrderId,
    /// Settled amount in minor units.
    pub amount_minor: u64,
    /// Currency of the settlement.
    pub currency: Currency,
}

/// Result of applying a payment event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaymentOutcome {
    /// The settlement was recorded.
    Applied,
    /// The same provider event was already recorded; no state changed.
    Replayed,
}

/// Where a refund is allowed to travel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefundPath {
    /// Full unused grant: call Provision `reverse_credit`.
    OnChainFull,
    /// Partially consumed grant: fiat-only refund; the credit stays on the deployment.
    FiatOnly,
}

/// Durable order state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderState {
    Draft,
    PendingPayment,
    Paid,
    CreditGrantPending,
    CreditGranted,
    Fulfilled,
    CreditGrantFailed { reason: String },
    RefundPending { path: RefundPath },
    Refunded { path: RefundPath },
    RefundRejected { path: RefundPath, reason: String },
}

/// One persisted order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    id: OrderId,
    deployment: DeploymentRef,
    price_yen: u64,
    cycles_per_yen: u128,
    credit_cycles: u128,
    state: OrderState,
    settlement_event_id: Option<String>,
    consumed_cycles: u128,
}

impl Order {
    /// Snapshot the price and rate at order creation. Rate is integer cycles per yen; no float
    /// arithmetic can drift between quote and settlement.
    pub fn new(
        id: OrderId,
        deployment: DeploymentRef,
        price_yen: u64,
        cycles_per_yen: u128,
    ) -> Result<Self, OrderError> {
        if price_yen == 0 {
            return Err(OrderError::ZeroPrice);
        }
        if cycles_per_yen == 0 {
            return Err(OrderError::ZeroRate);
        }
        let credit_cycles = u128::from(price_yen)
            .checked_mul(cycles_per_yen)
            .ok_or(OrderError::AmountOverflow)?;
        Ok(Self {
            id,
            deployment,
            price_yen,
            cycles_per_yen,
            credit_cycles,
            state: OrderState::Draft,
            settlement_event_id: None,
            consumed_cycles: 0,
        })
    }

    pub fn id(&self) -> &OrderId {
        &self.id
    }

    pub fn deployment(&self) -> &DeploymentRef {
        &self.deployment
    }

    pub fn price_yen(&self) -> u64 {
        self.price_yen
    }

    pub fn cycles_per_yen(&self) -> u128 {
        self.cycles_per_yen
    }

    pub fn credit_cycles(&self) -> u128 {
        self.credit_cycles
    }

    pub fn state(&self) -> &OrderState {
        &self.state
    }

    pub fn consumed_cycles(&self) -> u128 {
        self.consumed_cycles
    }

    /// The single idempotency key used by Provision grant/reversal calls.
    pub fn idempotency_key(&self) -> &str {
        self.id.as_str()
    }

    /// Move a draft to awaiting payment. Idempotent while already pending.
    pub fn submit(&mut self) -> Result<(), OrderError> {
        match self.state {
            OrderState::Draft => {
                self.state = OrderState::PendingPayment;
                Ok(())
            }
            OrderState::PendingPayment => Ok(()),
            _ => Err(self.invalid_state("submit")),
        }
    }

    /// Apply a verified settlement. Replaying the exact provider event is a no-op; reusing the
    /// order with a different event fails closed.
    pub fn apply_payment(&mut self, event: &PaymentEvent) -> Result<PaymentOutcome, OrderError> {
        if event.order_id != self.id {
            return Err(OrderError::PaymentOrderMismatch);
        }
        if let Some(existing) = &self.settlement_event_id {
            if existing == &event.provider_event_id {
                return Ok(PaymentOutcome::Replayed);
            }
            return Err(OrderError::ConflictingPayment);
        }
        if !matches!(self.state, OrderState::PendingPayment) {
            return Err(self.invalid_state("apply_payment"));
        }
        if event.currency != Currency::Jpy {
            return Err(OrderError::PaymentCurrencyMismatch);
        }
        if event.amount_minor != self.price_yen {
            return Err(OrderError::PaymentAmountMismatch {
                expected: self.price_yen,
                actual: event.amount_minor,
            });
        }
        self.settlement_event_id = Some(event.provider_event_id.clone());
        self.state = OrderState::Paid;
        Ok(PaymentOutcome::Applied)
    }

    /// Start the durable grant intent. Idempotent while the grant is pending.
    pub fn begin_grant(&mut self) -> Result<(), OrderError> {
        match self.state {
            OrderState::Paid => {
                self.state = OrderState::CreditGrantPending;
                Ok(())
            }
            OrderState::CreditGrantPending => Ok(()),
            _ => Err(self.invalid_state("begin_grant")),
        }
    }

    pub fn grant_succeeded(&mut self) -> Result<(), OrderError> {
        match self.state {
            OrderState::CreditGrantPending => {
                self.state = OrderState::CreditGranted;
                Ok(())
            }
            OrderState::CreditGranted | OrderState::Fulfilled => Ok(()),
            _ => Err(self.invalid_state("grant_succeeded")),
        }
    }

    pub fn grant_failed(&mut self, reason: impl Into<String>) -> Result<(), OrderError> {
        match self.state {
            OrderState::CreditGrantPending => {
                self.state = OrderState::CreditGrantFailed {
                    reason: reason.into(),
                };
                Ok(())
            }
            OrderState::CreditGrantFailed { .. } => Ok(()),
            _ => Err(self.invalid_state("grant_failed")),
        }
    }

    /// Re-arm a failed grant with the same order id. Provision's idempotency key makes the
    /// retry safe even when the previous call actually committed but its reply was lost.
    pub fn retry_grant(&mut self) -> Result<(), OrderError> {
        match self.state {
            OrderState::CreditGrantFailed { .. } => {
                self.state = OrderState::CreditGrantPending;
                Ok(())
            }
            OrderState::CreditGrantPending | OrderState::CreditGranted | OrderState::Fulfilled => {
                Ok(())
            }
            _ => Err(self.invalid_state("retry_grant")),
        }
    }

    /// Mark a granted order fulfilled (the deployment is usable). Idempotent.
    pub fn fulfill(&mut self) -> Result<(), OrderError> {
        match self.state {
            OrderState::CreditGranted => {
                self.state = OrderState::Fulfilled;
                Ok(())
            }
            OrderState::Fulfilled => Ok(()),
            _ => Err(self.invalid_state("fulfill")),
        }
    }

    /// Request a refund. Only an unused full grant takes the on-chain path; consumed credit
    /// takes the fiat-only path and leaves the deployment credit intact (ADR 0133).
    pub fn request_refund(&mut self, consumed_cycles: u128) -> Result<RefundPath, OrderError> {
        if consumed_cycles > self.credit_cycles {
            return Err(OrderError::RefundExceedsCredit {
                credit: self.credit_cycles,
                consumed: consumed_cycles,
            });
        }
        let path = if consumed_cycles == 0 {
            RefundPath::OnChainFull
        } else {
            RefundPath::FiatOnly
        };
        match self.state {
            OrderState::CreditGranted | OrderState::Fulfilled => {
                self.consumed_cycles = consumed_cycles;
                self.state = OrderState::RefundPending { path };
                Ok(path)
            }
            OrderState::RefundPending { path } => Ok(path),
            _ => Err(self.invalid_state("request_refund")),
        }
    }

    pub fn refund_completed(&mut self) -> Result<(), OrderError> {
        match self.state {
            OrderState::RefundPending { path } => {
                self.state = OrderState::Refunded { path };
                Ok(())
            }
            OrderState::Refunded { .. } => Ok(()),
            _ => Err(self.invalid_state("refund_completed")),
        }
    }

    pub fn refund_rejected(&mut self, reason: impl Into<String>) -> Result<(), OrderError> {
        match self.state {
            OrderState::RefundPending { path } => {
                self.state = OrderState::RefundRejected {
                    path,
                    reason: reason.into(),
                };
                Ok(())
            }
            OrderState::RefundRejected { .. } => Ok(()),
            _ => Err(self.invalid_state("refund_rejected")),
        }
    }

    /// Re-arm a rejected refund with its original path. On-chain retries reuse the order id;
    /// fiat-only retries never call Provision.
    pub fn retry_refund(&mut self) -> Result<(), OrderError> {
        match self.state {
            OrderState::RefundRejected { path, .. } => {
                self.state = OrderState::RefundPending { path };
                Ok(())
            }
            OrderState::RefundPending { .. } | OrderState::Refunded { .. } => Ok(()),
            _ => Err(self.invalid_state("retry_refund")),
        }
    }

    /// A grant intent exists while the paid order awaits its credit call.
    pub fn grant_intent(&self) -> Option<GrantRequest> {
        matches!(self.state, OrderState::CreditGrantPending).then(|| GrantRequest {
            order_id: self.id.clone(),
            deployment: self.deployment.clone(),
            cycles: self.credit_cycles,
        })
    }

    /// An on-chain refund intent exists only for the unused full path.
    pub fn refund_intent(&self) -> Option<RefundRequest> {
        match self.state {
            OrderState::RefundPending {
                path: RefundPath::OnChainFull,
            } => Some(RefundRequest {
                order_id: self.id.clone(),
                deployment: self.deployment.clone(),
                cycles: self.credit_cycles,
            }),
            _ => None,
        }
    }

    /// Drive one grant attempt through the supplied Provision transport.
    pub fn drive_grant<G: CreditGateway>(
        &mut self,
        gateway: &G,
    ) -> Result<GrantDriveOutcome, OrderError> {
        let Some(request) = self.grant_intent() else {
            return Ok(GrantDriveOutcome::NotDue);
        };
        match gateway.grant(&request) {
            Ok(receipt) => {
                self.grant_succeeded()?;
                Ok(GrantDriveOutcome::Completed {
                    replayed: receipt.replayed,
                })
            }
            Err(reason) => {
                self.grant_failed(reason.clone())?;
                Ok(GrantDriveOutcome::Failed(reason))
            }
        }
    }

    /// Drive one on-chain refund attempt. Fiat-only refunds have no intent and are reported as
    /// `FiatOnly`.
    pub fn drive_refund<G: CreditGateway>(
        &mut self,
        gateway: &G,
    ) -> Result<RefundDriveOutcome, OrderError> {
        let Some(request) = self.refund_intent() else {
            return Ok(match self.state {
                OrderState::RefundPending {
                    path: RefundPath::FiatOnly,
                } => RefundDriveOutcome::FiatOnly,
                _ => RefundDriveOutcome::NotDue,
            });
        };
        match gateway.reverse(&request) {
            Ok(()) => {
                self.refund_completed()?;
                Ok(RefundDriveOutcome::Completed)
            }
            Err(reason) => {
                self.refund_rejected(reason.clone())?;
                Ok(RefundDriveOutcome::Failed(reason))
            }
        }
    }

    fn invalid_state(&self, action: &str) -> OrderError {
        OrderError::InvalidState {
            action: action.to_owned(),
            state: format!("{:?}", self.state),
        }
    }
}

/// Request passed to a Provision credit transport.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRequest {
    pub order_id: OrderId,
    pub deployment: DeploymentRef,
    pub cycles: u128,
}

/// Request passed to a Provision reversal transport.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundRequest {
    pub order_id: OrderId,
    pub deployment: DeploymentRef,
    pub cycles: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrantReceipt {
    pub replayed: bool,
}

/// Transport to Provision `grant_credit` / `reverse_credit`. Implementations own their async
/// runtime and map the request to the canister call.
pub trait CreditGateway {
    fn grant(&self, request: &GrantRequest) -> Result<GrantReceipt, String>;
    fn reverse(&self, request: &RefundRequest) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantDriveOutcome {
    NotDue,
    Completed { replayed: bool },
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefundDriveOutcome {
    NotDue,
    FiatOnly,
    Completed,
    Failed(String),
}

/// Provider webhook verifier. The adapter verifies the signature over the raw body and
/// returns only a normalized event; the core never sees card data.
pub trait PaymentGateway {
    fn verify(
        &self,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<PaymentEvent, PaymentGatewayError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentGatewayError(pub String);

impl fmt::Display for PaymentGatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PaymentGatewayError {}

/// Business-rule failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OrderError {
    InvalidIdentifier(String),
    ZeroPrice,
    ZeroRate,
    AmountOverflow,
    InvalidState { action: String, state: String },
    PaymentOrderMismatch,
    PaymentAmountMismatch { expected: u64, actual: u64 },
    PaymentCurrencyMismatch,
    ConflictingPayment,
    RefundExceedsCredit { credit: u128, consumed: u128 },
}

impl fmt::Display for OrderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier(message) => write!(f, "invalid identifier: {message}"),
            Self::ZeroPrice => f.write_str("price is zero"),
            Self::ZeroRate => f.write_str("cycles-per-yen rate is zero"),
            Self::AmountOverflow => f.write_str("cycle amount overflow"),
            Self::InvalidState { action, state } => {
                write!(f, "{action} is invalid in state {state}")
            }
            Self::PaymentOrderMismatch => f.write_str("payment belongs to a different order"),
            Self::PaymentAmountMismatch { expected, actual } => {
                write!(f, "payment amount {actual} does not match {expected}")
            }
            Self::PaymentCurrencyMismatch => f.write_str("unsupported payment currency"),
            Self::ConflictingPayment => f.write_str("order already has a different payment event"),
            Self::RefundExceedsCredit { credit, consumed } => {
                write!(f, "consumed {consumed} exceeds credit {credit}")
            }
        }
    }
}

impl std::error::Error for OrderError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn order() -> Order {
        Order::new(
            OrderId::new("order-1").unwrap(),
            DeploymentRef::new("deployment-1").unwrap(),
            10_000,
            1_000,
        )
        .unwrap()
    }

    fn payment(event: &str, amount: u64) -> PaymentEvent {
        PaymentEvent {
            provider_event_id: event.to_owned(),
            order_id: OrderId::new("order-1").unwrap(),
            amount_minor: amount,
            currency: Currency::Jpy,
        }
    }

    struct FakeGateway {
        grant_result: Result<GrantReceipt, String>,
        reverse_result: Result<(), String>,
    }

    impl CreditGateway for FakeGateway {
        fn grant(&self, _request: &GrantRequest) -> Result<GrantReceipt, String> {
            self.grant_result.clone()
        }

        fn reverse(&self, _request: &RefundRequest) -> Result<(), String> {
            self.reverse_result.clone()
        }
    }

    fn paid_order() -> Order {
        let mut order = order();
        order.submit().unwrap();
        order.apply_payment(&payment("evt-1", 10_000)).unwrap();
        order
    }

    #[test]
    fn price_and_rate_are_snapshotted_with_checked_arithmetic() {
        let order = order();
        assert_eq!(order.credit_cycles(), 10_000_000);
        assert_eq!(order.price_yen(), 10_000);
        assert_eq!(order.cycles_per_yen(), 1_000);
        assert_eq!(
            Order::new(
                OrderId::new("o").unwrap(),
                DeploymentRef::new("d").unwrap(),
                u64::MAX,
                u128::MAX
            ),
            Err(OrderError::AmountOverflow)
        );
    }

    #[test]
    fn payment_replay_is_idempotent_and_conflict_fails_closed() {
        let mut order = order();
        order.submit().unwrap();
        assert_eq!(
            order.apply_payment(&payment("evt-1", 10_000)),
            Ok(PaymentOutcome::Applied)
        );
        assert_eq!(
            order.apply_payment(&payment("evt-1", 10_000)),
            Ok(PaymentOutcome::Replayed)
        );
        assert_eq!(
            order.apply_payment(&payment("evt-2", 10_000)),
            Err(OrderError::ConflictingPayment)
        );
        assert_eq!(*order.state(), OrderState::Paid);
    }

    #[test]
    fn payment_amount_and_order_must_match() {
        let mut order = order();
        order.submit().unwrap();
        assert_eq!(
            order.apply_payment(&payment("evt-1", 9_999)),
            Err(OrderError::PaymentAmountMismatch {
                expected: 10_000,
                actual: 9_999
            })
        );
        let mut other = payment("evt-1", 10_000);
        other.order_id = OrderId::new("order-2").unwrap();
        assert_eq!(
            order.apply_payment(&other),
            Err(OrderError::PaymentOrderMismatch)
        );
    }

    #[test]
    fn grant_drive_uses_order_id_and_converges() {
        let mut order = paid_order();
        let gateway = FakeGateway {
            grant_result: Ok(GrantReceipt { replayed: false }),
            reverse_result: Ok(()),
        };
        order.begin_grant().unwrap();
        let request = order.grant_intent().expect("grant intent");
        assert_eq!(request.order_id.as_str(), "order-1");
        assert_eq!(request.cycles, 10_000_000);
        assert_eq!(
            order.drive_grant(&gateway),
            Ok(GrantDriveOutcome::Completed { replayed: false })
        );
        assert_eq!(*order.state(), OrderState::CreditGranted);
        order.fulfill().unwrap();
        assert_eq!(*order.state(), OrderState::Fulfilled);
    }

    #[test]
    fn grant_failure_is_terminal_and_visible() {
        let mut order = paid_order();
        let gateway = FakeGateway {
            grant_result: Err("canister unavailable".into()),
            reverse_result: Ok(()),
        };
        order.begin_grant().unwrap();
        assert_eq!(
            order.drive_grant(&gateway),
            Ok(GrantDriveOutcome::Failed("canister unavailable".into()))
        );
        assert!(matches!(
            order.state(),
            OrderState::CreditGrantFailed { reason } if reason == "canister unavailable"
        ));
    }

    #[test]
    fn unused_full_grant_uses_the_on_chain_refund_path() {
        let mut order = paid_order();
        order.begin_grant().unwrap();
        order.grant_succeeded().unwrap();
        assert_eq!(order.request_refund(0), Ok(RefundPath::OnChainFull));
        let request = order.refund_intent().expect("refund intent");
        assert_eq!(request.cycles, 10_000_000);
        let gateway = FakeGateway {
            grant_result: Ok(GrantReceipt { replayed: false }),
            reverse_result: Ok(()),
        };
        assert_eq!(
            order.drive_refund(&gateway),
            Ok(RefundDriveOutcome::Completed)
        );
        assert_eq!(
            *order.state(),
            OrderState::Refunded {
                path: RefundPath::OnChainFull
            }
        );
    }

    #[test]
    fn consumed_grant_is_fiat_only_and_never_reverses_credit() {
        let mut order = paid_order();
        order.begin_grant().unwrap();
        order.grant_succeeded().unwrap();
        assert_eq!(order.request_refund(2_000_000), Ok(RefundPath::FiatOnly));
        assert!(order.refund_intent().is_none());
        let gateway = FakeGateway {
            grant_result: Ok(GrantReceipt { replayed: false }),
            reverse_result: Err("must not be called".into()),
        };
        assert_eq!(
            order.drive_refund(&gateway),
            Ok(RefundDriveOutcome::FiatOnly)
        );
        order.refund_completed().unwrap();
        assert_eq!(
            *order.state(),
            OrderState::Refunded {
                path: RefundPath::FiatOnly
            }
        );
        assert_eq!(order.credit_cycles(), 10_000_000);
    }

    #[test]
    fn refund_cannot_exceed_credit_or_precede_grant() {
        let mut order = order();
        assert_eq!(
            order.request_refund(0),
            Err(OrderError::InvalidState {
                action: "request_refund".into(),
                state: "Draft".into()
            })
        );
        order.submit().unwrap();
        order.apply_payment(&payment("evt-1", 10_000)).unwrap();
        assert_eq!(
            order.request_refund(10_000_001),
            Err(OrderError::RefundExceedsCredit {
                credit: 10_000_000,
                consumed: 10_000_001
            })
        );
    }

    #[test]
    fn failed_grant_can_retry_with_the_same_idempotency_key() {
        let mut order = paid_order();
        let failing = FakeGateway {
            grant_result: Err("reply lost".into()),
            reverse_result: Ok(()),
        };
        order.begin_grant().unwrap();
        order.drive_grant(&failing).unwrap();
        assert!(matches!(
            order.state(),
            OrderState::CreditGrantFailed { .. }
        ));
        order.retry_grant().unwrap();
        assert_eq!(order.grant_intent().unwrap().order_id.as_str(), "order-1");
        let succeeding = FakeGateway {
            grant_result: Ok(GrantReceipt { replayed: true }),
            reverse_result: Ok(()),
        };
        assert_eq!(
            order.drive_grant(&succeeding),
            Ok(GrantDriveOutcome::Completed { replayed: true })
        );
        assert_eq!(*order.state(), OrderState::CreditGranted);
    }

    #[test]
    fn rejected_refund_retains_its_path_and_can_retry() {
        let mut order = paid_order();
        order.begin_grant().unwrap();
        order.grant_succeeded().unwrap();
        order.request_refund(0).unwrap();
        let failing = FakeGateway {
            grant_result: Ok(GrantReceipt { replayed: false }),
            reverse_result: Err("transient".into()),
        };
        assert_eq!(
            order.drive_refund(&failing),
            Ok(RefundDriveOutcome::Failed("transient".into()))
        );
        assert!(matches!(
            order.state(),
            OrderState::RefundRejected {
                path: RefundPath::OnChainFull,
                ..
            }
        ));
        order.retry_refund().unwrap();
        assert!(order.refund_intent().is_some());
        let succeeding = FakeGateway {
            grant_result: Ok(GrantReceipt { replayed: false }),
            reverse_result: Ok(()),
        };
        assert_eq!(
            order.drive_refund(&succeeding),
            Ok(RefundDriveOutcome::Completed)
        );
    }

    #[test]
    fn identifiers_are_bounded_and_non_empty() {
        assert!(OrderId::new("").is_err());
        assert!(OrderId::new("x".repeat(MAX_IDENTIFIER_LEN + 1)).is_err());
        assert!(DeploymentRef::new("d").is_ok());
    }
}
