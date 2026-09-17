//! Lifecycle of the Portal-owned pi daemon (PRD §4.4 / §4.11).
//!
//! Portal spawns `pi --mode daemon --daemon-socket <private>` as a *supervised
//! child* — not detached — pointed at a Portal-private agent dir and sessions
//! dir. `ensure_running()` probes first, so a daemon that outlived a Portal
//! restart is adopted rather than duplicated.
//!
//! ```text
//! ensure_running()
//!   connected client? ────────────────────────────────► reuse
//!   probe socket  ─ v7 hello ─────────────────────────► adopt (child = None)
//!                 ─ wrong version ─► shutdown, respawn
//!                 ─ nothing there ─► spawn + readiness loop (≤30 s)
//! shutdown(grace)
//!   `shutdown{}` ─► wait for exit ─► SIGTERM ─► SIGKILL   (adopted: cmd only)
//! ```

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdin, Command as TokioCommand};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time;
use tracing::{debug, info, warn};

#[cfg(not(unix))]
use super::pi_client::ConnectPolicy;
use super::pi_client::{PiClient, DEFAULT_REQUEST_TIMEOUT};
use super::protocol::Command;

/// Candidate binaries, in resolution order, when `[subagent].command` is unset.
const PI_BINARY_CANDIDATES: [&str; 2] = ["pi", "prime-agent"];
/// How long `ensure_running` waits for a freshly spawned daemon to greet.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Interval of the readiness loop.
const READINESS_POLL: Duration = Duration::from_millis(100);
/// Grace given to `shutdown{}` before signalling.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Grace between SIGTERM and SIGKILL.
const SIGKILL_GRACE: Duration = Duration::from_secs(2);
/// Time allowed for a wrong-version daemon to vacate the socket.
const REPLACE_TIMEOUT: Duration = Duration::from_secs(5);
/// daemon.log is truncated once it passes this size.
const DAEMON_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Tail of daemon.log quoted in startup failures.
const DAEMON_LOG_TAIL_BYTES: usize = 4096;
/// `sockaddr_un.sun_path` is ~104 bytes on macOS and 108 on Linux. Bind fails
/// with an opaque EINVAL past that, so check it and say what to do instead.
#[cfg(unix)]
const MAX_SOCKET_PATH_BYTES: usize = 100;

/// Everything `PiDaemon` needs that comes from configuration.
#[derive(Debug, Clone)]
pub struct PiDaemonConfig {
    /// Resolved argv for pi (`command[0]` exists or is on PATH).
    pub command: Vec<String>,
    /// `<state_dir>` — ledger, socket, agent dir and sessions all live under it.
    pub state_dir: PathBuf,
    /// cwd for the daemon process.
    pub workspace_root: PathBuf,
    /// Environment variables forwarded into the `env_clear()`ed child.
    pub env_passthrough: Vec<String>,
}

/// Snapshot for `portal_subagent_status` / `portal_status`. Paths and versions
/// only — never credentials (PRD §8 acceptance).
#[derive(Debug, Clone, Default)]
pub struct DaemonHealth {
    pub running: bool,
    pub pid: Option<u32>,
    pub protocol: Option<u32>,
    pub app_version: Option<String>,
    pub adopted: bool,
    pub socket: Option<String>,
}

impl DaemonHealth {
    pub fn to_json(&self) -> Value {
        json!({
            "running": self.running,
            "pid": self.pid,
            "protocol": self.protocol,
            "app_version": self.app_version,
            "adopted": self.adopted,
            "socket": self.socket,
        })
    }
}

#[derive(Default)]
struct DaemonState {
    child: Option<Child>,
    /// Held open for pi change P1: stdin EOF is the daemon's exit signal, so
    /// a SIGKILLed Portal cannot leave a daemon behind.
    child_stdin: Option<ChildStdin>,
    client: Option<Arc<PiClient>>,
    adopted: bool,
    pid: Option<u32>,
}

