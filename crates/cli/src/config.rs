//! Project-scoped `gleaph.toml` configuration (ADR 0062).
//!
//! Owns config discovery (walk-up from the working directory, `GLEAPH_CONFIG` override), strict
//! TOML parsing, per-network `[deployment.<network>]` profiles, and the field-level merge that
//! supplies command defaults with precedence flag > env > config > built-in default. Subcommand
//! modules keep receiving resolved values; this module is the only place that touches the file.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Config file name discovered by walking up from the working directory.
pub const CONFIG_FILE: &str = "gleaph.toml";

/// `.gleaph/` directory name, located in the same directory as `gleaph.toml` (ADR 0062 §13).
pub const GLEAPH_DIR: &str = ".gleaph";

/// Platform-fixed canister-id mapping directory under `.gleaph/` (committed).
pub const MAPPINGS_DIR: &str = "data/mappings";

/// Per-user Router-id cache directory under `.gleaph/` (gitignored).
pub const ACCOUNT_CACHE_DIR: &str = "cache/account";

/// Current config format version; any other value is rejected.
pub const CONFIG_FORMAT_VERSION: u32 = 1;

/// Built-in network default when neither the flag, an env var, nor the config supplies one.
pub const DEFAULT_NETWORK: &str = "ic";

/// Built-in migration directory default (working-directory-relative).
pub const DEFAULT_MIGRATIONS_DIR: &str = "migrations";

/// Built-in prepared directory default (working-directory-relative).
pub const DEFAULT_PREPARED_DIR: &str = "prepared";

/// Built-in default for the `[dirs] grants` entry (ADR 0074 policy surface).
pub const DEFAULT_GRANTS_DIR: &str = "grants";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read config {path}: {error}")]
    Read { path: PathBuf, error: String },
    #[error("parse config {path}: {error}")]
    Parse { path: PathBuf, error: String },
    #[error("unsupported config format_version {0}; expected {CONFIG_FORMAT_VERSION}")]
    FormatVersion(u32),
    #[error(
        "invalid deployment network name {name:?}; expected \"ic\", \"local\", or an http(s) URL"
    )]
    InvalidNetworkName { name: String },
    #[error(
        "fetch_root_key applies only to custom-URL deployment entries; remove it from [deployment.{network}]"
    )]
    FetchRootKeyOnNamedNetwork { network: String },
    #[error("GLEAPH_FETCH_ROOT_KEY must be \"true\" or \"false\", got {value:?}")]
    InvalidFetchRootKeyEnv { value: String },
}

/// The `GLEAPH_*` environment overrides, read once at dispatch entry.
///
/// Snapshotting keeps dispatch testable without mutating process-global variables.
#[derive(Clone, Debug, Default)]
pub struct ConfigEnv {
    pub config: Option<String>,
    pub environment: Option<String>,
    pub network: Option<String>,
    pub canister: Option<String>,
    pub identity: Option<String>,
    pub router: Option<String>,
    pub fetch_root_key: Option<bool>,
}

impl ConfigEnv {
    /// Read the overrides from the process environment.
    pub fn from_process() -> Result<Self, ConfigError> {
        Self::from_env_iter(std::env::vars())
    }

    /// Build a snapshot from an explicit variable iterator (unit-testable).
    pub fn from_env_iter<I>(iter: I) -> Result<Self, ConfigError>
    where
        I: Iterator<Item = (String, String)>,
    {
        let mut env = ConfigEnv::default();
        for (key, value) in iter {
            match key.as_str() {
                "GLEAPH_CONFIG" => env.config = Some(value),
                "GLEAPH_ENVIRONMENT" => env.environment = Some(value),
                "GLEAPH_NETWORK" => env.network = Some(value),
                "GLEAPH_CANISTER" => env.canister = Some(value),
                "GLEAPH_IDENTITY" => env.identity = Some(value),
                "GLEAPH_ROUTER" => env.router = Some(value),
                "GLEAPH_FETCH_ROOT_KEY" => {
                    env.fetch_root_key = Some(match value.as_str() {
                        "true" => true,
                        "false" => false,
                        other => {
                            return Err(ConfigError::InvalidFetchRootKeyEnv {
                                value: other.to_owned(),
                            });
                        }
                    });
                }
                _ => {}
            }
        }
        Ok(env)
    }
}

