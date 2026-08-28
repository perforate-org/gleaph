//! `gleaph grants apply` — apply the project's declarative data-plane grant policy.
//!
//! `grants/*.gql` files hold GRANT/REVOKE statements; each file is one program sent
//! through the same `gql_mutate` control path `prepared publish` uses (there is no
//! dedicated publication endpoint — GRANT/REVOKE ride the host control path and the
//! executor enforces registry-owner-only per statement). Application is additive
//! and idempotent: grant rows are upserts (re-granting replaces the row), so re-running
//! an unchanged policy converges; the policy file is a floor, never a reconcile — runtime
//! grants made by the registry owner are not revoked by absence from the file.
//!
//! This deliberately does NOT ride the migration lane: migrations are apply-once
//! immutable schema transformations replayable across environments, while grants are
//! mutable, revocable, environment-specific authorization relationships (a grant may be
//! revoked after its migration-era apply, which would falsify an apply-once ledger).

use std::path::{Path, PathBuf};

use gleaph_gql::parser;

use crate::config::{self, LoadedConfig};
use crate::remote::RemoteTransport;

/// `gleaph grants` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum GrantsCommand {
    /// Apply the grant policy directory in sorted filename order (additive, idempotent).
    Apply(GrantsApplyArgs),
}

/// CLI arguments for `gleaph grants apply`.
#[derive(Debug, clap::Args)]
pub struct GrantsApplyArgs {
    /// Grant policy directory (`*.gql`, applied in sorted filename order); defaults to
    /// `[dirs] grants` in gleaph.toml, then the built-in `grants`.
    #[arg(short = 'd', long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// Apply a single policy file instead of the whole directory.
    #[arg(long, value_name = "PATH", conflicts_with = "dir")]
    pub file: Option<PathBuf>,
    /// Router canister principal (required unless supplied by GLEAPH_CANISTER or `gleaph.toml`).
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: Option<String>,
    /// Network name (ic/local) or an HTTP(S) endpoint URL.
    #[arg(short = 'n', long, value_name = "NETWORK")]
    pub network: Option<String>,
    /// PEM file containing a Secp256k1 identity.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<PathBuf>,
    /// Fetch the network root key before querying a custom endpoint.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub fetch_root_key: Option<bool>,
}

/// One validated grant policy file.
#[derive(Debug)]
struct PolicyFile {
    path: PathBuf,
    /// Relative display name (e.g. `grants/knowledge.gql`).
    display: String,
    program: String,
}

#[derive(Debug, thiserror::Error)]
pub enum GrantsError {
    #[error("read policy {0}: {1}")]
    Read(String, #[source] std::io::Error),
    #[error("policy {0}: {1}")]
    Invalid(String, String),
    #[error("transport: {0}")]
    Remote(String),
}

/// Validate one policy file: it must parse and be authorization-only. A policy file is
/// not a program — MATCH/INSERT mixed with GRANT is rejected so `grants apply` can never
/// smuggle data-plane work through the policy surface.
fn validate(display: &str, source: &str) -> Result<(), GrantsError> {
    let program =
        parser::parse(source).map_err(|e| GrantsError::Invalid(display.into(), e.to_string()))?;
    let flags = gleaph_gql::program_modification::classify_program(&program);
    if flags.has_data_modification || flags.has_call_procedure || flags.has_catalog_modification {
        return Err(GrantsError::Invalid(
            display.into(),
            "policy files may contain only GRANT/REVOKE statements".into(),
        ));
    }
    if !flags.has_authorization_modification {
        return Err(GrantsError::Invalid(
            display.into(),
            "no GRANT/REVOKE statements found".into(),
        ));
    }
    Ok(())
}

/// Collect policy files: sorted `*.gql` of the directory (deterministic order, like the
/// migration lane's numbered ordering — the filename controls application order).
fn collect_policies(dir: &Path) -> Result<Vec<PolicyFile>, GrantsError> {
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| GrantsError::Read(dir.display().to_string(), e))?;
    if !meta.is_dir() {
        return Err(GrantsError::Invalid(
            dir.display().to_string(),
            "not a directory; create grants/ or pass --dir/--file".into(),
        ));
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| GrantsError::Read(dir.display().to_string(), e))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "gql"))
        .collect();
    entries.sort();
    if entries.is_empty() {
        return Err(GrantsError::Invalid(
            dir.display().to_string(),
            "no .gql policy files found".into(),
        ));
    }
    entries
        .into_iter()
        .map(|path| {
            let display = path.display().to_string();
            let program = std::fs::read_to_string(&path)
                .map_err(|e| GrantsError::Read(display.clone(), e))?;
            Ok(PolicyFile {
                path,
                display,
                program,
            })
        })
        .collect()
}