pub struct PiDaemon {
    config: PiDaemonConfig,
    state: AsyncMutex<DaemonState>,
}

impl PiDaemon {
    pub fn new(config: PiDaemonConfig) -> Self {
        Self {
            config,
            state: AsyncMutex::new(DaemonState::default()),
        }
    }

    /// Read back the resolved argv / paths (used by diagnostics).
    #[allow(dead_code)] // surfaced by portal_status once that tool exists
    pub fn config(&self) -> &PiDaemonConfig {
        &self.config
    }

    /// `<state_dir>/pi` — everything pi-specific.
    pub fn pi_dir(&self) -> PathBuf {
        self.config.state_dir.join("pi")
    }

    /// `PRIME_AGENT_CODING_AGENT_DIR`: auth.json, settings.json, harness, logs.
    pub fn agent_dir(&self) -> PathBuf {
        self.pi_dir().join("agent")
    }

    /// `PRIME_AGENT_SESSION_DIR`: the session JSONL files that *are* the
    /// sub-agent's accumulated harness (PRD §7.5).
    pub fn sessions_dir(&self) -> PathBuf {
        self.pi_dir().join("sessions")
    }

    pub fn log_path(&self) -> PathBuf {
        self.pi_dir().join("daemon.log")
    }

    /// Private transport endpoint. A private path means a leaked daemon can
    /// never collide with the user's own interactive pi (PRD §9 risk 2).
    pub fn socket_path(&self) -> PathBuf {
        self.pi_dir().join("daemon.sock")
    }

    /// Reject a `state_dir` so deep that the daemon socket cannot be bound.
    #[cfg(unix)]
    fn check_socket_path(&self) -> Result<()> {
        let socket = self.socket_path();
        let len = socket.as_os_str().len();
        if len > MAX_SOCKET_PATH_BYTES {
            anyhow::bail!(
                "the daemon socket path is {len} bytes ({}), over the {MAX_SOCKET_PATH_BYTES}-byte \
                 limit for unix sockets; set a shorter [subagent].state_dir",
                socket.display()
            );
        }
        Ok(())
    }

    /// Create `<state_dir>` and the pi subtree, 0700 on unix (§4.10).
    pub fn prepare_dirs(&self) -> Result<()> {
        #[cfg(unix)]
        self.check_socket_path()?;
        for dir in [
            self.config.state_dir.clone(),
            self.pi_dir(),
            self.agent_dir(),
            self.sessions_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating subagent state dir {}", dir.display()))?;
            restrict_permissions(&dir);
        }
        Ok(())
    }

    /// The connected client, if any. Does not start anything.
    pub async fn client(&self) -> Option<Arc<PiClient>> {
        let state = self.state.lock().await;
        state
            .client
            .as_ref()
            .filter(|c| c.is_connected())
            .map(Arc::clone)
    }

    pub async fn health(&self) -> DaemonHealth {
        let state = self.state.lock().await;
        let connected = state.client.as_ref().is_some_and(|c| c.is_connected());
        let hello = state.client.as_ref().map(|c| c.hello().clone());
        DaemonHealth {
            running: connected,
            pid: state.pid,
            protocol: hello.as_ref().map(|h| h.protocol.version),
            app_version: hello.and_then(|h| h.app_version),
            adopted: state.adopted,
            socket: Some(self.socket_path().display().to_string()),
        }
    }

