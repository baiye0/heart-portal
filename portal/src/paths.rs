//! User configuration and compatibility with explicitly selected legacy data.
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub fn home_dir() -> Result<PathBuf> {
    // Native Windows installations consistently use USERPROFILE, even when Git
    // or another shell sets HOME differently.
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let home = home
        .map(PathBuf::from)
        .or_else(system_home_dir)
        .context("User home directory is unavailable; supply --config with an absolute path")?;
    anyhow::ensure!(home.is_absolute(), "User home directory must be absolute");
    Ok(home)
}

#[cfg(unix)]
fn system_home_dir() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // Services may omit HOME. Use the reentrant account lookup, not getpwuid's
    // shared static buffer, since kit refresh runs on background threads.
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let mut record: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut record,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() || record.pw_dir.is_null() {
            return None;
        }
        let bytes = unsafe { std::ffi::CStr::from_ptr(record.pw_dir) }.to_bytes();
        return Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)));
    }
}

#[cfg(not(unix))]
fn system_home_dir() -> Option<PathBuf> {
    None
}

pub fn data_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".heart-portal"))
}

pub fn expand_home(path: &Path) -> Result<PathBuf> {
    if path == Path::new("~") {
        return home_dir();
    }
    if let Ok(rest) = path.strip_prefix("~") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(path.to_path_buf())
}

pub fn resolve_relative(path: &Path, config: &Path) -> Result<PathBuf> {
    let path = expand_home(path)?;
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::path::absolute(config)?
        .parent()
        .context("Config has no parent directory")?
        .join(path))
}

/// Preserve an existing legacy kit directory before adopting config-relative
/// defaults. Never silently switch an installed kit set on binary replacement.
pub fn resolve_kits_compatible(
    path: &Path,
    config: &Path,
    working: &Path,
) -> Result<(PathBuf, bool)> {
    let expanded = expand_home(path)?;
    let modern = resolve_relative(path, config)?;
    if !expanded.is_absolute() {
        let legacy = working.join(&expanded);
        if legacy != modern && legacy.is_dir() {
            return Ok((legacy, true));
        }
    }
    Ok((modern, false))
}

#[derive(Debug, Serialize)]
pub struct ConfigLocation {
    pub path: PathBuf,
    pub source: &'static str,
}

pub fn legacy_dirs() -> Result<Vec<PathBuf>> {
    let exe = std::env::current_exe()?;
    #[cfg(any(windows, target_os = "macos"))]
    if crate::user_installation::is_managed(&exe)? {
        return Ok(vec![crate::user_installation::legacy_root(&exe)?]);
    }
    #[cfg(any(windows, target_os = "macos"))]
    if crate::user_installation::migrated_source(&exe)? {
        return Ok(vec![crate::user_installation::root()?]);
    }
    let mut root = exe.parent().context("Executable has no parent")?;
    if matches!(
        root.file_name().and_then(|n| n.to_str()),
        Some("release" | "debug")
    ) && root
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        == Some("target")
    {
        root = root
            .parent()
            .and_then(Path::parent)
            .context("Invalid checkout path")?;
    }
    let mut dirs = vec![root.to_path_buf()];
    let cwd = std::env::current_dir()?;
    if cwd != root {
        dirs.push(cwd);
    }
    Ok(dirs)
}

pub fn locate_config(explicit: Option<&Path>, legacy_dirs: &[PathBuf]) -> Result<ConfigLocation> {
    if explicit.is_some() {
        return select_config(explicit, &[], Path::new(""));
    }
    if let Some(root) = legacy_dirs.first() {
        match crate::bounded_file::read(&root.join(".portal-launch.json"), crate::bounded_file::CONFIG_LIMIT) {
            Ok(bytes) => {
                let launch: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("Invalid saved Portal launch configuration"))?;
                let config = launch["arguments"][1]
                    .as_str()
                    .context("Saved Portal launch has no config path")?;
                let mut location = select_config(Some(Path::new(config)), &[], Path::new(""))?;
                location.source = "saved-launch";
                return Ok(location);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => anyhow::bail!("Cannot read saved Portal launch configuration"),
        }
    }
    select_config(None, legacy_dirs, &data_dir().unwrap_or_default())
}

/// Existing explicit/portable layouts keep their identity. New installations
/// share one stable default independent of the downloaded executable's folder.
pub fn select_config(
    explicit: Option<&Path>,
    legacy_dirs: &[PathBuf],
    data: &Path,
) -> Result<ConfigLocation> {
    if let Some(path) = explicit {
        let path = std::path::absolute(expand_home(path)?)?;
        anyhow::ensure!(path.is_file(), "Config file not found: {}", path.display());
        return Ok(ConfigLocation {
            path,
            source: "explicit",
        });
    }
    for dir in legacy_dirs {
        let path = std::path::absolute(dir.join("portal.toml"))?;
        // exists includes directories: a broken config must be reported, never
        // silently replaced with another account's central/default configuration.
        if path.try_exists()? {
            return Ok(ConfigLocation {
                path,
                source: "legacy",
            });
        }
    }
    anyhow::ensure!(
        data.is_absolute(),
        "Cannot resolve an absolute user config directory; supply --config"
    );
    Ok(ConfigLocation {
        path: data.join("portal.toml"),
        source: "user",
    })
}

