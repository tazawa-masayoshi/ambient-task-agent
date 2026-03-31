//! Bedrock Converse API ↔ 内部型 (types.rs) の変換レイヤー

use std::collections::HashMap;

use aws_sdk_bedrockruntime::types as br;
use aws_smithy_types::Document;

use super::types;

// ============================================================================
// serde_json::Value ↔ aws_smithy_types::Document 変換
// ============================================================================

pub fn serde_value_to_document(value: serde_json::Value) -> Document {
    match value {
        serde_json::Value::Null => Document::Null,
        serde_json::Value::Bool(b) => Document::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Document::Number(aws_smithy_types::Number::NegInt(i))
            } else if let Some(u) = n.as_u64() {
                Document::Number(aws_smithy_types::Number::PosInt(u))
            } else if let Some(f) = n.as_f64() {
                Document::Number(aws_smithy_types::Number::Float(f))
            } else {
                Document::Null
            }
        }
        serde_json::Value::String(s) => Document::String(s),
        serde_json::Value::Array(arr) => {
            Document::Array(arr.into_iter().map(serde_value_to_document).collect())
        }
        serde_json::Value::Object(map) => {
            let hm: HashMap<String, Document> = map
                .into_iter()
                .map(|(k, v)| (k, serde_value_to_document(v)))
                .collect();
            Document::Object(hm)
        }
    }
}

