// agent-harness crate の型を re-export（共通型）
pub use agent_harness::types::{
    ContentBlock, Message, Role, StopReason, ToolDefinition, ToolResultBlock,
    ToolResultContent, Usage,
};

use serde::{Deserialize, Serialize};

// ============================================================================
// Anthropic API 固有の型（harness には含まない）
// ============================================================================

#[derive(Debug, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<SystemBlock>>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub cache_type: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
pub enum ToolChoice {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "none")]
    None,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct MessagesResponse {
    pub id: String,
    pub model: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub usage: ApiUsage,
}

/// API レスポンスの Usage（cache フィールドが Option）
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct ApiUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

impl ApiUsage {
    /// harness の Usage に変換
    #[allow(dead_code)]
    pub fn to_harness_usage(&self) -> Usage {
        Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens.unwrap_or(0),
            cache_read_input_tokens: self.cache_read_input_tokens.unwrap_or(0),
        }
    }
}

/// 後方互換: AggregatedUsage は harness の Usage と同一
pub type AggregatedUsage = Usage;

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_block_text_serde() {
        let block = ContentBlock::Text {
            text: "Hello".to_string(),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        match parsed {
            ContentBlock::Text { text } => assert_eq!(text, "Hello"),
            _ => panic!("Expected Text"),
        }
    }

    #[test]
    fn test_content_block_tool_use_serde() {
        let json = r#"{"type":"tool_use","id":"toolu_123","name":"Read","input":{"file_path":"/tmp/test.rs"}}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        match block {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_123");
                assert_eq!(name, "Read");
                assert_eq!(input["file_path"], "/tmp/test.rs");
            }
            _ => panic!("Expected ToolUse"),
        }
    }

    #[test]
    fn test_stop_reason_deserialize() {
        let json = r#""end_turn""#;
        let reason: StopReason = serde_json::from_str(json).unwrap();
        assert_eq!(reason, StopReason::EndTurn);
    }

    #[test]
    fn test_api_usage_to_harness() {
        let api = ApiUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: Some(10),
            cache_read_input_tokens: None,
        };
        let h = api.to_harness_usage();
        assert_eq!(h.input_tokens, 100);
        assert_eq!(h.output_tokens, 50);
        assert_eq!(h.cache_creation_input_tokens, 10);
        assert_eq!(h.cache_read_input_tokens, 0);
    }
}
