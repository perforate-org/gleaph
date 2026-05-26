use crate::token::Span;

use super::catalog::{ObjectName, Statement};
use super::expr::Expr;
use super::query::{BindingTypeAnnotation, TypedPrefix, YieldItem};

// ════════════════════════════════════════════════════════════════════════════════
// §6 — Top-level program
// ════════════════════════════════════════════════════════════════════════════════

/// Top-level GQL program: an optional session activity followed by an optional
/// transaction activity.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct GqlProgram {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub session_activity: Vec<SessionCommand>,
    pub transaction_activity: Option<TransactionActivity>,
}

/// A transaction activity contains an optional start-transaction command, a
/// statement block, and an optional commit/rollback.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct TransactionActivity {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub start: Option<StartTransactionCommand>,
    pub body: Option<StatementBlock>,
    pub end: Option<TransactionEnd>,
}

/// A statement block: a primary statement optionally followed by NEXT-chained
/// statements (GQL `statementBlock`).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct StatementBlock {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub first: Statement,
    pub next: Vec<NextStatement>,
}

impl StatementBlock {
    /// Iterate over all statements in the block (first + chained).
    pub fn iter_statements(&self) -> impl Iterator<Item = &Statement> {
        std::iter::once(&self.first).chain(self.next.iter().map(|n| &n.statement))
    }
}

/// A NEXT-chained statement with optional YIELD clause
/// (GQL `nextStatement`).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct NextStatement {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    pub yield_items: Option<Vec<YieldItem>>,
    pub statement: Statement,
}

/// How to end a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum TransactionEnd {
    Commit,
    Rollback,
}

// ════════════════════════════════════════════════════════════════════════════════
// §7 — Session commands
// ════════════════════════════════════════════════════════════════════════════════

/// A session command (SET, RESET, or CLOSE).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum SessionCommand {
    Set(SessionSetCommand),
    Reset(SessionResetCommand),
    Close,
}

/// SESSION SET — set a session attribute.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum SessionSetCommand {
    /// SESSION SET SCHEMA <catalog-qualified name>
    Schema(ObjectName),
    /// SESSION SET [PROPERTY] GRAPH <graph name>
    Graph {
        property_keyword: bool,
        name: ObjectName,
    },
    /// SESSION SET TIME ZONE <value>
    TimeZone(Box<Expr>),
    /// SESSION SET VALUE [IF NOT EXISTS] $name [TYPED|:: type] = <value>
    Parameter {
        if_not_exists: bool,
        name: String,
        typed_prefix: TypedPrefix,
        type_annotation: Option<BindingTypeAnnotation>,
        value: Box<Expr>,
    },
    /// SESSION SET [PROPERTY] GRAPH [IF NOT EXISTS] $name [TYPED|:: type] = <graph-expr>
    GraphParameter {
        property_keyword: bool,
        if_not_exists: bool,
        name: String,
        typed_prefix: TypedPrefix,
        type_annotation: Option<BindingTypeAnnotation>,
        value: Box<Expr>,
    },
    /// SESSION SET [BINDING] TABLE [IF NOT EXISTS] $name [TYPED|:: type] = <table-expr>
    BindingTableParameter {
        binding_keyword: bool,
        if_not_exists: bool,
        name: String,
        typed_prefix: TypedPrefix,
        type_annotation: Option<BindingTypeAnnotation>,
        value: Box<Expr>,
    },
}

/// SESSION RESET — reset a session attribute.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum SessionResetCommand {
    /// SESSION RESET (no arguments — reset everything)
    All,
    /// RESET [ALL] PARAMETERS
    AllParameters {
        /// Whether `ALL` was explicitly specified.
        all_keyword: bool,
    },
    /// RESET [ALL] CHARACTERISTICS
    AllCharacteristics {
        /// Whether `ALL` was explicitly specified.
        all_keyword: bool,
    },
    /// RESET SCHEMA
    Schema,
    /// RESET [PROPERTY] GRAPH
    Graph { property_keyword: bool },
    /// RESET TIME ZONE
    TimeZone,
    /// RESET [PARAMETER] $name
    Parameter {
        parameter_keyword: bool,
        name: String,
    },
}

// ════════════════════════════════════════════════════════════════════════════════
// §8 — Transaction commands
// ════════════════════════════════════════════════════════════════════════════════

/// START TRANSACTION with optional transaction characteristics.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct StartTransactionCommand {
    #[cfg_attr(feature = "ast-rkyv-no-span", rkyv(with = rkyv::with::Skip))]
    pub span: Span,
    /// Transaction characteristics (may include multiple comma-separated modes).
    pub access_modes: Vec<TransactionAccessMode>,
}

/// Transaction access mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "ast-rkyv-no-span",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum TransactionAccessMode {
    ReadOnly,
    ReadWrite,
}
