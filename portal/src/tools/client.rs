//! Client commands are data requests, never shell commands.
use anyhow::{Context, Result};
use std::{future::Future, path::PathBuf, pin::Pin};

pub trait ClientHandler: Send + Sync {
    fn handle_client_command<'a>(
        &'a self,
        verb: &'a str,
        args: &'a str,
        scene_id: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
}

/// Desktop publishes a rotating local capability. Re-read it on every call so
/// a supervised Portal can survive Desktop restarts without retaining a token.
pub struct DesktopClientHandler {
    pub file: PathBuf,
    pub endpoint: String,
}
impl ClientHandler for DesktopClientHandler {
    fn handle_client_command<'a>(
        &'a self,
        verb: &'a str,
        args: &'a str,
        scene_id: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let bytes = tokio::fs::read(&self.file)
                .await
                .context("Desktop client unavailable")?;
            anyhow::ensure!(bytes.len() < 4096, "Invalid Desktop client registration");
            let registration: serde_json::Value = serde_json::from_slice(&bytes)?;
            let port = registration["port"]
                .as_u64()
                .filter(|p| *p > 0 && *p <= 65535)
                .context("Invalid client port")?;
            let token = registration["token"]
                .as_str()
                .context("Missing client token")?;
            let response = reqwest::Client::builder()
                .no_proxy().redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(10)).build()?
                .post(format!("http://127.0.0.1:{port}/command"))
                .bearer_auth(token)
                .json(&serde_json::json!({"verb": verb, "args": args, "sceneId": scene_id, "endpoint": self.endpoint}))
                .send().await.context("Desktop client unavailable")?;
            anyhow::ensure!(
                response.status().is_success(),
                "Desktop client rejected request ({})",
                response.status()
            );
            let value: serde_json::Value = response.json().await?;
            if let Some(error) = value["error"].as_str() {
                anyhow::bail!("{error}");
            }
            Ok(value["text"]
                .as_str()
                .context("Invalid client response")?
                .to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::PortalConfig, tools::ToolHost};
    use serde_json::json;
    use std::sync::Arc;

    struct Echo;
    impl ClientHandler for Echo {
        fn handle_client_command<'a>(
            &'a self,
            verb: &'a str,
            args: &'a str,
            scene: Option<&'a str>,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async move { Ok(format!("{verb}|{args}|{}", scene.unwrap_or("none"))) })
        }
    }

    #[tokio::test]
    async fn dispatches_client_commands_and_preserves_metadata() {
        let config = PortalConfig::default();
        let host = ToolHost::new(&config).with_client_handler(Arc::new(Echo));
        for (command, expected) in [
            ("@context scene-b", "context|scene-b|scene-a"),
            ("  @context\t scene-b  ", "context|scene-b|scene-a"),
            ("@scenes", "scenes||scene-a"),
            ("@context", "context||scene-a"),
            (
                "@unknown ; echo forbidden",
                "unknown|; echo forbidden|scene-a",
            ),
        ] {
            let request = serde_json::from_value(json!({"jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"portal_exec", "arguments":{"command":command,"background":true}, "_meta":{"scene_id":"scene-a"}}})).unwrap();
            let response = crate::handle_request(&request, &host, "test").await;
            assert_eq!(response.result.unwrap()["content"][0]["text"], expected);
        }
    }

    #[tokio::test]
    async fn disabled_client_commands_never_fall_through_to_shell() {
        let mut config = PortalConfig::default();
        config.tools.exec = false;
        let host = ToolHost::new(&config).with_client_handler(Arc::new(Echo));
        assert!(host
            .call("portal_exec", json!({"command":"@scenes"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("disabled"));
    }
    #[tokio::test]
    async fn standalone_and_explicit_shell_preserve_at_syntax() {
        let mut config = PortalConfig::default();
        config.security.workspace_root = std::env::temp_dir();
        #[cfg(unix)]
        let command = "@portal_missing_fixture 2>/dev/null; printf compat-shell";
        #[cfg(windows)]
        let command = "@echo compat-shell";
        let standalone = ToolHost::new(&config);
        let desktop = ToolHost::new(&config).with_client_handler(Arc::new(Echo));
        for (host, arguments) in [
            (&standalone, json!({"command": command})),
            (&desktop, json!({"command": command, "shell": "default"})),
        ] {
            let response = host.call("portal_exec", arguments).await.unwrap();
            assert_eq!(response["isError"], false, "{response}");
            assert!(response["content"][0]["text"].as_str().unwrap().contains("compat-shell"));
        }
        for (host, advertised) in [(&standalone, false), (&desktop, true)] {
            let tool = host.list_builtin_tools().into_iter().find(|t| t.name == "portal_exec").unwrap();
            assert_eq!(tool.description.contains("@context"), advertised);
        }
    }

    #[tokio::test]
    async fn scene_metadata_falls_back_when_higher_priority_metadata_has_no_scene() {
        let host = ToolHost::new(&PortalConfig::default()).with_client_handler(Arc::new(Echo));
        for (params_meta, params_legacy_meta, envelope, expected) in [
            (json!({"scene_id":"first"}), json!({"scene_id":"second"}), json!({"scene_id":"third"}), "first"),
            (json!({"trace":"unrelated"}), json!({"scene_id":"second"}), json!({"scene_id":"third"}), "second"),
            (json!({"scene_id":42}), json!(null), json!({"scene_id":"third"}), "third"),
        ] {
            for envelope_key in ["meta", "_meta"] {
                let mut value = json!({"jsonrpc":"2.0", "id":1, "method":"tools/call",
                    "params":{"name":"portal_exec", "arguments":{"command":"@context"}, "_meta":params_meta, "meta":params_legacy_meta}});
                value[envelope_key] = envelope.clone();
                let request = serde_json::from_value(value).unwrap();
                let response = crate::handle_request(&request, &host, "test").await;
                assert_eq!(response.result.unwrap()["content"][0]["text"], format!("context||{expected}"));
            }
        }
    }

    #[tokio::test]
    async fn desktop_transport_authenticates_and_reloads_registration() {
        use axum::{http::HeaderMap, routing::post, Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/command", post(|headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
                assert_eq!(headers["authorization"], "Bearer fixture-capability");
                assert_eq!(body, json!({"verb":"context", "args":"scene-b", "sceneId":"scene-a", "endpoint":"https://fixture.test/a"}));
                Json(json!({"text":"跨场景历史"}))
            }))).await.unwrap();
        });
        let file =
            std::env::temp_dir().join(format!("portal-client-{}.json", uuid::Uuid::new_v4()));
        tokio::fs::write(
            &file,
            json!({"port":port,"token":"fixture-capability"}).to_string(),
        )
        .await
        .unwrap();
        let handler = DesktopClientHandler {
            file: file.clone(),
            endpoint: "https://fixture.test/a".into(),
        };
        assert_eq!(
            handler
                .handle_client_command("context", "scene-b", Some("scene-a"))
                .await
                .unwrap(),
            "跨场景历史"
        );
        tokio::fs::remove_file(file).await.unwrap();
        assert!(handler
            .handle_client_command("context", "scene-b", None)
            .await
            .unwrap_err()
            .to_string()
            .contains("unavailable"));
        server.abort();
    }
}
