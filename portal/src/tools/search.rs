//! Workspace search — recursive grep under the portal workspace root.

use crate::config::PortalConfig;
use crate::tools::file::{canonical_workspace_root, resolve_existing_path};
use anyhow::Result;
use regex::Regex;
use serde_json::Value;
use std::path::Path;
use tracing::debug;
use walkdir::WalkDir;

/// Search recursively under `workspace_root` for lines matching `pattern` (Rust regex syntax).
pub async fn search(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let pattern = arguments
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?;

    let max_matches = arguments
        .get("max_matches")
        .and_then(super::value_as_u64)
        .unwrap_or(200)
        .min(2000) as usize;

    let path_filter = arguments
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    let root = resolve_existing_path(config, path_filter)?;

    let re = Regex::new(pattern)
        .map_err(|e| anyhow::anyhow!("Invalid regex: {}", e))?;

    // Keep both paths in canonical form. This is especially important on
    // Windows, where canonicalization may use the verbatim-path prefix.
    let workspace = canonical_workspace_root(config)?;
    let max_file = config.security.max_file_size;

    debug!(
        "portal_search: pattern={:?} under {}",
        pattern,
        root.display()
    );

    let matches = tokio::task::spawn_blocking(move || {
        grep_workspace(&workspace, &root, &re, max_matches, max_file)
    })
    .await
    .map_err(|e| anyhow::anyhow!("Search task failed: {}", e))??;

    let text = serde_json::to_string(&matches)?;
    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "match_count": matches.len()
    }))
}

#[derive(serde::Serialize)]
struct GrepMatch {
    path: String,
    line: usize,
    text: String,
}

fn grep_workspace(
    workspace_root: &Path,
    search_root: &Path,
    re: &Regex,
    max_matches: usize,
    max_file_bytes: usize,
) -> Result<Vec<GrepMatch>> {
    let mut out = Vec::new();

    for entry in WalkDir::new(search_root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if out.len() >= max_matches {
            break;
        }

        let path = entry.path();
        // Never follow a nested symlink/reparse point. `follow_links(false)`
        // prevents directory traversal, while this also protects file reads.
        if entry.file_type().is_symlink() || entry.file_type().is_dir() {
            continue;
        }

        let canonical = match path.canonicalize() {
            Ok(path) if path.starts_with(workspace_root) => path,
            _ => continue,
        };

        let meta = match std::fs::metadata(&canonical) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > max_file_bytes as u64 {
            continue;
        }

        let rel = match canonical.strip_prefix(workspace_root) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        let bytes = match std::fs::read(&canonical) {
            Ok(b) => b,
            Err(_) => continue,
        };

        if bytes.contains(&0) {
            continue;
        }

        let text = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };

        for (i, line) in text.lines().enumerate() {
            if out.len() >= max_matches {
                break;
            }
            if re.is_match(line) {
                out.push(GrepMatch {
                    path: rel.clone(),
                    line: i + 1,
                    text: line.chars().take(2000).collect(),
                });
            }
        }
    }

    Ok(out)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn nested_file_symlink_cannot_escape_workspace() {
        let temp = std::env::temp_dir().join(format!(
            "portal-search-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = temp.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("inside.txt"), "search-marker inside").unwrap();

        let outside = temp.join("outside.txt");
        std::fs::write(&outside, "search-marker outside").unwrap();
        symlink(&outside, workspace.join("leak.txt")).unwrap();

        let workspace = workspace.canonicalize().unwrap();
        let re = Regex::new("search-marker").unwrap();
        let matches = grep_workspace(&workspace, &workspace, &re, 10, 1024).unwrap();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "inside.txt");
        assert_eq!(matches[0].text, "search-marker inside");

        std::fs::remove_dir_all(&temp).unwrap();
    }
}
