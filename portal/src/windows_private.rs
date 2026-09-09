//! Managed credentials are private from creation, even in a shared directory.
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use anyhow::{Context, Result};
use windows_sys::Win32::{
    Foundation::{LocalFree, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SetSecurityInfo, SE_FILE_OBJECT,
        },
        GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

struct LocalMemory(*mut core::ffi::c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn descriptor() -> Result<LocalMemory> {
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut size = 0;
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut size,
        );
        anyhow::ensure!(size > 0, "Cannot determine current Windows user SID");
        // TOKEN_USER contains pointers: keep the returned buffer pointer-aligned.
        let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut sid = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let _sid_memory = LocalMemory(sid.cast());
        let mut len = 0;
        while *sid.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16(std::slice::from_raw_parts(sid, len))?;
        // Tasks run as this same user. SYSTEM is the only additional principal;
        // do not inherit broad Users/Everyone access from a portable install.
        let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut sd = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(LocalMemory(sd))
    }
}

fn open_private(path: &Path, create: bool) -> Result<File> {
    let sd = descriptor()?;
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    unsafe {
        let handle = CreateFileW(
            name.as_ptr(),
            READ_CONTROL | WRITE_DAC | if create { GENERIC_WRITE } else { 0 },
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            if create { CREATE_NEW } else { OPEN_EXISTING },
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = File::from_raw_handle(handle);
        let (mut present, mut defaulted, mut acl) = (0, 0, std::ptr::null_mut());
        if GetSecurityDescriptorDacl(sd.0, &mut present, &mut acl, &mut defaulted) == 0
            || present == 0
            || acl.is_null()
        {
            anyhow::bail!("Cannot construct private Portal file permissions");
        }
        // Also repair old files. Explicitly applying the DACL before any write
        // fails closed on volumes which cannot enforce Windows file permissions.
        let error = SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        );
        anyhow::ensure!(
            error == 0,
            "Cannot protect Portal credentials: {}",
            std::io::Error::from_raw_os_error(error as i32)
        );
        Ok(file)
    }
}

pub fn create(path: &Path) -> Result<File> {
    open_private(path, true).context(
        "Creating private Portal state; use an ACL-capable folder owned by your Windows user",
    )
}

pub fn protect_existing(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Managed Portal credentials must be regular files"
            );
            match open_private(path, false) {
                Ok(file) => drop(file),
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub fn protect_installation(root: &Path) -> Result<()> {
    for name in [
        ".portal-launch.json",
        ".portal-direct.json",
        ".portal-upgrade.json",
        ".portal-connection.url",
    ] {
        protect_existing(&root.join(name))?;
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".tmp")
            && [".portal-launch.", ".portal-direct.", ".portal-upgrade."]
                .iter()
                .any(|prefix| name.starts_with(prefix))
        {
            protect_existing(&entry.path())?;
        }
    }
    // Interrupted older starts can leave token-bearing requests behind.
    for parent in [".portal-start", ".portal-upgrades"] {
        let folder = root.join(parent);
        if folder.is_dir() {
            for stage in std::fs::read_dir(folder)? {
                let stage = stage?;
                if stage.file_type()?.is_dir() {
                    protect_existing(&stage.path().join("request.json"))?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn powershell(root: &Path, script: &str) {
        let exe = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let output = std::process::Command::new(exe)
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("PORTAL_ACL_TEST_ROOT", root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn assert_private(root: &Path) {
        powershell(
            root,
            r#"
$ErrorActionPreference = 'Stop'
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
Get-ChildItem -LiteralPath $env:PORTAL_ACL_TEST_ROOT -File -Recurse -Force | ForEach-Object {
    $acl = [IO.File]::GetAccessControl($_.FullName)
    if (-not $acl.AreAccessRulesProtected) { throw 'Unprotected ACL' }
    foreach ($rule in $acl.GetAccessRules($true,$true,[Security.Principal.SecurityIdentifier])) {
        if ($rule.IdentityReference.Value -notin @($sid,'S-1-5-18')) { throw 'Unrelated principal' }
    }
}
"#,
        );
    }

    #[test]
    fn private_json_and_temporary_files_never_inherit_public_read_access() {
        let root = std::env::temp_dir().join(format!("portal-rust-acl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        powershell(
            &root,
            r#"
$ErrorActionPreference = 'Stop'
$acl = [IO.Directory]::GetAccessControl($env:PORTAL_ACL_TEST_ROOT)
$everyone = [Security.Principal.SecurityIdentifier]::new('S-1-1-0')
$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($everyone,'ReadAndExecute','ContainerInherit,ObjectInherit','None','Allow'))
[IO.Directory]::SetAccessControl($env:PORTAL_ACL_TEST_ROOT,$acl)
"#,
        );
        let mut temp = create(&root.join("unpublished.tmp")).unwrap();
        assert_private(&root); // Observe protection before writing any bytes.
        temp.write_all(b"private credential").unwrap();
        drop(temp);
        for name in ["request.json", ".portal-direct.json"] {
            let path = root.join(name);
            std::fs::write(&path, b"legacy broad file").unwrap();
            crate::windows_upgrade::write_json(
                &path,
                &serde_json::json!({"token": "new credential"}),
            )
            .unwrap();
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(value["token"], "new credential");
        }
        assert_private(&root);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn old_state_and_orphan_requests_are_repaired_without_changing_bytes() {
        let root =
            std::env::temp_dir().join(format!("portal-rust-migrate-acl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".portal-start/old")).unwrap();
        for name in [
            ".portal-launch.json",
            ".portal-direct.json",
            ".portal-upgrade.json",
            ".portal-connection.url",
            ".portal-launch.json.orphan.tmp",
            ".portal-start/old/request.json",
        ] {
            std::fs::write(root.join(name), b"unchanged credential bytes").unwrap();
        }
        protect_installation(&root).unwrap();
        assert_private(&root);
        assert_eq!(
            std::fs::read(root.join(".portal-launch.json")).unwrap(),
            b"unchanged credential bytes"
        );
        assert!(create(&root.join("missing/request.json")).is_err());
        assert!(!root.join("missing/request.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
