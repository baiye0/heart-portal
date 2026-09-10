//! An independent worker replaces Portal while its supervisor pauses.
//! Preserve the existing launch origin so upgrades do not change TCC attribution.
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};

pub fn installation_root(exe: &Path) -> Result<PathBuf> {
    let parent = exe.parent().context("Executable has no parent")?;
    anyhow::ensure!(
        matches!(exe.file_name().and_then(|s| s.to_str()), Some("heart-portal" | "heart-portal-macos-arm64" | "heart-portal-macos-x86_64")),
        "Keep the release filename or rename the executable to heart-portal"
    );
    if parent.file_name().and_then(|s| s.to_str()) == Some("release")
        && parent.parent().and_then(Path::file_name).and_then(|s| s.to_str()) == Some("target") {
        return Ok(parent.parent().and_then(Path::parent).context("Invalid installation path")?.to_path_buf());
    }
    Ok(parent.to_path_buf())
}

/// Re-enter the saved transaction from the user's normal launch origin. Exec
/// first replaces this process with Python so rollback cannot kill its caller
/// or leave it executing the candidate inode after restoring the old binary.
pub fn recover_interrupted() -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    if std::env::var("HEART_PORTAL_SUPERVISED").as_deref() == Ok("1") {
        return Ok(());
    }
    let target = std::env::current_exe()?.canonicalize()?;
    let root = installation_root(&target)?;
    let journal_path = root.join(".portal-upgrade.json");
    if !journal_path.exists() {
        return Ok(());
    }
    let lock = std::fs::OpenOptions::new()
        .read(true).write(true).create(true).truncate(false).mode(0o600)
        .open(root.join(".portal-upgrade.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(()); // The normal startup guard reports active maintenance.
        }
        return Err(error).context("Checking interrupted upgrade");
    }
    if !journal_path.exists() {
        return Ok(());
    }
    let journal: serde_json::Value = serde_json::from_slice(&std::fs::read(journal_path)?)?;
    let stage = PathBuf::from(journal["stage"].as_str().context("Upgrade journal has no stage")?)
        .canonicalize()?;
    anyhow::ensure!(stage.parent() == Some(root.join(".portal-upgrades").as_path()),
        "Upgrade recovery stage is outside this installation");
    for name in ["portal-macos-upgrade.py", "portal-macos.py", "request.json"] {
        anyhow::ensure!(stage.join(name).canonicalize()?.parent() == Some(stage.as_path()),
            "Upgrade recovery file is outside this installation");
    }
    let request: serde_json::Value = serde_json::from_slice(&std::fs::read(stage.join("request.json"))?)?;
    anyhow::ensure!(Path::new(request["root"].as_str().context("Recovery request has no root")?)
        .canonicalize()? == root, "Recovery request belongs to another installation");
    if let Some(path) = request["target"].as_str() {
        anyhow::ensure!(Path::new(path).canonicalize()? == target,
            "Recovery request belongs to another executable");
    }
    let python = std::fs::read_to_string(root.join(".portal-python"))
        .map(|s| PathBuf::from(s.trim()))
        .unwrap_or_else(|_| PathBuf::from("/usr/bin/python3"));
    anyhow::ensure!(python.is_absolute() && python.is_file(),
        "Upgrade recovery requires Python 3.9+; install it or update .portal-python");
    // The saved worker reacquires maintenance and rechecks the journal. Another
    // recovery owner winning this handoff must never cause a second replacement.
    drop(lock);
    eprintln!("Recovering interrupted upgrade before starting Portal: {}", stage.display());
    let error = std::process::Command::new(python)
        .arg("-c").arg(include_str!("../../scripts/portal-macos-recover.py"))
        .arg(&root).arg(&stage).arg(&target).args(std::env::args_os().skip(1)).exec();
    Err(error).context("Starting interrupted-upgrade recovery")
}

pub fn startup_guard() -> Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    let Ok(root) = installation_root(&std::env::current_exe()?) else {
        return Ok(None);
    };
    // launchd's owned job is explicitly restarted by the worker while it holds
    // the transaction lock. Direct launches must wait until it commits.
    if std::env::var("HEART_PORTAL_SUPERVISED").as_deref() == Ok("1") {
        return Ok(None);
    }
    // A legacy start.sh remains unsupervised. Let only this transaction's
    // restart cross the lock; do not pretend it has KeepAlive/portal_restart.
    if let Ok(nonce) = std::env::var("HEART_PORTAL_UPGRADE_START") {
        if root.join(".portal-upgrade.json").is_file()
            && std::fs::read_to_string(root.join(".portal-launch-nonce")).ok().as_deref() == Some(nonce.as_str())
        {
            return Ok(None);
        }
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join(".portal-upgrade.lock"))?;
    anyhow::ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0,
        "Portal maintenance/upgrade is in progress"
    );
    anyhow::ensure!(
        !root.join(".portal-upgrade.json").exists(),
        "An interrupted upgrade needs recovery; inspect .portal-upgrade.json and rerun its stage/portal-macos-upgrade.py"
    );
    Ok(Some(file))
}