/// Apply the grant policy. All files are validated before any wire call so a failing
/// policy never leaves a partially validated directory; each file then executes as one
/// `gql_mutate` program with a content-stable idempotency key (the same convention
/// `prepared publish` uses), so a re-run of an unchanged policy is a no-op replay and a
/// changed file is a fresh mutation.
pub fn apply(
    args: &GrantsApplyArgs,
    loaded: Option<&LoadedConfig>,
    project_root: Option<&Path>,
    canister: &str,
    network: &str,
    identity: Option<&Path>,
    fetch_root_key: bool,
) -> Result<(), GrantsError> {
    let policies: Vec<PolicyFile> = if let Some(file) = &args.file {
        let display = file.display().to_string();
        let program =
            std::fs::read_to_string(file).map_err(|e| GrantsError::Read(display.clone(), e))?;
        vec![PolicyFile {
            path: file.clone(),
            display,
            program,
        }]
    } else {
        let dir = config::resolved_dir(args.dir.as_deref(), loaded, config::DirKey::Grants);
        let dir = if dir.is_absolute() {
            dir
        } else {
            match project_root {
                Some(root) => root.join(dir),
                None => dir,
            }
        };
        collect_policies(&dir)?
    };
    for policy in &policies {
        validate(&policy.display, &policy.program)?;
    }

    let remote =
        RemoteTransport::connect(canister, network, identity, fetch_root_key, project_root)
            .map_err(GrantsError::Remote)?;
    for policy in &policies {
        // Same control path and idempotency-key convention as `prepared publish`
        // (gleaph-authorization:<deterministic suffix>); the key derives from the policy
        // content so an unchanged re-apply replays the same (caller, graph, key) scope
        // while a changed file is a new mutation.
        let mutation_key = format!("gleaph-authorization:grants:{}", policy.program);
        let decoded: Result<
            gleaph_graph_kernel::plan_exec::GqlQueryResult,
            gleaph_graph_kernel::federation::RouterError,
        > = remote
            .update_args(
                "gql_mutate",
                (&policy.program, &Vec::<u8>::new(), &mutation_key),
            )
            .map_err(GrantsError::Remote)?;
        match decoded {
            Ok(_) => println!("applied {}", policy.display),
            Err(error) => {
                return Err(GrantsError::Invalid(
                    policy.display.clone(),
                    format!("router rejected the policy: {error:?}"),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate_str(source: &str) -> Result<(), GrantsError> {
        validate("test.gql", source)
    }

    #[test]
    fn accepts_pure_grant_policy() {
        validate_str(
            "GRANT MATCH ON GRAPH knowledge NODES Person TO PUBLIC\n\
             GRANT TRAVERSE ON GRAPH knowledge EDGES RELATED_TO TO PUBLIC\n",
        )
        .unwrap();
    }

    #[test]
    fn accepts_revoke_policy() {
        validate_str("REVOKE MATCH ON GRAPH knowledge NODES Person FROM PUBLIC").unwrap();
    }

    #[test]
    fn rejects_insert_only_policy() {
        let err = validate_str("INSERT (n:Person)").unwrap_err();
        assert!(err.to_string().contains("only GRANT/REVOKE"), "{err}");
    }

    #[test]
    fn rejects_insert_grant_sequence_as_parse_error() {
        // The parser itself refuses the sequence; validate() surfaces it as an error
        // either way — no data modification passes the policy surface.
        assert!(
            validate_str(
                "INSERT (n:Person)\nGRANT MATCH ON GRAPH knowledge NODES Person TO PUBLIC"
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_match_only_policy() {
        let err = validate_str("MATCH (n) RETURN n").unwrap_err();
        assert!(err.to_string().contains("no GRANT/REVOKE"), "{err}");
    }

    #[test]
    fn rejects_parse_error() {
        assert!(validate_str("GRANT BROKEN (").is_err());
    }

    #[test]
    fn rejects_empty_policy() {
        let err = validate_str("").unwrap_err();
        assert!(err.to_string().contains("no GRANT/REVOKE"), "{err}");
    }

    #[test]
    fn collect_policies_sorts_and_filters_gql() {
        let tmp = std::env::temp_dir().join(format!("gleaph-grants-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("b.gql"),
            "GRANT MATCH ON GRAPH g NODES N TO PUBLIC",
        )
        .unwrap();
        std::fs::write(
            tmp.join("a.gql"),
            "GRANT TRAVERSE ON GRAPH g NODES N TO PUBLIC",
        )
        .unwrap();
        std::fs::write(tmp.join("notes.txt"), "ignored").unwrap();
        let policies = collect_policies(&tmp).unwrap();
        let names: Vec<String> = policies
            .iter()
            .map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.gql", "b.gql"]);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn collect_policies_rejects_empty_dir() {
        let tmp = std::env::temp_dir().join(format!("gleaph-grants-empty-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let err = collect_policies(&tmp).unwrap_err();
        assert!(err.to_string().contains("no .gql"), "{err}");
        std::fs::remove_dir(&tmp).unwrap();
    }
}
