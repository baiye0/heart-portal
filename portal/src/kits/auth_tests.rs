use super::*;
use crate::kits::{loader, manager::KitManager, tests::TestKits};
use serde_json::{json, Value};

fn set_provision(dir: &Path, provision: Value) {
    let path = dir.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    manifest["provision"] = provision;
    std::fs::write(path, manifest.to_string()).unwrap();
}

fn first(root: &TestKits) -> loader::LoadedKit {
    root.scan().kits.remove(0)
}

#[test]
fn auth_methods_are_or_and_fields_within_a_method_are_and() {
    let root = TestKits::new();
    let dir = root.install("issue-tracker", "SERVICE_URL=https://example.test");
    set_provision(
        &dir,
        json!({
            "env": [{"name": "SERVICE_URL", "required": true}],
            "auth": {"methods": [
                {"id": "pat", "provider": "env", "flow": "bearer", "env": ["TEST_SERVICE_PAT"]},
                {"id": "basic", "provider": "env", "flow": "basic", "env": ["TEST_SERVICE_EMAIL", "TEST_SERVICE_TOKEN"]}
            ]}
        }),
    );
    let kit = first(&root);
    assert!(kit.environment.error.is_none());
    assert!(kit.auth.error.is_some());
    assert_eq!(kit.auth.methods[1].missing_env.len(), 2);
    root.write_env(
        &dir,
        "SERVICE_URL=https://example.test\nTEST_SERVICE_PAT=pat-value",
    );
    let kit = first(&root);
    assert!(kit.configuration_error().is_none());
    assert_eq!(kit.auth.methods[0].status, "configured");
    assert_eq!(kit.auth.methods[1].status, "needs-configuration");
    root.write_env(
        &dir,
        "SERVICE_URL=https://example.test\nTEST_SERVICE_EMAIL=test@example.test",
    );
    assert!(first(&root).auth.error.is_some());
    root.write_env(&dir, "SERVICE_URL=https://example.test\nTEST_SERVICE_EMAIL=test@example.test\nTEST_SERVICE_TOKEN=token-value");
    assert!(first(&root).configuration_error().is_none());
    // A usable auth alternative does not bypass a separate global requirement.
    root.write_env(&dir, "TEST_SERVICE_PAT=pat-value");
    assert!(first(&root).environment.error.is_some());
}

#[test]
fn auth_env_requires_local_values_without_host_inheritance() {
    let root = TestKits::new();
    let dir = root.install("inherited", "");
    set_provision(
        &dir,
        json!({"auth": {"methods": [
            {"id": "test-inherited", "provider": "env", "env": ["PATH"]}
        ]}}),
    );
    assert!(first(&root).configuration_error().is_some());
    root.write_env(&dir, "PATH=local-value");
    assert!(first(&root).configuration_error().is_none());
    root.write_env(&dir, "PATH={{YOUR_PATH}}");
    assert!(first(&root).auth.error.is_some());
}

#[tokio::test]
async fn file_provider_checks_presence_and_detects_rotation_without_exposing_bytes() {
    let root = TestKits::new();
    let dir = root.install("service-account", "");
    set_provision(
        &dir,
        json!({"auth": {"methods": [
            {"id": "account", "provider": "file", "flow": "service_account", "files": ["credentials.json"]}
        ]}}),
    );
    assert_eq!(
        first(&root).auth.methods[0].missing_files,
        vec!["credentials.json"]
    );
    std::fs::write(dir.join("credentials.json"), "").unwrap();
    assert!(first(&root).auth.error.is_some());
    std::fs::write(dir.join("credentials.json"), "private-content-1").unwrap();
    let old = first(&root);
    assert!(old.configuration_error().is_none());
    let public = serde_json::to_string(&old.auth).unwrap();
    assert!(!public.contains("private-content") && !public.contains("fingerprints"));
    assert!(!format!("{:?}", old.auth).contains("private-content"));
    let manager = KitManager::new(vec![old.clone()]);
    assert!(!manager.refresh_kits(root.scan(), false).await);
    std::fs::write(dir.join("credentials.json"), "private-content-2").unwrap();
    let rotated = first(&root);
    assert_ne!(old.auth, rotated.auth);
    assert_eq!(public, serde_json::to_string(&rotated.auth).unwrap());
    // Only file contents changed: the automatic scan must retire this generation
    // even though the manifest, .env and public auth status are unchanged.
    let report = manager.refresh_kits_target(root.scan(), false, None).await;
    assert_eq!(report.reloaded, ["service-account"]);
    assert!(!manager.refresh_kits(root.scan(), false).await);
    std::fs::remove_file(dir.join("credentials.json")).unwrap();
    assert!(first(&root).auth.error.is_some());
}

