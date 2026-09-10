use super::*;
#[test]
fn existing_legacy_kits_are_preserved_even_when_config_directory_differs() {
    let root = TestKits::new();
    let working = root.0.join("working");
    let config = root.0.join("settings/portal.toml");
    std::fs::create_dir_all(working.join("kits")).unwrap();
    std::fs::create_dir_all(config.parent().unwrap().join("kits")).unwrap();
    let (effective, legacy) =
        resolve_kits_compatible(Path::new("kits"), &config, &working).unwrap();
    assert!(legacy);
    assert_eq!(effective, working.join("kits"));
}
use crate::kits::tests::TestKits;
use serde_json::json;

#[test]
fn migration_preserves_config_relative_kits_when_saved_working_directory_has_none() {
    let root = TestKits::new();
    let install = root.0.join("installation");
    let settings = root.0.join("settings");
    std::fs::create_dir(&install).unwrap();
    std::fs::create_dir_all(settings.join("kits")).unwrap();
    let source = settings.join("portal.toml");
    let original = "workspace='./workspace'\nkits_dir='./kits'\n";
    std::fs::write(&source, original).unwrap();
    std::fs::write(
        install.join(".portal-launch.json"),
        json!({
            "arguments": ["--config", source], "working_directory": install
        })
        .to_string(),
    )
    .unwrap();
    for legacy_exists in [false, true] {
        if legacy_exists {
            std::fs::create_dir(install.join("kits")).unwrap();
        }
        let profile = if legacy_exists {
            "legacy"
        } else {
            "config-relative"
        };
        let plan = plan_migration_with_installation(
            &source,
            &root.0.join("data"),
            Some(profile),
            Some(&install),
        )
        .unwrap();
        let (expected, _) =
            resolve_kits_compatible(Path::new("./kits"), &source, &install).unwrap();
        plan.apply().unwrap();
        let loaded = crate::config::PortalConfig::load(plan.destination.to_str().unwrap()).unwrap();
        assert_eq!(PathBuf::from(loaded.kits_dir.unwrap()), expected);
        assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
        assert!(
            plan_migration_with_installation(
                &source,
                &root.0.join("data"),
                Some(profile),
                Some(&install)
            )
            .unwrap()
            .already_applied
        );
    }
}

#[test]
fn config_selection_preserves_existing_installations_and_centralizes_new_ones() {
    let root = TestKits::new();
    let install = root.0.join("download");
    let data = root.0.join("profile/.heart-portal");
    std::fs::create_dir_all(&install).unwrap();
    let fresh = select_config(None, &[install.clone()], &data).unwrap();
    assert_eq!(fresh.path, data.join("portal.toml"));
    assert_eq!(fresh.source, "user");
    assert!(!data.exists(), "inspection does not create files");
    let legacy = install.join("portal.toml");
    std::fs::write(&legacy, "name='legacy'").unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let central = data.join("portal.toml");
    std::fs::write(&central, "name='another-installation'").unwrap();
    assert_eq!(
        select_config(None, &[install.clone()], &data).unwrap().path,
        legacy
    );
    assert_eq!(
        select_config(Some(&central), &[install], &data)
            .unwrap()
            .path,
        central
    );
    assert!(select_config(Some(&root.0.join("missing")), &[], &data).is_err());
}

#[test]
fn saved_launch_config_survives_changing_name_and_cwd() {
    let root = TestKits::new();
    let external = root.0.join("external.toml");
    std::fs::write(&external, "name='saved'").unwrap();
    let launch = root.0.join(".portal-launch.json");
    std::fs::write(
        &launch,
        json!({"arguments": ["--config", external]}).to_string(),
    )
    .unwrap();
    let selected = locate_config(None, &[root.0.clone()]).unwrap();
    assert_eq!(selected.path, external);
    assert_eq!(selected.source, "saved-launch");
    std::fs::remove_file(&external).unwrap();
    assert!(locate_config(None, &[root.0.clone()]).is_err());
    std::fs::write(&launch, "not valid JSON secret-content").unwrap();
    let error = locate_config(None, &[root.0.clone()]).unwrap_err();
    assert!(!error.to_string().contains("secret-content"));
}

#[test]
fn relative_kits_workspace_and_home_paths_resolve_from_config_location() {
    let root = TestKits::new();
    let config = root.0.join("portal.toml");
    std::fs::write(&config, "workspace='./my-workspace'\nkits_dir='./my-kits'").unwrap();
    let loaded = crate::config::PortalConfig::load(config.to_str().unwrap()).unwrap();
    assert_eq!(loaded.security.workspace_root, root.0.join("my-workspace"));
    assert_eq!(
        PathBuf::from(loaded.kits_dir.unwrap()),
        root.0.join("my-kits")
    );
    std::fs::write(
        &config,
        "workspace='~/.heart-portal/workspace'\nkits_dir='~/.heart-portal/kits'",
    )
    .unwrap();
    let loaded = crate::config::PortalConfig::load(config.to_str().unwrap()).unwrap();
    assert_eq!(
        loaded.security.workspace_root,
        data_dir().unwrap().join("workspace")
    );
    assert_eq!(
        PathBuf::from(loaded.kits_dir.unwrap()),
        data_dir().unwrap().join("kits")
    );
}

