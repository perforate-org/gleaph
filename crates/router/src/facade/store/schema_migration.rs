//! Router-owned schema migration ledger and catalog commit boundary (ADR 0058).

mod driver;
mod index;
mod text;
mod text_backfill;
mod vector;

pub(crate) use driver::real_index_migration_driver;

use candid::Principal;
use gleaph_gql::ast::{GraphTypeDefinition, GraphTypeSpec, Statement, StatementBlock};
use gleaph_gql::parser;
use gleaph_gql::token::Span;
use gleaph_gql::type_check::{GraphTypePropertySchema, collect_graph_type_vocabulary};
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationArgsV1, ApplySchemaMigrationResult,
    ApplySchemaMigrationResultV1, MAX_SCHEMA_MIGRATION_STATEMENTS, SchemaMigrationApplyStatus,
    SchemaMigrationChecksumAlgorithm, SchemaMigrationGraphSelector, SchemaMigrationRecord,
    SchemaMigrationRecordState, SchemaMigrationRecordV1, SchemaMigrationStatementProfile,
};
use std::collections::BTreeMap;

use super::{RouterStore, validate_metadata_name};
use crate::facade::auth;
use crate::facade::stable::{
    self, ROUTER_GRAPH_TYPE_CATALOG, ROUTER_SCHEMA_MIGRATIONS, graph_type_catalog,
    schema_migration::StableSchemaMigrationRecord,
};
use crate::state::RouterError;

impl RouterStore {
    /// Control-plane apply entrypoint. Non-index migrations keep the synchronous ADR 0058
    /// co-write; CREATE INDEX delegates one bounded, resumable step to ADR 0059 orchestration.
    pub(crate) async fn admin_apply_schema_migration_control<D: index::IndexMigrationDriver>(
        &self,
        caller: Principal,
        args: ApplySchemaMigrationArgs,
        driver: &D,
    ) -> Result<ApplySchemaMigrationResult, RouterError> {
        let ApplySchemaMigrationArgs::V1(inner) = &args;
        if gleaph_index_ddl::try_parse_vector(&inner.statement).is_some() {
            return vector::apply_vector_index_migration(self, caller, args, driver).await;
        }
        if gleaph_index_ddl::try_parse_text(&inner.statement).is_some() {
            return text::apply_text_index_migration(self, caller, args, driver).await;
        }
        if gleaph_index_ddl::try_parse(&inner.statement).is_some() {
            return index::apply_index_migration(self, caller, args, driver).await;
        }
        // ADR 0070: a `CREATE GRAPH` statement naming an unregistered graph provisions its shard
        // through the shared admission flow before the synchronous co-write below binds the schema
        // at the newly allocated `GraphId`. Pre-registered names skip provisioning inside the
        // bridge; a replayed migration re-enters the bridge but short-circuits on the registered
        // name without any remote effect.
        self::preprovision_unregistered_create_graphs(caller, &inner.statement).await?;
        self.admin_apply_schema_migration(caller, args)
    }

    /// Apply one immutable, additive schema migration and record it in the canonical ledger.
    ///
    /// All validation, catalog mutation, and ledger insertion happen in this synchronous update
    /// call. There is deliberately no inter-canister `await` between the catalog write and the
    /// ledger write. After catalog mutation begins, any unexpected error traps so the IC message
    /// rolls back the catalog and ledger co-write instead of returning after a partial write.
    pub(crate) fn admin_apply_schema_migration(
        &self,
        caller: Principal,
        args: ApplySchemaMigrationArgs,
    ) -> Result<ApplySchemaMigrationResult, RouterError> {
        auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_CATALOG)?;

        let ApplySchemaMigrationArgs::V1(args) = args;
        validate_apply_args(&args)?;

        let chain = inspect_canonical_chain(None, 0)?;
        if let Some(existing) = ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.get(&args.id))
        {
            let existing = existing.0;
            let existing_v1 = record_v1(&existing)?;
            validate_graph_selector_for_profile(&existing_v1.graph_selector, &existing_v1.profile)?;
            if existing_v1.resolved_graph.is_some() {
                return Err(RouterError::InvalidArgument(
                    "schema migration record uses an unsupported graph-specific lifecycle".into(),
                ));
            }
            if record_matches_args(existing_v1, &args) {
                return Ok(ApplySchemaMigrationResult::V1(
                    ApplySchemaMigrationResultV1 {
                        status: SchemaMigrationApplyStatus::Replay,
                        record: existing,
                    },
                ));
            }
            return Err(RouterError::Conflict(format!(
                "schema migration id `{}` already exists with a different payload",
                args.id
            )));
        }

        if chain.count >= stable::schema_migration::MAX_SCHEMA_MIGRATIONS {
            return Err(RouterError::InvalidArgument(format!(
                "schema migration ledger is full (maximum {})",
                stable::schema_migration::MAX_SCHEMA_MIGRATIONS
            )));
        }

        match args.parent.as_deref() {
            None if chain.count != 0 => {
                return Err(RouterError::Conflict(
                    "a non-empty schema migration ledger already has a root".into(),
                ));
            }
            Some(parent) => {
                let Some(head) = chain.head.as_deref() else {
                    return Err(RouterError::NotFound(format!(
                        "schema migration parent `{parent}`"
                    )));
                };
                if head != parent {
                    return Err(RouterError::Conflict(format!(
                        "schema migration parent `{parent}` is not the current head `{head}`"
                    )));
                }
                let parent_sequence = gleaph_migration_api::parse_schema_migration_id(head)
                    .ok_or_else(|| {
                        RouterError::Internal(format!(
                            "schema migration head `{head}` has an invalid id"
                        ))
                    })?;
                let sequence = gleaph_migration_api::parse_schema_migration_id(&args.id)
                    .ok_or_else(|| {
                        RouterError::Internal(format!(
                            "schema migration id `{}` has an invalid grammar",
                            args.id
                        ))
                    })?;
                if sequence <= parent_sequence {
                    return Err(RouterError::Conflict(format!(
                        "schema migration sequence {:06} must be greater than parent sequence {:06}",
                        sequence, parent_sequence
                    )));
                }
            }
            None => {}
        }

        // Derive the narrow execution profile from the parsed statement and verify the supplied
        // checksum over the exact request envelope before touching either catalog or ledger.
        let block = parse_and_validate_statement(&args.statement)?;
        let profile = statement_profile(&block)?;
        let expected_checksum = gleaph_migration_api::schema_migration_checksum(
            &args.id,
            args.parent.as_deref(),
            &args.graph_selector,
            args.statement.as_bytes(),
        );
        if args.checksum != expected_checksum {
            return Err(RouterError::InvalidArgument(
                "schema migration checksum does not match id, parent, and exact statement bytes"
                    .into(),
            ));
        }

        // The migration dialect is intentionally narrower than the general Router GQL surface;
        // parsing/profile/checksum validation above completed before this preflight.
        validate_graph_selector_for_profile(&args.graph_selector, &profile)?;
        preflight_catalog_statement(&block)?;

        // Catalog application and ledger insertion are one synchronous co-write boundary. Any
        // unexpected catalog error traps so the IC message rolls back the prior stable state.
        if let Err(error) =
            graph_type_catalog::apply_catalog_statement_block(&block, &args.statement)
        {
            ic_cdk::trap(format!("schema migration catalog commit failed: {error}"));
        }
        let recorded_at = super::ic_time_ns();
        let record = SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
            id: args.id.clone(),
            parent: args.parent.clone(),
            graph_selector: args.graph_selector.clone(),
            resolved_graph: None,
            checksum: args.checksum.clone(),
            actor: caller,
            recorded_at,
            statement: args.statement,
            profile,
            state: SchemaMigrationRecordState::Applied {
                applied_at: recorded_at,
            },
        });
        ROUTER_SCHEMA_MIGRATIONS.with_borrow_mut(|ledger| {
            ledger.insert(args.id, StableSchemaMigrationRecord(record.clone()));
        });

        Ok(ApplySchemaMigrationResult::V1(
            ApplySchemaMigrationResultV1 {
                status: SchemaMigrationApplyStatus::Applied,
                record,
            },
        ))
    }

    /// Return one bounded root-to-head page of the canonical schema migration chain.
    pub(crate) fn list_schema_migrations(
        &self,
        args: gleaph_migration_api::ListSchemaMigrationsArgs,
    ) -> Result<gleaph_migration_api::ListSchemaMigrationsResult, RouterError> {
        let gleaph_migration_api::ListSchemaMigrationsArgs::V1(args) = args;
        let limit = args.limit;
        if limit == 0 || limit > stable::schema_migration::MAX_SCHEMA_MIGRATION_LIST_LIMIT {
            return Err(RouterError::InvalidArgument(format!(
                "schema migration page limit must be in 1..={}",
                stable::schema_migration::MAX_SCHEMA_MIGRATION_LIST_LIMIT
            )));
        }

        let chain = inspect_canonical_chain(args.start_after.as_deref(), limit as usize)?;
        let next_cursor = chain.has_more.then(|| {
            record_v1(
                chain
                    .page
                    .last()
                    .expect("a non-empty page has a next cursor"),
            )
            .expect("validated migration record")
            .id
            .clone()
        });
        Ok(gleaph_migration_api::ListSchemaMigrationsResult::V1(
            gleaph_migration_api::ListSchemaMigrationsResultV1 {
                migrations: chain.page,
                next_start_after: next_cursor,
            },
        ))
    }
}

