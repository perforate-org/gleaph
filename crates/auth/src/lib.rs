//! Authorization substrate for Gleaph (Internet Computer graph canisters).
//!
//! Two orthogonal dimensions replace the former five-role ladder ([ADR 0074]):
//!
//! - **Admin capabilities** (`AdminCaps`): a global administrative bitset covering
//!   platform/federation operations, prepared-query registration, index DDL, catalog
//!   management, procedure calls, and grant administration. Seeded via the bootstrap
//!   init path; principals with **no row in stable storage** hold an empty set
//!   (**default deny**).
//! - **Data-plane grants**: `(principal | PUBLIC) × privilege` rows with a dormant
//!   `expires_at` field and an optional compiled conditional-policy predicate
//!   ([ADR 0075] §1). `PUBLIC` is a virtual pseudo-subject resolved at evaluation
//!   time, never persisted as a principal.
//!
//! Default is empty everywhere → deny. Administrative capability never implies
//! data-plane access (ADR 0074 invariant 1). Enforced on the **router** canister;
//! graph shards trust the router as the only GQL entrypoint.
//!
//! [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
//! [ADR 0075]: https://github.com/gleaph/gleaph/blob/main/design/adr/0075-conditional-policies-constant-pushdown.md

use candid::{CandidType, Principal};
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;
use std::rc::Rc;

bitflags::bitflags!(
    /// Global administrative capabilities ([ADR 0074] §1).
    ///
    /// Succeeds the residue of the former role ladder: `PREPARE_REGISTER`, `INDEX_CREATE`,
    /// and `INDEX_DROP` migrate the old `ManagerCapability` bits; the remaining bits cover
    /// the former Manager/Admin authority split into narrowest governing capabilities.
    ///
    /// [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct AdminCaps: u64 {
        /// Register/replace/drop prepared queries.
        const PREPARE_REGISTER = 1 << 0;
        /// Create property indexes (GQL `CREATE INDEX` and vector-index DDL).
        const INDEX_CREATE = 1 << 1;
        /// Drop property indexes (GQL `DROP INDEX` and vector-index DDL).
        const INDEX_DROP = 1 << 2;
        /// Catalog DDL: graph-type catalog statements, schema migrations, catalog interning.
        const MANAGE_CATALOG = 1 << 3;
        /// Named `CALL` procedures (until procedures become catalog objects).
        const CALL_PROCEDURE = 1 << 4;
        /// Federation topology: graph/shard registration, backfill, maintenance sweeps,
        /// dispatch activation, diagnostics.
        const MANAGE_FEDERATION = 1 << 5;
        /// Grant administration: writing other principals' capability rows.
        const MANAGE_AUTHORIZATION = 1 << 6;
        /// Emergency self-elevation ([ADR 0080] §3): writes flagged, approval-free
        /// metadata elevation rows with approver = requester.
        const EMERGENCY_ELEVATE = 1 << 7;
    }
);

impl AdminCaps {
    /// Stable bit names, in bit order. Used by introspection surfaces.
    pub const NAMES: [&'static str; 8] = [
        "PREPARE_REGISTER",
        "INDEX_CREATE",
        "INDEX_DROP",
        "MANAGE_CATALOG",
        "CALL_PROCEDURE",
        "MANAGE_FEDERATION",
        "MANAGE_AUTHORIZATION",
        "EMERGENCY_ELEVATE",
    ];

    /// Names of the set bits, in bit order.
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .enumerate()
            .filter(|(bit, _)| self.bits() & (1 << bit) != 0)
            .map(|(_, name)| *name)
            .collect()
    }
}

impl fmt::Display for AdminCaps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.names().join(","))
    }
}

/// Failure modes for privileged authorization writes.
///
/// The anonymous principal must never receive a persisted privileged row, and an
/// elevation row is only complete with a real justification, so write APIs reject both
/// before mutating stable storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthWriteError {
    /// A privileged write or bootstrap targeted [`Principal::anonymous`].
    AnonymousPrincipal,
    /// An elevation row was written with an empty justification ([ADR 0080] §3).
    EmptyJustification,
    /// An elevation justification exceeded [`MAX_ELEVATION_JUSTIFICATION_BYTES`].
    JustificationTooLong(usize),
}

impl fmt::Display for AuthWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthWriteError::AnonymousPrincipal => {
                f.write_str("anonymous principal cannot hold a stored authorization row")
            }
            AuthWriteError::EmptyJustification => {
                f.write_str("elevation justification must not be empty")
            }
            AuthWriteError::JustificationTooLong(max) => {
                write!(f, "elevation justification exceeds {max} bytes")
            }
        }
    }
}

impl std::error::Error for AuthWriteError {}

/// Authoritative, memory-independent validation of bootstrap principals.
///
/// This is the single source of truth for the rule "no anonymous bootstrap identity". Both the
/// stateful [`AuthState::bootstrap_principals`] write path and pre-mutation init preflight (e.g.
/// the router canister `init`) call this so the rule is enforced before any stable structure is
/// cleared or written, and is never duplicated.
pub fn validate_bootstrap_principals(
    issuing_principal: Principal,
    initial_admins: &[Principal],
) -> Result<(), AuthWriteError> {
    if issuing_principal == Principal::anonymous()
        || initial_admins.iter().any(|p| *p == Principal::anonymous())
    {
        return Err(AuthWriteError::AnonymousPrincipal);
    }
    Ok(())
}

/// Stored administrative-capability row for one principal.
///
/// Fresh-state format ([ADR 0074] §6): exactly 8 little-endian capability bits. Legacy
/// role-ladder bytes (9+ bytes) are rejected by the decoder rather than interpreted.
///
/// [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapsRecord {
    pub caps: u64,
}

impl Storable for CapsRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.caps.to_le_bytes().to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.caps.to_le_bytes().to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        assert!(
            b.len() == 8,
            "CapsRecord expects exactly 8 bytes, got {}",
            b.len()
        );
        Self {
            caps: u64::from_le_bytes(b.try_into().unwrap()),
        }
    }
}

/// Stable principal → administrative-capability map.
pub struct AuthState<M: Memory> {
    map: StableBTreeMap<Principal, CapsRecord, M>,
}

impl<M: Memory> AuthState<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn get_record(&self, p: &Principal) -> Option<CapsRecord> {
        self.map.get(p)
    }

    /// Effective capabilities for authorization: empty for unknown principals.
    ///
    /// Defense in depth: the anonymous principal never resolves to any capability, even if a
    /// corrupt privileged row exists in stable storage. All effective-authorization reads derive
    /// from this method, so anonymous always resolves to the empty set (default deny).
    pub fn caps_of(&self, p: &Principal) -> AdminCaps {
        if *p == Principal::anonymous() {
            return AdminCaps::empty();
        }
        self.get_record(p)
            .map(|r| AdminCaps::from_bits_truncate(r.caps))
            .unwrap_or(AdminCaps::empty())
    }

    /// Whether `p` holds `cap`.
    pub fn has_cap(&self, p: &Principal, cap: AdminCaps) -> bool {
        self.caps_of(p).contains(cap)
    }

    /// Insert or replace the full capability row (grant administration).
    ///
    /// Rejects [`Principal::anonymous`] before any mutation so a privileged row can never be
    /// persisted for the anonymous principal.
    pub fn upsert_caps(&mut self, p: Principal, caps: AdminCaps) -> Result<(), AuthWriteError> {
        if p == Principal::anonymous() {
            return Err(AuthWriteError::AnonymousPrincipal);
        }
        self.map.insert(p, CapsRecord { caps: caps.bits() });
        Ok(())
    }

    /// Bootstrap: grant the full capability set to `issuing_principal` and every entry in
    /// `initial_admins`.
    ///
    /// All-or-nothing: if the issuing principal or any initial admin is [`Principal::anonymous`],
    /// no rows are inserted and [`AuthWriteError::AnonymousPrincipal`] is returned.
    pub fn bootstrap_principals(
        &mut self,
        issuing_principal: Principal,
        initial_admins: &[Principal],
    ) -> Result<(), AuthWriteError> {
        validate_bootstrap_principals(issuing_principal, initial_admins)?;
        self.upsert_caps(issuing_principal, AdminCaps::all())?;
        for p in initial_admins {
            if *p != issuing_principal {
                self.upsert_caps(*p, AdminCaps::all())?;
            }
        }
        Ok(())
    }

    pub fn len(&self) -> u64 {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Operation of a data-plane grant ([ADR 0074] §2).
///
/// Slice 2a adds the GRANT/REVOKE grammar surface: [`Privilege::Graph`] rows are created
/// through owner-only `GRANT` statements and consumed by later plan-time enforcement
/// slices. Each variant carries its own resource payload so impossible combinations
/// (e.g. a direction modifier on a prepared query) cannot be constructed.
///
/// [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
#[derive(Clone, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub enum Privilege {
    /// `EXECUTE ON PREPARED QUERY <name>`; the name is the Router-global prepared
    /// operation name (ADR 0063).
    ExecutePreparedQuery { name: String },
    /// Data-plane graph privilege ([ADR 0074] §2): one `(operation, resource)` pair over
    /// one graph, e.g. `TRAVERSE OUTGOING ON EDGES KNOWS`.
    Graph(GraphPrivilege),
    /// Metadata-plane elevation ([ADR 0080] §1): `ReadMetadata` over the scope. Shares
    /// storage and grammar with data-plane rows but never coverage semantics — the
    /// leading discriminant makes metadata and data-plane canonical keys disjoint, so an
    /// exact-key lookup for one plane can never be satisfied by a row of the other.
    Metadata(MetadataScope),
}

/// One data-plane `(operation, resource)` authorization pair over one graph.
///
/// The `graph` discriminator is an opaque identifier owned by the embedding system (the
/// Router's logical `GraphId`). It is part of the canonical key so that two graphs which
/// independently allocated the same numeric label/property ids never collide into one
/// grant row.
///
/// Serialization derives let plan-time requirement sets ([ADR 0074] §4) embed these rows
/// in durable records (e.g. the Router prepared-query record) without a mirrored shape:
/// this type stays the single representation of one grantable row.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub struct GraphPrivilege {
    pub graph: u32,
    pub operation: GraphOperation,
    pub resource: GraphResource,
}

/// Operation half of a [`GraphPrivilege`] ([ADR 0074] §2).
///
/// `Traverse` optionally carries a directional modifier (`OUTGOING`/`INCOMING`). `None`
/// means traversal without an orientation requirement (undirected edge labels, vertex
/// selectors); directed-edge grants normalize an omitted modifier into both directional
/// rows before storage, so `None` never stands for BOTH on a directed label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub enum GraphOperation {
    Match,
    Traverse(Option<Direction>),
    Read,
    ReadProperty,
    Create,
    Update,
    Delete,
}

/// Logical traversal direction of a directed-edge privilege (`OUTGOING = source → target`,
/// [ADR 0074] §2). Graph semantics, independent of physical storage orientation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub enum Direction {
    Outgoing,
    Incoming,
}

impl Direction {
    fn discriminant(self) -> u8 {
        match self {
            Direction::Outgoing => 1,
            Direction::Incoming => 2,
        }
    }

    fn from_discriminant(byte: u8) -> Self {
        match byte {
            1 => Direction::Outgoing,
            2 => Direction::Incoming,
            other => panic!("corrupt grant key: unknown direction byte {other}"),
        }
    }
}

/// Resource half of a [`GraphPrivilege`] ([ADR 0074] §2).
///
/// Ids are opaque graph-scoped catalog ids assigned by the embedding system. Phase 1 has
/// no edge-property resource; property-level reads attach to vertex labels only.
///
/// `AllVertexLabels` is the wildcard resource for `NODES *` ([ADR 0089] §5): a single
/// grant row covering "match any vertex label" instead of enumerating every label. The
/// resource carries no payload (zero bytes after the kind byte), so its key sorts
/// before any concrete `VertexLabel(u32)` for the same subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub enum GraphResource {
    VertexLabel(u32),
    EdgeLabel(u32),
    VertexProperty {
        label: u32,
        property: u32,
    },
    /// Wildcard: covers every vertex label in the graph (`NODES *`, [ADR 0089] §5).
    AllVertexLabels,
}

/// Resource scope of a metadata-plane elevation ([ADR 0080] §1).
///
/// `Graph` scopes one elevation to a single graph's metadata plane (topology, schema
/// dictionary, shard registry); `ControlPlane` is cross-graph operational scope. The only
/// metadata operation in this phase is `ReadMetadata`, so the operation is not part of
/// the encoding — a second metadata operation would be a fresh-state format change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub enum MetadataScope {
    /// One graph's metadata plane, keyed by the Router's logical `GraphId`.
    Graph(u32),
    /// Cross-graph operational scope (sweeps, fleet tooling).
    ControlPlane,
}

impl MetadataScope {
    fn kind(&self) -> u8 {
        match self {
            MetadataScope::Graph(_) => 0,
            MetadataScope::ControlPlane => 1,
        }
    }

    fn payload_bytes(&self) -> Vec<u8> {
        match self {
            MetadataScope::Graph(graph) => graph.to_le_bytes().to_vec(),
            MetadataScope::ControlPlane => Vec::new(),
        }
    }

    fn decode(kind: u8, payload: &[u8]) -> Self {
        match (kind, payload.len()) {
            (0, 4) => MetadataScope::Graph(u32::from_le_bytes(
                payload.try_into().expect("scope graph payload"),
            )),
            (1, 0) => MetadataScope::ControlPlane,
            (kind @ (0 | 1), n) => {
                panic!("corrupt grant key: metadata scope kind {kind} with {n} payload bytes")
            }
            (kind, _) => panic!("corrupt grant key: unknown metadata scope kind {kind}"),
        }
    }
}

impl GraphResource {
    fn kind(&self) -> u8 {
        match self {
            GraphResource::VertexLabel(_) => 0,
            GraphResource::EdgeLabel(_) => 1,
            GraphResource::VertexProperty { .. } => 2,
            GraphResource::AllVertexLabels => 3,
        }
    }

