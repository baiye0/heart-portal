//! Local auth preflight. Checks verify configuration, never assert
//! service authorization. Kit-managed flows can bootstrap through their own tools.
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::environment::KitEnvironment;
use super::manifest::{KitAuthMethod, KitManifest};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AuthState {
    pub required: bool,
    pub methods: Vec<AuthMethodStatus>,
    pub error: Option<String>,
    /// Only digests are retained for reload detection; neither bytes nor digests
    /// belong in user-visible status or debug output.
    /// Derived PartialEq includes them in KitManager's auth-state comparison.
    #[serde(skip)]
    fingerprints: Vec<FileFingerprint>,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct FileFingerprint(Option<[u8; 32]>);

impl std::fmt::Debug for FileFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthMethodStatus {
    pub id: String,
    pub provider: String,
    pub label: Option<String>,
    pub flow: Option<String>,
    /// configured | needs-configuration | managed-by-kit | unsupported | invalid
    pub status: String,
    pub env: Vec<String>,
    pub files: Vec<String>,
    pub missing_env: Vec<String>,
    pub missing_files: Vec<String>,
    pub instructions: Option<String>,
    pub url: Option<String>,
    pub tools: Vec<String>,
    pub error: Option<String>,
}

struct Context<'a> {
    kit_dir: &'a Path,
    environment: &'a KitEnvironment,
}

#[derive(Default)]
struct Check {
    missing_env: Vec<String>,
    missing_files: Vec<String>,
    fingerprints: Vec<FileFingerprint>,
    managed_by_kit: bool,
    error: Option<String>,
}

fn check_method(method: &KitAuthMethod, context: &Context<'_>) -> Check {
    if !matches!(method.provider.as_str(), "env" | "file" | "kit") {
        return Check {
            error: Some(format!("Unsupported auth provider '{}'; update Portal or use provider 'kit' with setup instructions", method.provider)),
            ..Check::default()
        };
    }
    let mut check = check_requirements(method, context);
    match method.provider.as_str() {
        "env" if method.env.is_empty() => {
            check.error = Some("The env provider requires at least one env variable".into());
        }
        "file" if method.files.is_empty() => {
            check.error = Some("The file provider requires at least one credential file".into());
        }
        "kit" => {
            check.managed_by_kit = true;
            if method.tools.is_empty()
                && method
                    .instructions
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .is_empty()
                && method.url.is_none()
            {
                check.error = Some(
                    "Kit-managed auth requires tools, instructions or an authorization URL".into(),
                );
            }
        }
        _ => {}
    }
    check
}

fn check_requirements(method: &KitAuthMethod, context: &Context<'_>) -> Check {
    let mut check = Check::default();
    for name in &method.env {
        if !super::environment::valid_name(name) {
            check.error = Some("Invalid environment variable name in auth method".into());
        } else if !context.environment.configured(name) {
            check.missing_env.push(name.clone());
        }
    }
    for file in &method.files {
        let fingerprint = credential_fingerprint(context.kit_dir, file);
        if fingerprint.0.is_none() {
            check.missing_files.push(file.clone());
        }
        check.fingerprints.push(fingerprint);
    }
    check
}

fn credential_fingerprint(kit_dir: &Path, file: &str) -> FileFingerprint {
    const MAX_BYTES: usize = 1024 * 1024;
    match crate::bounded_file::read_beneath(kit_dir, Path::new(file), MAX_BYTES) {
        Ok(bytes) if !bytes.is_empty() && bytes.len() <= MAX_BYTES as usize => {
            FileFingerprint(Some(Sha256::digest(&bytes).into()))
        }
        _ => FileFingerprint::default(),
    }
}

impl AuthState {
    pub fn load(kit_dir: &Path, manifest: &KitManifest, environment: &KitEnvironment) -> Self {
        let Some(auth) = manifest.provision.as_ref().and_then(|p| p.auth.as_ref()) else {
            return Self::default();
        };
        let mut state = Self {
            required: auth.required,
            ..Self::default()
        };
        if auth.version != 1 {
            state.error = Some(format!(
                "Unsupported provision.auth version {}; update Portal",
                auth.version
            ));
            return state;
        }
        if auth.methods.is_empty() {
            state.error = Some("provision.auth.methods must not be empty; omit auth for kits with no authentication".into());
            return state;
        }
        let context = Context {
            kit_dir,
            environment,
        };
        let mut ids = std::collections::HashSet::new();
        for method in &auth.methods {
            let supported = matches!(method.provider.as_str(), "env" | "file" | "kit");
            let mut check = check_method(method, &context);
            if method.id.trim().is_empty() || !ids.insert(&method.id) {
                state.error = Some("Auth method ids must be nonempty and unique".into());
            }
            if method
                .tools
                .iter()
                .any(|name| !manifest.tools.iter().any(|t| &t.name == name))
            {
                check.error = Some("Auth setup tool is not declared in the kit manifest".into());
            }
            let mut safe_url = method.url.clone();
            if let Some(url) = &method.url {
                if !url::Url::parse(url).is_ok_and(|u| {
                    u.host_str().is_some()
                        && (u.scheme() == "https"
                            || (u.scheme() == "http"
                                && u.host_str()
                                    .is_some_and(crate::relay_client::is_loopback_host)))
                }) {
                    safe_url = None;
                    check.error = Some(
                        "Auth URL must use HTTPS (HTTP is allowed only for loopback hosts)".into(),
                    );
                }
            }
            let status = if !supported {
                "unsupported"
            } else if check.error.is_some() {
                "invalid"
            } else if !check.missing_env.is_empty() || !check.missing_files.is_empty() {
                "needs-configuration"
            } else if check.managed_by_kit {
                "managed-by-kit"
            } else {
                "configured"
            };
            state.fingerprints.extend(check.fingerprints);
            state.methods.push(AuthMethodStatus {
                id: method.id.clone(),
                provider: method.provider.clone(),
                label: method.label.clone(),
                flow: method.flow.clone(),
                status: status.into(),
                missing_env: check.missing_env,
                env: method.env.clone(),
                files: method.files.clone(),
                missing_files: check.missing_files,
                instructions: method.instructions.clone(),
                url: safe_url,
                tools: method
                    .tools
                    .iter()
                    .map(|name| format!("{}_{}", manifest.name.replace('-', "_"), name))
                    .collect(),
                error: check.error,
            });
        }
        if auth.required
            && !state
                .methods
                .iter()
                .any(|m| matches!(m.status.as_str(), "configured" | "managed-by-kit"))
            && state.error.is_none()
        {
            state.error = Some("No usable authentication method; inspect portal_kits_setup for alternatives and missing requirements".into());
        }
        state
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