/// One parsed `gleaph.toml`; every table is strict (`deny_unknown_fields`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Config format version; optional, defaults to 1.
    #[serde(default)]
    format_version: Option<u32>,
    /// Default `-n/--network` value; the built-in default remains "ic".
    #[serde(default)]
    default_network: Option<String>,
    #[serde(default)]
    dirs: Option<Dirs>,
    #[serde(default)]
    deployment: BTreeMap<String, DeploymentProfile>,
    #[serde(default)]
    codegen: Option<CodegenConfig>,
    #[serde(default)]
    load: Option<LoadConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dirs {
    migrations: Option<PathBuf>,
    prepared: Option<PathBuf>,
    grants: Option<PathBuf>,
}

/// One `[deployment.<network>]` entry. `fetch_root_key` is valid only for custom-URL keys
/// (ADR 0062 §7); the presence check happens in [`Config::validate`].
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentProfile {
    pub canister: Option<String>,
    pub identity: Option<PathBuf>,
    pub fetch_root_key: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodegenConfig {
    pub target: Option<String>,
    pub output: Option<PathBuf>,
    pub graph: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadConfig {
    pub graph: Option<String>,
    pub key: Option<String>,
    pub state_file: Option<PathBuf>,
}

/// A parsed config plus the path it was read from (the base for relative paths).
#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: Config,
}

impl Config {
    /// Load the nearest `gleaph.toml` (walk-up from `cwd`), or the `GLEAPH_CONFIG` path when set.
    /// An explicit path that does not exist is an error (no walk-up fallback); absence otherwise
    /// yields `Ok(None)`.
    pub fn load(cwd: &Path, env: &ConfigEnv) -> Result<Option<LoadedConfig>, ConfigError> {
        let path = match &env.config {
            Some(explicit) => PathBuf::from(explicit),
            None => match find_up(cwd, CONFIG_FILE)? {
                Some(found) => found,
                None => return Ok(None),
            },
        };
        let text = std::fs::read_to_string(&path).map_err(|error| ConfigError::Read {
            path: path.clone(),
            error: error.to_string(),
        })?;
        let config: Config = toml::from_str(&text).map_err(|error| ConfigError::Parse {
            path: path.clone(),
            error: error.to_string(),
        })?;
        config.validate()?;
        Ok(Some(LoadedConfig { path, config }))
    }

    /// Fail-closed checks that are not representable as TOML schema rules.
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(version) = self.format_version
            && version != CONFIG_FORMAT_VERSION
        {
            return Err(ConfigError::FormatVersion(version));
        }
        for (name, profile) in &self.deployment {
            let is_url = name.starts_with("http://") || name.starts_with("https://");
            if name != "ic" && name != "local" && !is_url {
                return Err(ConfigError::InvalidNetworkName { name: name.clone() });
            }
            if !is_url && profile.fetch_root_key.is_some() {
                return Err(ConfigError::FetchRootKeyOnNamedNetwork {
                    network: name.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn default_network(&self) -> &str {
        self.default_network.as_deref().unwrap_or(DEFAULT_NETWORK)
    }

    pub fn deployment(&self, network: &str) -> Option<&DeploymentProfile> {
        self.deployment.get(network)
    }

    pub fn codegen(&self) -> Option<&CodegenConfig> {
        self.codegen.as_ref()
    }

    pub fn load_config(&self) -> Option<&LoadConfig> {
        self.load.as_ref()
    }
}

fn find_up(start: &Path, name: &str) -> Result<Option<PathBuf>, ConfigError> {
    let mut directory = Some(start);
    while let Some(current) = directory {
        let candidate = current.join(name);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        directory = current.parent();
    }
    Ok(None)
}

/// Resolve a config-file-relative path against the config file's directory; absolute paths pass
/// through unchanged.
pub fn resolve_config_path(config_path: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_owned()
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(value)
    }
}

/// The `.gleaph/` directory for a loaded config: `<config_dir>/.gleaph`.
pub fn gleaph_dir(loaded: &LoadedConfig) -> PathBuf {
    loaded
        .path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(GLEAPH_DIR)
}

/// The platform-fixed id mapping file for an environment: `<config_dir>/.gleaph/data/mappings/<env>.ids.json`.
pub fn mapping_path(loaded: &LoadedConfig, environment: &str) -> PathBuf {
    gleaph_dir(loaded)
        .join(MAPPINGS_DIR)
        .join(format!("{environment}.ids.json"))
}

/// Read the platform-fixed canister ids for an environment from `.gleaph/data/mappings/<env>.ids.json`.
/// Returns an empty map when the file is absent (the mapping is generated by `gleaph network start`).
pub fn read_mapping(
    loaded: &LoadedConfig,
    environment: &str,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let path = mapping_path(loaded, environment);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return Ok(BTreeMap::new()),
    };
    serde_json::from_str(&text).map_err(|error| ConfigError::Read {
        path,
        error: format!("parse mapping: {error}"),
    })
}

/// Write the platform-fixed canister ids for an environment to
/// `.gleaph/data/mappings/<env>.ids.json`, creating the directory as needed.
pub fn write_mapping(
    loaded: &LoadedConfig,
    environment: &str,
    mapping: &BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    let path = mapping_path(loaded, environment);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| ConfigError::Read {
            path: parent.to_owned(),
            error: format!("create mapping dir: {error}"),
        })?;
    }
    let text = serde_json::to_string_pretty(mapping).map_err(|error| ConfigError::Read {
        path: path.clone(),
        error: format!("encode mapping: {error}"),
    })?;
    std::fs::write(&path, text).map_err(|error| ConfigError::Read {
        path,
        error: format!("write mapping: {error}"),
    })
}

/// The per-user Router-id cache file: `<config_dir>/.gleaph/cache/account/<env>.router.json`.
pub fn router_cache_path(loaded: &LoadedConfig, environment: &str) -> PathBuf {
    gleaph_dir(loaded)
        .join(ACCOUNT_CACHE_DIR)
        .join(format!("{environment}.router.json"))
}

/// Read the cached Router id for an environment, if present.
pub fn read_router_cache(loaded: &LoadedConfig, environment: &str) -> Option<String> {
    let path = router_cache_path(loaded, environment);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<String>(&text).ok())
}