#[test]
fn migration_is_private_atomic_idempotent_and_preserves_live_source_and_paths() {
    let root = TestKits::new();
    let install = root.0.join("legacy");
    let data = root.0.join("central");
    std::fs::create_dir(&install).unwrap();
    let source = install.join("portal.toml");
    let original = "name='config-name'\nworkspace='./work'\nkits_dir='./kits'\nportal_mcp_token='private-mcp-token'\n[future]\nenabled=true\n";
    std::fs::write(&source, original).unwrap();
    std::fs::write(
        install.join(".portal-launch.json"),
        serde_json::json!({
            "arguments": ["--config", source], "working_directory": install,
            "name": "saved-name", "environment": {"PORTAL_MCP_TOKEN": "private-environment-token"}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(install.join(".portal-name"), "saved-name").unwrap();
    std::fs::write(
        install.join(".portal-connection.url"),
        "https://relay.example/being/?token=private-relay-token",
    )
    .unwrap();
    let plan = plan_migration(&source, &data, Some("desktop")).unwrap();
    assert!(!plan.already_applied);
    assert!(!data.exists(), "planning must not write");
    let public = serde_json::to_string(&plan).unwrap() + &format!("{plan:?}");
    assert!(!public.contains("private-mcp-token") && !public.contains("private-relay-token"));
    assert!(!public.contains("private-environment-token"));
    plan.apply().unwrap();
    let migrated = crate::config::PortalConfig::load(plan.destination.to_str().unwrap()).unwrap();
    assert_eq!(migrated.security.workspace_root, install.join("work"));
    assert_eq!(
        PathBuf::from(migrated.kits_dir.unwrap()),
        install.join("kits")
    );
    assert_eq!(migrated.name, "saved-name");
    assert_eq!(
        migrated.portal_mcp_token.as_deref(),
        Some("private-environment-token")
    );
    assert!(migrated
        .connect_link
        .unwrap()
        .contains("private-relay-token"));
    assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
    assert!(std::fs::read_to_string(&plan.destination)
        .unwrap()
        .contains("[future]"));
    let again = plan_migration(&source, &data, Some("desktop")).unwrap();
    assert!(again.already_applied);
    again.apply().unwrap();
    assert!(
        plan_migration(&plan.destination, &data, Some("desktop"))
            .unwrap()
            .already_applied
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&plan.destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn migration_uses_only_matching_saved_identity_and_never_overwrites_other_configs() {
    let root = TestKits::new();
    let source = root.0.join("portal.toml");
    std::fs::write(&source, "name='old'\nworkspace='./workspace'").unwrap();
    let launch = root.0.join(".portal-launch.json");
    std::fs::write(&launch, json!({"arguments": ["--config", source], "name":"from-launch", "environment": {"PORTAL_CONNECT_LINK":"http://localhost:1234/being/?token=secret"}}).to_string()).unwrap();
    let data = root.0.join("central");
    let plan = plan_migration(&source, &data, None).unwrap();
    plan.apply().unwrap();
    let migrated = crate::config::PortalConfig::load(plan.destination.to_str().unwrap()).unwrap();
    assert_eq!(migrated.name, "from-launch");
    let before = std::fs::read(&plan.destination).unwrap();
    std::fs::write(&source, "name='different'\nworkspace='./different'").unwrap();
    assert!(plan_migration(&source, &data, None).is_err());
    assert_eq!(std::fs::read(&plan.destination).unwrap(), before);
    assert!(plan_migration(&source, &data, Some("../../escape")).is_err());
    std::fs::write(
        &launch,
        json!({"arguments": ["--config", root.0.join("another.toml")]}).to_string(),
    )
    .unwrap();
    assert!(plan_migration(&source, &data, Some("other")).is_err());
}

#[test]
fn a_destination_created_after_planning_is_not_replaced() {
    let root = TestKits::new();
    let source = root.0.join("portal.toml");
    std::fs::write(&source, "name='source'").unwrap();
    let data = root.0.join("data");
    let plan = plan_migration(&source, &data, None).unwrap();
    std::fs::create_dir(&data).unwrap();
    std::fs::write(&plan.destination, "name='other'").unwrap();
    assert!(plan.apply().is_err());
    assert_eq!(
        std::fs::read_to_string(&plan.destination).unwrap(),
        "name='other'"
    );
    assert_eq!(
        std::fs::read_dir(&data).unwrap().count(),
        1,
        "temporary copy is cleaned"
    );
}

#[test]
fn invalid_migration_errors_do_not_echo_configuration_secrets() {
    let root = TestKits::new();
    let source = root.0.join("portal.toml");
    std::fs::write(&source, "connect='unterminated-private-token").unwrap();
    let error = plan_migration(&source, &root.0.join("data"), None).unwrap_err();
    assert!(!format!("{error:#}").contains("private-token"));
}