    /// Idempotent: returns a live client, starting or adopting a daemon if needed.
    pub async fn ensure_running(&self) -> Result<Arc<PiClient>> {
        let mut state = self.state.lock().await;

        if let Some(client) = state.client.as_ref().filter(|c| c.is_connected()) {
            return Ok(Arc::clone(client));
        }
        // A dead client is dropped so the pieces below can be replaced wholesale.
        state.client = None;

        self.prepare_dirs()?;

        #[cfg(unix)]
        {
            let socket = self.socket_path();
            match PiClient::probe_unix(&socket).await {
                Ok(client) => match client.hello().check_protocol() {
                    Ok(()) => {
                        let pid = client.hello().supervisor_pid;
                        info!(
                            "adopted the pi daemon already listening on {} (pid {:?})",
                            socket.display(),
                            pid
                        );
                        state.adopted = state.child.is_none();
                        state.pid = pid.or(state.pid);
                        state.client = Some(Arc::clone(&client));
                        return Ok(client);
                    }
                    Err(e) => {
                        warn!("replacing the daemon on {}: {e:#}", socket.display());
                        client
                            .notify(Command::Shutdown { force: false })
                            .await;
                        wait_for_socket_gone(&socket, REPLACE_TIMEOUT).await;
                    }
                },
                Err(e) => debug!("no usable pi daemon on {}: {e:#}", socket.display()),
            }
        }

        let client = self.spawn_locked(&mut state).await?;
        Ok(client)
    }

    /// Drop the current daemon (if Portal owns it) and start a fresh one.
    #[allow(dead_code)] // operator escape hatch; no tool exposes it yet
    pub async fn restart(&self) -> Result<Arc<PiClient>> {
        self.shutdown(SHUTDOWN_GRACE).await;
        self.ensure_running().await
    }

    /// Spawn pi, wire up stderr capture, and wait for it to accept a client.
    async fn spawn_locked(&self, state: &mut DaemonState) -> Result<Arc<PiClient>> {
        let argv = &self.config.command;
        if argv.is_empty() {
            anyhow::bail!("no pi command configured; set [subagent].command in portal.toml");
        }

        let socket = self.socket_path();
        let mut cmd = TokioCommand::new(&argv[0]);
        cmd.args(&argv[1..]);

        #[cfg(unix)]
        {
            cmd.args(["--mode", "daemon", "--daemon-socket"])
                .arg(&socket);
        }
        #[cfg(not(unix))]
        {
            // Windows named pipes are Phase 4; until then pi is driven over
            // piped stdio, which the same PiClient speaks.
            cmd.args(["--mode", "rpc"]);
        }

        cmd.env_clear();
        for key in &self.config.env_passthrough {
            if let Some(val) = std::env::var_os(key) {
                cmd.env(key, val);
            }
        }
        cmd.env("PRIME_AGENT_CODING_AGENT_DIR", self.agent_dir())
            .env("PRIME_AGENT_SESSION_DIR", self.sessions_dir())
            // pi change P1: exit gracefully when Portal's stdin pipe closes.
            .env("PRIME_AGENT_DAEMON_EXIT_ON_STDIN_CLOSE", "1")
            .env("PRIME_AGENT_HEADLESS", "1")
            .current_dir(&self.config.workspace_root)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // On unix the protocol lives on the socket, so stdout is noise. On the
        // stdio fallback it *is* the protocol.
        #[cfg(unix)]
        cmd.stdout(Stdio::null());
        #[cfg(not(unix))]
        cmd.stdout(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!("spawning the pi daemon: {}", self.config.command.join(" "))
        })?;
        let pid = child.id();
        // Kept for the lifetime of the daemon; dropping it asks pi to exit.
        let child_stdin = child.stdin.take();
        #[cfg(not(unix))]
        let child_stdout = child.stdout.take();

        if let Some(stderr) = child.stderr.take() {
            spawn_log_capture(stderr, self.log_path());
        }

        info!(
            "spawned pi daemon (pid {:?}); agent dir {}, sessions {}",
            pid,
            self.agent_dir().display(),
            self.sessions_dir().display()
        );

        state.child = Some(child);
        state.child_stdin = child_stdin;
        state.adopted = false;
        state.pid = pid;

        #[cfg(unix)]
        let connect = self.await_socket_ready(state, &socket).await;
        #[cfg(not(unix))]
        let connect = self.await_stdio_ready(state, child_stdout).await;

