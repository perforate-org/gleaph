//! What every test process of one run shares: one PocketIC server and the fixture snapshots.
//!
//! A run is one `cargo nextest` invocation - which starts every test in a process of its own and
//! names the run in `NEXTEST_RUN_ID` - or, under `cargo test`, one test binary's process. Process
//! ancestry cannot identify a run: a configured target runner (`CARGO_TARGET_<triple>_RUNNER`, for
//! example a build-cache wrapper) puts a process of its own between the runner and each test. A
//! later run deletes the directory of a run that has ended (see [`RunScope::has_ended`]).
//!
//! **Server.** One PocketIC server serves every instance of the run. Without it each nextest test
//! process would start its own server, which outlives the test by the server's idle TTL.
//!
//! **Snapshots.** Installing the Router, Index and Graph canisters is the largest part of a
//! test's setup. A fixture is installed once per run and checkpointed to a PocketIC state
//! directory; each test then mounts it read-only, which gives the test a fresh instance holding a
//! private copy of that state - nothing is shared between tests. (Each instance still compiles
//! the canister wasm on first use: PocketIC keeps no compilation cache across instances.) A run
//! never outlives the build it runs, so a snapshot cannot be older than the wasm it was installed
//! from.

use candid::{CandidType, Decode, Encode};
use pocket_ic::{PocketIc, PocketIcBuilder, PocketIcState};
use std::fs::{self, File};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use url::Url;

const IDS_FILE: &str = "ids.candid";
const STATE_DIR: &str = "state";
const SERVER_PORT_FILE: &str = "server.port";

/// How long the shared server outlives its last request. A test that builds a wasm of its own
/// between two calls leaves the server idle for minutes under load, so this is generous.
const SERVER_IDLE_TTL: Duration = Duration::from_secs(10 * 60);
/// A nextest run directory this old whose server has stopped belongs to a run that has ended.
const ENDED_NEXTEST_RUN_AGE: Duration = Duration::from_secs(60 * 60);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(60);

/// The URL of this run's PocketIC server, starting it from `server_binary()` if it is not running.
pub(crate) fn server_url(server_binary: fn() -> PathBuf) -> Url {
    static URL: OnceLock<Url> = OnceLock::new();
    URL.get_or_init(|| {
        let _lock = RunLock::acquire("server");
        let port_file = run_dir().join(SERVER_PORT_FILE);
        let port = match read_port(&port_file) {
            Some(port) if server_is_listening(port) => port,
            _ => start_server(&server_binary(), &port_file),
        };
        Url::parse(&format!("http://127.0.0.1:{port}/")).expect("server url")
    })
    .clone()
}

fn read_port(port_file: &Path) -> Option<u16> {
    let text = fs::read_to_string(port_file).ok()?;
    // The server writes the port and then a newline; without the newline the write is partial.
    text.strip_suffix('\n')?.parse().ok()
}

fn server_is_listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(1),
    )
    .is_ok()
}

