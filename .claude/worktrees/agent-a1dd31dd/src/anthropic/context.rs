use super::types::{ContentBlock, Message};

/// context compaction の閾値（推定トークン数）
const COMPACTION_THRESHOLD: u64 = 150_000;
/// 保持する直近ターン数（user + assistant の往復）
const KEEP_RECENT_TURNS: usize = 6;

/// メッセージ履歴のトークン数を概算し、閾値を超えたら中間ターンを圧縮する。
/// 最初の user メッセージと直近 N ターンは保持する。
pub fn maybe_compact_context(messages: &mut [Message]) {
    let estimated = estimate_tokens(messages);
    if estimated < COMPACTION_THRESHOLD {
        return;
    }

    tracing::info!(
        "Context compaction triggered: ~{} tokens (threshold: {})",
        estimated,
        COMPACTION_THRESHOLD
    );

    compact_middle_turns(messages);

    let after = estimate_tokens(messages);
    tracing::info!(
        "Context compacted: ~{} → ~{} tokens",
        estimated,
        after
    );
}

/// 文字数ベースの簡易トークン推定
/// 日本語: ~1.5 トークン/文字、英語: ~0.25 トークン/文字
/// 安全のため 0.5 トークン/文字で概算
fn estimate_tokens(messages: &[Message]) -> u64 {
    let total_chars: usize = messages.iter().map(message_char_count).sum();
    (total_chars as f64 * 0.5) as u64
}

fn message_char_count(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::ToolUse { input, .. } => {
                serde_json::to_string(input).unwrap_or_default().len()
            }
            ContentBlock::ToolResult { content, .. } => match content {
                super::types::ToolResultContent::Text(t) => t.len(),
                super::types::ToolResultContent::Blocks(blocks) => {
                    blocks.iter().map(|b| match b {
                        super::types::ToolResultBlock::Text { text } => text.len(),
                    }).sum()
                }
            },
        })
        .sum()
}

/// 中間ターンのツール結果を要約に圧縮する
fn compact_middle_turns(messages: &mut [Message]) {
    if messages.len() <= KEEP_RECENT_TURNS * 2 + 1 {
        // 十分短い: 圧縮不要
        return;
    }

    // 最初の user メッセージ (index 0) は保持
    // 直近 KEEP_RECENT_TURNS * 2 メッセージは保持
    // 間のメッセージのツール結果を要約に置換
    let keep_from = messages.len().saturating_sub(KEEP_RECENT_TURNS * 2);

    for msg in messages[1..keep_from].iter_mut() {
        for block in msg.content.iter_mut() {
            match block {
                ContentBlock::ToolResult { content, .. } => {
                    let original_len = match content {
                        super::types::ToolResultContent::Text(t) => t.len(),
                        super::types::ToolResultContent::Blocks(blocks) => {
                            blocks.iter().map(|b| match b {
                                super::types::ToolResultBlock::Text { text } => text.len(),
                            }).sum()
                        }
                    };
                    if original_len > 500 {
                        // 長いツール結果を要約に置き換え
                        let summary = match content {
                            super::types::ToolResultContent::Text(t) => {
                                let preview = truncate_at_char_boundary(t, 200);
                                format!(
                                    "[compacted: {} bytes → preview] {}...",
                                    original_len, preview
                                )
                            }
                            super::types::ToolResultContent::Blocks(_) => {
                                format!("[compacted: {} bytes]", original_len)
                            }
                        };
                        *content = super::types::ToolResultContent::Text(summary);
                    }
                }
                ContentBlock::Text { text } => {
                    // assistant のテキスト応答: 思考過程を保持（短縮はしない）
                    // ただし極端に長い場合は要約
                    if text.len() > 5000 {
                        let preview = truncate_at_char_boundary(text, 1000);
                        *text = format!(
                            "[compacted: {} chars → preview] {}...",
                            text.len(),
                            preview
                        );
                    }
                }
                ContentBlock::ToolUse { .. } => {
                    // ToolUse は保持（LLM が自身の呼び出しを認識するため）
                }
            }
        }
    }
}

/// UTF-8 境界を考慮した安全な文字列スライス
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::{Role, ToolResultContent};

    #[test]
    fn test_estimate_tokens_simple() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello world".to_string(), // 11 chars
            }],
        }];
        let tokens = estimate_tokens(&messages);
        assert_eq!(tokens, 5); // 11 * 0.5 = 5.5 → 5
    }

    #[test]
    fn test_compact_short_history() {
        let mut messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Hello".to_string(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Hi".to_string(),
                }],
            },
        ];
        let original_len = messages.len();
        maybe_compact_context(&mut messages);
        assert_eq!(messages.len(), original_len); // 変更なし
    }

    #[test]
    fn test_compact_long_tool_results() {
        let mut messages = Vec::new();

        // 最初の user メッセージ
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Do something".to_string(),
            }],
        });

        // 20 ターン分の会話（中間に長いツール結果）
        for i in 0..20 {
            messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("tool_{}", i),
                    name: "Read".to_string(),
                    input: serde_json::json!({"file_path": "/tmp/test.rs"}),
                }],
            });
            messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: format!("tool_{}", i),
                    content: ToolResultContent::Text("x".repeat(1000)),
                    is_error: None,
                }],
            });
        }

        compact_middle_turns(&mut messages);

        // 中間のツール結果が圧縮されている
        if let ContentBlock::ToolResult { content, .. } = &messages[2].content[0] {
            match content {
                ToolResultContent::Text(t) => assert!(t.contains("[compacted")),
                _ => panic!("Expected compacted text"),
            }
        }

        // 直近ターンは圧縮されていない
        let last_tool_result = &messages[messages.len() - 1];
        if let ContentBlock::ToolResult { content, .. } = &last_tool_result.content[0] {
            match content {
                ToolResultContent::Text(t) => assert!(!t.contains("[compacted")),
                _ => panic!("Expected uncompacted text"),
            }
        }
    }
}