        match connect {
            Ok(client) => {
                state.client = Some(Arc::clone(&client));
                Ok(client)
            }
            Err(e) => {
                // A failed start must not leave a half-live child around.
                if let Some(mut child) = state.child.take() {
                    let _ = child.start_kill();
                }
                state.child_stdin = None;
                state.pid = None;
                Err(e)
            }
        }
    }

    /// Readiness loop: connect + hello every 100 ms for up to 30 s, aborting
    /// early (with the log tail) if the child dies first.
    #[cfg(unix)]
    async fn await_socket_ready(
        &self,
        state: &mut DaemonState,
        socket: &Path,
    ) -> Result<Arc<PiClient>> {
        let deadline = time::Instant::now() + STARTUP_TIMEOUT;
        let mut last_err;
        loop {
            if let Some(child) = state.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    anyhow::bail!(
                        "pi daemon exited during startup ({status}){}",
                        self.log_tail_suffix()
                    );
                }
            }

            match PiClient::connect_unix(socket).await {
                Ok(client) => return Ok(client),
                Err(e) => last_err = format!("{e:#}"),
            }

            if time::Instant::now() >= deadline {
                anyhow::bail!(
                    "pi daemon not ready within {STARTUP_TIMEOUT:?}: {last_err}{}",
                    self.log_tail_suffix()
                );
            }
            time::sleep(READINESS_POLL).await;
        }
    }

    /// Degraded transport for platforms without a Unix socket client.
    #[cfg(not(unix))]
    async fn await_stdio_ready(
        &self,
        state: &mut DaemonState,
        stdout: Option<tokio::process::ChildStdout>,
    ) -> Result<Arc<PiClient>> {
        let stdout = stdout.ok_or_else(|| anyhow::anyhow!("pi stdout was not piped"))?;
        let stdin = state
            .child_stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("pi stdin was not piped"))?;
        warn!("using the experimental stdio transport for pi; named-pipe support is pending");
        PiClient::connect_io(stdout, stdin, "pi stdio".to_string(), ConnectPolicy::LENIENT).await
    }

    /// Ask the daemon to stop, then escalate. Adopted daemons get the request
    /// only — Portal never signals a process it does not own.
    pub async fn shutdown(&self, grace: Duration) {
        let mut state = self.state.lock().await;

        if let Some(client) = state.client.take() {
            if client.is_connected() {
                debug!("asking the pi daemon to shut down");
                let _ = time::timeout(
                    DEFAULT_REQUEST_TIMEOUT,
                    client.request(Command::Shutdown { force: false }, DEFAULT_REQUEST_TIMEOUT),
                )
                .await;
            }
        }

        // Closing stdin is the P1 exit signal; do it before escalating.
        state.child_stdin = None;

        let Some(mut child) = state.child.take() else {
            if state.adopted {
                debug!("adopted daemon left running; shutdown requested only");
            }
            state.pid = None;
            return;
        };

        match time::timeout(grace, child.wait()).await {
            Ok(Ok(status)) => {
                debug!("pi daemon exited cleanly ({status})");
                state.pid = None;
                return;
            }
            Ok(Err(e)) => warn!("waiting for the pi daemon failed: {e}"),
            Err(_) => warn!("pi daemon ignored shutdown after {grace:?}; terminating"),
        }

        let _ = child.start_kill();
        if time::timeout(SIGKILL_GRACE, child.wait()).await.is_err() {
            warn!("pi daemon survived termination; killing");
            let _ = child.kill().await;
        }
        state.pid = None;
    }

    /// `" (daemon.log tail: …)"`, or empty when there is nothing to show.
    fn log_tail_suffix(&self) -> String {
        match read_log_tail(&self.log_path(), DAEMON_LOG_TAIL_BYTES) {
            Some(tail) if !tail.trim().is_empty() => format!("; daemon.log tail: {}", tail.trim()),
            _ => String::new(),
        }
    }

    /// Resolve the pi binary: explicit config, then Portal's bundled copy,
    /// then `pi` / `prime-agent` on PATH. `None` ⇒ the sub-agent is unavailable
    /// and its tools are not advertised.
    pub fn resolve_command(configured: Option<&[String]>) -> Option<Vec<String>> {
        if let Some(argv) = configured.filter(|a| !a.is_empty()) {
            let program = expand_home(&argv[0]);
            let resolved = if argv[0].contains('/') || argv[0].contains('\\') {
                program.exists().then(|| program.display().to_string())
            } else {
                find_on_path(&argv[0]).map(|p| p.display().to_string())
            }?;
            let mut out = vec![resolved];
            out.extend(argv[1..].iter().cloned());
            return Some(out);
        }

        let bundled = expand_home("~/.heart-portal/pi/bin/pi");
        if bundled.exists() {
            return Some(vec![bundled.display().to_string()]);
        }

        PI_BINARY_CANDIDATES
            .iter()
            .find_map(|name| find_on_path(name))
            .map(|p| vec![p.display().to_string()])
    }
}