#[derive(Debug, Serialize)]
pub struct MigrationPlan {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_preserved: bool,
    pub already_applied: bool,
    /// Values never appear in serialized plans/debug output.
    #[serde(skip)]
    document: PrivateDocument,
}

struct PrivateDocument(String);
impl std::fmt::Debug for PrivateDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
pub fn plan_migration(source: &Path, data: &Path, profile: Option<&str>) -> Result<MigrationPlan> {
    plan_migration_with_installation(source, data, profile, None)
}

pub fn plan_migration_with_installation(
    source: &Path,
    data: &Path,
    profile: Option<&str>,
    installation: Option<&Path>,
) -> Result<MigrationPlan> {
    let source = std::path::absolute(expand_home(source)?)?;
    let destination = if let Some(profile) = profile {
        anyhow::ensure!(
            !profile.is_empty()
                && profile
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Profile must contain only ASCII letters, digits, hyphens or underscores"
        );
        data.join("profiles").join(profile).join("portal.toml")
    } else {
        data.join("portal.toml")
    };
    let content =
        crate::bounded_file::text(&source, crate::bounded_file::CONFIG_LIMIT).context("Cannot read migration source configuration")?;
    if source.canonicalize().ok() == destination.canonicalize().ok() && destination.is_file() {
        return Ok(MigrationPlan {
            source,
            destination,
            source_preserved: true,
            already_applied: true,
            document: PrivateDocument(content),
        });
    }
    // TOML errors can embed the input line (including tokens); keep errors generic.
    let mut document: toml::Value = toml::from_str(content.trim_start_matches('\u{feff}'))
        .map_err(|_| {
            anyhow::anyhow!("Invalid TOML in migration source; source remains unchanged")
        })?;
    let table = document
        .as_table_mut()
        .context("Migration source must be a TOML table")?;
    let relative_kits = table
        .get("kits_dir")
        .and_then(toml::Value::as_str)
        .map(|path| expand_home(Path::new(path)))
        .transpose()?
        .filter(|path| !path.is_absolute());
    let resolved =
        crate::config::PortalConfig::load(source.to_str().context("Config path must be Unicode")?)
            .map_err(|_| {
                anyhow::anyhow!(
                    "Migration source configuration is invalid; source remains unchanged"
                )
            })?;
    // Persist effective paths before relocating the file. Workspaces/kits and
    // their credentials are not moved, copied, or recursively traversed.
    table.insert(
        "workspace".into(),
        toml::Value::String(
            resolved
                .security
                .workspace_root
                .to_string_lossy()
                .into_owned(),
        ),
    );
    if let Some(kits) = &resolved.kits_dir {
        table.insert(
            "kits_dir".into(),
            toml::Value::String(
                resolve_relative(Path::new(kits), &source)?
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
    }
    let dir = installation.unwrap_or(source.parent().context("Config has no parent")?);
    // Import the persistent launch identity only if the saved launch points to
    // this exact config. Never import a different Portal's link or OS tokens.
    let saved_launch =
        match crate::bounded_file::read(&dir.join(".portal-launch.json"), crate::bounded_file::CONFIG_LIMIT) {
            Ok(bytes) => Some(serde_json::from_slice::<serde_json::Value>(&bytes).map_err(
                |_| anyhow::anyhow!("Invalid saved launch metadata; repair it before migration"),
            )?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => anyhow::bail!("Cannot read saved launch metadata"),
        };
    let launch_matches = saved_launch.as_ref().is_some_and(|launch| {
        launch["arguments"][1]
            .as_str()
            .and_then(|p| Path::new(p).canonicalize().ok())
            == source.canonicalize().ok()
    });
    if saved_launch.is_some() && !launch_matches {
        anyhow::bail!("Saved launch uses a different config; migrate that config explicitly");
    }
    if let Some(relative) = relative_kits {
        let working = saved_launch.as_ref().and_then(|launch| launch["working_directory"].as_str())
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok().filter(|cwd| Some(cwd.as_path()) == source.parent()))
            .context("Cannot infer the legacy kits working directory. Supply valid saved launch metadata or set kits_dir to its original absolute path before migration")?;
        anyhow::ensure!(
            working.is_absolute(),
            "Saved working directory must be absolute"
        );
        let (effective, _) = resolve_kits_compatible(&relative, &source, &working)?;
        table.insert(
            "kits_dir".into(),
            toml::Value::String(effective.to_string_lossy().into_owned()),
        );
    }
    let read_optional = |name: &str| -> Result<Option<String>> {
        match crate::bounded_file::text(&dir.join(name), crate::bounded_file::CONFIG_LIMIT) {
            Ok(value) => Ok(Some(value.trim().into()).filter(|v: &String| !v.is_empty())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => anyhow::bail!("Cannot read saved Portal identity file {}", name),
        }
    };
    let launch = saved_launch.as_ref();
    // A saved environment token overrides TOML at runtime. Dropping it during
    // relocation could turn an authenticated listener into an open listener.
    if let Some(token) = launch
        .and_then(|l| l["environment"]["PORTAL_MCP_TOKEN"].as_str())
        .filter(|token| !token.is_empty())
    {
        table.insert("portal_mcp_token".into(), toml::Value::String(token.into()));
    }
    let connection = launch
        .and_then(|l| l["environment"]["PORTAL_CONNECT_LINK"].as_str())
        .map(str::to_owned)
        .or(read_optional(".portal-connection.url")?);
    if let Some(connection) = connection.filter(|c| !c.is_empty()) {
        crate::relay_client::parse_loom_link(&connection)
            .context("Saved Portal link is invalid")?;
        table.insert("connect".into(), toml::Value::String(connection));
    }
    let name = launch
        .and_then(|l| l["name"].as_str())
        .map(str::to_owned)
        .or(read_optional(".portal-name")?);
    if let Some(name) = name {
        table.insert("name".into(), toml::Value::String(name));
    }
    let document = PrivateDocument(toml::to_string_pretty(&document)?);
    let already_applied = match crate::bounded_file::text(&destination, crate::bounded_file::CONFIG_LIMIT) {
        Ok(existing) => {
            anyhow::ensure!(existing == document.0, "Destination already contains a different config: {}. Use --profile to keep installations separate; nothing was overwritten", destination.display());
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => anyhow::bail!("Cannot read destination configuration"),
    };
    Ok(MigrationPlan {
        source,
        destination,
        source_preserved: true,
        already_applied,
        document,
    })
}

/// Only auto-import the current executable's saved identity when it refers to
/// this source. For another installation with an external config, the caller
/// supplies --installation explicitly; unrelated launch credentials are ignored.
pub fn matching_installation(source: &Path, root: &Path) -> Result<bool> {
    let bytes = match crate::bounded_file::read(&root.join(".portal-launch.json"), crate::bounded_file::CONFIG_LIMIT) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => anyhow::bail!("Cannot read saved launch metadata"),
    };
    let launch: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!("Invalid saved launch metadata; repair it before migration")
    })?;
    let config = launch["arguments"][1]
        .as_str()
        .context("Saved Portal launch has no config path")?;
    let source = expand_home(source)?
        .canonicalize()
        .context("Cannot resolve migration source")?;
    Ok(Path::new(config)
        .canonicalize()
        .is_ok_and(|path| path == source))
}

impl MigrationPlan {
    pub fn apply(&self) -> Result<()> {
        if self.already_applied {
            anyhow::ensure!(
                crate::bounded_file::text(&self.destination, crate::bounded_file::CONFIG_LIMIT)? == self.document.0,
                "Destination changed after planning; migration was not applied"
            );
            return Ok(());
        }
        publish_config(&self.destination, &self.document.0)
    }
}

/// Initial creation and migration use the same private, no-replace publication.
/// Existing configuration is never rewritten, including a concurrent creator's file.
pub fn initialize_config(location: &ConfigLocation) -> Result<()> {
    if location.path.try_exists()? {
        return Ok(());
    }
    anyhow::ensure!(
        location.source == "user",
        "Config file not found: {}",
        location.path.display()
    );
    if let Err(error) = publish_config(&location.path, include_str!("../../portal.example.toml")) {
        if !location.path.is_file() {
            return Err(error);
        }
    }
    Ok(())
}

fn publish_config(destination: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    let parent = destination.parent().context("Destination has no parent")?;
    std::fs::create_dir_all(parent)?;
    let stage = parent.join(format!(".portal-config-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        #[cfg(windows)]
        let mut file = crate::windows_private::create(&stage)?;
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&stage)?
        };
        #[cfg(not(any(windows, unix)))]
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        // Atomic publication without replacing a destination another process
        // created after planning. A crash can only leave an unused temp file.
        std::fs::hard_link(&stage, destination).context(
            "Cannot publish config atomically without overwriting; source remains unchanged",
        )?;
        Ok(())
    })();
    let _ = std::fs::remove_file(stage);
    result
}
#[cfg(test)]
#[path = "paths_tests.rs"]
mod tests;