/// Provision every `CREATE GRAPH` statement in the migration whose property-graph name is not yet
/// federation-registered (ADR 0070). Runs before any catalog or ledger write; the provisioning
/// request store owns idempotency of the remote issuance.
async fn preprovision_unregistered_create_graphs(
    caller: Principal,
    statement: &str,
) -> Result<(), RouterError> {
    // Authorize before any remote side effect; the synchronous co-write re-checks below.
    crate::facade::auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_CATALOG)?;
    let block = parse_and_validate_statement(statement)?;
    for stmt in block.iter_statements() {
        if let Statement::CreateGraph(create) = stmt {
            let graph_name = gleaph_graph_catalog::object_name_key(&create.name);
            crate::provisioning::graph::create_graph_admission(caller, &graph_name).await?;
        }
    }
    Ok(())
}

fn validate_apply_args(args: &ApplySchemaMigrationArgsV1) -> Result<(), RouterError> {
    validate_id(&args.id, "id")?;
    if let Some(parent) = &args.parent {
        validate_id(parent, "parent")?;
    }
    if args.statement.is_empty()
        || args.statement.len() > stable::schema_migration::MAX_SCHEMA_MIGRATION_STATEMENT_BYTES
    {
        return Err(RouterError::InvalidArgument(format!(
            "schema migration statement must be 1..={} UTF-8 bytes",
            stable::schema_migration::MAX_SCHEMA_MIGRATION_STATEMENT_BYTES
        )));
    }
    if args.checksum.algorithm != SchemaMigrationChecksumAlgorithm::Sha256 {
        return Err(RouterError::InvalidArgument(
            "schema migration checksum algorithm must be Sha256".into(),
        ));
    }
    if args.checksum.digest.len() != gleaph_migration_api::SCHEMA_MIGRATION_CHECKSUM_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "schema migration Sha256 digest must be exactly {} bytes",
            gleaph_migration_api::SCHEMA_MIGRATION_CHECKSUM_BYTES
        )));
    }
    validate_graph_selector(&args.graph_selector)?;
    Ok(())
}

fn validate_graph_selector(selector: &SchemaMigrationGraphSelector) -> Result<(), RouterError> {
    if let SchemaMigrationGraphSelector::Named(name) = selector {
        if name.is_empty() {
            return Err(RouterError::InvalidArgument(
                "schema migration graph selector name must not be empty".into(),
            ));
        }
        if name.len() > gleaph_migration_api::MAX_SCHEMA_MIGRATION_GRAPH_NAME_BYTES {
            return Err(RouterError::InvalidArgument(format!(
                "schema migration graph selector name exceeds {} UTF-8 bytes",
                gleaph_migration_api::MAX_SCHEMA_MIGRATION_GRAPH_NAME_BYTES
            )));
        }
    }
    Ok(())
}

fn validate_graph_selector_for_profile(
    selector: &SchemaMigrationGraphSelector,
    profiles: &[SchemaMigrationStatementProfile],
) -> Result<(), RouterError> {
    validate_graph_selector(selector)?;
    if profiles.contains(&SchemaMigrationStatementProfile::CreateIndex) {
        return Err(RouterError::InvalidArgument(
            "CREATE INDEX migrations require the planned backfill lifecycle and are not supported yet".into(),
        ));
    }
    if matches!(selector, SchemaMigrationGraphSelector::Named(_)) {
        return Err(RouterError::InvalidArgument(
            "named graph selectors are only supported for CREATE INDEX migrations".into(),
        ));
    }
    Ok(())
}

fn validate_id(id: &str, field: &str) -> Result<(), RouterError> {
    if gleaph_migration_api::parse_schema_migration_id(id).is_none() {
        return Err(RouterError::InvalidArgument(format!(
            "schema migration {field} has invalid id grammar (expected six digits, underscore, and lowercase slug)"
        )));
    }
    Ok(())
}

fn record_matches_args(
    existing: &SchemaMigrationRecordV1,
    args: &ApplySchemaMigrationArgsV1,
) -> bool {
    existing.id == args.id
        && existing.parent == args.parent
        && existing.graph_selector == args.graph_selector
        && existing.checksum == args.checksum
        && existing.statement == args.statement
}

fn record_v1(record: &SchemaMigrationRecord) -> Result<&SchemaMigrationRecordV1, RouterError> {
    match record {
        SchemaMigrationRecord::V1(record) => Ok(record),
    }
}

#[derive(Default)]
struct CanonicalChainInspection {
    count: usize,
    head: Option<String>,
    page: Vec<SchemaMigrationRecord>,
    has_more: bool,
}