/// Copy the daemon's stderr into `<state>/pi/daemon.log`, truncating at 5 MiB.
/// The daemon is not detached, so without this its diagnostics would vanish.
fn spawn_log_capture(mut stderr: tokio::process::ChildStderr, path: PathBuf) {
    tokio::spawn(async move {
        use std::io::Write;
        let mut buf = [0u8; 8192];
        loop {
            let n = match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let rotate = std::fs::metadata(&path)
                .map(|m| m.len() > DAEMON_LOG_MAX_BYTES)
                .unwrap_or(false);
            let mut opts = std::fs::OpenOptions::new();
            opts.create(true);
            if rotate {
                opts.write(true).truncate(true);
            } else {
                opts.append(true);
            }
            match opts.open(&path) {
                Ok(mut f) => {
                    let _ = f.write_all(&buf[..n]);
                }
                Err(e) => {
                    debug!("cannot write {}: {e}", path.display());
                    break;
                }
            }
        }
    });
}

fn read_log_tail(path: &Path, max: usize) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let from = data.len().saturating_sub(max);
    Some(String::from_utf8_lossy(&data[from..]).to_string())
}

#[cfg(unix)]
async fn wait_for_socket_gone(socket: &Path, timeout: Duration) {
    let deadline = time::Instant::now() + timeout;
    while socket.exists() && time::Instant::now() < deadline {
        time::sleep(READINESS_POLL).await;
    }
}

#[cfg(unix)]
fn restrict_permissions(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // The state dir holds auth.json and transcripts (PRD §9 risk 10).
    if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
        debug!("could not chmod 0700 {}: {e}", dir.display());
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_dir: &Path) {}

/// `~` / `~/...` → the user's home, else unchanged.
pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .filter(|p| !p.as_os_str().is_empty())
}