    fn payload_bytes(&self) -> Vec<u8> {
        match self {
            GraphResource::VertexLabel(label) | GraphResource::EdgeLabel(label) => {
                label.to_le_bytes().to_vec()
            }
            GraphResource::VertexProperty { label, property } => {
                let mut v = Vec::with_capacity(8);
                v.extend_from_slice(&label.to_le_bytes());
                v.extend_from_slice(&property.to_le_bytes());
                v
            }
            GraphResource::AllVertexLabels => Vec::new(),
        }
    }

    fn decode(kind: u8, payload: &[u8]) -> Self {
        let read_u32 = |off: usize| -> u32 {
            u32::from_le_bytes(
                payload[off..off + 4]
                    .try_into()
                    .expect("corrupt grant key: truncated resource id"),
            )
        };
        match (kind, payload.len()) {
            (0, 4) => GraphResource::VertexLabel(read_u32(0)),
            (1, 4) => GraphResource::EdgeLabel(read_u32(0)),
            (2, 8) => GraphResource::VertexProperty {
                label: read_u32(0),
                property: read_u32(4),
            },
            (3, 0) => GraphResource::AllVertexLabels,
            (kind @ 0..=3, n) => {
                panic!("corrupt grant key: resource kind {kind} with {n} payload bytes")
            }
            (kind, _) => panic!("corrupt grant key: unknown resource kind {kind}"),
        }
    }
}

impl Privilege {
    /// Operation discriminator used as the leading byte of the stable grant key.
    fn discriminant(&self) -> u8 {
        match self {
            Privilege::ExecutePreparedQuery { .. } => 1,
            Privilege::Graph(_) => 2,
            Privilege::Metadata(_) => 3,
        }
    }

    /// Variable resource payload following the discriminant in the stable grant key.
    fn resource_bytes(&self) -> Vec<u8> {
        match self {
            Privilege::ExecutePreparedQuery { name } => name.as_bytes().to_vec(),
            Privilege::Graph(graph_privilege) => {
                let mut v = Vec::with_capacity(16);
                v.extend_from_slice(&graph_privilege.graph.to_le_bytes());
                let operation = &graph_privilege.operation;
                v.push(match operation {
                    GraphOperation::Match => 0,
                    GraphOperation::Traverse(_) => 1,
                    GraphOperation::Read => 2,
                    GraphOperation::ReadProperty => 3,
                    GraphOperation::Create => 4,
                    GraphOperation::Update => 5,
                    GraphOperation::Delete => 6,
                });
                if let GraphOperation::Traverse(direction) = operation {
                    v.push(direction.map(Direction::discriminant).unwrap_or(0));
                }
                v.push(graph_privilege.resource.kind());
                v.extend_from_slice(&graph_privilege.resource.payload_bytes());
                v
            }
            Privilege::Metadata(scope) => {
                let mut v = Vec::with_capacity(5);
                v.push(scope.kind());
                v.extend_from_slice(&scope.payload_bytes());
                v
            }
        }
    }

    /// Decode the privilege encoded in a canonical grant-key resource payload.
    ///
    /// Total for payloads produced by [`Self::resource_bytes`]; malformed encodings trap
    /// (corrupt stable state is not recoverable input).
    fn decode(discriminant: u8, resource: &[u8]) -> Self {
        match discriminant {
            1 => Privilege::ExecutePreparedQuery {
                name: String::from_utf8(resource.to_vec())
                    .expect("corrupt grant key: non-utf8 prepared query name"),
            },
            3 => {
                assert!(
                    !resource.is_empty(),
                    "corrupt grant key: truncated metadata scope"
                );
                Privilege::Metadata(MetadataScope::decode(resource[0], &resource[1..]))
            }
            2 => {
                assert!(
                    resource.len() >= 6,
                    "corrupt grant key: truncated graph privilege"
                );
                let graph = u32::from_le_bytes(resource[0..4].try_into().unwrap());
                let operation_byte = resource[4];
                let mut at = 5usize;
                let operation = match operation_byte {
                    0 => GraphOperation::Match,
                    1 => {
                        let direction = match resource[at] {
                            0 => None,
                            d => Some(Direction::from_discriminant(d)),
                        };
                        at += 1;
                        GraphOperation::Traverse(direction)
                    }
                    2 => GraphOperation::Read,
                    3 => GraphOperation::ReadProperty,
                    4 => GraphOperation::Create,
                    5 => GraphOperation::Update,
                    6 => GraphOperation::Delete,
                    other => panic!("corrupt grant key: unknown graph operation {other}"),
                };
                let resource_kind = resource[at];
                at += 1;
                let resource = GraphResource::decode(resource_kind, &resource[at..]);
                Privilege::Graph(GraphPrivilege {
                    graph,
                    operation,
                    resource,
                })
            }
            other => panic!("corrupt grant key: unknown privilege discriminant {other}"),
        }
    }
}

/// Subject of a data-plane grant: a concrete principal or the virtual `PUBLIC`
/// pseudo-subject ([ADR 0074] §1). `Public` is resolved at evaluation time and is never a
/// persisted principal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantSubject {
    Principal(Principal),
    Public,
}

impl GrantSubject {
    fn kind(&self) -> u8 {
        match self {
            GrantSubject::Public => 0,
            GrantSubject::Principal(_) => 1,
        }
    }

    /// Exact principal blob. Length-prefixed in the stable key, so distinct principals of any
    /// IC-supported length map to distinct keys.
    fn principal_bytes(&self) -> Vec<u8> {
        match self {
            GrantSubject::Principal(p) => p.as_slice().to_vec(),
            GrantSubject::Public => Vec::new(),
        }
    }

    /// Canonical subject of an evaluation for caller `p`: the principal itself, except that
    /// the anonymous principal evaluates as `Public` (it cannot hold stored rows, so its only
    /// reachable grants are the PUBLIC baseline).
    pub fn effective_for(p: &Principal) -> Self {
        if *p == Principal::anonymous() {
            GrantSubject::Public
        } else {
            GrantSubject::Principal(*p)
        }
    }
}

/// Canonical stable key of one grant row: `op ‖ resource ‖ subject`.
///
/// All subjects of one privilege sort adjacently (resource prefix), so cascade scans over a
/// privilege read a contiguous range. Lookup is always by exact canonical key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GrantKey(Vec<u8>);

impl GrantKey {
    pub fn new(privilege: &Privilege, subject: &GrantSubject) -> Self {
        let resource = privilege.resource_bytes();
        let principal = subject.principal_bytes();
        let mut key = Vec::with_capacity(1 + 2 + resource.len() + 1 + 2 + principal.len());
        key.push(privilege.discriminant());
        key.extend_from_slice(&(resource.len() as u16).to_le_bytes());
        key.extend_from_slice(&resource);
        key.push(subject.kind());
        key.extend_from_slice(&(principal.len() as u16).to_le_bytes());
        key.extend_from_slice(&principal);
        Self(key)
    }

    /// Decode the canonical key parts. Decoding is total for keys produced by [`Self::new`];
    /// malformed tails trap (corrupt stable state is not recoverable input).
    #[cfg(test)]
    pub(crate) fn parts(&self) -> (u8, String, u8) {
        let b = &self.0;
        let op = b[0];
        let resource_len = u16::from_le_bytes(b[1..3].try_into().unwrap()) as usize;
        let resource = String::from_utf8(b[3..3 + resource_len].to_vec()).expect("utf8 resource");
        let subject_kind = b[3 + resource_len];
        (op, resource, subject_kind)
    }

    /// Decode the privilege and subject this canonical key addresses.
    ///
    /// Total for keys produced by [`Self::new`]; malformed encodings trap (corrupt stable
    /// state is not recoverable input).
    pub fn decode(&self) -> (Privilege, GrantSubject) {
        let b = &self.0;
        assert!(
            b.len() >= 4,
            "corrupt grant key: shorter than the fixed header"
        );
        let discriminant = b[0];
        let resource_len =
            u16::from_le_bytes(b[1..3].try_into().expect("corrupt grant key")) as usize;
        let subject_kind_at = 3 + resource_len;
        assert!(
            b.len() >= subject_kind_at + 3,
            "corrupt grant key: truncated subject"
        );
        let privilege = Privilege::decode(discriminant, &b[3..subject_kind_at]);
        let principal_len = u16::from_le_bytes(
            b[subject_kind_at + 1..subject_kind_at + 3]
                .try_into()
                .expect("corrupt grant key"),
        ) as usize;
        let principal_at = subject_kind_at + 3;
        assert!(
            b.len() == principal_at + principal_len,
            "corrupt grant key: trailing bytes after subject"
        );
        let subject = match b[subject_kind_at] {
            0 => GrantSubject::Public,
            1 => GrantSubject::Principal(Principal::from_slice(
                &b[principal_at..principal_at + principal_len],
            )),
            kind => panic!("corrupt grant key: unknown subject kind {kind}"),
        };
        (privilege, subject)
    }
}

impl Storable for GrantKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
    }
}

/// Comparison operator of a compiled conditional-policy comparison ([ADR 0075] §2).
///
/// Stable discriminants are the [`PredicateOp`] wire encoding; they never change
/// meaning across releases (pre-production fresh-state contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub enum PredicateOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl PredicateOp {
    fn discriminant(self) -> u8 {
        match self {
            PredicateOp::Eq => 0,
            PredicateOp::Ne => 1,
            PredicateOp::Lt => 2,
            PredicateOp::Le => 3,
            PredicateOp::Gt => 4,
            PredicateOp::Ge => 5,
        }
    }

    fn from_discriminant(byte: u8) -> Self {
        match byte {
            0 => PredicateOp::Eq,
            1 => PredicateOp::Ne,
            2 => PredicateOp::Lt,
            3 => PredicateOp::Le,
            4 => PredicateOp::Gt,
            5 => PredicateOp::Ge,
            other => panic!("corrupt grant row: unknown predicate op {other}"),
        }
    }
}

/// Scalar literal of a compiled comparison, restricted to catalog scalar kinds
/// ([ADR 0075] §2). The compiler rejects literals whose kind does not match the
/// property's declared scalar type.
#[derive(Clone, Debug, PartialEq, CandidType, serde::Serialize, serde::Deserialize)]
pub enum PredicateLiteral {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

/// Equality on encoded bytes; float payloads compare bitwise like the stable encoding.
impl Eq for PredicateLiteral {}

/// Right-hand side of one compiled comparison ([ADR 0075] §2 `ValueExpr`).
///
/// `MsgCaller` is stored unresolved: the Router substitutes the invoking caller as a
/// literal constant at execution time ([ADR 0075] §5); no identity is ever persisted.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub enum PredicateValue {
    /// Scalar literal of the property's catalog scalar type.
    Literal(PredicateLiteral),
    /// `MSG_CALLER()` — resolved per execution by the Router.
    MsgCaller,
}

/// One AND-conjunct: `<property> <op> <value>` over the policy selector's label
/// ([ADR 0075] §2). The property id is graph-scoped and monotonic, so vocabulary-drop
/// sweeps address predicate references exactly like grant resources.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub struct PredicateComparison {
    pub property: u32,
    pub op: PredicateOp,
    pub value: PredicateValue,
}

impl PredicateComparison {
    fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(14);
        v.extend_from_slice(&self.property.to_le_bytes());
        v.push(self.op.discriminant());
        match &self.value {
            PredicateValue::MsgCaller => v.push(0),
            PredicateValue::Literal(literal) => {
                let (tag, payload): (u8, [u8; 16]) = match literal {
                    PredicateLiteral::Bool(b) => (0, {
                        let mut p = [0u8; 16];
                        p[0] = u8::from(*b);
                        p
                    }),
                    PredicateLiteral::Int(i) => {
                        let mut p = [0u8; 16];
                        p[..8].copy_from_slice(&i.to_le_bytes());
                        (1, p)
                    }
                    PredicateLiteral::Float(f) => {
                        let mut p = [0u8; 16];
                        p[..8].copy_from_slice(&f.to_le_bytes());
                        (2, p)
                    }
                    PredicateLiteral::String(s) => (3, prefix_bytes(s)),
                };
                v.push(1);
                v.push(tag);
                v.extend_from_slice(&payload);
            }
        }
        v
    }

    fn decode(bytes: &[u8]) -> Self {
        assert!(
            bytes.len() >= 6,
            "corrupt grant row: truncated policy comparison"
        );
        let property = u32::from_le_bytes(bytes[0..4].try_into().expect("property payload"));
        let op = PredicateOp::from_discriminant(bytes[4]);
        let value = match bytes[5] {
            0 => PredicateValue::MsgCaller,
            1 => {
                assert!(
                    bytes.len() >= 23,
                    "corrupt grant row: truncated literal comparison"
                );
                let tag = bytes[6];
                let payload: [u8; 16] = bytes[7..23].try_into().expect("literal payload");
                let literal = match tag {
                    0 => PredicateLiteral::Bool(payload[0] == 1),
                    1 => PredicateLiteral::Int(i64::from_le_bytes(
                        payload[..8].try_into().expect("int payload"),
                    )),
                    2 => PredicateLiteral::Float(f64::from_le_bytes(
                        payload[..8].try_into().expect("float payload"),
                    )),
                    3 => PredicateLiteral::String(decode_prefix_bytes(&payload)),
                    other => panic!("corrupt grant row: unknown literal kind {other}"),
                };
                PredicateValue::Literal(literal)
            }
            other => panic!("corrupt grant row: unknown comparison value kind {other}"),
        };
        Self {
            property,
            op,
            value,
        }
    }
}

/// Fixed-width string carrier for stable encoding: length-prefixed UTF-8 in the first
/// byte, content in the remaining bytes. Longer literals cannot be expressed by the DSL
/// (the compiler rejects them at GRANT time), so decoding treats an over-length prefix
/// as corrupt state.
fn prefix_bytes(s: &str) -> [u8; 16] {
    let bytes = s.as_bytes();
    assert!(
        bytes.len() <= 15,
        "policy string literals are capped at 15 bytes"
    );
    let mut out = [0u8; 16];
    out[0] = bytes.len() as u8;
    out[1..=bytes.len()].copy_from_slice(bytes);
    out
}

fn decode_prefix_bytes(payload: &[u8; 16]) -> String {
    let len = payload[0] as usize;
    assert!(
        len <= 15,
        "corrupt grant row: string length {len} overflows"
    );
    String::from_utf8(payload[1..=len].to_vec())
        .expect("corrupt grant row: non-utf8 policy literal")
}