#[tokio::test]
async fn kit_managed_login_remains_callable_and_is_not_reported_as_authorized() {
    for flow in ["oauth", "device_code", "cli", "custom-company-login"] {
        let root = TestKits::new();
        let dir = root.install("login-kit", "PORTAL_TEST_KIT_TOKEN=fixture-only");
        set_provision(
            &dir,
            json!({"auth": {"methods": [
                {"id": "login", "provider": "kit", "flow": flow, "tools": ["ping"], "instructions": "Use the kit login tool"}
            ]}}),
        );
        let kit = first(&root);
        assert!(kit.configuration_error().is_none());
        assert_eq!(kit.auth.methods[0].status, "managed-by-kit");
        assert_eq!(kit.auth.methods[0].tools, vec!["login_kit_ping"]);
        let manager = KitManager::new(vec![kit]);
        assert_eq!(manager.list_healthy_tools().await.len(), 1);
        manager
            .call_tool("login-kit", "ping", json!({}))
            .await
            .unwrap();
        manager.shutdown().await;
    }
}

#[test]
fn future_and_invalid_auth_cannot_silently_pass_as_configured() {
    let root = TestKits::new();
    let dir = root.install("future", "");
    for auth in [
        json!({"version": 99, "methods": []}),
        json!({"methods": []}),
        json!({"methods": [{"id": "new", "provider": "future-provider"}]}),
        json!({"methods": [{"id": "empty", "provider": "env"}]}),
        json!({"methods": [{"id": "empty", "provider": "file"}]}),
        json!({"methods": [{"id": "empty", "provider": "kit"}]}),
        json!({"methods": [{"id": "login", "provider": "kit", "tools": ["missing"]}]}),
        json!({"methods": [{"id": "login", "provider": "kit", "url": "javascript:bad"}]}),
        json!({"methods": [{"id": "same", "provider": "kit", "tools": ["ping"]}, {"id": "same", "provider": "kit", "tools": ["ping"]}]}),
    ] {
        set_provision(&dir, json!({"auth": auth}));
        assert!(first(&root).auth.error.is_some());
    }
    set_provision(
        &dir,
        json!({"auth": {"methods": [
            {"id": "future", "provider": "future-provider"},
            {"id": "fallback", "provider": "kit", "tools": ["ping"]}
        ]}}),
    );
    let kit = first(&root);
    assert!(kit.configuration_error().is_none());
    assert_eq!(kit.auth.methods[0].status, "unsupported");
    set_provision(
        &dir,
        json!({"auth": {"required": false, "methods": [
            {"id": "optional", "provider": "env", "env": ["TEST_OPTIONAL_AUTH"]}
        ]}}),
    );
    assert!(first(&root).configuration_error().is_none());
}

#[tokio::test]
async fn setup_preserves_grove_metadata_but_does_not_return_defaults_or_execute_commands() {
    let root = TestKits::new();
    let dir = root.install("setup", "");
    set_provision(
        &dir,
        json!({
            "runtime": {"name": "node", "version": ">=18"},
            "install": "touch must-not-exist", "post_install": "touch also-must-not-exist",
            "deps": [{"name": "some-sdk", "type": "npm", "install_hint": "npm install", "required": true}],
            "instructions": "Read the local README",
            "future_extension": {"secret": "extension-private-value"},
            "env": [{"name": "TEST_SETUP_DEFAULT", "default": "private-default"}]
        }),
    );
    let kit = first(&root);
    assert_eq!(
        kit.manifest.provision.as_ref().unwrap().extensions["future_extension"]["secret"],
        "extension-private-value"
    );
    let manager = KitManager::new(vec![kit]);
    let setup = manager.setup("setup").await.unwrap();
    assert_eq!(setup["runtime"]["name"], "node");
    assert_eq!(setup["dependencies"][0]["install_hint"], "npm install");
    assert!(!setup.to_string().contains("private-default"));
    assert!(!setup.to_string().contains("extension-private-value"));
    assert!(!dir.join("must-not-exist").exists());
    assert!(manager.setup("unknown").await.is_err());
}