pub fn publish_ready() -> Result<()> {
    if std::env::var("HEART_PORTAL_SUPERVISED").as_deref() != Ok("1") && !crate::macos_supervisor::attached() {
        return Ok(());
    }
    let Ok(root) = installation_root(&std::env::current_exe()?) else {
        return Ok(());
    };
    let Ok(nonce) = std::env::var("HEART_PORTAL_READY_NONCE")
        .or_else(|_| std::fs::read_to_string(root.join(".portal-launch-nonce"))) else {
        return Ok(());
    };
    let target = std::env::var_os("HEART_PORTAL_READY_FILE").map(PathBuf::from)
        .unwrap_or_else(|| root.join(".portal-ready.json"));
    let path = target.with_extension(format!("{}.tmp", std::process::id()));
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(&serde_json::to_vec(&serde_json::json!({
        "pid": std::process::id(), "version": crate::upgrade::PORTAL_VERSION, "nonce": nonce
    }))?)?;
    file.sync_all()?;
    std::fs::rename(path, target)?;
    Ok(())
}

pub fn show_status() -> Result<()> {
    let root = installation_root(&std::env::current_exe()?)?;
    let path = root.join(".portal-upgrade-status.json");
    if !path.exists() {
        println!("No upgrade recorded for {}", root.display());
        return Ok(());
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    if matches!(
        value["state"].as_str(),
        Some("failed" | "rolled_back" | "recovery_required")
    ) {
        bail!("The last upgrade did not succeed; see status above");
    }
    Ok(())
}

pub async fn handoff(bytes: &[u8], version: Option<&str>) -> Result<()> {
    handoff_to(bytes, version, &std::env::current_exe()?).await
}

pub async fn migrate(target: &Path) -> Result<()> {
    let current = std::env::current_exe()?.canonicalize()?;
    anyhow::ensure!(current != target.canonicalize()?, "Run migration from the downloaded new executable, pointing --target at the installed old one");
    handoff_to(&std::fs::read(current)?, Some(crate::upgrade::PORTAL_VERSION), target).await
}

async fn handoff_to(bytes: &[u8], version: Option<&str>, target: &Path) -> Result<()> {
    let target = target.canonicalize()?;
    let root = installation_root(&target)?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let stage = root.join(".portal-upgrades").join(&id);
    std::fs::create_dir_all(&stage)?;
    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700))?;
    let label = format!("town.beings.heart-portal.upgrade.{id}");
    let plist = PathBuf::from(std::env::var_os("HOME").context("HOME is missing")?)
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));
    std::fs::write(stage.join("candidate"), bytes)?;
    std::fs::set_permissions(
        stage.join("candidate"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    std::fs::write(
        stage.join("portal-macos.py"),
        include_str!("../../scripts/portal-macos.py"),
    )?;
    std::fs::write(
        stage.join("portal-macos-upgrade.py"),
        include_str!("../../scripts/portal-macos-upgrade.py"),
    )?;
    std::fs::write(
        stage.join("portal-macos-supervisor.py"),
        include_str!("../../scripts/portal-macos-supervisor.py"),
    )?;
    std::fs::write(
        stage.join("request.json"),
        serde_json::to_vec(&serde_json::json!({
            "root": root, "target": target,
            "version": version, "parent_pid": std::process::id(),
            "parent_executable": std::env::current_exe()?.canonicalize()?, "worker_plist": plist
        }))?,
    )?;
    // Persist the request and recovery code before making the job runnable.
    for entry in std::fs::read_dir(&stage)? {
        std::fs::File::open(entry?.path())?.sync_all()?;
    }
    // bootstrap creates a sibling job owned by launchd; setsid/double-fork alone
    // cannot escape launchd's process-group cleanup when Portal is booted out.
    let python = std::fs::read_to_string(root.join(".portal-python"))
        .map(|s| PathBuf::from(s.trim()))
        .unwrap_or_else(|_| PathBuf::from("/usr/bin/python3"));
    anyhow::ensure!(
        python.is_absolute() && python.is_file(),
        "Installed Python 3 runtime is missing; reinstall Python and update .portal-python"
    );
    let output = tokio::process::Command::new(&python)
        .arg(stage.join("portal-macos-upgrade.py")).arg("--dispatch")
        .stdin(Stdio::null()).output().await.context("Starting the macOS updater requires Python 3")?;
    anyhow::ensure!(
        output.status.success(),
        "Could not start independent updater: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(360);
    loop {
        if stage.join("error.json").exists() {
            let error: serde_json::Value =
                serde_json::from_slice(&std::fs::read(stage.join("error.json"))?)?;
            bail!(
                "Upgrade rejected; installed binary unchanged: {}",
                error["message"]
            );
        }
        if !stage.join("accepted.json").exists() && stage.join("result.json").exists() {
            bail!(
                "Updater completed before acceptance: {}",
                std::fs::read_to_string(stage.join("result.json"))?
            );
        }
        if stage.join("accepted.json").exists() {
            let accepted: serde_json::Value = serde_json::from_slice(&std::fs::read(stage.join("accepted.json"))?)?;
            if accepted["signature_identity_preserved"] == false {
                eprintln!("Signing identity changed; macOS may require one-time authorization. Upgrade will continue.");
            }
            eprintln!("Upgrade accepted by independent worker; this command will exit.");
            eprintln!("Check completion with: heart-portal upgrade --status");
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("Updater acceptance is still unknown. Inspect {} before retrying; the registered worker may still complete.", stage.display());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_is_the_running_checkout_not_home_or_cwd() {
        assert_eq!(
            installation_root(Path::new("/tmp/中文 portal/target/release/heart-portal")).unwrap(),
            Path::new("/tmp/中文 portal")
        );
        assert_eq!(installation_root(Path::new("/tmp/user package/heart-portal")).unwrap(), Path::new("/tmp/user package"));
        assert_eq!(installation_root(Path::new("/tmp/user package/heart-portal-macos-arm64")).unwrap(), Path::new("/tmp/user package"));
        assert!(installation_root(Path::new("/tmp/unrelated-app")).is_err());
    }
}
