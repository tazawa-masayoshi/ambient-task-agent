use super::types::{ContentBlock, Message, Role};

/// Stage 1 (Micro-compact): ツール出力を要約に圧縮
const MICRO_COMPACT_THRESHOLD: u64 = 120_000;
/// Stage 3 (Truncate): 古いターンを強制削除
const HARD_TRUNCATE_THRESHOLD: u64 = 180_000;
/// 保持する直近ターン数（user + assistant の往復）
const KEEP_RECENT_TURNS: usize = 6;

/// 3段階コンテキスト圧縮（claw-code パターン）
/// Stage 1: Micro-compact — ツール出力を要約に圧縮（LLM 不要）
/// Stage 2: Auto-compact — LLM による要約（将来実装）
/// Stage 3: Truncate — 古いターンを強制削除（最終手段）
pub fn maybe_compact_context(messages: &mut Vec<Message>) {
    let estimated = estimate_tokens(messages);

    if estimated < MICRO_COMPACT_THRESHOLD {
        return;
    }

    // Stage 1: Micro-compact（ツール出力の圧縮）
    tracing::info!(
        "Stage 1 (Micro-compact): ~{} tokens (threshold: {})",
        estimated,
        MICRO_COMPACT_THRESHOLD
    );
    compact_middle_turns(messages);
    let after_micro = estimate_tokens(messages);
    tracing::info!("Stage 1 done: ~{} → ~{} tokens", estimated, after_micro);

    if after_micro < HARD_TRUNCATE_THRESHOLD {
        return;
    }

    // Stage 3: Hard truncate（古いターン削除）
    tracing::warn!(
        "Stage 3 (Truncate): ~{} tokens exceeds {} — removing old turns",
        after_micro,
        HARD_TRUNCATE_THRESHOLD
    );
    hard_truncate(messages);
    let after_truncate = estimate_tokens(messages);
    tracing::info!("Stage 3 done: ~{} → ~{} tokens", after_micro, after_truncate);
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

/// 中間ターンのツール結果を要約に圧縮し、統計サマリを挿入する
fn compact_middle_turns(messages: &mut [Message]) {
    if messages.len() <= KEEP_RECENT_TURNS * 2 + 1 {
        return;
    }

    let keep_from = messages.len().saturating_sub(KEEP_RECENT_TURNS * 2);

    // 圧縮対象のメッセージから統計を収集
    let stats = collect_compaction_stats(&messages[1..keep_from]);

    // 中間メッセージのツール結果を圧縮
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
                    if text.len() > 5000 {
                        let preview = truncate_at_char_boundary(text, 1000);
                        *text = format!(
                            "[compacted: {} chars → preview] {}...",
                            text.len(),
                            preview
                        );
                    }
                }
                ContentBlock::ToolUse { .. } => {}
            }
        }
    }

    // 統計サマリを最初の user メッセージの後に挿入
    if !stats.is_empty() {
        let summary_block = ContentBlock::Text {
            text: format!("<compaction-summary>\n{}\n</compaction-summary>", stats),
        };
        // index 1 に挿入（最初の user メッセージの直後）
        if messages.len() > 1 {
            messages[1].content.insert(0, summary_block);
        }
    }
}

/// 圧縮対象のメッセージから統計情報を収集
fn collect_compaction_stats(messages: &[Message]) -> String {
    let mut tool_counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut files_referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut user_count = 0u32;
    let mut assistant_count = 0u32;

    for msg in messages {
        match msg.role {
            Role::User => user_count += 1,
            Role::Assistant => assistant_count += 1,
        }
        for block in &msg.content {
            if let ContentBlock::ToolUse { name, input, .. } = block {
                    *tool_counts.entry(name.clone()).or_default() += 1;
                    if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
                        files_referenced.insert(path.to_string());
                    }
                    if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                        files_referenced.insert(path.to_string());
                    }
            }
        }
    }

    let mut parts = Vec::new();

    parts.push(format!(
        "圧縮済み: {} ターン（user: {}, assistant: {}）",
        user_count + assistant_count,
        user_count,
        assistant_count,
    ));

    if !tool_counts.is_empty() {
        let mut tools: Vec<_> = tool_counts.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1));
        let tool_summary: Vec<String> = tools
            .iter()
            .take(10)
            .map(|(name, count)| format!("{}({})", name, count))
            .collect();
        parts.push(format!("使用ツール: {}", tool_summary.join(", ")));
    }

    if !files_referenced.is_empty() {
        let mut files: Vec<_> = files_referenced.into_iter().collect();
        files.sort();
        if files.len() > 10 {
            let total = files.len();
            files.truncate(10);
            parts.push(format!("参照ファイル: {} ...他{}件", files.join(", "), total - 10));
        } else {
            parts.push(format!("参照ファイル: {}", files.join(", ")));
        }
    }

    parts.join("\n")
}

/// Stage 3: 古いターンを強制削除。最初の user メッセージ + 直近ターンのみ保持。
/// drain で in-place 削除（clone を回避）。
fn hard_truncate(messages: &mut Vec<Message>) {
    if messages.len() <= KEEP_RECENT_TURNS * 2 + 2 {
        return;
    }

    let keep_from = messages.len().saturating_sub(KEEP_RECENT_TURNS * 2);

    // index 0 は最初の user メッセージ（常に保持）
    // index 1 に compaction-summary があればそれも保持
    let has_summary = messages.get(1).is_some_and(|m| {
        m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("<compaction-summary>")))
    });

    let remove_start = if has_summary { 2 } else { 1 };
    let removed = keep_from - remove_start;
    messages.drain(remove_start..keep_from);
    tracing::info!("Hard truncate: removed {} messages, kept {}", removed, messages.len());
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