/// A compiled conditional-policy predicate ([ADR 0075] §1–§2, extended by
/// [ADR 0082] §1–§4): a bounded AND-only conjunction of catalog-checked comparisons
/// over one vertex label, plus an optional bounded EXISTS-traversal chain.
///
/// This is the canonical storage form attached to a grant row. Compilation from syntax,
/// catalog validation, and lowering into plan machinery all happen in the embedding
/// system (the Router); this crate owns only the deterministic representation and its
/// encoding.
///
/// The conjunction depth cap ([`MAX_PREDICATE_CONJUNCTS`]) bounds both the encoded row
/// size and lowering work. An empty conjunction is not representable: a grant either
/// carries a predicate or it does not (`GrantRow.predicate`).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub struct CompiledPredicate {
    /// Vertex label id the comparisons evaluate against (graph-scoped, monotonic).
    pub label: u32,
    pub conjuncts: Vec<PredicateComparison>,
    /// Optional bounded traversal clause ([ADR 0082] §1): the row is visible iff at
    /// least one matching chain exists from the selector vertex.
    pub chain: Option<PredicateChain>,
}

/// Maximum AND-conjuncts per policy predicate ([ADR 0075] §2 determinism bound).
pub const MAX_PREDICATE_CONJUNCTS: usize = 8;

/// Fixed chain-depth bound of a conditional-policy traversal clause ([ADR 0082] §2):
/// exactly the demonstrated direct-grant and org-membership patterns. Not a
/// configurable knob.
pub const MAX_CHAIN_HOPS: usize = 2;

/// Direction of one chain hop ([ADR 0082] §2), following the ADR 0074 §2 directedness
/// rules at GRANT-time validation: an undirected spelling over a directed label means
/// both orientations must be probed; directional spellings over undirected labels are
/// rejected by the compiler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub enum PredicateHopDirection {
    /// `-[:E]->`
    Outgoing,
    /// `<-[:E]-`
    Incoming,
    /// `-[:E]-` (undirected)
    Both,
}

impl PredicateHopDirection {
    fn discriminant(self) -> u8 {
        match self {
            Self::Outgoing => 0,
            Self::Incoming => 1,
            Self::Both => 2,
        }
    }

    fn from_discriminant(tag: u8) -> Self {
        match tag {
            0 => Self::Outgoing,
            1 => Self::Incoming,
            2 => Self::Both,
            other => panic!("corrupt grant row: unknown chain hop direction {other}"),
        }
    }
}

/// One bounded traversal hop ([ADR 0082] §2): expand along one concrete edge label in
/// one direction to vertices of one concrete destination label. Wildcard labels are
/// not representable — the clause enumerates every resource it reads.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub struct PredicateChainHop {
    /// Traversed edge label id (graph-scoped, monotonic).
    pub edge_label: u32,
    pub direction: PredicateHopDirection,
    /// Reached vertex label id (graph-scoped, monotonic).
    pub dest_label: u32,
}

/// The bounded EXISTS clause ([ADR 0082] §1): 1..=[`MAX_CHAIN_HOPS`] hops from the
/// selector vertex, with 1..=[`MAX_PREDICATE_CONJUNCTS`] AND-comparisons evaluated on
/// the terminal destination vertex.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize)]
pub struct PredicateChain {
    pub hops: Vec<PredicateChainHop>,
    pub terminal_conjuncts: Vec<PredicateComparison>,
}

impl PredicateChain {
    fn to_bytes(&self) -> Vec<u8> {
        // Bounds live at the single serialization point ([ADR 0082] §4 determinism):
        // hop depth and terminal conjunction caps are encoding-level facts.
        assert!(
            (1..=MAX_CHAIN_HOPS).contains(&self.hops.len()),
            "policy chain exceeds the hop bound"
        );
        assert!(
            !self.terminal_conjuncts.is_empty(),
            "stored chains hold at least one terminal conjunct"
        );
        assert!(
            self.terminal_conjuncts.len() <= MAX_PREDICATE_CONJUNCTS,
            "policy chain terminal conjunction exceeds the depth cap"
        );
        let mut v = Vec::new();
        v.push(self.hops.len() as u8);
        for hop in &self.hops {
            v.extend_from_slice(&hop.edge_label.to_le_bytes());
            v.push(hop.direction.discriminant());
            v.extend_from_slice(&hop.dest_label.to_le_bytes());
        }
        v.push(self.terminal_conjuncts.len() as u8);
        for conjunct in &self.terminal_conjuncts {
            v.extend_from_slice(&conjunct.to_bytes());
        }
        v
    }

    /// Decodes a chain block from `bytes`, returning the decoded chain and the number
    /// of bytes consumed.
    #[allow(clippy::type_complexity)]
    fn decode(bytes: &[u8]) -> (Self, usize) {
        assert!(
            !bytes.is_empty(),
            "corrupt grant row: truncated chain header"
        );
        let hop_count = bytes[0] as usize;
        assert!(
            (1..=MAX_CHAIN_HOPS).contains(&hop_count),
            "corrupt grant row: chain hop count {hop_count}"
        );
        let mut at = 1usize;
        let mut hops = Vec::with_capacity(hop_count);
        for _ in 0..hop_count {
            assert!(
                bytes.len() >= at + 9,
                "corrupt grant row: truncated chain hop"
            );
            hops.push(PredicateChainHop {
                edge_label: u32::from_le_bytes(
                    bytes[at..at + 4].try_into().expect("edge label payload"),
                ),
                direction: PredicateHopDirection::from_discriminant(bytes[at + 4]),
                dest_label: u32::from_le_bytes(
                    bytes[at + 5..at + 9]
                        .try_into()
                        .expect("dest label payload"),
                ),
            });
            at += 9;
        }
        assert!(
            bytes.len() > at,
            "corrupt grant row: truncated chain conjunct count"
        );
        let terminal_count = bytes[at] as usize;
        at += 1;
        assert!(
            (1..=MAX_PREDICATE_CONJUNCTS).contains(&terminal_count),
            "corrupt grant row: chain terminal conjunct count {terminal_count}"
        );
        let mut terminal_conjuncts = Vec::with_capacity(terminal_count);
        for _ in 0..terminal_count {
            // Comparisons are self-delimiting via their value-kind byte, exactly like
            // source-side conjuncts.
            let width = match bytes.get(at + 5) {
                Some(0) => 6,
                Some(1) => 23,
                _ => panic!("corrupt grant row: malformed comparison value header"),
            };
            terminal_conjuncts.push(PredicateComparison::decode(&bytes[at..at + width]));
            at += width;
        }
        (
            Self {
                hops,
                terminal_conjuncts,
            },
            at,
        )
    }
}

impl CompiledPredicate {
    /// V2 encoding discriminator ([ADR 0082] §4): the version byte leads the payload so
    /// pre-chain V1 bytes are rejected at decode instead of misread. Pre-production
    /// destructive evolution — fresh state required, no decode shims.
    const ENCODING_VERSION: u8 = 2;

    pub(crate) fn encode(&self) -> Vec<u8> {
        assert!(
            self.conjuncts.len() <= MAX_PREDICATE_CONJUNCTS,
            "policy conjunction exceeds the depth cap"
        );
        // Without a chain the row IS its conjunction, so it cannot be empty; with a
        // chain, a pure-EXISTS condition carries zero source conjuncts ([ADR 0082] §2).
        assert!(
            self.chain.is_some() || !self.conjuncts.is_empty(),
            "stored predicates have at least one conjunct"
        );
        let mut v = Vec::new();
        v.push(Self::ENCODING_VERSION);
        v.extend_from_slice(&self.label.to_le_bytes());
        v.push(self.conjuncts.len() as u8);
        for conjunct in &self.conjuncts {
            v.extend_from_slice(&conjunct.to_bytes());
        }
        if let Some(chain) = &self.chain {
            v.push(1);
            v.extend_from_slice(&chain.to_bytes());
        } else {
            v.push(0);
        }
        v
    }

    pub(crate) fn decode(bytes: &[u8]) -> Self {
        assert!(bytes.len() >= 6, "corrupt grant row: truncated predicate");
        assert!(
            bytes[0] == Self::ENCODING_VERSION,
            "corrupt grant row: unknown predicate encoding version {}",
            bytes[0]
        );
        let label = u32::from_le_bytes(bytes[1..5].try_into().expect("label payload"));
        let count = bytes[5] as usize;
        assert!(
            count <= MAX_PREDICATE_CONJUNCTS,
            "corrupt grant row: predicate conjunct count {count}"
        );
        let mut at = 6usize;
        let mut conjuncts = Vec::with_capacity(count);
        for _ in 0..count {
            // Comparisons are self-delimiting via their value-kind byte: literal forms
            // occupy 6 + 17 bytes, MSG_CALLER occupies 6. Scan the fixed header first,
            // then advance by the encoded width of the value.
            let width = match bytes.get(at + 5) {
                Some(0) => 6,
                Some(1) => 23,
                _ => panic!("corrupt grant row: malformed comparison value header"),
            };
            conjuncts.push(PredicateComparison::decode(&bytes[at..at + width]));
            at += width;
        }
        let chain = match bytes.get(at) {
            Some(0) => {
                at += 1;
                // Without a chain the conjunction is the whole condition, so an empty
                // one is corrupt state ([ADR 0075] §1).
                assert!(
                    !conjuncts.is_empty(),
                    "corrupt grant row: predicate conjunct count 0"
                );
                None
            }
            Some(1) => {
                at += 1;
                let (chain, consumed) = PredicateChain::decode(&bytes[at..]);
                at += consumed;
                Some(chain)
            }
            other => panic!(
                "corrupt grant row: unknown chain presence flag {:?}",
                other.copied()
            ),
        };
        assert!(
            at == bytes.len(),
            "corrupt grant row: trailing predicate bytes"
        );
        Self {
            label,
            conjuncts,
            chain,
        }
    }

    /// Canonical inline text ([ADR 0075] §1: introspection prints the condition on the
    /// grant; [ADR 0082] §3 extends it with the chain inline). Names resolve through
    /// catalogs at print time; this form prints monotonic ids so introspection stays
    /// truthful when names change.
    pub fn display_conditions(&self, names: &dyn PredicateNames) -> String {
        let mut out = String::from("WHERE ");
        for (index, conjunct) in self.conjuncts.iter().enumerate() {
            if index > 0 {
                out.push_str(" AND ");
            }
            out.push_str(&display_comparison(conjunct, |id| names.property_name(id)));
        }
        if let Some(chain) = &self.chain {
            if !self.conjuncts.is_empty() {
                out.push_str(" AND ");
            }
            out.push_str("EXISTS { ");
            out.push_str(&display_chain(self.label, chain, names));
            out.push_str(" }");
            return out;
        }
        out
    }
}

/// Catalog name resolution for conditional-policy introspection ([ADR 0075] §1,
/// extended by [ADR 0082] §3): every referenced id resolves at print time, and
/// unresolvable ids print as `<kind id>` so the text never lies after renames.
pub trait PredicateNames {
    fn property_name(&self, id: u32) -> Option<String>;
    fn edge_label_name(&self, id: u32) -> Option<String>;
    fn vertex_label_name(&self, id: u32) -> Option<String>;
}

/// Renders one comparison with a resolved property name.
fn display_comparison(
    conjunct: &PredicateComparison,
    name: impl Fn(u32) -> Option<String>,
) -> String {
    let op = match conjunct.op {
        PredicateOp::Eq => "=",
        PredicateOp::Ne => "<>",
        PredicateOp::Lt => "<",
        PredicateOp::Le => "<=",
        PredicateOp::Gt => ">",
        PredicateOp::Ge => ">=",
    };
    let name =
        name(conjunct.property).unwrap_or_else(|| format!("<property {}>", conjunct.property));
    format!("{name} {op} {}", display_value(&conjunct.value))
}

/// Renders one chain as its bounded pattern plus the terminal WHERE group
/// ([ADR 0082] §2 grammar shape).
fn display_chain(
    selector_label: u32,
    chain: &PredicateChain,
    names: &dyn PredicateNames,
) -> String {
    use std::fmt::Write as _;
    let label_text = |label: u32| {
        names
            .vertex_label_name(label)
            .unwrap_or_else(|| format!("<label {label}>"))
    };
    let mut pattern = format!("(:{})", label_text(selector_label));
    for hop in &chain.hops {
        let edge = names
            .edge_label_name(hop.edge_label)
            .unwrap_or_else(|| format!("<edge {}>", hop.edge_label));
        let arrow = match hop.direction {
            PredicateHopDirection::Outgoing => "->",
            PredicateHopDirection::Incoming => "<-",
            PredicateHopDirection::Both => "-",
        };
        let _ = write!(
            pattern,
            "-[:{edge}]{arrow}(:{})",
            label_text(hop.dest_label)
        );
    }
    let mut out = pattern;
    out.push_str(" WHERE ");
    for (index, conjunct) in chain.terminal_conjuncts.iter().enumerate() {
        if index > 0 {
            out.push_str(" AND ");
        }
        out.push_str(&display_comparison(conjunct, |id| names.property_name(id)));
    }
    out
}

fn display_value(value: &PredicateValue) -> String {
    match value {
        PredicateValue::MsgCaller => "MSG_CALLER()".to_owned(),
        PredicateValue::Literal(PredicateLiteral::Bool(b)) => b.to_string(),
        PredicateValue::Literal(PredicateLiteral::Int(i)) => i.to_string(),
        PredicateValue::Literal(PredicateLiteral::Float(f)) => f.to_string(),
        PredicateValue::Literal(PredicateLiteral::String(s)) => format!("'{s}'"),
    }
}

/// Maximum encoded justification bytes of an elevation row ([ADR 0080] §3). Bounded so
/// one row stays small and the friction of writing a real incident reference is real.
pub const MAX_ELEVATION_JUSTIFICATION_BYTES: usize = 512;

