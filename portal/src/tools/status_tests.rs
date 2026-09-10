use super::*;
use crate::kits::tests::TestKits;
use std::time::Duration;

async fn query(host: &ToolHost) -> Value {
    let response = host.call("portal_status", json!({})).await.unwrap();
    assert_eq!(response["isError"], false);
    serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn status_uses_loaded_config_and_never_returns_credentials() {
    let root = TestKits::new();
    let config_path = root.0.join("portal.toml");
    std::fs::write(&config_path, "name = 'on-disk-name'").unwrap();
    let mut config = PortalConfig {
        name: "loaded-name".into(),
        connect_link: Some("https://example.test/being?token=private-relay-token".into()),
        portal_mcp_token: Some("private-mcp-token".into()),
        kits_enabled: false,
        kits_dir: Some(root.0.to_string_lossy().into_owned()),
        ..PortalConfig::default()
    };
    config.tools.exec = false;
    config.tools.file = false;
    config.tools.custom_tools_enabled = false;
    config.security.exec_allowlist = vec!["command --token=private-command-token".into()];
    let mut runtime = RuntimeStatus::for_test(&config);
    runtime.name = "effective-cli-name".into();
    runtime.config_location = ConfigLocation {
        path: config_path.clone(),
        source: "explicit",
    };
    runtime.config_loaded = true;
    let host = ToolHost::new_with_runtime(&config, runtime);
    assert!(host
        .list_tools()
        .await
        .iter()
        .any(|tool| tool.name == "portal_status"));
    config.name = "not-applied-name".into();
    config.tools.exec = true;
    std::fs::write(config_path, "this config is now invalid TOML").unwrap();
    let status = query(&host).await;
    assert_eq!(status["portal"]["name"], "effective-cli-name");
    assert_eq!(status["portal"]["version"], PORTAL_VERSION);
    assert_eq!(status["connection"]["mode"], "relay");
    assert!(status["connection"]["listener"].is_null());
    assert_eq!(status["config"]["loaded_from_file"], true);
    assert_eq!(status["config"]["source"], "explicit");
    assert_eq!(status["tools"]["exec"], false);
    assert_eq!(status["tools"]["file"], false);
    assert_eq!(status["security"]["mcp_token_configured"], true);
    assert_eq!(status["security"]["exec_allowlist_entries"], 1);
    assert_eq!(status["kits"]["loaded"], 0);
    assert_eq!(status["kits"]["hot_reload"], false);
    assert!(status["kits"]["refresh_interval_seconds"].is_null());
    assert_eq!(status["capabilities"]["kit_reload"]["available"], false);
    assert_eq!(status["capabilities"]["tools_reload"]["kits"], false);
    assert_eq!(
        status["capabilities"]["tools_reload"]["custom_tools"],
        false
    );
    assert_eq!(status["capabilities"]["portal_config_hot_reload"], false);
    assert_eq!(
        status["capabilities"]["custom_tools_config_path_override"],
        false
    );
    let output = status.to_string();
    for secret in [
        "private-relay-token",
        "private-mcp-token",
        "private-command-token",
        "example.test",
    ] {
        assert!(!output.contains(secret));
    }
}

#[tokio::test]
async fn status_cannot_be_shadowed_and_does_not_start_or_refresh_kits() {
    let root = TestKits::new();
    let dir = root.install("portal", "PORTAL_TEST_KIT_TOKEN=private-kit-token");
    let manifest_path = dir.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["tools"][0]["name"] = json!("status");
    std::fs::write(manifest_path, manifest.to_string()).unwrap();
    let host = ToolHost::new(&PortalConfig {
        kits_dir: Some(root.0.to_string_lossy().into_owned()),
        ..PortalConfig::default()
    });
    assert_eq!(
        host.list_tools()
            .await
            .iter()
            .filter(|tool| tool.name == "portal_status")
            .count(),
        1
    );
    root.write_env(&dir, "");
    root.install(
        "late-install",
        "PORTAL_TEST_KIT_TOKEN=another-private-token",
    );
    let changes = host.subscribe_tools_changed();
    for _ in 0..2 {
        let status = query(&host).await;
        assert_eq!(status["kits"]["loaded"], 1);
        assert_eq!(status["kits"]["items"][0]["status"], "not-started");
        assert_eq!(status["kits"]["by_status"]["not-started"], 1);
        assert_eq!(
            status["kits"]["items"][0]["service_authorization"],
            "not-verified-by-portal"
        );
        assert_eq!(status["kits"]["items"][0]["next_action"], "call-tool");
        assert!(status["kits"]["items"][0]["process_id"].is_null());
        assert!(status["kits"]["items"][0]["diagnostics"]["last_call"].is_null());
        assert!(!status.to_string().contains("private-kit-token"));
    }
    assert!(!changes.has_changed().unwrap());
    assert!(!host.restart_requested.load(Ordering::Acquire));
    assert!(host
        .call("portal_status", json!({"reload": true}))
        .await
        .is_err());
}

#[tokio::test]
async fn transport_state_is_live_across_host_clones() {
    let host = ToolHost::new(&PortalConfig {
        kits_enabled: false,
        ..PortalConfig::default()
    });
    let transport = host.clone();
    for (state, expected) in [
        (ConnectionState::Starting, "starting"),
        (ConnectionState::Connecting, "connecting"),
        (ConnectionState::Connected, "connected"),
        (ConnectionState::Retrying, "retrying"),
        (ConnectionState::Invalid, "invalid"),
        (ConnectionState::Listening, "listening"),
    ] {
        transport.set_connection_state(state);
        let status = query(&host).await;
        assert_eq!(status["connection"]["state"], expected);
        assert_eq!(status["portal"]["pid"], std::process::id());
    }
}

#[tokio::test]
async fn kit_status_and_setup_are_readonly_even_after_files_change() {
    let root = TestKits::new();
    let directory = root.install("sample", "PORTAL_TEST_KIT_TOKEN=private-test-token");
    let host = ToolHost::new(&PortalConfig { kits_dir: Some(root.0.to_string_lossy().into()), ..PortalConfig::default() });
    root.write_env(&directory, "");
    let changes = host.subscribe_tools_changed();
    for tool in ["portal_kits_status", "portal_kits_setup"] {
        let result = host.call(tool, json!({"kit":"sample"})).await.unwrap();
        assert!(!result.to_string().contains("needs-configuration"));
    }
    assert!(!changes.has_changed().unwrap());
    let refreshed = host.call("portal_kits_reload", json!({"kit":"sample"})).await.unwrap();
    assert!(refreshed.to_string().contains("needs-configuration"));
}

#[tokio::test]
async fn binary_identity_is_cached_before_the_file_is_replaced() {
    let root = TestKits::new();
    let binary = root.0.join("binary-fixture");
    std::fs::write(&binary, b"abc").unwrap();
    let original_id = binary_id(&binary).unwrap();
    assert_eq!(
        original_id,
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let config = PortalConfig {
        kits_enabled: false,
        ..PortalConfig::default()
    };
    let mut runtime = RuntimeStatus::for_test(&config);
    runtime.executable = Some(binary.clone());
    runtime.build_id = Some(original_id.clone());
    let host = ToolHost::new_with_runtime(&config, runtime);
    std::fs::write(&binary, b"replacement").unwrap();
    assert_ne!(binary_id(&binary).unwrap(), original_id);
    assert_eq!(query(&host).await["portal"]["build_id"], original_id);
    assert!(binary_id(&root.0.join("missing-binary")).is_err());
}

#[tokio::test]
async fn mcp_client_can_discover_and_query_status_without_exec_or_file_tools() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut config = PortalConfig {
        kits_enabled: false,
        ..PortalConfig::default()
    };
    config.tools.exec = false;
    config.tools.file = false;
    config.tools.custom_tools_enabled = false;
    let host = ToolHost::new(&config);
    host.set_connection_state(ConnectionState::Listening);
    let (client, server) = tokio::io::duplex(65536);
    let handler = tokio::spawn(async move {
        crate::handle_connection(server, &host, "test", Some("private-auth-token")).await
    });
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = BufReader::new(reader);
    for (id, method, params) in [
        (1, "auth", json!({"token": "private-auth-token"})),
        (2, "initialize", json!({})),
        (3, "tools/list", json!({})),
        (
            4,
            "tools/call",
            json!({"name": "portal_status", "arguments": {}}),
        ),
    ] {
        let request = format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        );
        writer.write_all(request.as_bytes()).await.unwrap();
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id);
        assert!(response.get("error").is_none_or(Value::is_null));
        if id == 3 {
            let tools = response["result"]["tools"].as_array().unwrap();
            let status = tools
                .iter()
                .find(|tool| tool["name"] == "portal_status")
                .unwrap();
            assert_eq!(status["annotations"]["readOnlyHint"], true);
            assert!(!tools.iter().any(|tool| tool["name"] == "portal_exec"));
        } else if id == 4 {
            let status: Value =
                serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(status["connection"]["state"], "listening");
            assert!(!line.contains("private-auth-token"));
        }
    }
    writer.shutdown().await.unwrap();
    drop(writer);
    tokio::time::timeout(Duration::from_secs(3), handler)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