/// Validate the canonical ledger in stable-map order while retaining only a bounded result page.
/// Stable map keys are the ordering source; every record must carry the same id, link to the
/// previous record, and have a strictly larger six-digit sequence.
fn inspect_canonical_chain(
    start_after: Option<&str>,
    page_limit: usize,
) -> Result<CanonicalChainInspection, RouterError> {
    let page_limit =
        page_limit.min(stable::schema_migration::MAX_SCHEMA_MIGRATION_LIST_LIMIT as usize);
    ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| {
        let mut inspection = CanonicalChainInspection {
            page: Vec::with_capacity(page_limit),
            ..CanonicalChainInspection::default()
        };
        let mut previous_id = None;
        let mut previous_sequence = None;
        let mut cursor_seen = start_after.is_none();

        for entry in ledger.iter() {
            let key = entry.key();
            let stored = entry.value();
            let record = record_v1(&stored.0)?;
            if key != &record.id {
                return Err(RouterError::Internal(format!(
                    "schema migration map key `{key}` does not match record id `{}`",
                    record.id
                )));
            }
            let sequence =
                gleaph_migration_api::parse_schema_migration_id(&record.id).ok_or_else(|| {
                    RouterError::Internal(format!(
                        "schema migration record `{}` has invalid id grammar",
                        record.id
                    ))
                })?;
            match previous_id.as_deref() {
                None if record.parent.is_some() => {
                    return Err(RouterError::Internal(
                        "schema migration ledger root must not have a parent".into(),
                    ));
                }
                Some(previous) if record.parent.as_deref() != Some(previous) => {
                    return Err(RouterError::Internal(format!(
                        "schema migration `{}` does not reference previous migration `{previous}`",
                        record.id
                    )));
                }
                _ => {}
            }
            if let Some(previous_sequence) = previous_sequence
                && sequence <= previous_sequence
            {
                return Err(RouterError::Internal(format!(
                    "schema migration `{}` does not have a strictly increasing sequence",
                    record.id
                )));
            }

            if !cursor_seen {
                if start_after == Some(record.id.as_str()) {
                    cursor_seen = true;
                }
            } else if inspection.page.len() < page_limit {
                inspection.page.push(stored.0.clone());
            } else if page_limit != 0 {
                inspection.has_more = true;
            }

            inspection.count = inspection.count.checked_add(1).ok_or_else(|| {
                RouterError::Internal("schema migration ledger count overflow".into())
            })?;
            previous_id = Some(record.id.clone());
            previous_sequence = Some(sequence);
        }

        if !cursor_seen {
            return Err(RouterError::NotFound(format!(
                "schema migration `{}`",
                start_after.expect("cursor is present when it was not found")
            )));
        }
        inspection.head = previous_id;
        Ok(inspection)
    })
}

fn parse_and_validate_statement(source: &str) -> Result<StatementBlock, RouterError> {
    if let Some(index_ddl) = gleaph_index_ddl::try_parse(source) {
        return match index_ddl {
            Ok(_) => Err(RouterError::InvalidArgument(
                "CREATE INDEX migrations require the planned backfill lifecycle and are not supported yet"
                    .into(),
            )),
            Err(error) => Err(RouterError::InvalidArgument(format!(
                "invalid migration CREATE INDEX syntax: {error}"
            ))),
        };
    }
    let program = parser::parse(source)
        .map_err(|error| RouterError::InvalidArgument(format!("invalid migration GQL: {error}")))?;
    gleaph_gql::validate::validate(&program)
        .map_err(|error| RouterError::InvalidArgument(format!("invalid migration GQL: {error}")))?;
    if !program.session_activity.is_empty() {
        return Err(RouterError::InvalidArgument(
            "schema migration GQL must not contain SESSION commands".into(),
        ));
    }
    let transaction = program.transaction_activity.ok_or_else(|| {
        RouterError::InvalidArgument(
            "schema migration GQL must contain at least one statement".into(),
        )
    })?;
    if transaction.start.is_some() || transaction.end.is_some() {
        return Err(RouterError::InvalidArgument(
            "schema migration GQL must not contain transaction control".into(),
        ));
    }
    let block = transaction.body.ok_or_else(|| {
        RouterError::InvalidArgument(
            "schema migration GQL must contain at least one statement".into(),
        )
    })?;
    let mut count = 0usize;
    for statement in block.iter_statements() {
        count += 1;
        if count > MAX_SCHEMA_MIGRATION_STATEMENTS {
            return Err(RouterError::InvalidArgument(format!(
                "schema migration exceeds {MAX_SCHEMA_MIGRATION_STATEMENTS} additive statements"
            )));
        }
        match statement {
            Statement::CreateGraphType(statement) => {
                if statement.or_replace || statement.if_not_exists || statement.copy_of.is_some() {
                    return Err(RouterError::InvalidArgument(
                        "schema migrations allow only additive CREATE GRAPH TYPE".into(),
                    ));
                }
                if !has_explicit_graph_type_body(source, &statement.span)? {
                    return Err(RouterError::InvalidArgument(
                        "schema migration CREATE GRAPH TYPE requires an explicit body".into(),
                    ));
                }
                validate_simple_name(&statement.name, "graph type")?;
                validate_graph_type_definition(&statement.definition)?;
            }
            Statement::CreateGraph(statement) => {
                if statement.or_replace || statement.if_not_exists || statement.copy_of.is_some() {
                    return Err(RouterError::InvalidArgument(
                        "schema migrations allow only additive CREATE GRAPH".into(),
                    ));
                }
                validate_simple_name(&statement.name, "graph")?;
                match &statement.graph_type {
                    Some(GraphTypeSpec::Typed {
                        name,
                        typed_keyword: true,
                    }) => {
                        validate_simple_name(name, "graph type reference")?;
                    }
                    Some(GraphTypeSpec::Typed {
                        typed_keyword: false,
                        ..
                    })
                    | Some(GraphTypeSpec::Inline(_))
                    | Some(GraphTypeSpec::Any { .. })
                    | Some(GraphTypeSpec::Like(_))
                    | None => {
                        return Err(RouterError::InvalidArgument(
                            "schema migrations require literal TYPED graph schema".into(),
                        ));
                    }
                }
            }
            _ => {
                return Err(RouterError::InvalidArgument(
                    "schema migrations allow only additive CREATE GRAPH TYPE or CREATE GRAPH statements"
                        .into(),
                ));
            }
        }
    }
    Ok(block)
}

/// The general GQL AST represents a bodyless `CREATE GRAPH TYPE name` and an explicit empty body
/// with the same empty definition. Migration v1 requires the body to be present for every `CREATE
/// GRAPH TYPE` statement, so retain this migration-owned lexical distinction per statement without
/// changing the general-purpose GQL grammar or AST.
fn has_explicit_graph_type_body(source: &str, span: &Span) -> Result<bool, RouterError> {
    let statement_source = source.get(span.start..span.end).ok_or_else(|| {
        RouterError::InvalidArgument("invalid migration GQL statement span".into())
    })?;
    let lexical = gleaph_gql::lexer::tokenize_with_comments(statement_source)
        .map_err(|error| RouterError::InvalidArgument(format!("invalid migration GQL: {error}")))?;
    Ok(lexical
        .tokens
        .iter()
        .any(|token| matches!(token.token, gleaph_gql::token::Token::LBrace)))
}