/// Approval evidence carried by a loop-issued elevation row ([ADR 0080] §3–§4): the row
/// IS the record — no separate audit store exists in this slice. `approver` equals the
/// requester exactly on emergency rows (`emergency = true`), which are the only
/// approval-free form.
///
/// Grammar-written metadata rows (`GRANT READ_METADATA …`) carry no evidence payload;
/// their authority and subject are visible through introspection like any other grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElevationEvidence {
    pub approver: Principal,
    pub justification: String,
    pub emergency: bool,
}

impl ElevationEvidence {
    /// Validate at the module that owns the row shape: non-empty justification within
    /// the encoding bound. Enforced before any canonical mutation by every writer.
    pub fn validate(&self) -> Result<(), AuthWriteError> {
        if self.justification.is_empty() {
            return Err(AuthWriteError::EmptyJustification);
        }
        if self.justification.len() > MAX_ELEVATION_JUSTIFICATION_BYTES {
            return Err(AuthWriteError::JustificationTooLong(
                MAX_ELEVATION_JUSTIFICATION_BYTES,
            ));
        }
        Ok(())
    }

    fn to_bytes(&self) -> Vec<u8> {
        assert!(
            self.justification.len() <= MAX_ELEVATION_JUSTIFICATION_BYTES,
            "elevation justification exceeds the encoding bound"
        );
        let approver = self.approver.as_slice();
        let mut v = Vec::with_capacity(4 + self.justification.len() + approver.len());
        v.push(u8::from(self.emergency));
        v.extend_from_slice(&(self.justification.len() as u16).to_le_bytes());
        v.extend_from_slice(self.justification.as_bytes());
        v.extend_from_slice(&(approver.len() as u16).to_le_bytes());
        v.extend_from_slice(approver);
        v
    }

    fn decode(bytes: &[u8]) -> Self {
        assert!(bytes.len() >= 5, "corrupt grant row: truncated evidence");
        assert!(
            bytes[0] & !1 == 0,
            "corrupt grant row: unknown emergency byte"
        );
        let emergency = bytes[0] == 1;
        let just_len =
            u16::from_le_bytes(bytes[1..3].try_into().expect("justification length")) as usize;
        let approver_len_at = 3 + just_len;
        assert!(
            bytes.len() >= approver_len_at + 2,
            "corrupt grant row: truncated evidence strings"
        );
        let justification = String::from_utf8(bytes[3..approver_len_at].to_vec())
            .expect("corrupt grant row: non-utf8 justification");
        let approver_len = u16::from_le_bytes(
            bytes[approver_len_at..approver_len_at + 2]
                .try_into()
                .expect("approver length"),
        ) as usize;
        let end = approver_len_at + 2 + approver_len;
        assert!(
            bytes.len() == end,
            "corrupt grant row: trailing evidence bytes"
        );
        Self {
            approver: Principal::from_slice(&bytes[approver_len_at + 2..end]),
            justification,
            emergency,
        }
    }
}

/// Stored grant row value. `expires_at_ns` is dormant for standing data-plane grants
/// ([ADR 0074] §1b): reads treat a row with `expires_at_ns < now` as absent, so
/// time-boxing is not a destructive schema change. Loop-issued elevation rows
/// ([ADR 0080] §3) always set it to the approved window's end.
///
/// `predicate` carries the optional compiled conditional-policy predicate ([ADR 0075]
/// §1): `Some` exactly when the GRANT carried a `FOR (v:Label) WHERE …` selector.
/// `evidence` carries the elevation approval record; `Some` only on metadata rows
/// written by the elevation loop (a predicate can never coexist with evidence).
///
/// Fresh-state encodings: tag `2`/`3` rows are predicate-shaped data-plane rows, tag `4`
/// rows carry elevation evidence; superseded tags `0`/`1` are rejected by the decoder
/// rather than interpreted.
///
/// [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
/// [ADR 0075]: https://github.com/gleaph/gleaph/blob/main/design/adr/0075-conditional-policies-constant-pushdown.md
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantRow {
    pub expires_at_ns: Option<u64>,
    pub predicate: Option<Rc<CompiledPredicate>>,
    pub evidence: Option<ElevationEvidence>,
}

impl Storable for GrantRow {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut v = Vec::new();
        match (&self.evidence, &self.predicate) {
            // Tag 4: elevation-evidence row ([ADR 0080] §3); predicates cannot attach.
            (Some(evidence), None) => {
                v.push(4);
                match self.expires_at_ns {
                    None => v.push(0),
                    Some(ts) => {
                        v.push(1);
                        v.extend_from_slice(&ts.to_le_bytes());
                    }
                }
                v.extend_from_slice(&evidence.to_bytes());
            }
            // Tag 2: predicate-free row (supersedes the rejected tag 0).
            (None, None) => {
                v.push(2);
                match self.expires_at_ns {
                    None => {}
                    Some(ts) => {
                        v.push(1);
                        v.extend_from_slice(&ts.to_le_bytes());
                    }
                }
            }
            // Tag 3: conditional-policy row ([ADR 0075] §1), expiry then predicate bytes.
            (None, Some(predicate)) => {
                v.push(3);
                match self.expires_at_ns {
                    None => v.push(0),
                    Some(ts) => {
                        v.push(1);
                        v.extend_from_slice(&ts.to_le_bytes());
                    }
                }
                v.extend_from_slice(&predicate.encode());
            }
            (Some(_), Some(_)) => {
                panic!("elevation rows never carry conditional-policy predicates")
            }
        }
        Cow::Owned(v)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        assert!(!b.is_empty(), "GrantRow expects at least 1 byte");
        match b[0] {
            2 => Self {
                expires_at_ns: decode_expiry_tail(&b[1..]),
                predicate: None,
                evidence: None,
            },
            3 => {
                assert!(b.len() >= 2, "GrantRow conditional row needs a payload");
                let (expires_at_ns, consumed) = decode_expiry(&b[1..]);
                Self {
                    expires_at_ns,
                    predicate: Some(Rc::new(CompiledPredicate::decode(&b[1 + consumed..]))),
                    evidence: None,
                }
            }
            4 => {
                let (expires_at_ns, consumed) = decode_expiry(&b[1..]);
                Self {
                    expires_at_ns,
                    predicate: None,
                    evidence: Some(ElevationEvidence::decode(&b[1 + consumed..])),
                }
            }
            other => panic!("unknown GrantRow tag {other}"),
        }
    }
}

/// Decode a predicate-free row's expiry tail: empty means `None`, 9 bytes of
/// `1 ‖ u64 le` mean `Some`. Superseded tags are rejected before this point.
fn decode_expiry_tail(b: &[u8]) -> Option<u64> {
    if b.is_empty() {
        return None;
    }
    decode_expiry(b).0
}

fn decode_expiry(b: &[u8]) -> (Option<u64>, usize) {
    match b.first() {
        None | Some(0) => (None, 1),
        Some(1) => (
            Some(u64::from_le_bytes(
                b[1..9].try_into().expect("GrantRow expiry payload"),
            )),
            9,
        ),
        other => panic!("unknown GrantRow expiry flag {other:?}"),
    }
}

/// Review window of an expired grant row ([ADR 0083] §2): rows whose `expires_at_ns`
/// passed are retained this long for post-use review (`list_elevations` /
/// `list_graph_grants`), then GC'd. A constant, not a knob — configurability waits for
/// a demonstrated operator need.
pub const EXPIRED_ROW_RETENTION_NS: u64 = 90 * 24 * 60 * 60 * 1_000_000_000;

/// Stable data-plane grant collection ([ADR 0074] §6).
///
/// Owns the `(principal | PUBLIC) × privilege` rows. The anonymous principal can never hold a
/// stored row (invariant 2); evaluations for anonymous callers consult the `PUBLIC` baseline
/// via [`GrantSubject::effective_for`].
///
/// [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
pub struct GrantState<M: Memory> {
    grants: StableBTreeMap<GrantKey, GrantRow, M>,
}

impl<M: Memory> GrantState<M> {
    pub fn init(memory: M) -> Self {
        Self {
            grants: StableBTreeMap::init(memory),
        }
    }

    /// Canonical write path for every grant row: the anonymous-subject guard lives here
    /// exactly once, so [`Self::grant`] and [`Self::grant_elevation`] cannot drift.
    fn put(
        &mut self,
        subject: GrantSubject,
        privilege: &Privilege,
        row: GrantRow,
    ) -> Result<(), AuthWriteError> {
        if let GrantSubject::Principal(p) = &subject
            && *p == Principal::anonymous()
        {
            return Err(AuthWriteError::AnonymousPrincipal);
        }
        let key = GrantKey::new(privilege, &subject);
        self.grants.insert(key, row);
        Ok(())
    }

    /// Insert or replace a predicate-shaped grant row (data-plane `GRANT` or prepared
    /// publication; also the evidence-free form of grammar-written metadata rows).
    ///
    /// Rejects [`Principal::anonymous`] subjects before any mutation; the virtual
    /// [`GrantSubject::Public`] subject is the only way to publish to unauthenticated callers.
    pub fn grant(
        &mut self,
        subject: GrantSubject,
        privilege: &Privilege,
        expires_at_ns: Option<u64>,
        predicate: Option<Rc<CompiledPredicate>>,
    ) -> Result<(), AuthWriteError> {
        self.put(
            subject,
            privilege,
            GrantRow {
                expires_at_ns,
                predicate,
                evidence: None,
            },
        )
    }

    /// Insert or replace one loop-issued elevation row ([ADR 0080] §3): metadata-scope
    /// privilege, window end at `expires_at_ns`, full approval evidence. Rejects
    /// anonymous subjects, empty or over-bound justifications, and non-metadata
    /// privileges before any mutation — elevation rows are evidence-complete by
    /// construction, never by caller discipline.
    pub fn grant_elevation(
        &mut self,
        subject: GrantSubject,
        privilege: &Privilege,
        expires_at_ns: u64,
        evidence: ElevationEvidence,
    ) -> Result<(), AuthWriteError> {
        assert!(
            matches!(privilege, Privilege::Metadata(_)),
            "elevation rows are written only for metadata-scope privileges"
        );
        evidence.validate()?;
        self.put(
            subject,
            privilege,
            GrantRow {
                expires_at_ns: Some(expires_at_ns),
                predicate: None,
                evidence: Some(evidence),
            },
        )
    }

    /// Remove the exact grant row for `(privilege, subject)`. Returns whether a row existed.
    pub fn revoke(&mut self, subject: GrantSubject, privilege: &Privilege) -> bool {
        self.grants
            .remove(&GrantKey::new(privilege, &subject))
            .is_some()
    }

    /// Whether the exact grant row exists and is unexpired at `now_ns`.
    ///
    /// A row with `expires_at_ns < now_ns` is treated as absent (fail closed); equality is
    /// still valid.
    pub fn holds(&self, subject: GrantSubject, privilege: &Privilege, now_ns: u64) -> bool {
        match self.grants.get(&GrantKey::new(privilege, &subject)) {
            Some(row) => !row.expires_at_ns.is_some_and(|expiry| expiry < now_ns),
            None => false,
        }
    }

    /// Whether the exact grant row for `(privilege, subject)` exists, regardless of expiry.
    ///
    /// Read-only preflight for revoke paths that must reject absent rows before any
    /// mutation; unlike [`Self::holds`] an expired row still exists as stored state.
    pub fn contains(&self, subject: GrantSubject, privilege: &Privilege) -> bool {
        self.grants
            .contains_key(&GrantKey::new(privilege, &subject))
    }

    /// Whether `subject` holds at least one unexpired data-plane grant targeting `graph`.
    ///
    /// Backs the grant-derived visibility arm of graph resolution (ADR 0074 slice 2b): a
    /// grantee may resolve a shared graph by name even though they are no tenant. Expired
    /// rows confer nothing (fail closed, same semantics as [`Self::holds`]).
    pub fn holds_any_graph_grant(&self, subject: GrantSubject, graph: u32, now_ns: u64) -> bool {
        // Canonical keys sort privilege-first, so a graph cannot be prefix-scanned; the row
        // count is the number of distinct (privilege × subject) grants, which stays small.
        self.grants.iter().any(|entry| {
            let row = entry.value();
            if row.expires_at_ns.is_some_and(|expiry| expiry < now_ns) {
                return false;
            }
            match entry.key().decode() {
                (Privilege::Graph(GraphPrivilege { graph: target, .. }), key_subject) => {
                    target == graph && key_subject == subject
                }
                _ => false,
            }
        })
    }

    /// Whether `subject` holds at least one unexpired vertex-label `MATCH` grant on
    /// `graph`, including the wildcard `NODES *` row ([ADR 0089] §5).
    ///
    /// Backs the unconstrained-scan marker demand: a caller may run an unconstrained
    /// vertex scan only when they hold `MATCH` on at least one vertex label (or the
    /// wildcard equivalent). Expired rows confer nothing (fail closed, same semantics as
    /// [`Self::holds`]).
    pub fn holds_any_vertex_label_match(
        &self,
        subject: GrantSubject,
        graph: u32,
        now_ns: u64,
    ) -> bool {
        self.grants.iter().any(|entry| {
            let row = entry.value();
            if row.expires_at_ns.is_some_and(|expiry| expiry < now_ns) {
                return false;
            }
            match entry.key().decode() {
                (
                    Privilege::Graph(GraphPrivilege {
                        graph: target,
                        operation: GraphOperation::Match,
                        resource: GraphResource::VertexLabel(_) | GraphResource::AllVertexLabels,
                    }),
                    key_subject,
                ) => target == graph && key_subject == subject,
                _ => false,
            }
        })
    }

    /// Collect every unexpired vertex-label `MATCH` grant `subject` holds on `graph`,
    /// including the wildcard `NODES *` row ([ADR 0089] §5).
    ///
    /// Returns `(concrete_labels, has_wildcard)`: the set of concrete vertex label ids
    /// the subject can match, plus whether the wildcard row is present. Used by the
    /// request-build bucket restriction to intersect the plan's resolved vertex labels
    /// with the caller's grantable set. Expired rows are excluded (fail closed).
    pub fn collect_vertex_label_match_set(
        &self,
        subject: GrantSubject,
        graph: u32,
        now_ns: u64,
    ) -> (BTreeSet<u32>, bool) {
        let mut labels = BTreeSet::new();
        let mut wildcard = false;
        for entry in self.grants.iter() {
            let row = entry.value();
            if row.expires_at_ns.is_some_and(|expiry| expiry < now_ns) {
                continue;
            }
            match entry.key().decode() {
                (
                    Privilege::Graph(GraphPrivilege {
                        graph: target,
                        operation: GraphOperation::Match,
                        resource: GraphResource::VertexLabel(label),
                    }),
                    key_subject,
                ) if target == graph && key_subject == subject => {
                    labels.insert(label);
                }
                (
                    Privilege::Graph(GraphPrivilege {
                        graph: target,
                        operation: GraphOperation::Match,
                        resource: GraphResource::AllVertexLabels,
                    }),
                    key_subject,
                ) if target == graph && key_subject == subject => {
                    wildcard = true;
                }
                _ => {}
            }
        }
        (labels, wildcard)
    }

