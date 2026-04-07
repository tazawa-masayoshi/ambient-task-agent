// agent-harness crate の型を re-export（共通型）
pub use agent_harness::types::{
    ContentBlock, Message, Role, StopReason, ToolDefinition, ToolResultBlock,
    ToolResultContent, Usage,
};

// claude-auth crate の型を re-export（Anthropic API 固有型）
pub use claude_auth::{SystemBlock, ToolChoice};

use serde::Serialize;

// ============================================================================
// Bedrock 呼び出し用パラメータバンドル（harness 型 + API 型を橋渡し）
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

}
