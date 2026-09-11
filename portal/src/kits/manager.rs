use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::mcp::{McpConnection, McpServerConfig};
use crate::tools::ToolInfo;

use super::environment::EnvStatus;
use super::loader::{command_binary_exists, KitScan, LoadedKit};

const MAX_FAILURES: u8 = 3;
const SPAWN_TIMEOUT_SECS: u64 = 30;
/// After a kit is marked unhealthy, wait this long before giving it another
/// chance so a transient failure does not require a Portal restart to recover.
const RECOVERY_COOLDOWN_SECS: u64 = 60;
const WARMUP_TIMEOUT_SECS: u64 = 10;

#[derive(Clone)]
pub struct KitManager {
    kits: Arc<Mutex<BTreeMap<String, KitState>>>,
    connections: Arc<std::sync::Mutex<Vec<Weak<McpConnection>>>>,
    retired: Arc<Mutex<tokio::task::JoinSet<()>>>,
    stopping: Arc<AtomicBool>,
}

struct KitState {
    process_slots: Arc<tokio::sync::Semaphore>,
    start_lock: Arc<Mutex<()>>,
    cancelled: tokio::sync::watch::Sender<bool>,
    kit: LoadedKit,
    /// Wrapped in `Arc` so a caller can clone the handle, release the manager
    /// lock, and perform the MCP call without blocking other kits.
    connection: Option<Arc<McpConnection>>,
    failure_count: u8,
    unhealthy: bool,
    /// When the most recent failure occurred; used to gate self-healing after
    /// `RECOVERY_COOLDOWN_SECS`.
    last_failure_at: Option<Instant>,
    /// Calls since last `portal_kit_usage` drain
    unsent_calls: u64,
    diagnostics: KitDiagnostics,
}

/// Only Portal-observed metadata; never kit responses, arguments or credentials.
#[derive(Debug, Clone, serde::Serialize)]
pub struct KitDiagnostics {
    pub generation: String,
    pub last_started_at_unix_ms: Option<u64>,
    pub last_start_duration_ms: Option<u64>,
    pub last_lifecycle_error: Option<&'static str>,
    pub last_call: Option<KitCallStatus>,
}