    /// Remove every stored row whose privilege targets graph `graph`, returning the count.
    ///
    /// Deletion is exact-key based on the decoded canonical keys: a row is removed only
    /// when its privilege targets this exact graph id — a data-plane [`Privilege::Graph`]
    /// or a graph-scoped metadata elevation ([`Privilege::Metadata`] over
    /// [`MetadataScope::Graph`]) — so rows of other graphs, other subjects' rows
    /// elsewhere, cross-graph [`MetadataScope::ControlPlane`] elevations, and
    /// [`Privilege::ExecutePreparedQuery`] rows (name-keyed, graph-agnostic) are
    /// untouched.
    ///
    /// This is the cascade-invalidation primitive of ADR 0074 §3 invariant 4: when the
    /// embedding system drops a graph's label/property vocabulary, every graph-scoped row
    /// targeting that vocabulary references ids that are monotonic and never reused, so no
    /// future grant can ever cover them again; sweeping them here keeps stored state and
    /// introspection truthful instead of accumulating permanently dead rows.
    pub fn revoke_all_for_graph(&mut self, graph: u32) -> usize {
        let dead: Vec<GrantKey> = self
            .grants
            .iter()
            .filter_map(|entry| match entry.key().decode() {
                (Privilege::Graph(GraphPrivilege { graph: target, .. }), _) if target == graph => {
                    Some(entry.key().clone())
                }
                (Privilege::Metadata(MetadataScope::Graph(target)), _) if target == graph => {
                    Some(entry.key().clone())
                }
                _ => None,
            })
            .collect();
        for key in &dead {
            self.grants.remove(key);
        }
        dead.len()
    }

    /// One bounded retention-sweep step ([ADR 0083] §3): walk keys in canonical order
    /// starting strictly after `resume_after` (`None` starts a fresh lap), visit at most
    /// `budget` keys, and remove every visited row whose review window passed — i.e.
    /// `now_ns > expires_at_ns + EXPIRED_ROW_RETENTION_NS`.
    ///
    /// The rule is generic over `expires_at_ns`: any time-boxed row shape is swept once
    /// its window passed, while rows without an expiry are never touched and a row
    /// exactly at the window edge stays stored (`saturating_add`, so an effectively
    /// immortal `u64::MAX` expiry is never swept). Removal happens only after the scan
    /// slice completes, mirroring [`Self::revoke_all_for_graph`].
    ///
    /// Returns [`RetentionSweepStep`]; its `resume_after` is the canonical key examined
    /// last, or `None` when the scan reached the end of the keyspace and the lap is
    /// complete. A pass is idempotent: resuming from a lost cursor restarts from the
    /// beginning and re-derives the same removals.
    pub fn sweep_expired_rows(
        &mut self,
        now_ns: u64,
        budget: usize,
        resume_after: Option<&GrantKey>,
    ) -> RetentionSweepStep {
        let lower = match resume_after {
            Some(key) => std::ops::Bound::Excluded(key.clone()),
            None => std::ops::Bound::Unbounded,
        };
        let mut visited = 0usize;
        let mut last_examined: Option<GrantKey> = None;
        let mut dead: Vec<GrantKey> = Vec::new();
        for entry in self.grants.range((lower, std::ops::Bound::Unbounded)) {
            let key = entry.key().clone();
            last_examined = Some(key.clone());
            visited += 1;
            if entry.value().expires_at_ns.is_some_and(|expires_at| {
                now_ns > expires_at.saturating_add(EXPIRED_ROW_RETENTION_NS)
            }) {
                dead.push(key);
            }
            if visited == budget {
                break;
            }
        }
        // A short slice means the scan reached the end of the keyspace: the lap is
        // complete and the next call starts from the beginning.
        if visited < budget {
            last_examined = None;
        }
        for key in &dead {
            self.grants.remove(key);
        }
        RetentionSweepStep {
            removed: dead.len(),
            resume_after: last_examined,
        }
    }

    /// All stored rows decoded to their canonical parts, ordered by canonical key.
    ///
    /// Backs owner-facing introspection surfaces. Malformed keys trap (see
    /// [`GrantKey::decode`]).
    pub fn rows(&self) -> Vec<GrantRowEntry> {
        self.grants
            .iter()
            .map(|entry| {
                let (privilege, subject) = entry.key().decode();
                GrantRowEntry {
                    subject,
                    privilege,
                    expires_at_ns: entry.value().expires_at_ns,
                    predicate: entry.value().predicate.clone(),
                    evidence: entry.value().evidence.clone(),
                }
            })
            .collect()
    }

    pub fn len(&self) -> u64 {
        self.grants.len()
    }

    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }
}

/// One decoded grant row, in canonical key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantRowEntry {
    pub subject: GrantSubject,
    pub privilege: Privilege,
    pub expires_at_ns: Option<u64>,
    /// Compiled conditional-policy predicate; `Some` exactly on conditional grants
    /// ([ADR 0075] §1).
    pub predicate: Option<Rc<CompiledPredicate>>,
    /// Approval evidence; `Some` exactly on loop-issued elevation rows ([ADR 0080] §4).
    pub evidence: Option<ElevationEvidence>,
}