#[allow(dead_code)] // Bedrock レスポンスの Document → JSON 変換で使用予定
pub fn document_to_serde_value(doc: Document) -> serde_json::Value {
    match doc {
        Document::Null => serde_json::Value::Null,
        Document::Bool(b) => serde_json::Value::Bool(b),
        Document::Number(n) => match n {
            aws_smithy_types::Number::PosInt(u) => serde_json::json!(u),
            aws_smithy_types::Number::NegInt(i) => serde_json::json!(i),
            aws_smithy_types::Number::Float(f) => {
                serde_json::Value::Number(serde_json::Number::from_f64(f).unwrap_or_else(|| {
                    serde_json::Number::from_f64(0.0).unwrap()
                }))
            }
        },
        Document::String(s) => serde_json::Value::String(s),
        Document::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(document_to_serde_value).collect())
        }
        Document::Object(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .into_iter()
                .map(|(k, v)| (k, document_to_serde_value(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
    }
}

// ============================================================================
// システムプロンプト変換
// ============================================================================

pub fn convert_system_blocks(blocks: &[types::SystemBlock]) -> Vec<br::SystemContentBlock> {
    blocks
        .iter()
        .map(|b| br::SystemContentBlock::Text(b.text.clone()))
        .collect()
}

// ============================================================================
// メッセージ変換
// ============================================================================

/// 内部 Message → Bedrock Message に変換。
/// Bedrock では連続する同一ロールのメッセージは許可されないため、
/// ToolResult は直前の user メッセージに統合する。
pub fn convert_messages(messages: &[types::Message]) -> Vec<br::Message> {
    let mut result: Vec<br::Message> = Vec::new();

    for msg in messages {
        let role = match msg.role {
            types::Role::User => br::ConversationRole::User,
            types::Role::Assistant => br::ConversationRole::Assistant,
        };

        let content_blocks: Vec<br::ContentBlock> = msg
            .content
            .iter()
            .map(convert_content_block)
            .collect();

        if content_blocks.is_empty() {
            continue;
        }

        // Bedrock は連続する同一ロールを拒否するため統合
        if let Some(last) = result.last_mut() {
            if last.role() == &role {
                // 既存メッセージに content を追加
                let mut existing: Vec<br::ContentBlock> = last.content().to_vec();
                existing.extend(content_blocks);
                *last = br::Message::builder()
                    .role(role)
                    .set_content(Some(existing))
                    .build()
                    .expect("valid message");
                continue;
            }
        }

        let br_msg = br::Message::builder()
            .role(role)
            .set_content(Some(content_blocks))
            .build()
            .expect("valid message");
        result.push(br_msg);
    }

    result
}

fn convert_content_block(block: &types::ContentBlock) -> br::ContentBlock {
    match block {
        types::ContentBlock::Text { text } => br::ContentBlock::Text(text.clone()),
        types::ContentBlock::ToolUse { id, name, input } => {
            let tool_use = br::ToolUseBlock::builder()
                .tool_use_id(id.clone())
                .name(name.clone())
                .input(serde_value_to_document(input.clone()))
                .build()
                .expect("valid tool use");
            br::ContentBlock::ToolUse(tool_use)
        }
        types::ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let text = match content {
                types::ToolResultContent::Text(t) => t.clone(),
                types::ToolResultContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| match b {
                        types::ToolResultBlock::Text { text } => text.as_str(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };

            let status = if is_error.unwrap_or(false) {
                br::ToolResultStatus::Error
            } else {
                br::ToolResultStatus::Success
            };

            let tool_result = br::ToolResultBlock::builder()
                .tool_use_id(tool_use_id.clone())
                .content(br::ToolResultContentBlock::Text(text))
                .status(status)
                .build()
                .expect("valid tool result");
            br::ContentBlock::ToolResult(tool_result)
        }
    }
}

// ============================================================================
// ツール定義変換
// ============================================================================

pub fn convert_tools(
    tools: &[types::ToolDefinition],
    tool_choice: &Option<types::ToolChoice>,
) -> Option<br::ToolConfiguration> {
    if tools.is_empty() {
        return None;
    }

    let br_tools: Vec<br::Tool> = tools
        .iter()
        .map(|t| {
            let spec = br::ToolSpecification::builder()
                .name(&t.name)
                .description(&t.description)
                .input_schema(br::ToolInputSchema::Json(serde_value_to_document(
                    t.input_schema.clone(),
                )))
                .build()
                .expect("valid tool spec");
            br::Tool::ToolSpec(spec)
        })
        .collect();

    let br_choice = match tool_choice {
        Some(types::ToolChoice::Auto) => {
            Some(br::ToolChoice::Auto(br::AutoToolChoice::builder().build()))
        }
        Some(types::ToolChoice::None) | None => None,
    };

    let mut builder = br::ToolConfiguration::builder().set_tools(Some(br_tools));
    if let Some(choice) = br_choice {
        builder = builder.tool_choice(choice);
    }

    Some(builder.build().expect("valid tool config"))
}

// ============================================================================
// StopReason 変換
// ============================================================================

pub fn map_stop_reason(reason: &br::StopReason) -> types::StopReason {
    match reason {
        br::StopReason::EndTurn => types::StopReason::EndTurn,
        br::StopReason::ToolUse => types::StopReason::ToolUse,
        br::StopReason::MaxTokens | br::StopReason::ModelContextWindowExceeded => {
            types::StopReason::MaxTokens
        }
        br::StopReason::StopSequence => types::StopReason::StopSequence,
        _ => types::StopReason::EndTurn,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serde_value_to_document_roundtrip() {
        let value = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "count": {"type": "integer"}
            },
            "required": ["name"]
        });
        let doc = serde_value_to_document(value.clone());
        let back = document_to_serde_value(doc);
        assert_eq!(value, back);
    }

    #[test]
    fn test_convert_system_blocks() {
        let blocks = vec![types::SystemBlock {
            block_type: "text".to_string(),
            text: "You are helpful.".to_string(),
            cache_control: None,
        }];
        let result = convert_system_blocks(&blocks);
        assert_eq!(result.len(), 1);
        match &result[0] {
            br::SystemContentBlock::Text(t) => assert_eq!(t, "You are helpful."),
            _ => panic!("Expected Text"),
        }
    }

    #[test]
    fn test_convert_messages_merges_same_role() {
        let messages = vec![
            types::Message {
                role: types::Role::User,
                content: vec![types::ContentBlock::Text {
                    text: "Hello".to_string(),
                }],
            },
            // tool_result は user ロール → 前の user と統合される
            types::Message {
                role: types::Role::User,
                content: vec![types::ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: types::ToolResultContent::Text("result".to_string()),
                    is_error: None,
                }],
            },
        ];
        let result = convert_messages(&messages);
        assert_eq!(result.len(), 1); // 統合されて1メッセージに
        assert_eq!(result[0].content().len(), 2);
    }

    #[test]
    fn test_convert_tools() {
        let tools = vec![types::ToolDefinition {
            name: "Read".to_string(),
            description: "Read a file".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"}
                },
                "required": ["file_path"]
            }),
        }];
        let result = convert_tools(&tools, &Some(types::ToolChoice::Auto));
        assert!(result.is_some());
        let config = result.unwrap();
        assert_eq!(config.tools().len(), 1);
    }

    #[test]
    fn test_map_stop_reason() {
        assert!(matches!(
            map_stop_reason(&br::StopReason::EndTurn),
            types::StopReason::EndTurn
        ));
        assert!(matches!(
            map_stop_reason(&br::StopReason::ToolUse),
            types::StopReason::ToolUse
        ));
        assert!(matches!(
            map_stop_reason(&br::StopReason::MaxTokens),
            types::StopReason::MaxTokens
        ));
        assert!(matches!(
            map_stop_reason(&br::StopReason::ModelContextWindowExceeded),
            types::StopReason::MaxTokens
        ));
    }
}
