//! Per-kit configuration, loaded without mutating Portal's process environment.
use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use super::manifest::KitManifest;

#[derive(Clone, Default, PartialEq, Eq)]
pub struct KitEnvironment {
    pub values: HashMap<String, String>,
    pub error: Option<String>,
}

// Debug output must not disclose credentials, including on assertion failures.
impl std::fmt::Debug for KitEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KitEnvironment")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Serialize)]
pub struct EnvStatus {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
    pub configured: bool,
}

fn usable(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && !(value.contains("{{") && value.contains("}}"))
}

pub(super) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn env_key(name: &str) -> String {
    #[cfg(windows)]
    {
        name.to_ascii_uppercase()
    }
    #[cfg(not(windows))]
    {
        name.to_string()
    }
}

impl KitEnvironment {
    /// Use the same runtime search environment for resolution and child startup.
    /// Explicit kit PATH values (including empty ones) override service defaults.
    pub fn process_values(&self) -> HashMap<String, String> {
        let mut values = self.values.clone();
        values.entry("PATH".into()).or_insert_with(|| {
            let inherited = std::env::var("PATH").unwrap_or_default();
            #[cfg(unix)]
            {
                const EXTRA: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
                if inherited.is_empty() {
                    EXTRA.into()
                } else {
                    format!("{EXTRA}:{inherited}")
                }
            }
            #[cfg(not(unix))]
            {
                inherited
            }
        });
        #[cfg(windows)]
        values.entry("PATHEXT".into()).or_insert_with(|| {
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        });
        values
    }

    pub fn load(kit_dir: &Path, manifest: &KitManifest) -> Self {
        let mut env = Self::default();
        match crate::bounded_file::text(&kit_dir.join(".env"), 256 * 1024) {
            Ok(content) => match super::dotenv::parse(&content) {
                Ok(values) => {
                    for (key, value) in values {
                        env.values.insert(env_key(&key), value);
                    }
                }
                Err(()) => {
                    env.error = Some("Invalid kit .env; check KEY=value syntax and quoting (values are literal, no variable expansion)".into());
                    return env;
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                env.error = Some(
                    "Cannot read kit .env; use a regular UTF-8 file no larger than 256 KiB".into(),
                );
                return env;
            }
        }

        if let Some(provision) = &manifest.provision {
            for var in &provision.env {
                if !valid_name(&var.name) {
                    env.error = Some("Invalid environment variable name in provision.env".into());
                    return env;
                }
                // A kit-local value wins over the inherited service environment,
                // then the declared default. Explicit empty values stay empty.
                if !env.values.contains_key(&env_key(&var.name)) {
                    if let Some(value) = std::env::var(&var.name)
                        .ok()
                        .or_else(|| var.default.clone())
                    {
                        if value.contains('\0') {
                            env.error = Some("Invalid environment value in provision.env".into());
                            return env;
                        }
                        env.values.insert(env_key(&var.name), value);
                    }
                }
            }
            // Auth alternatives can reference inherited values without adding
            // globally required env entries (which would turn OR into AND).
            for name in provision
                .auth
                .iter()
                .flat_map(|a| &a.methods)
                .flat_map(|m| &m.env)
            {
                if !valid_name(name) {
                    env.error = Some("Invalid environment variable name in provision.auth".into());
                    return env;
                }
                if !env.values.contains_key(&env_key(name)) {
                    if let Ok(value) = std::env::var(name) {
                        env.values.insert(env_key(name), value);
                    }
                }
            }
            let missing: Vec<_> = provision
                .env
                .iter()
                .filter(|var| var.required && !env.configured(&var.name))
                .map(|var| var.name.as_str())
                .collect();
            if !missing.is_empty() {
                env.error = Some(format!(
                    "Missing required environment variables: {}",
                    missing.join(", ")
                ));
            }
        }
        env
    }

    pub(super) fn configured(&self, name: &str) -> bool {
        self.values
            .get(&env_key(name))
            .is_some_and(|value| usable(value))
    }

    pub fn statuses(&self, manifest: &KitManifest) -> Vec<EnvStatus> {
        manifest
            .provision
            .iter()
            .flat_map(|p| &p.env)
            .map(|var| EnvStatus {
                name: var.name.clone(),
                description: var.description.clone(),
                required: var.required,
                configured: self.configured(&var.name),
            })
            .collect()
    }
}