/// Write the Router id cache for an environment, creating `.gleaph/cache/account/` as needed.
pub fn write_router_cache(loaded: &LoadedConfig, environment: &str, router_id: &str) {
    let path = router_cache_path(loaded, environment);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &path,
        serde_json::to_string(router_id).expect("encode router id"),
    );
}

/// Effective network: flag > `GLEAPH_NETWORK` > `default_network` > "ic".
pub fn effective_network(flag: Option<&str>, env: &ConfigEnv, config: Option<&Config>) -> String {
    flag.map(str::to_owned)
        .or_else(|| env.network.clone())
        .or_else(|| config.map(Config::default_network).map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_NETWORK.to_owned())
}

/// Effective environment: `GLEAPH_ENVIRONMENT` > effective network. The environment selects the
/// `.gleaph/data/mappings/<env>.ids.json` and `.gleaph/cache/account/<env>.router.json` files.
/// It defaults to the network name so a single-network project needs no extra config.
pub fn effective_environment(env: &ConfigEnv, network: &str) -> String {
    env.environment
        .clone()
        .unwrap_or_else(|| network.to_owned())
}

/// One resolved connection field set. `canister` stays optional until the caller requires it
/// (`migration`/`prepared`/`load` do; `codegen` can use the local manifest source).
#[derive(Clone, Debug)]
pub struct RemoteOptions {
    pub canister: Option<String>,
    pub network: String,
    pub identity: Option<PathBuf>,
    pub fetch_root_key: bool,
}