fn statement_profile(
    block: &StatementBlock,
) -> Result<Vec<SchemaMigrationStatementProfile>, RouterError> {
    let mut profiles = Vec::new();
    for statement in block.iter_statements() {
        let profile = match statement {
            Statement::CreateGraphType(_) => SchemaMigrationStatementProfile::CreateGraphType,
            Statement::CreateGraph(statement) => match &statement.graph_type {
                Some(GraphTypeSpec::Typed {
                    typed_keyword: true,
                    ..
                }) => SchemaMigrationStatementProfile::CreateTypedGraph,
                _ => {
                    return Err(RouterError::InvalidArgument(
                        "schema migration profile requires literal TYPED graph schema".into(),
                    ));
                }
            },
            _ => {
                return Err(RouterError::InvalidArgument(
                    "schema migration profile is unsupported for this statement".into(),
                ));
            }
        };
        profiles.push(profile);
    }
    Ok(profiles)
}

fn validate_simple_name(name: &gleaph_gql::ast::ObjectName, what: &str) -> Result<(), RouterError> {
    if name.parts.len() != 1 {
        return Err(RouterError::InvalidArgument(format!(
            "schema migration {what} names must be unqualified"
        )));
    }
    validate_metadata_name(&name.parts[0])
}

fn validate_graph_type_definition(
    definition: &gleaph_gql::ast::GraphTypeDefinition,
) -> Result<(), RouterError> {
    graph_type_catalog::validate_edge_ordering_policies(definition)?;
    let _ = GraphTypePropertySchema::try_from_definition(definition)
        .map_err(RouterError::InvalidArgument)?;
    let vocabulary = collect_graph_type_vocabulary(definition);
    for name in vocabulary
        .vertex_labels
        .iter()
        .chain(vocabulary.edge_labels.iter())
        .chain(vocabulary.properties.iter())
    {
        validate_metadata_name(name)?;
    }
    Ok(())
}