#[tokio::test]
async fn one_kit_reload_and_credential_file_rotation_leave_other_kit_running() {
    let root = TestKits::new();
    let dir_a = root.install("a", "PORTAL_TEST_KIT_TOKEN=a-token");
    root.install("b", "PORTAL_TEST_KIT_TOKEN=b-token");
    set_provision(
        &dir_a,
        json!({"auth": {"methods": [
            {"id": "account", "provider": "file", "files": ["credentials.json"]}
        ]}}),
    );
    std::fs::write(dir_a.join("credentials.json"), "credentials-v1").unwrap();
    let manager = KitManager::new(root.scan().kits);
    let a = manager.call_tool("a", "ping", json!({})).await.unwrap();
    let b = manager.call_tool("b", "ping", json!({})).await.unwrap();
    assert_ne!(a["token"], b["token"]);
    manager
        .refresh_kits_target(root.scan(), true, Some("a"))
        .await;
    let reloaded_a = manager.call_tool("a", "ping", json!({})).await.unwrap();
    assert_ne!(reloaded_a["pid"], a["pid"]);
    assert_eq!(
        manager.call_tool("b", "ping", json!({})).await.unwrap()["pid"],
        b["pid"]
    );
    std::fs::write(dir_a.join("credentials.json"), "credentials-v2").unwrap();
    assert!(manager.refresh_kits(root.scan(), false).await);
    assert_ne!(
        manager.call_tool("a", "ping", json!({})).await.unwrap()["pid"],
        reloaded_a["pid"]
    );
    assert_eq!(
        manager.call_tool("b", "ping", json!({})).await.unwrap()["pid"],
        b["pid"]
    );
    manager.shutdown().await;
}

#[test]
fn grove_platform_aliases_are_compatible() {
    let root = TestKits::new();
    let dir = root.install("cross-platform", "");
    set_provision(&dir, json!({"platforms": ["macos", "linux", "win32"]}));
    assert_eq!(root.scan().kits.len(), 1);
    set_provision(&dir, json!({"platforms": ["unsupported-os"]}));
    assert!(root.scan().kits.is_empty());
}

#[test]
fn external_credential_paths_are_not_observed() {
    let root = TestKits::new();
    let dir = root.install("outside", "");
    let external = root.0.join("credential");
    std::fs::write(&external, "private-v1").unwrap();
    for file in [
        "../credential".to_string(),
        external.to_string_lossy().into_owned(),
        "~/credential".into(),
    ] {
        set_provision(
            &dir,
            json!({"auth":{"methods":[{"id":"file", "provider":"file", "files":[file]}]}}),
        );
        let before = first(&root);
        assert!(before.configuration_error().is_some());
        std::fs::write(&external, "private-v2").unwrap();
        assert_eq!(before.auth, first(&root).auth);
    }
}

#[test]
fn auth_urls_require_tls_except_for_loopback() {
    let root = TestKits::new();
    let dir = root.install("login", "");
    for (url, allowed) in [
        ("https://example.test/login", true),
        ("http://localhost:1234/login", true),
        ("http://127.0.0.1:1234/login", true),
        ("http://[::1]:1234/login", true),
        ("http://localhost.example.test/login", false),
        ("http://example.test/login", false),
    ] {
        set_provision(
            &dir,
            json!({"auth":{"methods":[{"id":"login", "provider":"kit", "url":url}]}}),
        );
        assert_eq!(first(&root).auth.error.is_none(), allowed, "{url}");
        assert_eq!(first(&root).auth.methods[0].url.is_some(), allowed, "{url}");
    }
}