/// Resolve the shared connection fields with precedence flag > env > config > built-in default.
///
/// `fetch_root_key` uses the network resolution in `remote.rs` unchanged: the effective value here
/// is the flag/env/URL-entry value (default `false`), and `local` still fetches the root key
/// regardless (ADR 0062 §7).
pub fn merge_remote(
    canister_flag: Option<&str>,
    network_flag: Option<&str>,
    identity_flag: Option<&Path>,
    fetch_root_key_flag: Option<bool>,
    env: &ConfigEnv,
    loaded: Option<&LoadedConfig>,
) -> Result<RemoteOptions, ConfigError> {
    let config = loaded.map(|loaded| &loaded.config);
    let network = effective_network(network_flag, env, config);
    let profile = config.and_then(|config| config.deployment(&network));
    let canister = canister_flag
        .map(str::to_owned)
        .or_else(|| env.canister.clone())
        .or_else(|| profile.and_then(|profile| profile.canister.clone()));
    // Last resort: the active session's signing source (the same resolution
    // `network start` auto-registration uses), so data-plane commands work without
    // a repeated --identity after `gleaph identity new` / `login` / `import`.
    let has_icp_yaml = loaded
        .as_ref()
        .and_then(|loaded| loaded.path.parent())
        .map(crate::identity::has_icp_yaml)
        .unwrap_or(false);
    let identity = identity_flag
        .map(Path::to_path_buf)
        .or_else(|| env.identity.as_ref().map(PathBuf::from))
        .or_else(|| {
            loaded.zip(profile).and_then(|(loaded, profile)| {
                profile
                    .identity
                    .as_ref()
                    .map(|path| resolve_config_path(&loaded.path, path))
            })
        })
        // Last resort: the active session's signing source (the same resolution
        // `network start` auto-registration uses), so data-plane commands work without
        // a repeated --identity after `gleaph identity new` / `login` / `import`.
        .or_else(|| {
            crate::auth::load_session()
                .and_then(|session| crate::identity::session_pem(&session, has_icp_yaml).ok())
                .map(PathBuf::from)
        });
    // `--fetch-root-key` is a SetTrue flag, so it can only express "true": clap yields
    // `Some(false)` when the flag is absent, which must not shadow an env/config value.
    let fetch_root_key = fetch_root_key_flag
        .filter(|value| *value)
        .or(env.fetch_root_key)
        .or_else(|| profile.and_then(|profile| profile.fetch_root_key))
        .unwrap_or(false);
    Ok(RemoteOptions {
        canister,
        network,
        identity,
        fetch_root_key,
    })
}

/// Which `[dirs]` entry supplies a command's `--dir` default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirKey {
    Migrations,
    Prepared,
    Grants,
}