fn preflight_catalog_statement(block: &StatementBlock) -> Result<(), RouterError> {
    // Types created by an earlier statement in the same migration are visible to later
    // statements. The durable catalog is only mutated after the whole block preflights, so shadow
    // the in-block creations here instead of reading the persisted catalog.
    let mut created_types: BTreeMap<String, GraphTypeDefinition> = BTreeMap::new();
    for statement in block.iter_statements() {
        match statement {
            Statement::CreateGraphType(statement) => {
                let name = gleaph_graph_catalog::object_name_key(&statement.name);
                if ROUTER_GRAPH_TYPE_CATALOG
                    .with_borrow(|catalog| catalog.get_id(&name))
                    .is_some()
                    || created_types.contains_key(&name)
                {
                    return Err(RouterError::Conflict(format!(
                        "graph type `{name}` already exists"
                    )));
                }
                created_types.insert(name, statement.definition.clone());
            }
            Statement::CreateGraph(statement) => {
                let graph_name = gleaph_graph_catalog::object_name_key(&statement.name);
                let graph_id = crate::facade::stable::graph_catalog::lookup_graph_id(&graph_name)
                    .ok_or_else(|| {
                    RouterError::NotFound(format!("graph `{graph_name}` is not registered"))
                })?;
                if graph_type_catalog::parsed_graph_type_definition_for_graph_id(graph_id)?
                    .is_some()
                {
                    return Err(RouterError::Conflict(format!(
                        "graph `{graph_name}` already has a schema binding"
                    )));
                }
                let Some(GraphTypeSpec::Typed {
                    name,
                    typed_keyword: true,
                }) = &statement.graph_type
                else {
                    unreachable!("parse_and_validate_statement enforces the migration allowlist")
                };
                let type_name = gleaph_graph_catalog::object_name_key(name);
                let definition = match ROUTER_GRAPH_TYPE_CATALOG
                    .with_borrow(|catalog| catalog.get_id(&type_name))
                {
                    Some(type_id) => {
                        graph_type_catalog::parsed_graph_type_definition_for_type_id(type_id)?
                            .ok_or_else(|| {
                                RouterError::Internal(format!(
                                    "graph type `{type_name}` has no definition"
                                ))
                            })?
                    }
                    None => match created_types.get(&type_name) {
                        Some(definition) => definition.clone(),
                        None => {
                            return Err(RouterError::NotFound(format!("graph type `{type_name}`")));
                        }
                    },
                };
                validate_graph_type_definition(&definition)?;
                RouterStore::preflight_graph_type_vocabulary(graph_id, &definition)?;
            }
            _ => unreachable!("parse_and_validate_statement enforces the migration allowlist"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::auth;
    use crate::facade::stable::graph_catalog::lookup_graph_id;
    use crate::facade::stable::indexed_catalog::edge_index_uses_property_label;
    use crate::facade::stable::{
        ROUTER_EDGE_INLINE_PROPERTY_PROFILES, index_name_catalog, indexed_catalog,
    };
    use crate::facade::store::tests::{register_test_graph, test_init_args};
    use candid::Principal;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_graph_kernel::index::{EdgeIndexDirection, IndexedPropertyKind};
    use gleaph_migration_api::ResolvedSchemaMigrationGraph;
    use ic_stable_structures::{BTreeMap, Storable, VectorMemory};
    use std::borrow::Cow;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn migration_args(id: &str, parent: Option<&str>, statement: &str) -> ApplySchemaMigrationArgs {
        migration_args_with_selector(
            id,
            parent,
            &SchemaMigrationGraphSelector::Default,
            statement,
        )
    }

    fn migration_args_with_selector(
        id: &str,
        parent: Option<&str>,
        graph_selector: &SchemaMigrationGraphSelector,
        statement: &str,
    ) -> ApplySchemaMigrationArgs {
        ApplySchemaMigrationArgs::V1(ApplySchemaMigrationArgsV1 {
            id: id.to_owned(),
            parent: parent.map(str::to_owned),
            graph_selector: graph_selector.clone(),
            checksum: gleaph_migration_api::schema_migration_checksum(
                id,
                parent,
                graph_selector,
                statement.as_bytes(),
            ),
            statement: statement.to_owned(),
        })
    }

    fn list_args(
        start_after: Option<&str>,
        limit: u16,
    ) -> gleaph_migration_api::ListSchemaMigrationsArgs {
        gleaph_migration_api::ListSchemaMigrationsArgs::V1(
            gleaph_migration_api::ListSchemaMigrationsArgsV1 {
                start_after: start_after.map(str::to_owned),
                limit,
            },
        )
    }

    #[test]
    fn apply_replay_conflict_parent_checksum_and_paged_listing() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[11; 29]);
        auth::grant_admins(&[admin]);

        let root_statement = "CREATE GRAPH TYPE root_type { NODE Person }";
        let root_args = migration_args("000001_root", None, root_statement);
        let applied = store
            .admin_apply_schema_migration(admin, root_args.clone())
            .expect("root migration applies");
        let applied_record = match applied {
            ApplySchemaMigrationResult::V1(ApplySchemaMigrationResultV1 {
                status: SchemaMigrationApplyStatus::Applied,
                record,
            }) => record,
            other => panic!("unexpected root apply result: {other:?}"),
        };
        let ledger_before_replay = ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| {
            ledger
                .iter()
                .map(|entry| (entry.key().clone(), entry.value().0.clone()))
                .collect::<Vec<_>>()
        });
        let root_type_id = ROUTER_GRAPH_TYPE_CATALOG
            .with_borrow(|catalog| catalog.get_id("root_type"))
            .expect("root type id");
        let graph_type_before_replay =
            graph_type_catalog::parsed_graph_type_definition_for_type_id(root_type_id)
                .expect("root type definition");
        auth::grant_admin(Principal::self_authenticating([17; 32]));

        let replay = store
            .admin_apply_schema_migration(
                Principal::self_authenticating([17; 32]),
                root_args.clone(),
            )
            .expect("exact replay succeeds");
        let replay_record = match replay {
            ApplySchemaMigrationResult::V1(ApplySchemaMigrationResultV1 {
                status: SchemaMigrationApplyStatus::Replay,
                record,
            }) => record,
            other => panic!("unexpected replay result: {other:?}"),
        };
        assert_eq!(replay_record, applied_record);
        assert_eq!(
            ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| {
                ledger
                    .iter()
                    .map(|entry| (entry.key().clone(), entry.value().0.clone()))
                    .collect::<Vec<_>>()
            }),
            ledger_before_replay
        );
        assert_eq!(
            ROUTER_GRAPH_TYPE_CATALOG.with_borrow(|catalog| {
                (
                    catalog.len(),
                    catalog.get_id("root_type"),
                    catalog.get_name(root_type_id),
                )
            }),
            (1, Some(root_type_id), Some("root_type".to_owned()))
        );
        assert_eq!(
            graph_type_catalog::parsed_graph_type_definition_for_type_id(root_type_id)
                .expect("root type definition after replay"),
            graph_type_before_replay
        );

        // An existing id is a conflict even when the replacement payload's checksum is valid.
        let changed_payload = migration_args(
            "000001_root",
            None,
            "CREATE GRAPH TYPE replacement_type { NODE Person }",
        );
        assert!(matches!(
            store.admin_apply_schema_migration(admin, changed_payload),
            Err(RouterError::Conflict(_))
        ));

        let child_statement = "CREATE GRAPH TYPE child_type { NODE Person }";
        let child_args = migration_args("000002_child", Some("000001_root"), child_statement);
        store
            .admin_apply_schema_migration(admin, child_args.clone())
            .expect("child migration applies");

        // A new id must name the current head, and malformed checksums are rejected before any
        // catalog or ledger write.
        let divergent_parent = migration_args(
            "000003_divergent",
            Some("000001_root"),
            "CREATE GRAPH TYPE divergent_type { NODE Person }",
        );
        assert!(matches!(
            store.admin_apply_schema_migration(admin, divergent_parent),
            Err(RouterError::Conflict(_))
        ));
        let duplicate_sequence = migration_args(
            "000002_other",
            Some("000002_child"),
            "CREATE GRAPH TYPE duplicate_sequence { NODE Person }",
        );
        assert!(matches!(
            store.admin_apply_schema_migration(admin, duplicate_sequence),
            Err(RouterError::Conflict(_))
        ));
        let lower_sequence = migration_args(
            "000001_lower",
            Some("000002_child"),
            "CREATE GRAPH TYPE lower_sequence { NODE Person }",
        );
        assert!(matches!(
            store.admin_apply_schema_migration(admin, lower_sequence),
            Err(RouterError::Conflict(_))
        ));
        let ApplySchemaMigrationArgs::V1(mut invalid_checksum) = migration_args(
            "000003_checksum",
            Some("000002_child"),
            "CREATE GRAPH TYPE checksum_type { NODE Person }",
        );
        invalid_checksum.checksum.digest.fill(0);
        assert!(matches!(
            store.admin_apply_schema_migration(
                admin,
                ApplySchemaMigrationArgs::V1(invalid_checksum),
            ),
            Err(RouterError::InvalidArgument(_))
        ));
        assert_eq!(
            ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.len()),
            2
        );

        // A catalog conflict is preflighted before the ledger insert.
        let catalog_conflict =
            migration_args("000003_catalog", Some("000002_child"), root_statement);
        assert!(matches!(
            store.admin_apply_schema_migration(admin, catalog_conflict),
            Err(RouterError::Conflict(_))
        ));
        let count = ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.len());
        assert_eq!(count, 2);

        let first_page = store
            .list_schema_migrations(list_args(None, 1))
            .expect("first migration page");
        let (migrations, cursor) = match first_page {
            gleaph_migration_api::ListSchemaMigrationsResult::V1(page) => {
                (page.migrations, page.next_start_after)
            }
        };
        assert_eq!(migration_id(&migrations[0]), "000001_root");
        assert_eq!(cursor.as_deref(), Some("000001_root"));

        let second_page = store
            .list_schema_migrations(list_args(cursor.as_deref(), 1))
            .expect("second migration page");
        let (migrations, cursor) = match second_page {
            gleaph_migration_api::ListSchemaMigrationsResult::V1(page) => {
                (page.migrations, page.next_start_after)
            }
        };
        assert_eq!(migration_id(&migrations[0]), "000002_child");
        assert!(cursor.is_none());
    }

    #[test]
    fn migration_parser_rejects_inline_graph_and_non_schema_statements() {
        let rejected = [
            "CREATE GRAPH graph_name { NODE Person }",
            "CREATE GRAPH TYPE IF NOT EXISTS root_type { NODE Person }",
            "CREATE OR REPLACE GRAPH TYPE root_type { NODE Person }",
            "DROP GRAPH TYPE root_type",
            "CREATE GRAPH graph_name ANY",
            "CREATE GRAPH graph_name LIKE other_graph",
            "CREATE GRAPH graph_name AS COPY OF other_graph",
            "CREATE SCHEMA schema_name",
            "INSERT INTO graph_name VALUES (1)",
            "CREATE GRAPH TYPE root_type { NODE Person } NEXT INSERT (n)",
            "CREATE GRAPH TYPE root_type NEXT CREATE GRAPH TYPE child_type { NODE Person }",
            "SESSION SET GRAPH graph_name",
            "START TRANSACTION",
            "COMMIT",
            "ROLLBACK",
            "CREATE GRAPH TYPE $dynamic { NODE Person }",
        ];
        for statement in rejected {
            assert!(
                parse_and_validate_statement(statement).is_err(),
                "migration parser unexpectedly accepted `{statement}`"
            );
        }
        let typed = parse_and_validate_statement("CREATE GRAPH graph_name TYPED root_type")
            .expect("literal TYPED graph is accepted by the migration grammar");
        assert_eq!(
            statement_profile(&typed).expect("typed profile"),
            vec![SchemaMigrationStatementProfile::CreateTypedGraph]
        );
        let batch = parse_and_validate_statement(
            "CREATE GRAPH TYPE root_type { NODE Person } NEXT CREATE GRAPH graph_name TYPED root_type",
        )
        .expect("additive NEXT chain is accepted by the migration grammar");
        assert_eq!(
            statement_profile(&batch).expect("batch profile"),
            vec![
                SchemaMigrationStatementProfile::CreateGraphType,
                SchemaMigrationStatementProfile::CreateTypedGraph,
            ]
        );
    }

    #[test]
    fn apply_rejects_invalid_or_bodyless_graph_type_without_catalog_or_ledger_writes() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([16; 32]);
        auth::grant_admin(admin);
        let initial_catalog_len = ROUTER_GRAPH_TYPE_CATALOG.with_borrow(|catalog| catalog.len());

        for (id, type_name, statement, expected) in [
            (
                "000001_duplicate",
                "duplicate_properties",
                "CREATE GRAPH TYPE duplicate_properties { NODE Person { name STRING, name STRING } }",
                "duplicate graph type property",
            ),
            (
                "000001_empty",
                "empty",
                "CREATE GRAPH TYPE empty",
                "requires an explicit body",
            ),
        ] {
            let error = store
                .admin_apply_schema_migration(admin, migration_args(id, None, statement))
                .expect_err("invalid graph type migration must be rejected");
            let RouterError::InvalidArgument(message) = error else {
                panic!("expected InvalidArgument, got {error:?}");
            };
            assert!(
                message.contains(expected),
                "expected {expected:?} in {message:?}"
            );
            assert_eq!(
                ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.len()),
                0,
                "rejected migration must not append a ledger record"
            );
            assert_eq!(
                ROUTER_GRAPH_TYPE_CATALOG.with_borrow(|catalog| catalog.len()),
                initial_catalog_len,
                "rejected migration must not allocate a graph type id"
            );
            assert!(
                ROUTER_GRAPH_TYPE_CATALOG
                    .with_borrow(|catalog| catalog.get_id(type_name))
                    .is_none(),
                "rejected graph type must not enter the catalog"
            );
        }
    }

    #[test]
    fn apply_requires_non_anonymous_admin_and_preserves_empty_ledger_on_rejection() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([13; 32]);
        let outsider = Principal::self_authenticating([14; 32]);
        let args = migration_args(
            "000001_auth",
            None,
            "CREATE GRAPH TYPE auth_type { NODE Person }",
        );
        assert!(matches!(
            store.admin_apply_schema_migration(Principal::anonymous(), args.clone()),
            Err(RouterError::NotAuthorized)
        ));
        assert!(matches!(
            store.admin_apply_schema_migration(outsider, args.clone()),
            Err(RouterError::NotAuthorized)
        ));
        assert_eq!(
            ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.len()),
            0
        );

        auth::grant_admin(admin);
        store
            .admin_apply_schema_migration(admin, args)
            .expect("admin migration applies");
        assert_eq!(
            ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.len()),
            1
        );
    }

    #[test]
    fn apply_typed_graph_migration_binds_registered_graph_and_interns_vocabulary() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([18; 32]);
        auth::grant_admin(admin);
        register_test_graph(&store, admin, "typed_graph");

        store
            .admin_apply_schema_migration(
                admin,
                migration_args(
                    "000001_type",
                    None,
                    "CREATE GRAPH TYPE typed_type { NODE Person }",
                ),
            )
            .expect("graph type migration applies");
        let result = store
            .admin_apply_schema_migration(
                admin,
                migration_args(
                    "000002_bind",
                    Some("000001_type"),
                    "CREATE GRAPH typed_graph TYPED typed_type",
                ),
            )
            .expect("typed graph migration applies");
        assert!(matches!(
            result,
            ApplySchemaMigrationResult::V1(ApplySchemaMigrationResultV1 {
                status: SchemaMigrationApplyStatus::Applied,
                record: SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
                    profile,
                    ..
                }),
            }) if profile == [SchemaMigrationStatementProfile::CreateTypedGraph]
        ));

        let graph_id = lookup_graph_id("typed_graph").expect("registered graph");
        assert!(store.lookup_vertex_label_id(graph_id, "Person").is_ok());
        assert!(
            graph_type_catalog::parsed_graph_type_definition_for_graph_id(graph_id)
                .expect("typed graph binding")
                .is_some()
        );
        assert_eq!(
            ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.len()),
            2
        );
    }

    #[test]
    fn apply_multi_statement_migration_resolves_in_batch_type_and_binds_graph() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([23; 32]);
        auth::grant_admin(admin);
        register_test_graph(&store, admin, "batch_graph");

        let result = store
            .admin_apply_schema_migration(
                admin,
                migration_args(
                    "000001_batch",
                    None,
                    "CREATE GRAPH TYPE batch_type { NODE Person } NEXT CREATE GRAPH batch_graph TYPED batch_type",
                ),
            )
            .expect("multi-statement migration applies atomically");
        assert!(matches!(
            result,
            ApplySchemaMigrationResult::V1(ApplySchemaMigrationResultV1 {
                status: SchemaMigrationApplyStatus::Applied,
                record: SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
                    profile,
                    resolved_graph: None,
                    ..
                }),
            }) if profile == [
                SchemaMigrationStatementProfile::CreateGraphType,
                SchemaMigrationStatementProfile::CreateTypedGraph,
            ]
        ));

        assert!(
            ROUTER_GRAPH_TYPE_CATALOG
                .with_borrow(|catalog| catalog.get_id("batch_type"))
                .is_some()
        );
        let graph_id = lookup_graph_id("batch_graph").expect("registered graph");
        assert!(store.lookup_vertex_label_id(graph_id, "Person").is_ok());
        assert!(
            graph_type_catalog::parsed_graph_type_definition_for_graph_id(graph_id)
                .expect("typed graph binding")
                .is_some()
        );
        assert_eq!(
            ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.len()),
            1
        );
    }

    #[test]
    fn unregistered_create_graph_migration_fails_closed_without_provisioner() {
        struct NoopDriver;
        impl index::IndexMigrationDriver for NoopDriver {
            fn drive(
                &self,
                _request: index::IndexMigrationStepRequest,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                index::IndexMigrationStepResponse,
                                index::IndexMigrationDriveError,
                            >,
                        > + '_,
                >,
            > {
                unreachable!("graph migration does not drive a property-index step")
            }
        }

        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([31; 32]);
        auth::grant_admin(admin);

        // ADR 0070: without a configured provisioner the bridge must fail closed before any
        // catalog or ledger write; no graph may be registered as a side effect.
        let error = futures::executor::block_on(store.admin_apply_schema_migration_control(
            admin,
            migration_args(
                "000001_graph",
                None,
                "CREATE GRAPH TYPE g_type { NODE Person } NEXT CREATE GRAPH fresh_graph TYPED g_type",
            ),
            &NoopDriver,
        ))
        .expect_err("dev mode has no provisioner");
        assert!(
            matches!(error, RouterError::NotImplemented(ref message) if message.contains("provision canister")),
            "expected NotImplemented provisioner error, got {error:?}"
        );
        assert_eq!(
            ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.len()),
            0,
            "rejected migration must not append a ledger record"
        );
        assert!(lookup_graph_id("fresh_graph").is_none());
    }

    #[test]
    fn named_selector_is_rejected_for_existing_schema_profiles_before_writes() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([19; 32]);
        auth::grant_admin(admin);
        let args = migration_args_with_selector(
            "000001_type",
            None,
            &SchemaMigrationGraphSelector::Named("social".into()),
            "CREATE GRAPH TYPE typed_type { NODE Person }",
        );
        assert!(matches!(
            store.admin_apply_schema_migration(admin, args),
            Err(RouterError::InvalidArgument(message))
                if message.contains("named graph selectors")
        ));
        assert_eq!(
            ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.len()),
            0
        );
        assert!(
            ROUTER_GRAPH_TYPE_CATALOG
                .with_borrow(|catalog| { catalog.get_id("typed_type").is_none() })
        );
    }

    #[test]
    fn create_index_migration_is_rejected_before_catalog_or_ledger_writes() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([20; 32]);
        auth::grant_admin(admin);
        let args = migration_args(
            "000001_age_index",
            None,
            "CREATE INDEX person_age FOR (n:Person) ON (n.age)",
        );
        assert!(matches!(
            store.admin_apply_schema_migration(admin, args),
            Err(RouterError::InvalidArgument(message))
                if message.contains("backfill lifecycle")
        ));
        assert_eq!(
            ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.len()),
            0
        );
        assert!(
            ROUTER_GRAPH_TYPE_CATALOG.with_borrow(|catalog| { catalog.get_id("Person").is_none() })
        );
    }

    #[test]
    fn unsupported_create_index_record_does_not_replay_before_lifecycle_validation() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([21; 32]);
        auth::grant_admin(admin);
        let statement = "CREATE INDEX person_age FOR (n:Person) ON (n.age)";
        let selector = SchemaMigrationGraphSelector::Default;
        let record = SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
            id: "000001_age_index".into(),
            parent: None,
            graph_selector: selector.clone(),
            resolved_graph: None,
            checksum: gleaph_migration_api::schema_migration_checksum(
                "000001_age_index",
                None,
                &selector,
                statement.as_bytes(),
            ),
            actor: admin,
            recorded_at: 1,
            statement: statement.into(),
            profile: vec![SchemaMigrationStatementProfile::CreateIndex],
            state: SchemaMigrationRecordState::Applied { applied_at: 1 },
        });
        ROUTER_SCHEMA_MIGRATIONS.with_borrow_mut(|ledger| {
            ledger.insert(
                "000001_age_index".into(),
                StableSchemaMigrationRecord(record),
            );
        });

        assert!(matches!(
            store.admin_apply_schema_migration(
                admin,
                migration_args_with_selector(
                    "000001_age_index",
                    None,
                    &selector,
                    statement,
                ),
            ),
            Err(RouterError::InvalidArgument(message))
                if message.contains("backfill lifecycle")
        ));
    }

    #[test]
    fn named_schema_record_does_not_replay_before_selector_validation() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([22; 32]);
        auth::grant_admin(admin);
        let statement = "CREATE GRAPH TYPE typed_type { NODE Person }";
        let selector = SchemaMigrationGraphSelector::Named("social".into());
        let record = SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
            id: "000001_type".into(),
            parent: None,
            graph_selector: selector.clone(),
            resolved_graph: None,
            checksum: gleaph_migration_api::schema_migration_checksum(
                "000001_type",
                None,
                &selector,
                statement.as_bytes(),
            ),
            actor: admin,
            recorded_at: 1,
            statement: statement.into(),
            profile: vec![SchemaMigrationStatementProfile::CreateGraphType],
            state: SchemaMigrationRecordState::Applied { applied_at: 1 },
        });
        ROUTER_SCHEMA_MIGRATIONS.with_borrow_mut(|ledger| {
            ledger.insert("000001_type".into(), StableSchemaMigrationRecord(record));
        });

        assert!(matches!(
            store.admin_apply_schema_migration(
                admin,
                migration_args_with_selector("000001_type", None, &selector, statement),
            ),
            Err(RouterError::InvalidArgument(message))
                if message.contains("named graph selectors")
        ));
    }

    #[test]
    fn typed_graph_preflight_rejects_existing_inline_struct_index_without_mutation() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([19; 32]);
        auth::grant_admin(admin);
        register_test_graph(&store, admin, "conflict_graph");
        let graph_id = lookup_graph_id("conflict_graph").expect("registered graph");

        let type_statement = "CREATE GRAPH TYPE conflict_type { NODE City AS city, DIRECTED EDGE Road LABEL ROAD { stats RECORD { score FLOAT32 } INLINE } CONNECTING (city -> city) }";
        store
            .admin_apply_schema_migration(
                admin,
                migration_args("000001_type", None, type_statement),
            )
            .expect("graph type migration applies");

        let label_id =
            RouterStore::commit_intern_edge_label_name(graph_id, "ROAD").expect("edge label");
        let property_id =
            RouterStore::commit_intern_property_name(graph_id, "stats").expect("property");
        let index_name_id =
            index_name_catalog::intern_index_name(graph_id, "stats_index").expect("index name");
        indexed_catalog::create_named_index(
            graph_id,
            index_name_id,
            crate::planner_stats::IndexCatalogEntry {
                kind: IndexedPropertyKind::Edge,
                vertex_label: None,
                edge_label: Some("ROAD".into()),
                property: "stats".into(),
                edge_direction: Some(EdgeDirection::AnyDirection),
            },
            property_id,
            label_id.raw(),
            Some(EdgeIndexDirection::Any),
            false,
        )
        .expect("pre-existing edge index");

        let ledger_before = ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| {
            ledger
                .iter()
                .map(|entry| (entry.key().clone(), entry.value().0.clone()))
                .collect::<Vec<_>>()
        });
        let type_id_before = ROUTER_GRAPH_TYPE_CATALOG
            .with_borrow(|catalog| catalog.get_id("conflict_type"))
            .expect("type id");
        let graph_type_before =
            graph_type_catalog::parsed_graph_type_definition_for_type_id(type_id_before)
                .expect("type definition");
        let profile_before = ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow(|profiles| profiles.get_record(graph_id, label_id));

        let error = store
            .admin_apply_schema_migration(
                admin,
                migration_args(
                    "000002_bind",
                    Some("000001_type"),
                    "CREATE GRAPH conflict_graph TYPED conflict_type",
                ),
            )
            .expect_err("existing inline property index must reject typed binding");
        assert!(
            matches!(error, RouterError::Conflict(ref message) if message.contains("property index")),
            "expected property-index conflict, got {error:?}"
        );

        assert_eq!(
            ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| {
                ledger
                    .iter()
                    .map(|entry| (entry.key().clone(), entry.value().0.clone()))
                    .collect::<Vec<_>>()
            }),
            ledger_before
        );
        assert_eq!(
            graph_type_catalog::parsed_graph_type_definition_for_graph_id(graph_id)
                .expect("graph binding lookup"),
            None
        );
        assert_eq!(
            ROUTER_GRAPH_TYPE_CATALOG.with_borrow(|catalog| {
                (
                    catalog.len(),
                    catalog.get_id("conflict_type"),
                    catalog.get_name(type_id_before),
                )
            }),
            (1, Some(type_id_before), Some("conflict_type".to_owned()))
        );
        assert_eq!(
            graph_type_catalog::parsed_graph_type_definition_for_type_id(type_id_before)
                .expect("type definition after rejection"),
            graph_type_before
        );
        assert_eq!(
            store
                .lookup_edge_label_id(graph_id, "ROAD")
                .expect("label after rejection"),
            label_id
        );
        assert_eq!(
            store
                .lookup_property_id(graph_id, "stats")
                .expect("property after rejection"),
            property_id
        );
        assert_eq!(
            ROUTER_EDGE_INLINE_PROPERTY_PROFILES
                .with_borrow(|profiles| profiles.get_record(graph_id, label_id)),
            profile_before
        );
        assert!(edge_index_uses_property_label(
            graph_id,
            property_id,
            label_id.raw()
        ));
    }

    #[test]
    fn apply_enforces_statement_and_page_bounds_without_writes() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([15; 32]);
        auth::grant_admin(admin);
        assert!(matches!(
            store.list_schema_migrations(list_args(None, 0)),
            Err(RouterError::InvalidArgument(_))
        ));
        assert!(matches!(
            store.list_schema_migrations(list_args(None, 17)),
            Err(RouterError::InvalidArgument(_))
        ));

        let invalid_id = migration_args(
            "bad-id",
            None,
            "CREATE GRAPH TYPE invalid_id_type { NODE Person }",
        );
        assert!(matches!(
            store.admin_apply_schema_migration(admin, invalid_id),
            Err(RouterError::InvalidArgument(_))
        ));
        let oversized_statement =
            "x".repeat(gleaph_migration_api::MAX_SCHEMA_MIGRATION_STATEMENT_BYTES + 1);
        let oversized = migration_args("000001_oversized", None, &oversized_statement);
        assert!(matches!(
            store.admin_apply_schema_migration(admin, oversized),
            Err(RouterError::InvalidArgument(_))
        ));
        assert_eq!(
            ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.len()),
            0
        );
    }

    #[test]
    fn stable_record_codec_round_trips_versioned_wire_record() {
        let record = SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
            id: "000001_root".into(),
            parent: None,
            graph_selector: SchemaMigrationGraphSelector::Default,
            resolved_graph: None,
            checksum: gleaph_migration_api::schema_migration_checksum(
                "000001_root",
                None,
                &SchemaMigrationGraphSelector::Default,
                b"CREATE GRAPH TYPE root_type { NODE Person }",
            ),
            actor: Principal::from_slice(&[12; 29]),
            recorded_at: 42,
            statement: "CREATE GRAPH TYPE root_type { NODE Person }".into(),
            profile: vec![SchemaMigrationStatementProfile::CreateGraphType],
            state: SchemaMigrationRecordState::Applied { applied_at: 42 },
        });
        let stored = StableSchemaMigrationRecord(record.clone());
        let encoded = stored.to_bytes();
        let decoded = StableSchemaMigrationRecord::from_bytes(Cow::Owned(encoded.into_owned()));
        assert_eq!(decoded.0, record);

        let memory: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mut first = BTreeMap::init(memory.clone());
        first.insert("000001_root".to_owned(), stored);
        drop(first);
        let reopened = BTreeMap::<String, StableSchemaMigrationRecord, VectorMemory>::init(memory);
        assert_eq!(
            reopened.get(&"000001_root".to_owned()),
            Some(StableSchemaMigrationRecord(record))
        );
    }

    #[test]
    fn stable_record_bound_covers_maximum_wire_shape() {
        let id = format!(
            "000001_{}",
            "a".repeat(gleaph_migration_api::MAX_SCHEMA_MIGRATION_ID_BYTES - 7)
        );
        let parent = format!(
            "000000_{}",
            "p".repeat(gleaph_migration_api::MAX_SCHEMA_MIGRATION_ID_BYTES - 7)
        );
        let statement = "x".repeat(gleaph_migration_api::MAX_SCHEMA_MIGRATION_STATEMENT_BYTES);
        let graph_name = "g".repeat(gleaph_migration_api::MAX_SCHEMA_MIGRATION_GRAPH_NAME_BYTES);
        assert_eq!(
            id.len(),
            gleaph_migration_api::MAX_SCHEMA_MIGRATION_ID_BYTES
        );
        assert_eq!(
            parent.len(),
            gleaph_migration_api::MAX_SCHEMA_MIGRATION_ID_BYTES
        );
        let record =
            StableSchemaMigrationRecord(SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
                id: id.clone(),
                parent: Some(parent.clone()),
                graph_selector: SchemaMigrationGraphSelector::Named(graph_name.clone()),
                resolved_graph: Some(ResolvedSchemaMigrationGraph {
                    graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(u32::MAX),
                    graph_name,
                }),
                checksum: gleaph_migration_api::schema_migration_checksum(
                    &id,
                    Some(&parent),
                    &SchemaMigrationGraphSelector::Named(
                        "g".repeat(gleaph_migration_api::MAX_SCHEMA_MIGRATION_GRAPH_NAME_BYTES),
                    ),
                    statement.as_bytes(),
                ),
                actor: Principal::from_slice(&[0xff; 29]),
                recorded_at: u64::MAX,
                statement,
                profile: vec![
                    SchemaMigrationStatementProfile::CreateGraphType;
                    gleaph_migration_api::MAX_SCHEMA_MIGRATION_STATEMENTS
                ],
                state: SchemaMigrationRecordState::Applied {
                    applied_at: u64::MAX,
                },
            }));
        let encoded = record.to_bytes();
        let ic_stable_structures::storable::Bound::Bounded {
            max_size,
            is_fixed_size,
        } = StableSchemaMigrationRecord::BOUND
        else {
            panic!("schema migration record must use a bounded variable-size encoding");
        };
        assert!(!is_fixed_size);
        assert_eq!(
            max_size,
            crate::facade::stable::schema_migration::MAX_SCHEMA_MIGRATION_RECORD_BYTES
        );
        assert!(
            encoded.len() <= max_size as usize,
            "maximum record encoded to {} bytes, bound is {max_size}",
            encoded.len()
        );
    }

    fn migration_id(record: &SchemaMigrationRecord) -> &str {
        match record {
            SchemaMigrationRecord::V1(record) => &record.id,
        }
    }
}