/// Outcome of one bounded retention-sweep step ([ADR 0083] §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionSweepStep {
    /// Rows removed because their review window passed.
    pub removed: usize,
    /// Canonical key examined last; the next step resumes strictly after it. `None`
    /// means the lap completed and the next step starts from the beginning of the
    /// keyspace (safe: a pass is idempotent).
    pub resume_after: Option<GrantKey>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec_priv(name: &str) -> Privilege {
        Privilege::ExecutePreparedQuery {
            name: name.to_string(),
        }
    }

    fn principal(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    // --- AdminCaps ---

    #[test]
    fn caps_names_round_trip() {
        for name in AdminCaps::NAMES {
            let cap = AdminCaps::from_name(name).expect("known name parses");
            assert_eq!(cap.names(), [name]);
        }
        assert!(AdminCaps::from_name("NO_SUCH_CAP").is_none());
        assert_eq!(
            AdminCaps::all().names(),
            AdminCaps::NAMES.to_vec(),
            "all() covers every named bit"
        );
    }

    #[test]
    fn migrated_manager_bits_keep_their_positions() {
        // The three migrated ManagerCapability bits keep their historical positions so the
        // wire-visible bit values stay stable across the destructive replacement.
        assert_eq!(AdminCaps::PREPARE_REGISTER.bits(), 1 << 0);
        assert_eq!(AdminCaps::INDEX_CREATE.bits(), 1 << 1);
        assert_eq!(AdminCaps::INDEX_DROP.bits(), 1 << 2);
    }

    // --- AuthState ---

    #[test]
    fn unknown_principal_defaults_to_deny() {
        use ic_stable_structures::DefaultMemoryImpl;
        let auth = AuthState::init(DefaultMemoryImpl::default());
        let p = principal(1);
        assert_eq!(auth.caps_of(&p), AdminCaps::empty());
        assert!(!auth.has_cap(&p, AdminCaps::PREPARE_REGISTER));
        assert!(!auth.has_cap(&p, AdminCaps::MANAGE_AUTHORIZATION));
    }

    #[test]
    fn upsert_caps_and_has_cap() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let p = principal(2);
        auth.upsert_caps(p, AdminCaps::PREPARE_REGISTER | AdminCaps::INDEX_CREATE)
            .expect("non-anonymous upsert");
        assert!(auth.has_cap(&p, AdminCaps::PREPARE_REGISTER));
        assert!(auth.has_cap(&p, AdminCaps::INDEX_CREATE));
        assert!(!auth.has_cap(&p, AdminCaps::INDEX_DROP));
    }

    #[test]
    fn caps_record_rejects_legacy_role_ladder_bytes() {
        // Legacy AuthRecord rows were ≥9 bytes (role byte + manager_caps). Fresh-state contract:
        // reject old bytes instead of interpreting them.
        let legacy = {
            let mut v = vec![4u8]; // former Role::Admin discriminator
            v.extend_from_slice(&0u64.to_le_bytes());
            v
        };
        let result = std::panic::catch_unwind(|| CapsRecord::from_bytes(Cow::Owned(legacy)));
        assert!(result.is_err(), "legacy 9-byte role rows must be rejected");
    }

    #[test]
    fn caps_record_round_trip() {
        let record = CapsRecord {
            caps: AdminCaps::all().bits(),
        };
        let decoded = CapsRecord::from_bytes(record.to_bytes());
        assert_eq!(decoded, record);
    }

    #[test]
    fn upsert_caps_rejects_anonymous() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let err = auth
            .upsert_caps(Principal::anonymous(), AdminCaps::all())
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(auth.is_empty());
        assert_eq!(auth.caps_of(&Principal::anonymous()), AdminCaps::empty());
    }

    #[test]
    fn validate_bootstrap_principals_accepts_all_non_anonymous() {
        let issuer = principal(1);
        let admin = principal(2);
        validate_bootstrap_principals(issuer, &[admin]).expect("all non-anonymous is valid");
    }

    #[test]
    fn validate_bootstrap_principals_rejects_anonymous_issuer_with_valid_admin() {
        let valid = principal(2);
        assert_eq!(
            validate_bootstrap_principals(Principal::anonymous(), &[valid]),
            Err(AuthWriteError::AnonymousPrincipal)
        );
    }

    #[test]
    fn validate_bootstrap_principals_rejects_anonymous_initial_admin() {
        let issuer = principal(1);
        let valid = principal(2);
        assert_eq!(
            validate_bootstrap_principals(issuer, &[valid, Principal::anonymous()]),
            Err(AuthWriteError::AnonymousPrincipal)
        );
    }

    #[test]
    fn bootstrap_seeds_full_caps() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let issuer = principal(1);
        let other = principal(2);
        auth.bootstrap_principals(issuer, &[other])
            .expect("bootstrap");
        assert_eq!(auth.caps_of(&issuer), AdminCaps::all());
        assert_eq!(auth.caps_of(&other), AdminCaps::all());
        assert_eq!(auth.len(), 2);
    }

    #[test]
    fn bootstrap_rejects_anonymous_issuer_without_inserting_rows() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let real_admin = principal(1);
        let err = auth
            .bootstrap_principals(Principal::anonymous(), &[real_admin])
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(auth.is_empty(), "no rows inserted on rejected bootstrap");
        // The supplied valid initial admin was not elevated.
        assert_eq!(auth.caps_of(&real_admin), AdminCaps::empty());
    }

    #[test]
    fn bootstrap_rejects_anonymous_initial_admin_all_or_nothing() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let issuer = principal(1);
        let valid = principal(2);
        let err = auth
            .bootstrap_principals(issuer, &[valid, Principal::anonymous()])
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(
            auth.is_empty(),
            "issuer and valid admin must not be inserted when any initial admin is anonymous"
        );
        // Neither the issuer nor the valid initial admin from the same request was elevated.
        assert_eq!(auth.caps_of(&issuer), AdminCaps::empty());
        assert_eq!(auth.caps_of(&valid), AdminCaps::empty());
    }

    #[test]
    fn corrupt_anonymous_row_does_not_elevate_effective_caps() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        // Simulate a corrupt persisted row by inserting directly into the backing map,
        // bypassing the guarded write path.
        auth.map.insert(
            Principal::anonymous(),
            CapsRecord {
                caps: AdminCaps::all().bits(),
            },
        );
        assert_eq!(auth.caps_of(&Principal::anonymous()), AdminCaps::empty());
        assert!(!auth.has_cap(&Principal::anonymous(), AdminCaps::PREPARE_REGISTER));
    }

    // --- GrantState ---

    #[test]
    fn grant_then_holds_exact_key_addressing() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(3);
        grants
            .grant(GrantSubject::Principal(p), &exec_priv("q1"), None, None)
            .expect("principal subject");
        assert!(grants.holds(GrantSubject::Principal(p), &exec_priv("q1"), 0));
        // Different query name: no grant (exact canonical key drives lookup).
        assert!(!grants.holds(GrantSubject::Principal(p), &exec_priv("q2"), 0));
        // Different subject: no grant.
        assert!(!grants.holds(GrantSubject::Principal(principal(4)), &exec_priv("q1"), 0));
        // PUBLIC subject: separate row.
        assert!(!grants.holds(GrantSubject::Public, &exec_priv("q1"), 0));
    }

    #[test]
    fn grant_key_groups_subjects_by_resource_prefix() {
        let key_a = GrantKey::new(&exec_priv("q1"), &GrantSubject::Public);
        let key_b = GrantKey::new(&exec_priv("q1"), &GrantSubject::Principal(principal(1)));
        let key_c = GrantKey::new(&exec_priv("q2"), &GrantSubject::Public);
        assert!(key_a < key_b, "same privilege sorts subjects adjacently");
        assert!(key_b < key_c, "different privileges do not interleave");
        let (op, resource, subject_kind) = key_a.parts();
        assert_eq!(op, 1);
        assert_eq!(resource, "q1");
        assert_eq!(subject_kind, 0);
    }

    #[test]
    fn grant_keys_distinguish_short_principals() {
        // IC principals are variable length; the canonical key must not collapse them.
        let a = GrantKey::new(
            &exec_priv("q1"),
            &GrantSubject::Principal(Principal::from_slice(&[5; 10])),
        );
        let b = GrantKey::new(
            &exec_priv("q1"),
            &GrantSubject::Principal(Principal::from_slice(&[6; 10])),
        );
        assert_ne!(a, b);
    }

    #[test]
    fn grant_rejects_anonymous_subject() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let err = grants
            .grant(
                GrantSubject::Principal(Principal::anonymous()),
                &exec_priv("q1"),
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(grants.is_empty(), "rejected grant must persist no row");
    }

    #[test]
    fn public_row_is_the_only_path_for_anonymous_evaluation() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        grants
            .grant(GrantSubject::Public, &exec_priv("public-q"), None, None)
            .expect("public subject is storable");
        // Anonymous evaluation resolves to the PUBLIC subject.
        let anon = GrantSubject::effective_for(&Principal::anonymous());
        assert_eq!(anon, GrantSubject::Public);
        assert!(grants.holds(anon, &exec_priv("public-q"), 0));
        // A named principal's evaluation never falls through to the PUBLIC row.
        let named = GrantSubject::effective_for(&principal(5));
        assert!(!grants.holds(named, &exec_priv("public-q"), 0));
    }

    #[test]
    fn expired_rows_are_treated_as_absent() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        grants
            .grant(GrantSubject::Public, &exec_priv("timed"), Some(100), None)
            .expect("grant with expiry");
        assert!(grants.holds(GrantSubject::Public, &exec_priv("timed"), 100));
        assert!(!grants.holds(GrantSubject::Public, &exec_priv("timed"), 101));
        assert!(!grants.holds(GrantSubject::Public, &exec_priv("timed"), 1_000));
    }

    #[test]
    fn holds_any_graph_grant_scopes_to_graph_subject_and_expiry() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(7);
        let graph_traverse = |graph: u32| {
            Privilege::Graph(GraphPrivilege {
                graph,
                operation: GraphOperation::Traverse(Some(Direction::Outgoing)),
                resource: GraphResource::EdgeLabel(3),
            })
        };
        // No rows at all: nothing is visible.
        assert!(!grants.holds_any_graph_grant(GrantSubject::Principal(p), 9, 0));

        grants
            .grant(GrantSubject::Principal(p), &graph_traverse(9), None, None)
            .expect("grant on graph 9");
        assert!(grants.holds_any_graph_grant(GrantSubject::Principal(p), 9, 0));
        // A different graph's row does not leak visibility.
        assert!(!grants.holds_any_graph_grant(GrantSubject::Principal(p), 8, 0));
        // A different subject does not inherit the row.
        assert!(!grants.holds_any_graph_grant(GrantSubject::Principal(principal(8)), 9, 0));
        // Prepared-query EXECUTE rows are not graph data-plane grants.
        grants
            .grant(GrantSubject::Principal(p), &exec_priv("q1"), None, None)
            .expect("prepared execute grant");
        assert!(
            !grants.holds_any_graph_grant(GrantSubject::Principal(p), 7, 0),
            "EXECUTE rows target no graph"
        );

        // Expired rows confer nothing; equality of expiry and now still holds.
        grants.revoke(GrantSubject::Principal(p), &graph_traverse(9));
        grants
            .grant(
                GrantSubject::Principal(p),
                &graph_traverse(9),
                Some(50),
                None,
            )
            .expect("expiring grant");
        assert!(grants.holds_any_graph_grant(GrantSubject::Principal(p), 9, 50));
        assert!(!grants.holds_any_graph_grant(GrantSubject::Principal(p), 9, 51));

        // The PUBLIC subject sees only PUBLIC rows.
        grants
            .grant(GrantSubject::Public, &graph_traverse(11), None, None)
            .expect("public grant");
        assert!(grants.holds_any_graph_grant(GrantSubject::Public, 11, 0));
        assert!(!grants.holds_any_graph_grant(GrantSubject::Principal(p), 11, 0));
    }

    #[test]
    fn revoke_removes_only_the_exact_row() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(6);
        grants
            .grant(GrantSubject::Principal(p), &exec_priv("q1"), None, None)
            .expect("grant");
        grants
            .grant(GrantSubject::Public, &exec_priv("q1"), None, None)
            .expect("public grant");
        assert!(grants.revoke(GrantSubject::Principal(p), &exec_priv("q1")));
        assert!(!grants.revoke(GrantSubject::Principal(p), &exec_priv("q1")));
        assert!(grants.holds(GrantSubject::Public, &exec_priv("q1"), 0));
        assert_eq!(grants.len(), 1);
    }

    #[test]
    fn grant_row_round_trip() {
        let predicate = || {
            Rc::new(CompiledPredicate {
                label: 9,
                conjuncts: vec![
                    PredicateComparison {
                        property: 30,
                        op: PredicateOp::Eq,
                        value: PredicateValue::Literal(PredicateLiteral::Bool(true)),
                    },
                    PredicateComparison {
                        property: 31,
                        op: PredicateOp::Ge,
                        value: PredicateValue::MsgCaller,
                    },
                    PredicateComparison {
                        property: 32,
                        op: PredicateOp::Ne,
                        value: PredicateValue::Literal(PredicateLiteral::Int(-7)),
                    },
                    PredicateComparison {
                        property: 33,
                        op: PredicateOp::Lt,
                        value: PredicateValue::Literal(PredicateLiteral::Float(2.5)),
                    },
                    PredicateComparison {
                        property: 34,
                        op: PredicateOp::Gt,
                        value: PredicateValue::Literal(PredicateLiteral::String(
                            "public".to_owned(),
                        )),
                    },
                ],
                chain: None,
            })
        };
        let rows = vec![
            GrantRow {
                expires_at_ns: None,
                predicate: None,
                evidence: None,
            },
            GrantRow {
                expires_at_ns: Some(u64::MAX),
                predicate: None,
                evidence: None,
            },
            GrantRow {
                expires_at_ns: None,
                predicate: Some(predicate()),
                evidence: None,
            },
            GrantRow {
                expires_at_ns: Some(12345),
                predicate: Some(predicate()),
                evidence: None,
            },
        ];
        for row in rows {
            assert_eq!(GrantRow::from_bytes(row.to_bytes()), row);
        }
    }

    #[test]
    fn grant_row_conditional_round_trips_through_stable_state() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(12);
        let predicate = Rc::new(CompiledPredicate {
            label: 4,
            conjuncts: vec![PredicateComparison {
                property: 6,
                op: PredicateOp::Eq,
                value: PredicateValue::MsgCaller,
            }],
            chain: None,
        });
        grants
            .grant(
                GrantSubject::Principal(p),
                &graph_traverse(None, 3),
                Some(900),
                Some(predicate.clone()),
            )
            .expect("conditional grant");
        // The decoded rows expose the stored predicate unchanged (stable-state read path).
        let rows = grants.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].predicate.as_ref(), Some(&predicate));
        assert_eq!(rows[0].expires_at_ns, Some(900));

        // Re-granting without a condition replaces the stored predicate field.
        grants
            .grant(
                GrantSubject::Principal(p),
                &graph_traverse(None, 3),
                None,
                None,
            )
            .expect("unconditional re-grant");
        let rows = grants.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].predicate, None);
    }

    #[test]
    fn superseded_predicate_free_grant_row_tags_are_rejected() {
        // Fresh-state contract ([ADR 0075] migration): the pre-policy tag encodings
        // (0/1) are rejected instead of interpreted.
        for legacy in [vec![0u8], {
            let mut v = vec![1u8];
            v.extend_from_slice(&50u64.to_le_bytes());
            v
        }] {
            let result = std::panic::catch_unwind(|| GrantRow::from_bytes(Cow::Owned(legacy)));
            assert!(result.is_err(), "superseded tag must be rejected");
        }
    }

    /// One source-side comparison: `prop = MSG_CALLER()`.
    fn caller_comparison(property: u32) -> PredicateComparison {
        PredicateComparison {
            property,
            op: PredicateOp::Eq,
            value: PredicateValue::MsgCaller,
        }
    }

    /// The ADR 0082 §2 direct-grant chain shape: `(d)-[:GRANTED_TO]->(a:Account)`
    /// with one terminal equality.
    fn direct_grant_chain() -> PredicateChain {
        PredicateChain {
            hops: vec![PredicateChainHop {
                edge_label: 40,
                direction: PredicateHopDirection::Outgoing,
                dest_label: 5,
            }],
            terminal_conjuncts: vec![caller_comparison(60)],
        }
    }

    #[test]
    fn v2_predicate_with_chain_round_trips() {
        let predicate = CompiledPredicate {
            label: 9,
            conjuncts: vec![caller_comparison(30)],
            chain: Some(direct_grant_chain()),
        };
        let decoded = CompiledPredicate::decode(&predicate.encode());
        assert_eq!(decoded, predicate);
    }

    #[test]
    fn v2_two_hop_chain_round_trips() {
        // Org-membership pattern: (p)-[:SHARED_TO]->(g:Group)<-[:MEMBER_OF]-(:Account)
        // with two terminal conjuncts.
        let predicate = CompiledPredicate {
            label: 11,
            conjuncts: Vec::new(),
            chain: Some(PredicateChain {
                hops: vec![
                    PredicateChainHop {
                        edge_label: 41,
                        direction: PredicateHopDirection::Outgoing,
                        dest_label: 6,
                    },
                    PredicateChainHop {
                        edge_label: 42,
                        direction: PredicateHopDirection::Incoming,
                        dest_label: 5,
                    },
                ],
                terminal_conjuncts: vec![
                    caller_comparison(61),
                    PredicateComparison {
                        property: 62,
                        op: PredicateOp::Lt,
                        value: PredicateValue::Literal(PredicateLiteral::Int(10)),
                    },
                ],
            }),
        };
        let decoded = CompiledPredicate::decode(&predicate.encode());
        assert_eq!(decoded, predicate);

        // The undirected spelling survives as its own direction.
        let mut undirected = predicate.clone();
        undirected.chain.as_mut().expect("chain").hops[1].direction = PredicateHopDirection::Both;
        assert_eq!(CompiledPredicate::decode(&undirected.encode()), undirected);
    }

    #[test]
    fn v1_bytes_without_version_discriminator_are_rejected() {
        // The pre-chain V1 layout led with the label directly; its first byte here is
        // deliberately != ENCODING_VERSION, so old bytes fail closed at decode.
        let mut v1 = Vec::new();
        v1.extend_from_slice(&9u32.to_le_bytes());
        v1.push(1); // conjunct count
        v1.extend_from_slice(&caller_comparison(30).to_bytes());
        let result = std::panic::catch_unwind(move || CompiledPredicate::decode(&v1));
        assert!(
            result.is_err(),
            "V1 bytes must be rejected, not interpreted"
        );
    }

    #[test]
    fn v2_malformed_chain_fields_are_rejected() {
        let base = || {
            CompiledPredicate {
                label: 9,
                conjuncts: vec![caller_comparison(30)],
                chain: Some(direct_grant_chain()),
            }
            .encode()
        };

        // Unknown version byte.
        let mut bad_version = base();
        bad_version[0] = 3;
        assert!(std::panic::catch_unwind(move || CompiledPredicate::decode(&bad_version)).is_err());

        // Chain presence flag set but no chain payload follows.
        let mut truncated_chain = base();
        truncated_chain.truncate(truncated_chain.len() - direct_grant_chain().to_bytes().len());
        truncated_chain.push(1);
        assert!(
            std::panic::catch_unwind(move || CompiledPredicate::decode(&truncated_chain)).is_err()
        );

        // Hop count over the fixed bound (3 hops). The chain payload's hop-count byte
        // sits exactly `chain_len` bytes before the end of the encoding.
        let chain_len = direct_grant_chain().to_bytes().len();
        let mut too_many_hops = base();
        let hop_count_at = too_many_hops.len() - chain_len;
        too_many_hops[hop_count_at] = MAX_CHAIN_HOPS as u8 + 1;
        assert!(
            std::panic::catch_unwind(move || CompiledPredicate::decode(&too_many_hops)).is_err()
        );

        // Unknown direction discriminant on the first hop (edge label u32, then the
        // direction byte).
        let mut bad_direction = base();
        bad_direction[hop_count_at + 5] = 9;
        assert!(
            std::panic::catch_unwind(move || CompiledPredicate::decode(&bad_direction)).is_err()
        );

        // Zero terminal conjuncts is not representable.
        let mut no_terminal = base();
        let terminal_count_at = hop_count_at + 1 + 9;
        no_terminal[terminal_count_at] = 0;
        assert!(std::panic::catch_unwind(move || CompiledPredicate::decode(&no_terminal)).is_err());

        // Terminal conjunct count over the depth cap.
        let mut over_cap_terminal = base();
        over_cap_terminal[terminal_count_at] = MAX_PREDICATE_CONJUNCTS as u8 + 1;
        assert!(
            std::panic::catch_unwind(move || CompiledPredicate::decode(&over_cap_terminal))
                .is_err()
        );
    }

    #[test]
    fn v2_empty_conjunction_is_only_valid_with_a_chain() {
        // Pure-EXISTS condition ([ADR 0082] §2 flagship form): zero source conjuncts
        // with a chain round-trips.
        let pure_exists = CompiledPredicate {
            label: 9,
            conjuncts: Vec::new(),
            chain: Some(direct_grant_chain()),
        };
        assert_eq!(
            CompiledPredicate::decode(&pure_exists.encode()),
            pure_exists
        );

        // Without a chain, zero conjuncts is corrupt state.
        let mut chainless = pure_exists.encode();
        chainless.truncate(chainless.len() - direct_grant_chain().to_bytes().len());
        chainless.push(0);
        assert!(std::panic::catch_unwind(move || CompiledPredicate::decode(&chainless)).is_err());
    }

    #[test]
    fn chain_encoding_rejects_oversized_hop_and_conjunct_counts() {
        let mut chain = direct_grant_chain();
        chain.hops = vec![
            PredicateChainHop {
                edge_label: 40,
                direction: PredicateHopDirection::Outgoing,
                dest_label: 5,
            };
            MAX_CHAIN_HOPS + 1
        ];
        assert!(std::panic::catch_unwind(move || chain.to_bytes()).is_err());

        let mut chain = direct_grant_chain();
        chain.terminal_conjuncts = vec![caller_comparison(60); MAX_PREDICATE_CONJUNCTS + 1];
        assert!(std::panic::catch_unwind(move || chain.to_bytes()).is_err());
    }

    #[test]
    fn compiled_predicate_encoding_rejects_empty_and_oversized_conjunctions() {
        let empty = CompiledPredicate {
            label: 1,
            conjuncts: Vec::new(),
            chain: None,
        };
        assert!(std::panic::catch_unwind(|| empty.encode()).is_err());

        let over_cap = CompiledPredicate {
            label: 1,
            conjuncts: vec![
                PredicateComparison {
                    property: 1,
                    op: PredicateOp::Eq,
                    value: PredicateValue::MsgCaller
                };
                MAX_PREDICATE_CONJUNCTS + 1
            ],
            chain: None,
        };
        assert!(std::panic::catch_unwind(|| over_cap.encode()).is_err());
    }

    // --- Data-plane graph privileges (ADR 0074 §2, slice 2a grammar surface) ---

    fn graph_traverse(direction: Option<Direction>, label: u32) -> Privilege {
        Privilege::Graph(GraphPrivilege {
            graph: 7,
            operation: GraphOperation::Traverse(direction),
            resource: GraphResource::EdgeLabel(label),
        })
    }

    #[test]
    fn graph_privilege_key_round_trip() {
        let privileges = [
            graph_traverse(None, 3),
            graph_traverse(Some(Direction::Outgoing), 3),
            graph_traverse(Some(Direction::Incoming), 3),
            Privilege::Graph(GraphPrivilege {
                graph: 7,
                operation: GraphOperation::Match,
                resource: GraphResource::VertexLabel(9),
            }),
            Privilege::Graph(GraphPrivilege {
                graph: 7,
                operation: GraphOperation::ReadProperty,
                resource: GraphResource::VertexProperty {
                    label: 9,
                    property: 12,
                },
            }),
        ];
        for privilege in privileges {
            for subject in [GrantSubject::Public, GrantSubject::Principal(principal(8))] {
                let key = GrantKey::new(&privilege, &subject);
                assert_eq!(key.decode(), (privilege.clone(), subject));
            }
        }
    }

    #[test]
    fn graph_grants_hold_only_the_exact_operation_resource_and_graph() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(9);
        grants
            .grant(
                GrantSubject::Principal(p),
                &graph_traverse(Some(Direction::Outgoing), 3),
                None,
                None,
            )
            .expect("grant");
        // Exact key holds...
        assert!(grants.contains(
            GrantSubject::Principal(p),
            &graph_traverse(Some(Direction::Outgoing), 3)
        ));
        // ...but the other direction, the unoriented form, another label, another graph,
        // and the vertex analogue all stay denied (exact canonical key drives lookup).
        assert!(!grants.contains(
            GrantSubject::Principal(p),
            &graph_traverse(Some(Direction::Incoming), 3)
        ));
        assert!(!grants.contains(GrantSubject::Principal(p), &graph_traverse(None, 3)));
        assert!(!grants.contains(
            GrantSubject::Principal(p),
            &graph_traverse(Some(Direction::Outgoing), 4)
        ));
        assert!(!grants.contains(
            GrantSubject::Principal(p),
            &Privilege::Graph(GraphPrivilege {
                graph: 8,
                operation: GraphOperation::Traverse(Some(Direction::Outgoing)),
                resource: GraphResource::EdgeLabel(3),
            })
        ));
        assert!(!grants.contains(
            GrantSubject::Principal(p),
            &Privilege::Graph(GraphPrivilege {
                graph: 7,
                operation: GraphOperation::Traverse(Some(Direction::Outgoing)),
                resource: GraphResource::VertexLabel(3),
            })
        ));
    }

    #[test]
    fn grant_rows_lists_decoded_entries_in_canonical_order() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(10);
        grants
            .grant(
                GrantSubject::Principal(p),
                &exec_priv("q1"),
                Some(500),
                None,
            )
            .expect("prepared grant");
        grants
            .grant(GrantSubject::Public, &graph_traverse(None, 3), None, None)
            .expect("public traverse grant");
        let rows = grants.rows();
        assert_eq!(rows.len(), 2);
        // Canonical key order: prepared-query rows (discriminant 1) sort before graph
        // privileges (discriminant 2).
        assert_eq!(
            rows[0],
            GrantRowEntry {
                subject: GrantSubject::Principal(p),
                privilege: exec_priv("q1"),
                expires_at_ns: Some(500),
                predicate: None,
                evidence: None,
            }
        );
        assert_eq!(
            rows[1],
            GrantRowEntry {
                subject: GrantSubject::Public,
                privilege: graph_traverse(None, 3),
                expires_at_ns: None,
                predicate: None,
                evidence: None,
            }
        );
    }

    #[test]
    fn contains_sees_expired_rows_while_holds_does_not() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        grants
            .grant(
                GrantSubject::Public,
                &graph_traverse(None, 3),
                Some(100),
                None,
            )
            .expect("expiring grant");
        // Stored state survives expiry (contains), while evaluation treats it as absent
        // (holds) — revoke preflight must address stored rows, not effective ones.
        assert!(grants.contains(GrantSubject::Public, &graph_traverse(None, 3)));
        assert!(!grants.holds(GrantSubject::Public, &graph_traverse(None, 3), 101));
    }

    #[test]
    fn revoke_all_for_graph_removes_exactly_that_graphs_rows() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(11);
        let on_graph = |graph: u32, label: u32| {
            Privilege::Graph(GraphPrivilege {
                graph,
                operation: GraphOperation::Traverse(Some(Direction::Outgoing)),
                resource: GraphResource::EdgeLabel(label),
            })
        };
        let match_on = |graph: u32, label: u32| {
            Privilege::Graph(GraphPrivilege {
                graph,
                operation: GraphOperation::Match,
                resource: GraphResource::VertexLabel(label),
            })
        };
        // Dropped graph: two subjects (named + PUBLIC), two operations.
        grants
            .grant(GrantSubject::Principal(p), &on_graph(7, 3), None, None)
            .expect("principal row");
        grants
            .grant(GrantSubject::Public, &match_on(7, 9), None, None)
            .expect("public row");
        grants
            .grant(
                GrantSubject::Principal(p),
                &Privilege::Graph(GraphPrivilege {
                    graph: 7,
                    operation: GraphOperation::ReadProperty,
                    resource: GraphResource::VertexProperty {
                        label: 9,
                        property: 30,
                    },
                }),
                None,
                None,
            )
            .expect("property row");
        // Survivors: same numeric label ids under a different graph, a prepared-query
        // EXECUTE row (name-keyed, graph-agnostic), and an expired row of another graph
        // that still exists as stored state.
        grants
            .grant(GrantSubject::Principal(p), &on_graph(8, 3), None, None)
            .expect("sibling-graph row with identical label id");
        grants
            .grant(GrantSubject::Principal(p), &exec_priv("q1"), None, None)
            .expect("execute row");
        grants
            .grant(GrantSubject::Public, &on_graph(8, 3), Some(50), None)
            .expect("expired sibling-graph row");

        assert_eq!(
            grants.revoke_all_for_graph(7),
            3,
            "exactly the dropped graph's rows"
        );
        assert_eq!(grants.len(), 3, "survivors untouched");
        assert!(!grants.contains(GrantSubject::Principal(p), &on_graph(7, 3)));
        assert!(!grants.contains(GrantSubject::Public, &match_on(7, 9)));
        assert!(!grants.contains(
            GrantSubject::Principal(p),
            &Privilege::Graph(GraphPrivilege {
                graph: 7,
                operation: GraphOperation::ReadProperty,
                resource: GraphResource::VertexProperty {
                    label: 9,
                    property: 30
                },
            })
        ));
        assert!(grants.contains(GrantSubject::Principal(p), &on_graph(8, 3)));
        assert!(grants.contains(GrantSubject::Public, &on_graph(8, 3)));
        assert!(grants.contains(GrantSubject::Principal(p), &exec_priv("q1")));

        // Idempotent: a second sweep finds nothing and removes nothing.
        assert_eq!(grants.revoke_all_for_graph(7), 0);
        assert_eq!(grants.len(), 3);
    }

    #[test]
    fn revoke_all_for_graph_on_empty_or_unknown_graph_is_a_noop() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        assert_eq!(grants.revoke_all_for_graph(7), 0);
        grants
            .grant(GrantSubject::Public, &graph_traverse(None, 3), None, None)
            .expect("grant on graph 7");
        assert_eq!(
            grants.revoke_all_for_graph(8),
            0,
            "unknown graph removes nothing"
        );
        assert_eq!(grants.len(), 1);
    }

    // --- Metadata-plane elevation (ADR 0080 §1–§4) ---

    fn metadata_scope(graph: u32) -> Privilege {
        Privilege::Metadata(MetadataScope::Graph(graph))
    }

    /// THE plane-disjointness probe ([ADR 0080] §1): metadata-plane grants never satisfy
    /// data-plane demands and data-plane grants never satisfy metadata demands. Coverage
    /// is exact canonical-key lookup, and the leading discriminant byte separates the
    /// planes — a wrong implementation that treated metadata coverage as data coverage
    /// (or ignored expiry) fails here first, before any caller leans on the semantics.
    #[test]
    fn metadata_and_data_plane_coverage_are_provably_disjoint() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(20);

        // Metadata grant on graph 9.
        grants
            .grant_elevation(
                GrantSubject::Principal(p),
                &metadata_scope(9),
                1_000,
                ElevationEvidence {
                    approver: principal(21),
                    justification: "incident-1".into(),
                    emergency: false,
                },
            )
            .expect("metadata elevation");

        // Data-plane demands of the same numeric graph stay unsatisfied — every probed
        // operation/resource form, for the exact same subject inside the window.
        let data_demands = [
            Privilege::Graph(GraphPrivilege {
                graph: 9,
                operation: GraphOperation::Match,
                resource: GraphResource::VertexLabel(1),
            }),
            Privilege::Graph(GraphPrivilege {
                graph: 9,
                operation: GraphOperation::Read,
                resource: GraphResource::VertexLabel(1),
            }),
            Privilege::Graph(GraphPrivilege {
                graph: 9,
                operation: GraphOperation::Traverse(Some(Direction::Outgoing)),
                resource: GraphResource::EdgeLabel(2),
            }),
        ];
        for demand in &data_demands {
            assert!(
                !grants.holds(GrantSubject::Principal(p), demand, 500),
                "metadata grant must never satisfy data-plane demand {demand:?}"
            );
        }
        // Positive control so the probes above cannot pass vacuously: the same subject
        // inside the window DOES hold the metadata demand.
        assert!(grants.holds(GrantSubject::Principal(p), &metadata_scope(9), 500));
    }

    #[test]
    fn data_plane_grants_never_satisfy_metadata_demands() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(22);
        grants
            .grant(
                GrantSubject::Principal(p),
                &Privilege::Graph(GraphPrivilege {
                    graph: 5,
                    operation: GraphOperation::Match,
                    resource: GraphResource::VertexLabel(1),
                }),
                None,
                None,
            )
            .expect("data-plane grant");
        assert!(
            !grants.holds(GrantSubject::Principal(p), &metadata_scope(5), 100),
            "data-plane coverage must never satisfy the metadata demand"
        );
        // The scopes are distinct rows even at identical numeric ids: a ControlPlane
        // elevation covers every graph, but no graph-scoped form does.
        grants
            .grant_elevation(
                GrantSubject::Principal(principal(23)),
                &Privilege::Metadata(MetadataScope::ControlPlane),
                1_000,
                ElevationEvidence {
                    approver: principal(24),
                    justification: "fleet sweep".into(),
                    emergency: false,
                },
            )
            .expect("control-plane elevation");
        // Storage stays exact-key: the ControlPlane row addresses its own canonical key,
        // not every graph-scoped one — cross-graph coverage is evaluation policy
        // (probed at the facade layer).
        assert!(grants.holds(
            GrantSubject::Principal(principal(23)),
            &Privilege::Metadata(MetadataScope::ControlPlane),
            500
        ));
        assert!(!grants.holds(
            GrantSubject::Principal(principal(23)),
            &metadata_scope(777),
            500
        ));
        assert!(!grants.holds(GrantSubject::Principal(p), &metadata_scope(5), 500));
    }

    #[test]
    fn holds_any_graph_grant_ignores_metadata_rows() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(25);
        grants
            .grant_elevation(
                GrantSubject::Principal(p),
                &metadata_scope(9),
                1_000,
                ElevationEvidence {
                    approver: principal(26),
                    justification: "ops".into(),
                    emergency: false,
                },
            )
            .expect("graph metadata elevation");
        // Scan-derived data-plane visibility must not treat metadata rows as graph data
        // grants; ControlPlane rows target no single graph either.
        assert!(!grants.holds_any_graph_grant(GrantSubject::Principal(p), 9, 500));
        grants
            .grant_elevation(
                GrantSubject::Principal(p),
                &Privilege::Metadata(MetadataScope::ControlPlane),
                1_000,
                ElevationEvidence {
                    approver: principal(26),
                    justification: "ops".into(),
                    emergency: false,
                },
            )
            .expect("control-plane elevation");
        assert!(!grants.holds_any_graph_grant(GrantSubject::Principal(p), 9, 500));
    }

    #[test]
    fn expired_elevation_rows_deny_again_but_stay_stored() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(27);
        grants
            .grant_elevation(
                GrantSubject::Principal(p),
                &metadata_scope(3),
                100,
                ElevationEvidence {
                    approver: principal(28),
                    justification: "windowed".into(),
                    emergency: false,
                },
            )
            .expect("windowed elevation");
        assert!(grants.holds(GrantSubject::Principal(p), &metadata_scope(3), 100));
        assert!(!grants.holds(GrantSubject::Principal(p), &metadata_scope(3), 101));
        // Stored evidence survives expiry until GC so review stays possible.
        assert!(grants.contains(GrantSubject::Principal(p), &metadata_scope(3)));
        let rows = grants.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].evidence.as_ref().map(|e| e.justification.as_str()),
            Some("windowed")
        );
    }

    #[test]
    fn revoke_all_for_graph_sweeps_graph_scoped_metadata_rows_only() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(29);
        grants
            .grant_elevation(
                GrantSubject::Principal(p),
                &metadata_scope(7),
                1_000,
                ElevationEvidence {
                    approver: principal(30),
                    justification: "dropped-graph".into(),
                    emergency: false,
                },
            )
            .expect("metadata elevation on dropped graph");
        grants
            .grant_elevation(
                GrantSubject::Principal(p),
                &metadata_scope(8),
                1_000,
                ElevationEvidence {
                    approver: principal(30),
                    justification: "sibling".into(),
                    emergency: false,
                },
            )
            .expect("metadata elevation on sibling graph");
        grants
            .grant_elevation(
                GrantSubject::Principal(p),
                &Privilege::Metadata(MetadataScope::ControlPlane),
                1_000,
                ElevationEvidence {
                    approver: principal(30),
                    justification: "cross-graph".into(),
                    emergency: false,
                },
            )
            .expect("control-plane elevation");
        assert_eq!(
            grants.revoke_all_for_graph(7),
            1,
            "exactly the dropped graph's metadata row joins the cascade"
        );
        assert_eq!(grants.len(), 2, "sibling and cross-graph rows survive");
        assert!(!grants.contains(GrantSubject::Principal(p), &metadata_scope(7)));
        assert!(grants.contains(GrantSubject::Principal(p), &metadata_scope(8)));
        assert!(grants.contains(
            GrantSubject::Principal(p),
            &Privilege::Metadata(MetadataScope::ControlPlane)
        ));
    }

    #[test]
    fn elevation_rows_round_trip_through_the_stable_encoding() {
        let evidence = |emergency: bool| ElevationEvidence {
            approver: Principal::from_slice(&[0xEE; 29]),
            justification: "incident-4711; blast radius: one graph".into(),
            emergency,
        };
        let rows = vec![
            GrantRow {
                expires_at_ns: Some(u64::MAX),
                predicate: None,
                evidence: Some(evidence(false)),
            },
            GrantRow {
                expires_at_ns: Some(42),
                predicate: None,
                evidence: Some(evidence(true)),
            },
        ];
        for row in rows {
            assert_eq!(GrantRow::from_bytes(row.to_bytes()), row);
        }
    }

    #[test]
    fn grant_elevation_rejects_anonymous_subject_without_persisting() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let err = grants
            .grant_elevation(
                GrantSubject::Principal(Principal::anonymous()),
                &metadata_scope(1),
                10,
                ElevationEvidence {
                    approver: principal(31),
                    justification: "anon".into(),
                    emergency: false,
                },
            )
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(grants.is_empty(), "rejected elevation persists no row");
    }

    #[test]
    fn elevation_evidence_is_validated_before_any_write() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let empty = grants
            .grant_elevation(
                GrantSubject::Principal(principal(32)),
                &metadata_scope(1),
                10,
                ElevationEvidence {
                    approver: principal(33),
                    justification: String::new(),
                    emergency: false,
                },
            )
            .unwrap_err();
        assert_eq!(empty, AuthWriteError::EmptyJustification);
        let over_long = grants
            .grant_elevation(
                GrantSubject::Principal(principal(32)),
                &metadata_scope(1),
                10,
                ElevationEvidence {
                    approver: principal(33),
                    justification: "x".repeat(MAX_ELEVATION_JUSTIFICATION_BYTES + 1),
                    emergency: false,
                },
            )
            .unwrap_err();
        assert_eq!(
            over_long,
            AuthWriteError::JustificationTooLong(MAX_ELEVATION_JUSTIFICATION_BYTES)
        );
        assert!(grants.is_empty(), "invalid evidence persists no row");
    }

    #[test]
    fn metadata_privilege_keys_round_trip_and_sort_by_plane() {
        let privileges = [
            metadata_scope(7),
            metadata_scope(u32::MAX),
            Privilege::Metadata(MetadataScope::ControlPlane),
        ];
        for privilege in privileges {
            for subject in [GrantSubject::Public, GrantSubject::Principal(principal(34))] {
                let key = GrantKey::new(&privilege, &subject);
                assert_eq!(key.decode(), (privilege.clone(), subject));
            }
        }
        // Plane separation is visible in key order too: metadata keys (discriminant 3)
        // sort after every data-plane key, so no prefix scan can conflate them.
        let data_key = GrantKey::new(&graph_traverse(None, 3), &GrantSubject::Public);
        let metadata_key = GrantKey::new(&metadata_scope(7), &GrantSubject::Public);
        assert!(data_key < metadata_key);
    }

    #[test]
    fn reissued_elevation_replaces_prior_evidence() {
        // ADR 0083 invariant 3 trade-off: `GrantKey` carries no issuance time, so
        // re-elevating the same (subject, scope) overwrites the prior row's evidence
        // even inside its review window. Accepted v1 semantics, pinned here.
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(35);
        grants
            .grant_elevation(
                GrantSubject::Principal(p),
                &metadata_scope(3),
                1_000,
                ElevationEvidence {
                    approver: principal(36),
                    justification: "first".into(),
                    emergency: false,
                },
            )
            .expect("first issuance");
        grants
            .grant_elevation(
                GrantSubject::Principal(p),
                &metadata_scope(3),
                2_000,
                ElevationEvidence {
                    approver: principal(37),
                    justification: "second".into(),
                    emergency: true,
                },
            )
            .expect("superseding issuance");
        let rows = grants.rows();
        assert_eq!(rows.len(), 1, "the same canonical key holds one row");
        assert_eq!(rows[0].expires_at_ns, Some(2_000));
        let evidence = rows[0].evidence.as_ref().expect("evidence survives");
        assert_eq!(evidence.justification, "second");
        assert_eq!(evidence.approver, principal(37));
        assert!(evidence.emergency);
    }

    // --- Retention sweep (ADR 0083 §2–§3) ---

    /// Window end of a row expiring at `expires_at`: kept while
    /// `now <= expires_at + EXPIRED_ROW_RETENTION_NS`.
    fn window_end(expires_at: u64) -> u64 {
        expires_at.saturating_add(EXPIRED_ROW_RETENTION_NS)
    }

    fn elevation(expires_at: u64) -> GrantRow {
        GrantRow {
            expires_at_ns: Some(expires_at),
            predicate: None,
            evidence: Some(ElevationEvidence {
                approver: principal(50),
                justification: "retention".into(),
                emergency: false,
            }),
        }
    }

    #[test]
    fn sweep_keeps_window_edge_and_sweeps_one_ns_past_it() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let edge = principal(51);
        let later = principal(52);
        grants
            .put(
                GrantSubject::Principal(edge),
                &metadata_scope(3),
                elevation(1_000),
            )
            .expect("edge row");
        grants
            .put(
                GrantSubject::Principal(later),
                &metadata_scope(3),
                elevation(2_000),
            )
            .expect("later row");

        // Exactly at the review-window edge: still retained for post-use review.
        let step = grants.sweep_expired_rows(window_end(1_000), 16, None);
        assert_eq!(step.removed, 0);
        assert_eq!(step.resume_after, None, "a short slice completes the lap");
        assert_eq!(grants.len(), 2);

        // One nanosecond past the window: only that row joins the sweep.
        let step = grants.sweep_expired_rows(window_end(1_000) + 1, 16, None);
        assert_eq!(step.removed, 1);
        assert!(!grants.contains(GrantSubject::Principal(edge), &metadata_scope(3)));
        assert!(grants.contains(GrantSubject::Principal(later), &metadata_scope(3)));

        // Enforcement semantics are untouched by storage: the survivor still reads as
        // absent once its own expiry passed (`holds`), it merely stays stored.
        assert!(!grants.holds(
            GrantSubject::Principal(later),
            &metadata_scope(3),
            window_end(2_000) + 1
        ));
    }

    #[test]
    fn sweep_never_touches_rows_without_expiry() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(53);
        // Standing data-plane rows (named + PUBLIC), a standing grammar-form metadata
        // row, and an effectively immortal `u64::MAX` expiry (saturating window math).
        grants
            .grant(GrantSubject::Public, &exec_priv("q1"), None, None)
            .expect("public execute row");
        grants
            .grant(
                GrantSubject::Principal(p),
                &graph_traverse(None, 3),
                None,
                None,
            )
            .expect("standing traverse row");
        grants
            .grant(GrantSubject::Principal(p), &metadata_scope(9), None, None)
            .expect("standing metadata row");
        grants
            .grant(
                GrantSubject::Principal(p),
                &exec_priv("immortal"),
                Some(u64::MAX),
                None,
            )
            .expect("immortal row");

        let step = grants.sweep_expired_rows(u64::MAX, 16, None);
        assert_eq!(step.removed, 0, "no-expiry and immortal rows stay stored");
        assert_eq!(grants.len(), 4);
        assert!(grants.contains(GrantSubject::Public, &exec_priv("q1")));
        assert!(grants.contains(GrantSubject::Principal(p), &graph_traverse(None, 3)));
        assert!(grants.contains(GrantSubject::Principal(p), &metadata_scope(9)));
        assert!(grants.contains(GrantSubject::Principal(p), &exec_priv("immortal")));
    }

    #[test]
    fn sweep_is_generic_over_every_expiring_row_shape() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(54);
        // Tag 4: loop-issued elevation evidence row.
        grants
            .put(
                GrantSubject::Principal(p),
                &metadata_scope(5),
                elevation(100),
            )
            .expect("evidence row");
        // Tag 2: evidence-free grammar-form metadata row with an expiry.
        grants
            .grant(GrantSubject::Public, &metadata_scope(6), Some(100), None)
            .expect("expiring metadata row");
        // Tag 2 data-plane form.
        grants
            .grant(
                GrantSubject::Principal(p),
                &graph_traverse(None, 4),
                Some(100),
                None,
            )
            .expect("expiring data-plane row");
        // Tag 3: conditional-policy row with an expiry.
        grants
            .grant(
                GrantSubject::Principal(p),
                &graph_traverse(None, 3),
                Some(100),
                Some(Rc::new(CompiledPredicate {
                    label: 1,
                    conjuncts: vec![caller_comparison(30)],
                    chain: None,
                })),
            )
            .expect("expiring conditional row");
        assert_eq!(grants.len(), 4);

        let step = grants.sweep_expired_rows(window_end(100) + 1, 16, None);
        assert_eq!(step.removed, 4, "every time-boxed shape is swept alike");
        assert!(grants.is_empty());
        assert_eq!(step.resume_after, None, "the lap drained completely");
    }

    #[test]
    fn sweep_advances_cursor_when_nothing_is_removable() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        // Three standing rows occupy the earliest canonical slots; one dead row hides
        // behind them. Budget 2: the first step removes nothing yet must advance its
        // resume point — a driver that restarted from the beginning whenever a step
        // removed nothing would never reach the dead row.
        for i in 0u8..3 {
            grants
                .grant(
                    GrantSubject::Principal(principal(60 + i)),
                    &exec_priv("standing"),
                    None,
                    None,
                )
                .expect("standing row");
        }
        grants
            .put(
                GrantSubject::Principal(principal(63)),
                &metadata_scope(1),
                elevation(100),
            )
            .expect("dead row");

        let step = grants.sweep_expired_rows(window_end(100) + 1, 2, None);
        assert_eq!(step.removed, 0);
        assert!(
            step.resume_after.is_some(),
            "a full slice must advance the cursor even without removals"
        );

        let step = grants.sweep_expired_rows(window_end(100) + 1, 2, step.resume_after.as_ref());
        assert_eq!(step.removed, 1, "resumption reaches the dead row");
        assert_eq!(grants.len(), 3, "only the dead row left");
        assert!(
            step.resume_after.is_some(),
            "the slice ended exactly at the keyspace end, so the lap is not yet complete"
        );
        // One more step confirms completion from the (now empty) remainder.
        let step = grants.sweep_expired_rows(window_end(100) + 1, 2, step.resume_after.as_ref());
        assert_eq!(step.removed, 0);
        assert_eq!(step.resume_after, None, "the lap completed");
    }

    #[test]
    fn sweep_resume_drains_backlog_without_skip_or_revisit() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        // Five dead rows interleaved with three survivors across privilege planes.
        for i in 0u8..5 {
            grants
                .grant(
                    GrantSubject::Principal(principal(70 + i)),
                    &exec_priv("timed"),
                    Some(100),
                    None,
                )
                .expect("dead row");
        }
        grants
            .grant(GrantSubject::Public, &exec_priv("standing"), None, None)
            .expect("public survivor");
        grants
            .grant(
                GrantSubject::Principal(principal(80)),
                &graph_traverse(None, 3),
                None,
                None,
            )
            .expect("traverse survivor");
        grants
            .grant(
                GrantSubject::Principal(principal(81)),
                &metadata_scope(7),
                None,
                None,
            )
            .expect("metadata survivor");

        let deadline = window_end(100) + 1;
        let mut removed_total = 0usize;
        let mut steps = 0usize;
        let mut cursor: Option<GrantKey> = None;
        while steps < 16 {
            let step = grants.sweep_expired_rows(deadline, 2, cursor.as_ref());
            assert!(step.removed <= 2, "no step may exceed its bounded slice");
            removed_total += step.removed;
            cursor = step.resume_after;
            steps += 1;
            if cursor.is_none() {
                break;
            }
        }
        assert!(cursor.is_none(), "the backlog drained to lap completion");
        assert_eq!(removed_total, 5, "exactly the dead rows, no skips");
        assert!(steps >= 3, "drain needed multiple bounded steps");
        assert_eq!(grants.len(), 3, "survivors untouched");
        assert!(grants.contains(GrantSubject::Public, &exec_priv("standing")));
        assert!(grants.contains(
            GrantSubject::Principal(principal(80)),
            &graph_traverse(None, 3)
        ));
        assert!(grants.contains(GrantSubject::Principal(principal(81)), &metadata_scope(7)));

        // Idempotent: a fresh full lap finds nothing left to remove.
        let step = grants.sweep_expired_rows(deadline, 16, None);
        assert_eq!(step.removed, 0);
        assert_eq!(step.resume_after, None);
        assert_eq!(grants.len(), 3);
    }

    #[test]
    fn sweep_empty_store_completes_immediately() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let step = grants.sweep_expired_rows(u64::MAX, 16, None);
        assert_eq!(
            step,
            RetentionSweepStep {
                removed: 0,
                resume_after: None,
            }
        );
        assert!(grants.is_empty());
    }
}