/// `--dir` precedence: flag > `[dirs]` (config-relative) > built-in cwd-relative default.
pub fn resolved_dir(flag: Option<&Path>, loaded: Option<&LoadedConfig>, key: DirKey) -> PathBuf {
    if let Some(flag) = flag {
        return flag.to_owned();
    }
    if let Some(loaded) = loaded {
        let value = match key {
            DirKey::Migrations => loaded
                .config
                .dirs
                .as_ref()
                .and_then(|dirs| dirs.migrations.as_ref()),
            DirKey::Prepared => loaded
                .config
                .dirs
                .as_ref()
                .and_then(|dirs| dirs.prepared.as_ref()),
            DirKey::Grants => loaded
                .config
                .dirs
                .as_ref()
                .and_then(|dirs| dirs.grants.as_ref()),
        };
        if let Some(value) = value {
            return resolve_config_path(&loaded.path, value);
        }
    }
    match key {
        DirKey::Migrations => PathBuf::from(DEFAULT_MIGRATIONS_DIR),
        DirKey::Prepared => PathBuf::from(DEFAULT_PREPARED_DIR),
        DirKey::Grants => PathBuf::from(DEFAULT_GRANTS_DIR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_root(tag: &str) -> PathBuf {
        let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gleaph-cli-config-{}-{nonce}-{tag}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("temporary config root");
        root
    }

    fn write_config(root: &Path, content: &str) -> PathBuf {
        let path = root.join(CONFIG_FILE);
        fs::write(&path, content).expect("temporary config write");
        path
    }

    fn load_at(cwd: &Path, env: &ConfigEnv) -> Result<Option<LoadedConfig>, ConfigError> {
        Config::load(cwd, env)
    }

    #[test]
    fn discovery_walks_up_to_the_nearest_config() {
        let root = temp_root("walkup");
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).expect("nested directory");
        let config_path = write_config(&root, "format_version = 1\n");
        let loaded = load_at(&nested, &ConfigEnv::default())
            .expect("walk-up must find the ancestor config")
            .expect("config must load");
        assert_eq!(loaded.path, config_path);
    }

    #[test]
    fn discovery_without_a_config_yields_none() {
        let root = temp_root("absent");
        assert!(
            load_at(&root, &ConfigEnv::default())
                .expect("no error")
                .is_none()
        );
    }

    #[test]
    fn explicit_config_env_overrides_walk_up_and_missing_path_errors() {
        let root = temp_root("explicit");
        let missing = root.join("missing.toml");
        let env = ConfigEnv {
            config: Some(missing.to_string_lossy().into_owned()),
            ..ConfigEnv::default()
        };
        let error = load_at(&root, &env).expect_err("a missing explicit path must error");
        assert!(error.to_string().contains("read config"));
    }

    #[test]
    fn explicit_config_env_disables_walk_up() {
        let root = temp_root("env-override");
        let sibling = root.join("sibling");
        fs::create_dir_all(&sibling).expect("sibling directory");
        write_config(&root, "default_network = \"local\"\n");
        let explicit = sibling.join("other.toml");
        fs::write(&explicit, "format_version = 1\n").expect("explicit config write");
        let env = ConfigEnv {
            config: Some(explicit.to_string_lossy().into_owned()),
            ..ConfigEnv::default()
        };
        let loaded = load_at(&root, &env)
            .expect("explicit config must load")
            .expect("config must exist");
        assert_eq!(loaded.path, explicit);
        assert_eq!(
            loaded.config.default_network(),
            "ic",
            "walk-up must be disabled"
        );
    }

    #[test]
    fn rejects_unknown_top_level_and_table_keys() {
        let root = temp_root("unknown");
        write_config(&root, "unknown_key = 1\n");
        let error = load_at(&root, &ConfigEnv::default()).expect_err("unknown top-level key");
        assert!(matches!(error, ConfigError::Parse { .. }));

        let root = temp_root("unknown-table");
        write_config(
            &root,
            "[dirs]\nmigrations = \"migrations\"\nunknown = \"x\"\n",
        );
        let error = load_at(&root, &ConfigEnv::default()).expect_err("unknown table key");
        assert!(matches!(error, ConfigError::Parse { .. }));
    }

    #[test]
    fn rejects_bad_format_version() {
        let root = temp_root("version");
        write_config(&root, "format_version = 2\n");
        let error = load_at(&root, &ConfigEnv::default()).expect_err("future version");
        assert!(matches!(error, ConfigError::FormatVersion(2)));
    }

    #[test]
    fn rejects_unknown_deployment_network_shapes() {
        let root = temp_root("network");
        write_config(&root, "[deployment.staging]\ncanister = \"aaaaa-aa\"\n");
        let error = load_at(&root, &ConfigEnv::default()).expect_err("staging is not a URL");
        assert!(matches!(
            error,
            ConfigError::InvalidNetworkName { ref name } if name == "staging"
        ));
    }

    #[test]
    fn rejects_fetch_root_key_under_named_networks() {
        for network in ["ic", "local"] {
            let root = temp_root(&format!("named-{network}"));
            write_config(
                &root,
                &format!(
                    "[deployment.{network}]\ncanister = \"aaaaa-aa\"\nfetch_root_key = true\n"
                ),
            );
            let error =
                load_at(&root, &ConfigEnv::default()).expect_err("fetch_root_key is URL-only");
            match error {
                ConfigError::FetchRootKeyOnNamedNetwork { network: entry } => {
                    assert_eq!(
                        entry, network,
                        "network {network} must reject fetch_root_key"
                    )
                }
                other => panic!("expected FetchRootKeyOnNamedNetwork for {network}, got {other:?}"),
            }
        }
    }

    #[test]
    fn accepts_url_keyed_deployment_entries() {
        let root = temp_root("url");
        write_config(
            &root,
            "[deployment.\"https://example.com\"]\ncanister = \"aaaaa-aa\"\nfetch_root_key = true\n",
        );
        let loaded = load_at(&root, &ConfigEnv::default())
            .expect("URL entry must load")
            .expect("config must exist");
        let profile = loaded
            .config
            .deployment("https://example.com")
            .expect("profile");
        assert_eq!(profile.canister.as_deref(), Some("aaaaa-aa"));
        assert_eq!(profile.fetch_root_key, Some(true));
    }

    #[test]
    fn env_snapshot_parses_scalar_overrides() {
        let env = ConfigEnv::from_env_iter(
            [
                ("GLEAPH_ENVIRONMENT", "staging"),
                ("GLEAPH_NETWORK", "local"),
                ("GLEAPH_CANISTER", "aaaaa-aa"),
                ("GLEAPH_IDENTITY", "id.pem"),
                ("GLEAPH_ROUTER", "prod"),
                ("GLEAPH_FETCH_ROOT_KEY", "true"),
                ("UNRELATED", "x"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned())),
        )
        .expect("valid env snapshot");
        assert_eq!(env.environment.as_deref(), Some("staging"));
        assert_eq!(env.network.as_deref(), Some("local"));
        assert_eq!(env.canister.as_deref(), Some("aaaaa-aa"));
        assert_eq!(env.identity.as_deref(), Some("id.pem"));
        assert_eq!(env.router.as_deref(), Some("prod"));
        assert_eq!(env.fetch_root_key, Some(true));
    }

    #[test]
    fn effective_environment_defaults_to_network() {
        let env = ConfigEnv::default();
        assert_eq!(effective_environment(&env, "local"), "local");
        let env = ConfigEnv {
            environment: Some("staging".into()),
            ..ConfigEnv::default()
        };
        assert_eq!(effective_environment(&env, "ic"), "staging");
    }

    #[test]
    fn env_snapshot_rejects_non_boolean_fetch_root_key() {
        let error = ConfigEnv::from_env_iter(
            [("GLEAPH_FETCH_ROOT_KEY", "yes")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned())),
        )
        .expect_err("invalid fetch_root_key env value");
        assert!(matches!(error, ConfigError::InvalidFetchRootKeyEnv { .. }));
    }

    #[test]
    fn merge_precedence_flag_over_env_over_config_over_default() {
        let root = temp_root("precedence");
        write_config(
            &root,
            "\
default_network = \"ic\"
[deployment.ic]
canister = \"config-canister\"
identity = \"config-id.pem\"
[deployment.local]
canister = \"config-local\"
",
        );
        let loaded = load_at(&root, &ConfigEnv::default())
            .expect("config must load")
            .expect("config must exist");

        let env = ConfigEnv {
            network: Some("local".into()),
            canister: Some("env-canister".into()),
            ..ConfigEnv::default()
        };

        // Flag wins over env and config.
        let opts = merge_remote(
            Some("flag-canister"),
            Some("ic"),
            Some(Path::new("flag-id.pem")),
            Some(true),
            &env,
            Some(&loaded),
        )
        .expect("merge");
        assert_eq!(opts.canister.as_deref(), Some("flag-canister"));
        assert_eq!(opts.network, "ic");
        assert_eq!(opts.identity.as_deref(), Some(Path::new("flag-id.pem")));
        assert!(opts.fetch_root_key);

        // Env wins over config; network selects the deployment entry.
        let opts = merge_remote(None, None, None, None, &env, Some(&loaded)).expect("merge");
        assert_eq!(opts.canister.as_deref(), Some("env-canister"));
        assert_eq!(opts.network, "local");
        assert!(!opts.fetch_root_key);

        // Config supplies canister/identity; identity resolves against the config directory.
        let opts = merge_remote(None, None, None, None, &ConfigEnv::default(), Some(&loaded))
            .expect("merge");
        assert_eq!(opts.canister.as_deref(), Some("config-canister"));
        assert_eq!(
            opts.identity.as_deref(),
            Some(Path::new(&root.join("config-id.pem")))
        );
        assert_eq!(opts.network, "ic");
        assert!(!opts.fetch_root_key);

        // Nothing supplied: built-in defaults. The active session must NOT leak an identity
        // here, so the test runs against an isolated (empty) config home.
        let guard = crate::identity::test_support::lock_config_home();
        let isolated = std::env::temp_dir().join(format!(
            "gleaph-merge-remote-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&isolated).expect("temp config home");
        let prior = std::env::var_os("GLEAPH_CONFIG_HOME");
        // SAFETY: single-threaded here — the config-home lock is held.
        #[allow(unsafe_op_in_unsafe_fn)]
        unsafe {
            std::env::set_var("GLEAPH_CONFIG_HOME", &isolated)
        };
        let opts =
            merge_remote(None, None, None, None, &ConfigEnv::default(), None).expect("merge");
        assert_eq!(opts.canister, None);
        assert_eq!(opts.network, "ic");
        assert_eq!(opts.identity, None);
        assert!(!opts.fetch_root_key);
        let _ = std::fs::remove_dir_all(&isolated);
        // SAFETY: single-threaded test (config-home lock held); restoring prior state.
        match prior {
            Some(value) => unsafe { std::env::set_var("GLEAPH_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("GLEAPH_CONFIG_HOME") },
        }
        drop(guard);
    }

    #[test]
    fn absent_settrue_flag_does_not_shadow_env_fetch_root_key() {
        // clap's SetTrue yields Some(false) when the flag is absent; since the flag can
        // only express "true", that must fall through to env instead of overriding it.
        let env = ConfigEnv {
            network: Some("https://example.com".into()),
            fetch_root_key: Some(true),
            ..ConfigEnv::default()
        };
        let opts = merge_remote(None, None, None, Some(false), &env, None).expect("merge");
        assert!(
            opts.fetch_root_key,
            "env value must win over an absent SetTrue flag"
        );
    }

    #[test]
    fn fetch_root_key_comes_from_the_url_entry_only() {
        let root = temp_root("url-fetch");
        write_config(
            &root,
            "[deployment.\"https://example.com\"]\ncanister = \"aaaaa-aa\"\nfetch_root_key = true\n",
        );
        let loaded = load_at(&root, &ConfigEnv::default())
            .expect("config must load")
            .expect("config must exist");
        let opts = merge_remote(
            None,
            Some("https://example.com"),
            None,
            None,
            &ConfigEnv::default(),
            Some(&loaded),
        )
        .expect("merge");
        assert!(opts.fetch_root_key, "URL entry supplies fetch_root_key");
        assert_eq!(opts.canister.as_deref(), Some("aaaaa-aa"));

        // A URL entry omitting fetch_root_key yields the default false (the existing
        // resolve_network error then applies for custom URLs).
        let root = temp_root("url-no-fetch");
        write_config(
            &root,
            "[deployment.\"https://example.com\"]\ncanister = \"aaaaa-aa\"\n",
        );
        let loaded = load_at(&root, &ConfigEnv::default())
            .expect("config must load")
            .expect("config must exist");
        let opts = merge_remote(
            None,
            Some("https://example.com"),
            None,
            None,
            &ConfigEnv::default(),
            Some(&loaded),
        )
        .expect("merge");
        assert!(!opts.fetch_root_key);
    }

    #[test]
    fn resolved_dir_prefers_flag_then_config_then_default() {
        let root = temp_root("dirs");
        write_config(
            &root,
            "[dirs]\nmigrations = \"migrations\"\nprepared = \"prepared\"\n",
        );
        let loaded = load_at(&root, &ConfigEnv::default())
            .expect("config must load")
            .expect("config must exist");

        assert_eq!(
            resolved_dir(Some(Path::new("custom")), Some(&loaded), DirKey::Migrations),
            PathBuf::from("custom")
        );
        assert_eq!(
            resolved_dir(None, Some(&loaded), DirKey::Prepared),
            root.join("prepared")
        );
        assert_eq!(
            resolved_dir(None, None, DirKey::Migrations),
            PathBuf::from("migrations")
        );
    }

    #[test]
    fn router_cache_round_trips_per_environment() {
        let root = temp_root("router-cache");
        write_config(&root, "format_version = 1\n");
        let loaded = load_at(&root, &ConfigEnv::default())
            .expect("config must load")
            .expect("config must exist");

        // Absent cache -> None.
        assert_eq!(read_router_cache(&loaded, "local"), None);

        // Write then read back.
        write_router_cache(&loaded, "local", "aaaaa-aa");
        assert_eq!(
            read_router_cache(&loaded, "local").as_deref(),
            Some("aaaaa-aa")
        );

        // Environments are isolated.
        assert_eq!(read_router_cache(&loaded, "ic"), None);
    }

    #[test]
    fn mapping_round_trips_per_environment() {
        let root = temp_root("mapping");
        write_config(&root, "format_version = 1\n");
        let loaded = load_at(&root, &ConfigEnv::default())
            .expect("config must load")
            .expect("config must exist");

        // Absent mapping -> empty.
        assert!(read_mapping(&loaded, "local").expect("read").is_empty());

        // Write then read back.
        let mut mapping = BTreeMap::new();
        mapping.insert("account".to_owned(), "aaaaa-aa".to_owned());
        mapping.insert("provision".to_owned(), "bbbbb-bb".to_owned());
        write_mapping(&loaded, "local", &mapping).expect("write");
        assert_eq!(read_mapping(&loaded, "local").expect("read"), mapping);

        // Environments are isolated.
        assert!(read_mapping(&loaded, "ic").expect("read").is_empty());
    }
}
