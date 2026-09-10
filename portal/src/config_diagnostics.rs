//! Compatibility diagnostics: keep existing parsing/precedence, expose ignored
//! settings without retaining or logging their values.
use super::RawConfig;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConfigWarning {
    pub code: &'static str,
    pub field: String,
    pub message: &'static str,
}

const MAX_WARNINGS: usize = 32;

fn field_name(key: &str) -> &str {
    // Quoted TOML keys can contain URLs, control characters or arbitrarily long
    // text. Do not echo these into diagnostics or inject extra log lines.
    if !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        key
    } else {
        "<unrecognized-field>"
    }
}

pub(super) fn collect(raw: &RawConfig) -> Vec<ConfigWarning> {
    let mut warnings = Vec::new();
    let mut ignored = |prefix: &str, key: &str| {
        if warnings.len() > MAX_WARNINGS {
            return;
        }
        let custom = prefix.is_empty() && key == "custom_tools";
        warnings.push(ConfigWarning {
            code: if custom { "unsupported-custom-tools-section" } else { "unknown-field" },
            field: format!("{prefix}{}", field_name(key)),
            message: if custom {
                "custom_tools is ignored, including config_path. Use tools.custom_tools_enabled; definitions are loaded from <workspace>/tools/mcp.toml. See portal_status.config.custom_tools_config for the effective path."
            } else {
                "This field is not supported and is ignored. Check its spelling and section; it has no effect on the running configuration."
            },
        });
    };
    for key in raw.ignored_fields.keys() {
        ignored("", key);
    }
    if let Some(tools) = &raw.tools {
        for key in tools.ignored_fields.keys() {
            ignored("tools.", key);
        }
    }
    if let Some(security) = &raw.security {
        for key in security.ignored_fields.keys() {
            ignored("security.", key);
        }
    }
    if raw.bind.is_some() {
        for (present, field) in [
            (raw.bind_host.is_some(), "bind_host"),
            (raw.bind_port.is_some(), "bind_port"),
        ] {
            if present {
                warnings.push(ConfigWarning {
                    code: "shadowed-field",
                    field: field.into(),
                    message: "The flat bind setting takes precedence over this field.",
                });
            }
        }
    }
    if raw.workspace.is_some()
        && raw
            .security
            .as_ref()
            .is_some_and(|security| security.workspace_root.is_some())
    {
        warnings.push(ConfigWarning {
            code: "shadowed-field",
            field: "security.workspace_root".into(),
            message: "The flat workspace setting takes precedence over security.workspace_root.",
        });
    }
    if warnings.len() > MAX_WARNINGS {
        warnings.truncate(MAX_WARNINGS);
        warnings.push(ConfigWarning { code: "warnings-truncated", field: "<additional-fields>".into(),
            message: "Additional configuration warnings were omitted. Fix the reported fields and validate again." });
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::PortalConfig, kits::tests::TestKits};

    #[test]
    fn ignored_and_shadowed_settings_warn_without_exposing_values_or_activating_paths() {
        let root = TestKits::new();
        let config_path = root.0.join("portal.toml");
        std::fs::write(
            &config_path,
            r#"
name = "configured"
workspace = "active-workspace"
bind = "127.0.0.1:9100"
bind_port = 9000
"secret/url\nkey" = "private-root-value"
[custom_tools]
config_path = "private-external-path"
token = "private-token-value"
[tools]
exec = false
custom_tools_enabled = false
exce = true
[security]
workspace_root = "ignored-workspace"
exec_allowlist = ["echo"]
exec_allow_list = ["private-command-value"]
"#,
        )
        .unwrap();
        let config = PortalConfig::load(config_path.to_str().unwrap()).unwrap();
        assert!(!config.tools.exec && !config.tools.custom_tools_enabled);
        assert_eq!(config.bind_port, 9100);
        assert_eq!(
            config.security.workspace_root,
            root.0.join("active-workspace")
        );
        assert_eq!(config.security.exec_allowlist, vec!["echo"]);
        assert!(!root.0.join("active-workspace").exists());
        for field in [
            "custom_tools",
            "tools.exce",
            "security.exec_allow_list",
            "bind_port",
            "security.workspace_root",
            "<unrecognized-field>",
        ] {
            assert!(
                config.warnings.iter().any(|warning| warning.field == field),
                "{field}"
            );
        }
        let output = serde_json::to_string(&config.warnings).unwrap();
        assert!(!output.contains("private-") && !output.contains("secret/url"));
    }

    #[test]
    fn correct_config_is_quiet_and_warning_output_is_bounded() {
        let raw: RawConfig =
            toml::from_str("name='test'\n[tools]\nexec=false\n[security]\nmax_file_size=4096\n")
                .unwrap();
        assert!(collect(&raw).is_empty());
        let source: String = (0..100)
            .map(|index| format!("unknown_{index}='private-value'\n"))
            .collect();
        let raw: RawConfig = toml::from_str(&source).unwrap();
        let warnings = collect(&raw);
        assert_eq!(warnings.len(), MAX_WARNINGS + 1);
        assert_eq!(warnings.last().unwrap().code, "warnings-truncated");
    }
}