impl KitDiagnostics {
    fn new(missing_runtime: bool) -> Self {
        Self {
            generation: uuid::Uuid::new_v4().to_string(),
            last_started_at_unix_ms: None,
            last_start_duration_ms: None,
            last_lifecycle_error: missing_runtime.then_some("runtime-not-found"),
            last_call: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KitCallStatus {
    pub outcome: &'static str,
    pub completed_at_unix_ms: u64,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct KitReloadReport {
    pub added: Vec<String>,
    pub reloaded: Vec<String>,
    pub removed: Vec<String>,
    /// Invalid/partially written manifests do not evict a working kit.
    pub retained_invalid: Vec<String>,
}

impl KitReloadReport {
    pub fn changed(&self) -> bool {
        !self.added.is_empty() || !self.reloaded.is_empty() || !self.removed.is_empty()
    }
}

#[derive(Debug, serde::Serialize)]
pub struct KitStatus {
    pub name: String,
    pub version: String,
    pub tools: usize,
    pub status: String,
    pub configuration_error: Option<String>,
    pub env_file: String,
    pub env: Vec<EnvStatus>,
    pub auth: super::auth::AuthState,
    pub process_id: Option<u32>,
    pub diagnostics: KitDiagnostics,
    pub next_action: &'static str,
    pub service_authorization: &'static str,
}

impl KitManager {
    pub fn new(kits: Vec<LoadedKit>) -> Self {
        let mut states = BTreeMap::new();
        for kit in kits {
            let name = kit.manifest.name.clone();
            if states.contains_key(&name) {
                warn!(
                    "Duplicate kit name '{}'; keeping the last manifest loaded",
                    name
                );
            }
            // Pre-mark unhealthy if command binary is missing or empty
            let pre_unhealthy = !command_binary_exists(&kit.command);
            if pre_unhealthy {
                warn!(
                    "Kit '{}' pre-marked unhealthy: command binary not found: {}",
                    name,
                    kit.command.first().map(|s| s.as_str()).unwrap_or("<empty>")
                );
            }
            states.insert(
                name,
                KitState {
                    kit,
                    connection: None,
                    failure_count: 0,
                    unhealthy: pre_unhealthy,
                    last_failure_at: None,
                    unsent_calls: 0,
                    diagnostics: KitDiagnostics::new(pre_unhealthy),
                    process_slots: Arc::new(tokio::sync::Semaphore::new(
                        crate::mcp::limits::KIT_GENERATIONS,
                    )),
                    start_lock: Arc::new(Mutex::new(())),
                    cancelled: tokio::sync::watch::channel(false).0,
                },
            );
        }

        Self {
            kits: Arc::new(Mutex::new(states)),
            connections: Arc::new(std::sync::Mutex::new(Vec::new())),
            retired: Arc::new(Mutex::new(tokio::task::JoinSet::new())),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    pub async fn list_tools(&self) -> Vec<ToolInfo> {
        let kits = self.kits.lock().await;
        let mut tools = Vec::new();

        for state in kits.values() {
            push_kit_tools(state, &mut tools);
        }

        tools
    }

    pub async fn list_healthy_tools(&self) -> Vec<ToolInfo> {
        let mut kits = self.kits.lock().await;
        let mut tools = Vec::new();
        for state in kits.values_mut() {
            recover_if_cooled_down(state);
            if !state.unhealthy && state.kit.configuration_error().is_none() {
                push_kit_tools(state, &mut tools);
            }
        }
        tools
    }

    async fn connection_for(
        &self,
        name: &str,
        generation: &str,
        seconds: u64,
    ) -> Result<Arc<McpConnection>> {
        anyhow::ensure!(
            !self.stopping.load(Ordering::Acquire),
            "Portal is shutting down"
        );
        let gate = {
            let kits = self.kits.lock().await;
            kits.get(name)
                .context("Kit was removed")?
                .start_lock
                .clone()
        };
        let _guard = timeout(Duration::from_secs(5), gate.lock())
            .await
            .context("Kit is already starting; retry later")?;
        // Copy only spawn inputs. Runtime counters and diagnostics remain in the
        // registry and are updated only after confirming this generation still owns it.
        let (config, old_connection, kit_permit, mut cancelled) = {
            let mut kits = self.kits.lock().await;
            let state = kits.get_mut(name).context("Kit was removed")?;
            anyhow::ensure!(
                state.diagnostics.generation == generation,
                "Kit was reloaded; retry with its current tools"
            );
            if let Some(connection) = state.connection.as_ref().filter(|c| c.is_alive()) {
                return Ok(connection.clone());
            }
            check_startable(state)?;
            let kit_permit = state.process_slots.clone().try_acquire_owned()
                .context("This kit still has two process generations in use; let its old calls finish before retrying")?;
            let config = McpServerConfig {
                name: name.to_owned(),
                command: state.kit.command.clone(),
                env: kit_env(state),
                cwd: Some(state.kit.kit_dir.clone()),
            };
            (
                config,
                state.connection.take(),
                kit_permit,
                state.cancelled.subscribe(),
            )
        };
        anyhow::ensure!(
            !*cancelled.borrow(),
            "Kit startup cancelled by reload or shutdown"
        );
        if let Some(connection) = old_connection {
            connection.abort();
            if let Ok(mut owned) = Arc::try_unwrap(connection) {
                let _ = timeout(Duration::from_secs(5), owned.shutdown()).await;
            }
        }
        info!("Spawning kit '{}'", name);
        let started_at = unix_ms();
        let starting = Instant::now();
        let result = tokio::select! {
            biased;
            _ = cancelled.changed() => anyhow::bail!("Kit startup cancelled by reload or shutdown"),
            result = timeout(Duration::from_secs(seconds), McpConnection::spawn(config)) => result,
        };
        let duration_ms = starting.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let mut kits = self.kits.lock().await;
        let current = kits
            .get_mut(name)
            .filter(|state| state.diagnostics.generation == generation);
        let Some(state) = current.filter(|_| !self.stopping.load(Ordering::Acquire)) else {
            if let Ok(Ok(connection)) = &result {
                connection.abort();
            }
            anyhow::bail!("Kit was reloaded, removed or Portal is shutting down");
        };
        state.diagnostics.last_start_duration_ms = Some(duration_ms);
        match result {
            Ok(Ok(mut connection)) => {
                connection.set_capacity(vec![kit_permit]);
                let connection = Arc::new(connection);
                debug!("Kit '{}' spawned", name);
                state.diagnostics.last_started_at_unix_ms = Some(started_at);
                state.diagnostics.last_lifecycle_error = None;
                state.diagnostics.last_call = None;
                let mut connections = self.connections.lock().unwrap();
                connections.retain(|c| c.strong_count() > 0);
                connections.push(Arc::downgrade(&connection));
                state.connection = Some(connection.clone());
                Ok(connection)
            }
            failure => {
                let code = match failure {
                    Ok(Err(_)) => "mcp-start-failed",
                    Err(_) => "mcp-start-timeout",
                    Ok(Ok(_)) => unreachable!(),
                };
                state.diagnostics.last_lifecycle_error = Some(code);
                record_failure(state);
                warn!(
                    "Kit '{}' failed MCP startup; details returned to caller",
                    name
                );
                anyhow::bail!(
                    "Kit '{}' failed to start ({}). Inspect portal_kits_status and the kit runtime configuration.",
                    name, code
                )
            }
        }
    }

    async fn retire(&self, connection: Arc<McpConnection>) {
        let connection = match Arc::try_unwrap(connection) {
            Ok(mut owned) => {
                let _ = timeout(Duration::from_secs(5), owned.shutdown()).await;
                return;
            }
            Err(shared) => shared,
        };
        let mut retired = self.retired.lock().await;
        while retired.try_join_next().is_some() {}
        retired.spawn(async move {
            let mut connection = connection;
            loop {
                match Arc::try_unwrap(connection) {
                    Ok(mut owned) => {
                        let _ = timeout(Duration::from_secs(5), owned.shutdown()).await;
                        return;
                    }
                    Err(shared) => connection = shared,
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
    }

    pub async fn resolve_tool(&self, tool_name: &str) -> Option<(String, String)> {
        let kits = self.kits.lock().await;
        for state in kits.values() {
            for tool in &state.kit.manifest.tools {
                let routed_name = super::loader::tool_route(&state.kit.manifest.name, &tool.name);
                let normalized_query = tool_name.replace('-', "_");
                if routed_name == normalized_query {
                    return Some((state.kit.manifest.name.clone(), tool.name.clone()));
                }
            }
        }
        None
    }

    pub async fn call_tool(
        &self,
        kit_name: &str,
        tool_name: &str,
        mut arguments: Value,
    ) -> Result<Value> {
        let (params, generation) = {
            let kits = self.kits.lock().await;
            let state = kits.get(kit_name).context("Unknown kit")?;
            let params = state
                .kit
                .manifest
                .tools
                .iter()
                .find(|tool| tool.name == tool_name)
                .context("Unknown kit tool")?
                .params
                .clone();
            (params, state.diagnostics.generation.clone())
        };
        let connection = self
            .connection_for(kit_name, &generation, SPAWN_TIMEOUT_SECS)
            .await?;

        // Heart's act DSL passes all parameter values as JSON strings; coerce
        // them to the types declared in the kit tool's JSON Schema before the
        // MCP call so kit-side validation does not reject e.g. limit="3".
        coerce_kit_arguments(&mut arguments, &params);

        // Phase 2: perform the MCP call WITHOUT holding the manager lock.
        let result = connection.call_tool(tool_name, arguments).await;
        if result
            .as_ref()
            .is_err_and(|error| error.is::<crate::mcp::RequestRejected>())
        {
            return result;
        }
        let request_failure = result.as_ref().err().and_then(|error| {
            if error.is::<crate::mcp::RequestTimeout>() {
                Some("timeout")
            } else if error.is::<crate::mcp::RemoteError>() {
                Some("request-error")
            } else {
                None
            }
        });
        if let Some(outcome) = request_failure {
            let mut kits = self.kits.lock().await;
            if let Some(state) = kits.get_mut(kit_name).filter(|state| {
                state
                    .connection
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, &connection))
            }) {
                state.diagnostics.last_call = Some(KitCallStatus {
                    outcome,
                    completed_at_unix_ms: unix_ms(),
                });
            }
            return result;
        }
        // Phase 3: re-acquire the lock only to update health bookkeeping.
        let mut kits = self.kits.lock().await;
        // An old in-flight call must never reset health or close the replacement
        // process after a reload (including remove + reinstall with the same name).
        let current = kits
            .get(kit_name)
            .and_then(|state| state.connection.as_ref())
            .is_some_and(|active| Arc::ptr_eq(active, &connection));
        drop(connection);
        if !current {
            if result.is_ok() {
                if let Some(state) = kits.get_mut(kit_name) {
                    state.unsent_calls += 1;
                }
            }
            return result;
        }
        match kits.get_mut(kit_name) {
            Some(state) => match result {
                Ok(value) => {
                    state.failure_count = 0;
                    state.unsent_calls += 1;
                    state.diagnostics.last_lifecycle_error = None;
                    state.diagnostics.last_call = Some(KitCallStatus {
                        outcome: if value.get("isError").and_then(Value::as_bool) == Some(true) {
                            "tool-error"
                        } else {
                            "success"
                        },
                        completed_at_unix_ms: unix_ms(),
                    });
                    Ok(value)
                }
                Err(err) => {
                    warn!("Kit '{}' tool '{}' MCP call failed", kit_name, tool_name);
                    state.diagnostics.last_lifecycle_error = Some("mcp-call-failed");
                    state.diagnostics.last_call = Some(KitCallStatus {
                        outcome: "mcp-error",
                        completed_at_unix_ms: unix_ms(),
                    });
                    let retired = state.connection.take();
                    record_failure(state);
                    drop(kits);
                    if let Some(connection) = retired {
                        connection.abort();
                        self.retire(connection).await;
                    }
                    Err(err).with_context(|| {
                        format!("Failed to call kit '{}' tool '{}'", kit_name, tool_name)
                    })
                }
            },
            None => {
                warn!(
                    "Kit '{}' was removed during call_tool; returning Phase 2 result as-is",
                    kit_name
                );
                result.with_context(|| format!("Kit '{}' removed during call", kit_name))
            }
        }
    }

    /// Pre-spawn kits marked `eager: true` so the first tool call has no
    /// cold-start latency. Failures are logged and non-fatal.
    pub async fn warmup(&self) {
        // Phase 1: collect eager, healthy kit names (release lock afterwards).
        let eager_names: Vec<String> = {
            let kits = self.kits.lock().await;
            kits.iter()
                .filter(|(_, state)| {
                    state.kit.manifest.eager == Some(true)
                        && !state.unhealthy
                        && state.kit.configuration_error().is_none()
                })
                .map(|(name, _)| name.clone())
                .collect()
        };

        if eager_names.is_empty() {
            return;
        }

        info!("Warming up {} eager kit(s)", eager_names.len());

        for name in eager_names {
            if self.stopping.load(Ordering::Acquire) {
                break;
            }
            let generation = {
                let kits = self.kits.lock().await;
                kits.get(&name)
                    .filter(|s| s.kit.manifest.eager == Some(true))
                    .map(|s| s.diagnostics.generation.clone())
            };
            if let Some(generation) = generation {
                match self
                    .connection_for(&name, &generation, WARMUP_TIMEOUT_SECS)
                    .await
                {
                    Ok(_) => info!("Warmed up eager kit '{}'", name),
                    Err(_) => warn!(
                        "Eager kit '{}' is unavailable; Portal remains running",
                        name
                    ),
                }
            }
        }
    }

    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        let connections = {
            let mut kits = self.kits.lock().await;
            kits.values_mut()
                .filter_map(|state| {
                    state.cancelled.send_replace(true);
                    state.connection.take()
                })
                .collect::<Vec<_>>()
        };
        // Includes retired connections still held by in-flight tool calls.
        for connection in self
            .connections
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
        {
            connection.abort();
        }
        for connection in connections {
            self.retire(connection).await;
        }
        let mut retired = self.retired.lock().await;
        if timeout(Duration::from_secs(5), async {
            while retired.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            retired.shutdown().await;
        }
        info!("Kit processes shut down");
    }

    pub async fn statuses(&self) -> Vec<KitStatus> {
        let kits = self.kits.lock().await;
        kits.values()
            .map(|state| KitStatus {
                name: state.kit.manifest.name.clone(),
                version: state.kit.manifest.version.clone(),
                tools: state.kit.manifest.tools.len(),
                status: status_text(state).to_string(),
                configuration_error: state.kit.configuration_error().map(str::to_owned),
                env_file: state
                    .kit
                    .kit_dir
                    .join(".env")
                    .to_string_lossy()
                    .into_owned(),
                env: state.kit.environment.statuses(&state.kit.manifest),
                auth: state.kit.auth.clone(),
                process_id: state
                    .connection
                    .as_ref()
                    .filter(|c| c.is_alive())
                    .and_then(|c| c.process_id()),
                diagnostics: state.diagnostics.clone(),
                next_action: next_action(state),
                service_authorization: "not-verified-by-portal",
            })
            .collect()
    }

    /// Return known, non-secret setup metadata, never the raw manifest/defaults
    /// or arbitrary extension values. Commands/URLs are instructions, not run here.
    pub async fn setup(&self, kit_name: &str) -> Result<Value> {
        let kits = self.kits.lock().await;
        let state = kits
            .get(kit_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown kit: {}", kit_name))?;
        let kit = &state.kit;
        let provision = kit.manifest.provision.clone().unwrap_or_default();
        Ok(serde_json::json!({
            "kit": kit.manifest.name,
            "version": kit.manifest.version,
            "status": status_text(state),
            "configuration_error": kit.configuration_error(),
            "directory": kit.kit_dir,
            "env_file": kit.kit_dir.join(".env"),
            "env": kit.environment.statuses(&kit.manifest),
            "auth": kit.auth,
            "authorization": "Verified by the kit/service when used; configuration checks do not validate token scopes or account permissions.",
            "runtime": provision.runtime,
            "legacy_runtime": kit.manifest.runtime,
            "platforms": kit.manifest.platform.as_ref().unwrap_or(&provision.platforms),
            "dependencies": provision.deps,
            "install": provision.install,
            "post_install": provision.post_install,
            "instructions": provision.instructions,
            "permissions": kit.manifest.permissions,
            "setup_execution": "Instructions only. Run the appropriate setup step explicitly, then call portal_kits_reload for this kit.",
        }))
    }

    /// Reconcile in-memory kit state with a freshly scanned kit list.
    /// Existing connections are shut down so the next tool call re-spawns
    /// with the updated manifest; `unsent_calls` is preserved.
    #[cfg(test)]
    pub async fn refresh_kits(&self, fresh: KitScan, force: bool) -> bool {
        self.refresh_kits_target(fresh, force, None).await.changed()
    }

    pub async fn refresh_kits_target(
        &self,
        mut fresh: KitScan,
        force: bool,
        target: Option<&str>,
    ) -> KitReloadReport {
        let mut kits = self.kits.lock().await;
        if self.stopping.load(Ordering::Acquire) {
            return KitReloadReport::default();
        }
        // A disk scan cannot see the routes of invalid/partially written
        // manifests, or edits excluded by a targeted reload. Keep those loaded
        // owners in the conflict check, before changing any registry state.
        let present_dirs: std::collections::HashSet<_> = fresh
            .kits
            .iter()
            .map(|kit| kit.kit_dir.clone())
            .chain(fresh.invalid_dirs.iter().cloned())
            .collect();
        let mut rejected_dirs = std::collections::HashSet::new();
        let owners: Vec<_> = kits
            .values()
            .filter_map(|owner| {
                let unchanged_by_target =
                    target.is_some_and(|name| name != owner.kit.manifest.name);
                if !unchanged_by_target && !present_dirs.contains(&owner.kit.kit_dir) {
                    return None; // The old directory is uninstalled in this refresh.
                }
                let routes: std::collections::HashSet<_> = owner
                    .kit
                    .manifest
                    .tools
                    .iter()
                    .map(|tool| super::loader::tool_route(&owner.kit.manifest.name, &tool.name))
                    .collect();
                Some((owner, unchanged_by_target, routes))
            })
            .collect();
        for candidate in &fresh.kits {
            if target.is_some_and(|name| name != candidate.manifest.name) {
                continue;
            }
            let routes: std::collections::HashSet<_> = candidate
                .manifest
                .tools
                .iter()
                .map(|tool| super::loader::tool_route(&candidate.manifest.name, &tool.name))
                .collect();
            for (owner, unchanged_by_target, owner_routes) in &owners {
                if candidate.kit_dir == owner.kit.kit_dir && !*unchanged_by_target {
                    continue; // A kit may change its own routes.
                }
                if candidate.manifest.name == owner.kit.manifest.name
                    || candidate.kit_dir == owner.kit.kit_dir
                    || !routes.is_disjoint(owner_routes)
                {
                    warn!(
                        "Kit '{}' conflicts with loaded kit '{}'; retaining the loaded owner",
                        candidate.manifest.name, owner.kit.manifest.name
                    );
                    rejected_dirs.insert(candidate.kit_dir.clone());
                }
            }
        }
        // A rejected update also retains that directory's last usable manifest.
        fresh
            .kits
            .retain(|kit| !rejected_dirs.contains(&kit.kit_dir));
        fresh.invalid_dirs.extend(rejected_dirs);
        let mut retired_connections = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut report = KitReloadReport::default();

        for kit in fresh.kits {
            let name = kit.manifest.name.clone();
            if !seen.insert(name.clone()) {
                warn!(
                    "Kit '{}' appears multiple times in manifest list, skipping duplicate",
                    name
                );
                continue;
            }
            if target.is_some_and(|target| target != name) {
                continue;
            }

            if let Some(state) = kits.get_mut(&name) {
                if (force && target.is_none_or(|target| target == name))
                    || state.kit.manifest != kit.manifest
                    || state.kit.command != kit.command
                    || state.kit.kit_dir != kit.kit_dir
                    || state.kit.environment != kit.environment
                    || state.kit.auth != kit.auth
                    || (state.unhealthy
                        && state.last_failure_at.is_none()
                        && command_binary_exists(&kit.command))
                {
                    let old_version = state.kit.manifest.version.clone();
                    let new_version = kit.manifest.version.clone();
                    state.cancelled.send_replace(true);
                    state.cancelled = tokio::sync::watch::channel(false).0;
                    state.start_lock = Arc::new(Mutex::new(()));
                    state.kit = kit;
                    if let Some(connection) = state.connection.take() {
                        retired_connections.push(connection);
                    }
                    state.failure_count = 0;
                    state.unhealthy = !command_binary_exists(&state.kit.command);
                    state.last_failure_at = None;
                    state.diagnostics = KitDiagnostics::new(state.unhealthy);
                    report.reloaded.push(name.clone());
                    info!(
                        "Kit '{}' configuration refreshed (v{} → v{})",
                        name, old_version, new_version
                    );
                }
            } else {
                let version = kit.manifest.version.clone();
                let pre_unhealthy = !command_binary_exists(&kit.command);
                if pre_unhealthy {
                    warn!(
                        "Kit '{}' pre-marked unhealthy: command binary not found: {}",
                        name,
                        kit.command.first().map(|s| s.as_str()).unwrap_or("<empty>")
                    );
                }
                kits.insert(
                    name.clone(),
                    KitState {
                        kit,
                        connection: None,
                        failure_count: 0,
                        unhealthy: pre_unhealthy,
                        last_failure_at: None,
                        unsent_calls: 0,
                        diagnostics: KitDiagnostics::new(pre_unhealthy),
                        process_slots: Arc::new(tokio::sync::Semaphore::new(
                            crate::mcp::limits::KIT_GENERATIONS,
                        )),
                        start_lock: Arc::new(Mutex::new(())),
                        cancelled: tokio::sync::watch::channel(false).0,
                    },
                );
                info!("Kit '{}' discovered (v{})", name, version);
                report.added.push(name);
            }
        }

        let to_remove: Vec<String> = kits
            .keys()
            .filter(|name| {
                target.is_none_or(|target| target == name.as_str())
                    && !seen.contains(*name)
                    && !fresh.invalid_dirs.contains(&kits[*name].kit.kit_dir)
            })
            .cloned()
            .collect();
        for name in to_remove {
            if let Some(mut state) = kits.remove(&name) {
                state.cancelled.send_replace(true);
                if let Some(connection) = state.connection.take() {
                    retired_connections.push(connection);
                }
                info!("Kit '{}' removed", name);
                report.removed.push(name);
            }
        }
        report.retained_invalid = kits
            .iter()
            .filter(|(_, state)| fresh.invalid_dirs.contains(&state.kit.kit_dir))
            .map(|(name, _)| name.clone())
            .collect();
        drop(kits);
        for connection in retired_connections {
            self.retire(connection).await;
        }
        report
    }

    /// Drain accumulated call counts per kit since the last drain, resetting
    /// counters to zero. Only kits with count > 0 are included.
    pub async fn drain_usage_counts(&self) -> HashMap<String, u64> {
        let mut result = HashMap::new();
        let mut kits = self.kits.lock().await;
        for (name, state) in kits.iter_mut() {
            let count = std::mem::take(&mut state.unsent_calls);
            if count > 0 {
                result.insert(name.clone(), count);
            }
        }
        result
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn next_action(state: &KitState) -> &'static str {
    if state.kit.configuration_error().is_some() {
        return "configure-kit";
    }
    if state.start_lock.try_lock().is_err() {
        return "wait-for-startup";
    }
    if state.diagnostics.last_lifecycle_error.is_some() {
        return "check-runtime-and-mcp";
    }
    if state
        .diagnostics
        .last_call
        .as_ref()
        .is_some_and(|call| matches!(call.outcome, "tool-error" | "request-error" | "timeout"))
    {
        return "inspect-tool-result";
    }
    "call-tool"
}

fn check_startable(state: &mut KitState) -> Result<()> {
    // Configuration failures do not consume restart attempts. A refresh picks
    // up corrected credentials immediately, even during the recovery cooldown.
    if let Some(error) = state.kit.configuration_error() {
        anyhow::bail!("Kit '{}' needs configuration: {}. Inspect portal_kits_setup for this kit (env file: {}), complete an available auth method, then call portal_kits_reload or wait for automatic refresh.",
            state.kit.manifest.name, error, state.kit.kit_dir.join(".env").display());
    }
    // Self-healing: an unhealthy kit gets another chance once the cooldown has
    // elapsed, so a transient fault does not require a Portal restart.
    recover_if_cooled_down(state);

    if state.unhealthy {
        anyhow::bail!(
            "Kit '{}' is unhealthy (mcp-start-failed). Inspect the kit runtime configuration.",
            state.kit.manifest.name
        );
    }

    if state.failure_count >= MAX_FAILURES {
        state.unhealthy = true;
        anyhow::bail!(
            "Kit '{}' is unhealthy after {} failed restart attempts (mcp-start-failed). Inspect the kit runtime configuration.",
            state.kit.manifest.name,
            state.failure_count
        );
    }

    Ok(())
}

/// Reset an unhealthy kit back to a retryable state once the recovery cooldown
/// has elapsed since its last recorded failure.
fn recover_if_cooled_down(state: &mut KitState) {
    if !state.unhealthy {
        return;
    }

    let cooled_down = state
        .last_failure_at
        .map(|at| at.elapsed() >= Duration::from_secs(RECOVERY_COOLDOWN_SECS))
        .unwrap_or(false);

    if cooled_down {
        info!(
            "Kit '{}' cooldown elapsed after {}s; giving it another chance",
            state.kit.manifest.name, RECOVERY_COOLDOWN_SECS
        );
        state.unhealthy = false;
        state.failure_count = 0;
    }
}

/// Normalize kit name for tool routing: replace hyphens with underscores.
fn kit_slug(name: &str) -> String {
    name.replace('-', "_")
}

fn push_kit_tools(state: &KitState, tools: &mut Vec<ToolInfo>) {
    for tool in &state.kit.manifest.tools {
        tools.push(ToolInfo {
            name: format!("{}_{}", kit_slug(&state.kit.manifest.name), tool.name),
            description: tool.description.clone(),
            input_schema: tool.params.clone(),
        });
    }
}

fn record_failure(state: &mut KitState) {
    state.failure_count = state.failure_count.saturating_add(1);
    state.last_failure_at = Some(Instant::now());
    if state.failure_count >= MAX_FAILURES {
        state.unhealthy = true;
        warn!(
            "Kit '{}' marked unhealthy after {} failures",
            state.kit.manifest.name, state.failure_count
        );
    }
}

fn kit_env(state: &KitState) -> HashMap<String, String> {
    let mut env = state.kit.environment.process_values();
    env.insert(
        "PORTAL_KIT_NAME".to_string(),
        state.kit.manifest.name.clone(),
    );
    env.insert(
        "PORTAL_KIT_DIR".to_string(),
        state.kit.kit_dir.to_string_lossy().to_string(),
    );
    env
}

fn status_text(state: &KitState) -> &'static str {
    if state.kit.configuration_error().is_some() {
        "needs-configuration"
    } else if state.start_lock.try_lock().is_err() {
        "starting"
    } else if state.unhealthy {
        "unhealthy"
    } else if state
        .connection
        .as_ref()
        .map(|connection| connection.is_alive())
        .unwrap_or(false)
    {
        "healthy"
    } else {
        "not-started"
    }
}

/// Best-effort coercion of string-typed act-DSL arguments to the types
/// declared in a kit tool's JSON Schema `properties`. Parse failures leave
/// the original value so the kit can report the real validation error.
fn coerce_kit_arguments(arguments: &mut Value, schema: &Value) {
    let Some(args_obj) = arguments.as_object_mut() else {
        return;
    };
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return;
    };

    for (key, prop_schema) in properties {
        let Some(value) = args_obj.get_mut(key) else {
            continue;
        };
        let Some(type_str) = prop_schema.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        let Value::String(s) = value else {
            continue;
        };

        let coerced = match type_str {
            "integer" => s.parse::<i64>().ok().map(Value::from),
            "number" => s.parse::<f64>().ok().map(Value::from),
            "boolean" => match s.as_str() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            "array" => serde_json::from_str::<Value>(s)
                .ok()
                .filter(|v| v.is_array()),
            _ => None,
        };

        if let Some(new_val) = coerced {
            *value = new_val;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kits::manifest::{KitManifest, KitToolDef};

    #[test]
    fn kit_slug_normalizes_hyphens() {
        assert_eq!(kit_slug("agent-reach"), "agent_reach");
        assert_eq!(kit_slug("cua-driver"), "cua_driver");
        assert_eq!(kit_slug("cursor"), "cursor");
        assert_eq!(kit_slug("a-b-c"), "a_b_c");
    }

    #[tokio::test]
    async fn resolve_tool_normalizes_kit_hyphens() {
        let manager = KitManager::new(vec![loaded_kit("my-kit", None)]);
        // Should resolve with underscored form
        let result = manager.resolve_tool("my_kit_ping").await;
        assert!(result.is_some(), "should resolve my_kit_ping");
        let (kit_name, tool_name) = result.unwrap();
        assert_eq!(
            kit_name, "my-kit",
            "should return original kit name for internal lookup"
        );
        assert_eq!(tool_name, "ping");
        // Should also resolve with hyphenated form (backward compat)
        let result2 = manager.resolve_tool("my-kit_ping").await;
        assert!(
            result2.is_some(),
            "should resolve my-kit_ping (hyphen form)"
        );
        let (kit_name2, tool_name2) = result2.unwrap();
        assert_eq!(kit_name2, "my-kit");
        assert_eq!(tool_name2, "ping");
    }

    #[tokio::test]
    async fn list_tools_uses_normalized_names() {
        let manager = KitManager::new(vec![loaded_kit("my-kit", None)]);
        let tools = manager.list_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].name, "my_kit_ping",
            "exposed tool name should use underscores"
        );
    }

    #[tokio::test]
    async fn list_healthy_tools_skips_unhealthy_kits() {
        // "broken" kit has a missing binary → pre-marked unhealthy by KitManager::new()
        // "healthy" kit uses /bin/echo → not pre-marked
        let manager = KitManager::new(vec![
            loaded_kit("healthy", None),
            loaded_kit("broken", None),
        ]);

        let all_tools = manager.list_tools().await;
        let healthy_tools = manager.list_healthy_tools().await;

        assert_eq!(all_tools.len(), 2);
        assert_eq!(healthy_tools.len(), 1);
        assert_eq!(healthy_tools[0].name, "healthy_ping");
    }

    #[tokio::test]
    async fn warmup_skips_unhealthy_and_non_eager_kits() {
        // eager=true healthy, eager=false healthy, eager=true but unhealthy (missing binary)
        let manager = KitManager::new(vec![
            loaded_kit("eager-ok", Some(true)),
            loaded_kit("not-eager", Some(false)),
            loaded_kit("broken", Some(true)),
        ]);

        manager.warmup().await;

        // Manager remains functional after warmup (failures are non-fatal).
        let all_tools = manager.list_tools().await;
        let healthy_tools = manager.list_healthy_tools().await;
        assert_eq!(all_tools.len(), 3);
        assert_eq!(healthy_tools.len(), 2);
        let statuses = manager.statuses().await;
        assert_eq!(statuses.len(), 3);
    }

    #[test]
    fn unhealthy_kit_recovers_after_cooldown() {
        let mut state = kit_state(loaded_kit("healthy", None));
        state.unhealthy = true;
        state.failure_count = MAX_FAILURES;
        state.last_failure_at =
            Instant::now().checked_sub(Duration::from_secs(RECOVERY_COOLDOWN_SECS + 1));
        assert!(
            state.last_failure_at.is_some(),
            "test host must have enough uptime to construct a past Instant"
        );

        recover_if_cooled_down(&mut state);

        assert!(
            !state.unhealthy,
            "an unhealthy kit should recover once the cooldown has elapsed"
        );
        assert_eq!(
            state.failure_count, 0,
            "recovery should reset failure_count"
        );
    }

    #[test]
    fn unhealthy_kit_stays_unhealthy_within_cooldown() {
        let mut state = kit_state(loaded_kit("healthy", None));
        state.unhealthy = true;
        state.failure_count = MAX_FAILURES;
        state.last_failure_at = Some(Instant::now());

        recover_if_cooled_down(&mut state);

        assert!(
            state.unhealthy,
            "a kit within its cooldown window must stay unhealthy"
        );
    }

    #[test]
    fn kit_env_includes_path() {
        let state = kit_state(loaded_kit("healthy", None));

        let env = kit_env(&state);

        let path = env.get("PATH").expect("kit_env must set PATH");
        #[cfg(unix)]
        assert!(
            path.contains("/opt/homebrew/bin"),
            "PATH should include Homebrew's bin dir, got: {}",
            path
        );
        #[cfg(unix)]
        assert!(
            path.contains("/usr/bin"),
            "PATH should include /usr/bin, got: {}",
            path
        );
        #[cfg(windows)]
        assert_eq!(path, &std::env::var("PATH").unwrap_or_default());
        assert_eq!(
            env.get("PORTAL_KIT_NAME").map(String::as_str),
            Some("healthy"),
            "existing kit env vars must be preserved"
        );
    }

    #[test]
    fn coerce_string_to_integer() {
        let mut args = serde_json::json!({"limit": "42"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["limit"], 42);
    }

    #[test]
    fn coerce_string_to_number() {
        let mut args = serde_json::json!({"ratio": "3.14"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "ratio": {"type": "number"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["ratio"], 3.14);
    }

    #[test]
    fn coerce_string_to_boolean() {
        let mut args = serde_json::json!({"flag": "true", "other": "false"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "flag": {"type": "boolean"},
                "other": {"type": "boolean"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["flag"], true);
        assert_eq!(args["other"], false);
    }

    #[test]
    fn coerce_leaves_unparseable_string_as_is() {
        let mut args = serde_json::json!({"limit": "not-a-number", "flag": "yes"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"},
                "flag": {"type": "boolean"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["limit"], "not-a-number");
        assert_eq!(args["flag"], "yes");
    }

    #[test]
    fn coerce_passes_native_values_through() {
        let mut args = serde_json::json!({"limit": 7, "flag": false, "ratio": 1.5});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"},
                "flag": {"type": "boolean"},
                "ratio": {"type": "number"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["limit"], 7);
        assert_eq!(args["flag"], false);
        assert_eq!(args["ratio"], 1.5);
    }

    #[test]
    fn coerce_ignores_args_missing_from_schema() {
        let mut args = serde_json::json!({"limit": "3", "extra": "keep"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["limit"], 3);
        assert_eq!(args["extra"], "keep");
    }

    #[test]
    fn coerce_handles_empty_or_null_schema() {
        let mut args = serde_json::json!({"limit": "3"});
        coerce_kit_arguments(&mut args, &Value::Null);
        assert_eq!(args["limit"], "3");

        coerce_kit_arguments(&mut args, &serde_json::json!({}));
        assert_eq!(args["limit"], "3");

        coerce_kit_arguments(&mut args, &serde_json::json!({"type": "object"}));
        assert_eq!(args["limit"], "3");
    }

    #[test]
    fn coerce_string_to_array() {
        let mut args = serde_json::json!({"tags": "[\"a\",\"b\"]"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "tags": {"type": "array"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["tags"], serde_json::json!(["a", "b"]));
    }

    fn kit_state(kit: LoadedKit) -> KitState {
        KitState {
            diagnostics: KitDiagnostics::new(false),
            process_slots: Arc::new(tokio::sync::Semaphore::new(
                crate::mcp::limits::KIT_GENERATIONS,
            )),
            start_lock: Arc::new(Mutex::new(())),
            cancelled: tokio::sync::watch::channel(false).0,
            kit,
            connection: None,
            failure_count: 0,
            unhealthy: false,
            last_failure_at: None,
            unsent_calls: 0,
        }
    }

    fn loaded_kit(name: &str, eager: Option<bool>) -> LoadedKit {
        // Use a short-lived real command so healthy kits work on every test OS.
        let argv = if name == "broken" {
            vec!["definitely-missing-kit-binary"]
        } else {
            #[cfg(windows)]
            {
                vec!["cmd.exe", "/D", "/C", "echo"]
            }
            #[cfg(not(windows))]
            {
                vec!["/bin/echo"]
            }
        };
        loaded_kit_argv(name, argv, eager)
    }

    fn loaded_kit_argv(name: &str, argv: Vec<&str>, eager: Option<bool>) -> LoadedKit {
        let command: Vec<String> = argv.into_iter().map(str::to_string).collect();
        LoadedKit {
            manifest: KitManifest {
                name: name.to_string(),
                version: "0.1.0".to_string(),
                description: None,
                author: None,
                platform: None,
                runtime: None,
                command: command.clone(),
                tools: vec![KitToolDef {
                    name: "ping".to_string(),
                    description: "Ping".to_string(),
                    params: serde_json::json!({"type": "object"}),
                }],
                permissions: None,
                workspace: None,
                eager,
                provision: None,
            },
            kit_dir: std::env::temp_dir(),
            command,
            environment: Default::default(),
            auth: Default::default(),
        }
    }
}