/// Minimal `which`: first executable match in `PATH`.
pub fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Short root: the daemon socket must fit in `sun_path` (~104 bytes) and
    /// macOS `$TMPDIR` alone is ~50.
    fn temp_dir(tag: &str) -> PathBuf {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = PathBuf::from("/tmp").join(format!("pd-{tag}-{}", &id[..8]));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn daemon_for(state_dir: PathBuf) -> PiDaemon {
        PiDaemon::new(PiDaemonConfig {
            command: vec!["/bin/false".to_string()],
            state_dir,
            workspace_root: PathBuf::from("/tmp"),
            env_passthrough: vec!["PATH".to_string()],
        })
    }

    #[test]
    fn paths_follow_the_documented_layout() {
        let d = daemon_for(PathBuf::from("/state/subagent"));
        assert_eq!(d.pi_dir(), PathBuf::from("/state/subagent/pi"));
        assert_eq!(d.agent_dir(), PathBuf::from("/state/subagent/pi/agent"));
        assert_eq!(d.sessions_dir(), PathBuf::from("/state/subagent/pi/sessions"));
        assert_eq!(d.socket_path(), PathBuf::from("/state/subagent/pi/daemon.sock"));
        assert_eq!(d.log_path(), PathBuf::from("/state/subagent/pi/daemon.log"));
    }

    #[test]
    fn prepare_dirs_creates_the_tree_privately() {
        let root = temp_dir("dirs");
        let d = daemon_for(root.join("subagent"));
        d.prepare_dirs().unwrap();
        assert!(d.agent_dir().is_dir());
        assert!(d.sessions_dir().is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.pi_dir()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "state dir must not be world-readable");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_command_honors_an_explicit_absolute_path() {
        let resolved = PiDaemon::resolve_command(Some(&[
            "/bin/sh".to_string(),
            "--flag".to_string(),
        ]))
        .unwrap();
        assert_eq!(resolved, vec!["/bin/sh".to_string(), "--flag".to_string()]);
    }

    #[test]
    fn resolve_command_rejects_a_missing_explicit_path() {
        assert!(PiDaemon::resolve_command(Some(&[
            "/nonexistent/pi-binary".to_string()
        ]))
        .is_none());
    }

    #[test]
    fn resolve_command_searches_path_for_a_bare_name() {
        let resolved = PiDaemon::resolve_command(Some(&["sh".to_string()])).unwrap();
        assert!(resolved[0].ends_with("/sh"), "{resolved:?}");
    }

    #[test]
    fn resolve_command_is_none_when_pi_is_absent() {
        // An empty PATH and no bundled copy ⇒ the sub-agent is unavailable.
        let empty = temp_dir("nopath");
        let resolved = PiDaemon::resolve_command(Some(&[
            empty.join("pi").display().to_string()
        ]));
        assert!(resolved.is_none());
        let _ = std::fs::remove_dir_all(empty);
    }

    #[test]
    fn expand_home_expands_only_a_leading_tilde() {
        let home = home_dir().unwrap();
        assert_eq!(expand_home("~/x/y"), home.join("x/y"));
        assert_eq!(expand_home("/abs/~/x"), PathBuf::from("/abs/~/x"));
        assert_eq!(expand_home("rel"), PathBuf::from("rel"));
    }

    #[cfg(unix)]
    #[test]
    fn an_overlong_state_dir_is_refused_with_an_actionable_message() {
        // Binding would fail with an opaque EINVAL; say what to change instead.
        let deep = PathBuf::from("/tmp").join("d".repeat(120));
        let d = daemon_for(deep);
        let err = d.prepare_dirs().unwrap_err().to_string();
        assert!(err.contains("unix sockets"), "{err}");
        assert!(err.contains("state_dir"), "{err}");

        // A sane path passes the check.
        let root = temp_dir("shortpath");
        assert!(daemon_for(root.join("s")).prepare_dirs().is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn health_reports_a_stopped_daemon() {
        let d = daemon_for(PathBuf::from("/state/subagent"));
        let h = d.health().await;
        assert!(!h.running);
        assert!(h.pid.is_none());
        assert!(!h.adopted);
        let json = h.to_json();
        assert_eq!(json["running"], false);
        assert!(json.get("token").is_none(), "health must never carry secrets");
    }

    #[tokio::test]
    async fn spawn_failure_reports_the_log_tail_and_leaves_no_child() {
        let root = temp_dir("failstart");
        let d = PiDaemon::new(PiDaemonConfig {
            command: vec!["/bin/sh".to_string(), "-c".to_string(),
                          "echo 'pi: no provider configured' >&2; exit 3".to_string()],
            state_dir: root.join("subagent"),
            workspace_root: PathBuf::from("/tmp"),
            env_passthrough: vec!["PATH".to_string()],
        });

        let err = match d.ensure_running().await {
            Ok(_) => panic!("a daemon that exits immediately must not be reported ready"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("pi daemon exited during startup"), "{err}");

        let health = d.health().await;
        assert!(!health.running);
        assert!(health.pid.is_none(), "a failed start must not leave a pid");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shutdown_on_a_stopped_daemon_is_a_noop() {
        let d = daemon_for(PathBuf::from("/state/subagent"));
        d.shutdown(Duration::from_millis(50)).await;
        assert!(!d.health().await.running);
    }
}