/// Start a server detached from this test process: its own process group, so a test runner's
/// signals do not reach it, and no inherited stdio, so it holds no pipe of the test that started
/// it once that test has exited.
fn start_server(server_binary: &Path, port_file: &Path) -> u16 {
    if port_file.exists() {
        fs::remove_file(port_file)
            .unwrap_or_else(|e| panic!("remove {}: {e}", port_file.display()));
    }
    let mut command = Command::new(server_binary);
    command
        .arg("--port-file")
        .arg(port_file)
        .arg("--ttl")
        .arg(SERVER_IDLE_TTL.as_secs().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    // The server exits on its own after its idle TTL; nothing waits for it.
    #[expect(
        clippy::zombie_processes,
        reason = "the server outlives this test process by design"
    )]
    let _server = command
        .spawn()
        .unwrap_or_else(|e| panic!("start PocketIC server {}: {e}", server_binary.display()));
    let started = Instant::now();
    loop {
        if let Some(port) = read_port(port_file) {
            return port;
        }
        assert!(
            started.elapsed() < SERVER_START_TIMEOUT,
            "PocketIC server {} wrote no port to {} within {SERVER_START_TIMEOUT:?}",
            server_binary.display(),
            port_file.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Mount the snapshot named `name`, installing it first with `install` if this run has none.
///
/// `install` receives a fresh instance with one application subnet and returns the identifiers
/// a test needs to address what it installed; they are stored beside the state and handed back
/// with every mounted copy.
pub(crate) fn mount_snapshot<Ids>(
    builder: fn() -> PocketIcBuilder,
    name: &str,
    install: impl FnOnce(&PocketIc) -> Ids,
) -> (PocketIc, Ids)
where
    Ids: CandidType + for<'de> candid::Deserialize<'de>,
{
    let snapshot = run_dir().join(name);
    {
        // One process installs; the others wait on the lock and then find the published snapshot.
        let _lock = RunLock::acquire(name);
        if !snapshot.is_dir() {
            install_snapshot(builder, &snapshot, install);
        }
    }
    let ids_path = snapshot.join(IDS_FILE);
    let ids_bytes =
        fs::read(&ids_path).unwrap_or_else(|e| panic!("read {}: {e}", ids_path.display()));
    let ids = Decode!(&ids_bytes, Ids).expect("decode snapshot ids");
    let pic = builder()
        .with_read_only_state(&PocketIcState::new_from_path(snapshot.join(STATE_DIR)))
        .build();
    (pic, ids)
}

/// Install into a private directory and publish it under `snapshot` only once it is complete, so
/// an install that panics part-way never leaves a snapshot another test would mount.
fn install_snapshot<Ids: CandidType>(
    builder: fn() -> PocketIcBuilder,
    snapshot: &Path,
    install: impl FnOnce(&PocketIc) -> Ids,
) {
    let staging = snapshot.with_extension(format!("staging-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging).unwrap_or_else(|e| panic!("clear {}: {e}", staging.display()));
    }
    let state_dir = staging.join(STATE_DIR);
    fs::create_dir_all(&state_dir)
        .unwrap_or_else(|e| panic!("create {}: {e}", state_dir.display()));

    let pic = builder()
        .with_application_subnet()
        .with_state(PocketIcState::new_from_path(state_dir))
        .build();
    let ids = install(&pic);
    // Deleting the instance is what makes the server checkpoint its state to the directory.
    pic.drop_and_take_state()
        .expect("snapshot instance was built with a state directory");

    let ids_path = staging.join(IDS_FILE);
    fs::write(&ids_path, Encode!(&ids).expect("encode snapshot ids"))
        .unwrap_or_else(|e| panic!("write {}: {e}", ids_path.display()));
    fs::rename(&staging, snapshot).unwrap_or_else(|e| {
        panic!(
            "publish {} as {}: {e}",
            staging.display(),
            snapshot.display()
        )
    });
}

/// An exclusive lock on one name in the run directory, held until dropped.
#[must_use]
struct RunLock {
    /// Closing the file releases the lock.
    _file: File,
}

impl RunLock {
    fn acquire(name: &str) -> Self {
        let path = run_dir().join(format!("{name}.lock"));
        let file = File::create(&path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
        file.lock()
            .unwrap_or_else(|e| panic!("lock {}: {e}", path.display()));
        Self { _file: file }
    }
}

/// The run a test process belongs to, which names its directory under `runs/`.
enum RunScope {
    /// Every test process of one `cargo nextest` invocation.
    Nextest { run_id: String },
    /// One test binary's process under `cargo test`.
    Process { pid: u32 },
}

impl RunScope {
    const NEXTEST_PREFIX: &str = "nextest-";
    const PROCESS_PREFIX: &str = "process-";

    fn current() -> Self {
        match std::env::var("NEXTEST_RUN_ID") {
            Ok(run_id) => Self::Nextest { run_id },
            Err(_) => Self::Process {
                pid: std::process::id(),
            },
        }
    }

    fn dir_name(&self) -> String {
        match self {
            Self::Nextest { run_id } => format!("{}{run_id}", Self::NEXTEST_PREFIX),
            Self::Process { pid } => format!("{}{pid}", Self::PROCESS_PREFIX),
        }
    }

    fn from_dir_name(name: &str) -> Option<Self> {
        if let Some(run_id) = name.strip_prefix(Self::NEXTEST_PREFIX) {
            return Some(Self::Nextest {
                run_id: run_id.to_owned(),
            });
        }
        let pid = name.strip_prefix(Self::PROCESS_PREFIX)?.parse().ok()?;
        Some(Self::Process { pid })
    }

    /// Whether the run that owns `dir` has ended. A process run has ended when its process has; a
    /// nextest run's processes are not known, so it has ended when its server has stopped and the
    /// directory has not changed for an hour.
    fn has_ended(&self, dir: &Path) -> bool {
        match self {
            Self::Process { pid } => !process_is_alive(*pid),
            Self::Nextest { .. } => {
                let server_stopped = read_port(&dir.join(SERVER_PORT_FILE))
                    .is_none_or(|port| !server_is_listening(port));
                let idle_for = fs::metadata(dir)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok());
                server_stopped && idle_for.is_some_and(|age| age > ENDED_NEXTEST_RUN_AGE)
            }
        }
    }
}

/// The directory of this run, created (and ended runs' directories pruned) on first use.
fn run_dir() -> &'static Path {
    static RUN_DIR: OnceLock<PathBuf> = OnceLock::new();
    RUN_DIR.get_or_init(|| {
        let runs = crate::fixture_cache_root().join("runs");
        let dir = runs.join(RunScope::current().dir_name());
        let created = !dir.exists();
        fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
        if created {
            prune_ended_runs(&runs, &dir);
        }
        dir
    })
}

/// Best-effort removal of the directories of runs that have ended; a failure here only leaves
/// disk space in use.
fn prune_ended_runs(runs: &Path, current: &Path) {
    let Ok(entries) = fs::read_dir(runs) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current {
            continue;
        }
        // A name this module did not give is left alone.
        let ended = entry
            .file_name()
            .to_str()
            .and_then(RunScope::from_dir_name)
            .is_some_and(|scope| scope.has_ended(&path));
        if ended {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn process_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
