pub use crate::protocol::{JsonRpcRequest, JsonRpcResponse};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notifications_omit_id_instead_of_serializing_null() {
        let value = serde_json::to_value(JsonRpcRequest::notification(
            "notifications/initialized",
            serde_json::json!({}),
        ))
        .unwrap();
        assert!(value.get("id").is_none());
    }
}

/// MCP tool metadata returned by `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, alias = "inputSchema")]
    pub input_schema: Value,
}
