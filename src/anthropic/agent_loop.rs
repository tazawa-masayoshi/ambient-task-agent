use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::claude::ProgressCallback;

use super::context::maybe_compact_context;
use super::llm_client::LlmClient;
use super::mcp::{parse_mcp_tool_name, McpManager};
use super::tool_impls::{execute_tool, ToolExecutionContext, ToolExecutionResult};
use super::types::*;

pub struct AgentLoopConfig {
    pub model: String,
    pub max_tokens_per_turn: u32,
    pub max_turns: u32,
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolDefinition>,
    pub cwd: PathBuf,
    pub timeout_secs: u64,
    pub json_schema: Option<String>,
    pub progress: Option<ProgressCallback>,
    pub mcp_manager: Option<Arc<McpManager>>,
}

pub struct AgentLoopResult {
    pub final_text: String,
    pub total_usage: AggregatedUsage,
    pub turn_count: u32,
}

/// エージェントループ: LLM 呼び出し → ツール実行 → 繰り返し
pub async fn run_agent_loop(
    client: &dyn LlmClient,
    config: AgentLoopConfig,
    initial_prompt: &str,
) -> Result<AgentLoopResult> {
    let mut messages: Vec<Message> = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: initial_prompt.to_string(),
        }],
    }];
    let mut total_usage = AggregatedUsage::default();
    let mut turn_count = 0u32;

    let tool_ctx = ToolExecutionContext {
        cwd: config.cwd.clone(),
        timeout_secs: config.timeout_secs,
    };

    let system = config.system_prompt.as_ref().map(|sp| {
        let mut text = sp.clone();
        // json_schema がある場合は system prompt に埋め込み
        if let Some(ref schema) = config.json_schema {
            text.push_str(&format!(
                "\n\nYou must respond with valid JSON matching this schema:\n{}",
                schema
            ));
        }
        vec![SystemBlock {
            block_type: "text".to_string(),
            text,
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
            }),
        }]
    });

    // json_schema のみ指定、system_prompt なしの場合
    let system = system.or_else(|| {
        config.json_schema.as_ref().map(|schema| {
            vec![SystemBlock {
                block_type: "text".to_string(),
                text: format!(
                    "You must respond with valid JSON matching this schema:\n{}",
                    schema
                ),
                cache_control: None,
            }]
        })
    });

    let has_tools = !config.tools.is_empty();
    let progress_for_stream = config.progress.clone();
    let mut verified = false;

    loop {
        if turn_count >= config.max_turns {
            tracing::info!("Agent loop: max_turns ({}) reached", config.max_turns);
            break;
        }
        turn_count += 1;

        // 1. context compaction チェック
        maybe_compact_context(&mut messages);

        // 2. LLM リクエスト構築
        let request = MessagesRequest {
            model: config.model.clone(),
            max_tokens: config.max_tokens_per_turn,
            system: system.clone(),
            messages: messages.clone(),
            tools: if has_tools {
                Some(config.tools.clone())
            } else {
                None
            },
            tool_choice: if has_tools {
                Some(ToolChoice::Auto)
            } else if config.json_schema.is_some() {
                Some(ToolChoice::None)
            } else {
                None
            },
            stream: true,
        };

        // 3. LLM 呼び出し（ストリーミング）
        let on_tool_use = progress_for_stream.as_ref().map(|cb| {
            let cb = Arc::clone(cb);
            Arc::new(move |name: &str| {
                cb(crate::claude::ProgressEvent::ToolUse(name.to_string()));
            }) as Arc<dyn Fn(&str) + Send + Sync>
        });

        let response = client
            .send_streaming(request, on_tool_use)
            .await?;

        // 4. usage 集計
        total_usage.add(&response.usage);

        tracing::debug!(
            "Agent loop turn {}: stop_reason={:?}, content_blocks={}",
            turn_count,
            response.stop_reason,
            response.content.len()
        );

        // 5. assistant メッセージを会話履歴に追加
        messages.push(Message {
            role: Role::Assistant,
            content: response.content.clone(),
        });

        // 6. stop_reason 判定
        match response.stop_reason {
            Some(StopReason::ToolUse) if has_tools => {
                // ツール呼び出しを抽出して並列実行
                let tool_calls: Vec<_> = response
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, name, input } => {
                            Some((id.clone(), name.clone(), input.clone()))
                        }
                        _ => None,
                    })
                    .collect();

                if tool_calls.is_empty() {
                    tracing::warn!("stop_reason=tool_use but no tool_use blocks found");
                    break;
                }

                // 全ツールを並列実行（builtin / MCP 自動ルーティング）
                let tool_futures: Vec<_> = tool_calls
                    .iter()
                    .map(|(id, name, input)| {
                        let id = id.clone();
                        let name = name.clone();
                        let input = input.clone();
                        let ctx = &tool_ctx;
                        let mcp = config.mcp_manager.clone();
                        async move {
                            tracing::info!("Executing tool: {} (id={})", name, id);
                            let result = dispatch_tool(&name, &input, ctx, mcp.as_deref()).await;
                            tracing::info!(
                                "Tool {} completed: is_error={}, output_len={}",
                                name,
                                result.is_error,
                                result.output.len()
                            );
                            (id, result)
                        }
                    })
                    .collect();

                let results = futures_util::future::join_all(tool_futures).await;

                // ツール結果を user メッセージとして追加
                let tool_result_blocks: Vec<ContentBlock> = results
                    .into_iter()
                    .map(|(id, result)| ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: ToolResultContent::Text(result.output),
                        is_error: if result.is_error { Some(true) } else { None },
                    })
                    .collect();

                messages.push(Message {
                    role: Role::User,
                    content: tool_result_blocks,
                });
            }
            _ => {
                // EndTurn 前に OPS_RESULT: completed を検出したら検証ターンを注入
                if !verified {
                    let last_text = extract_final_text(&messages);
                    if last_text.contains("OPS_RESULT: completed") {
                        verified = true;
                        tracing::info!("Agent loop: OPS_RESULT detected, injecting verification turn");
                        messages.push(Message {
                            role: Role::User,
                            content: vec![ContentBlock::Text {
                                text: concat!(
                                    "作業完了前の最終確認を行ってください:\n",
                                    "1. `git status` で未コミットの変更がないか確認\n",
                                    "2. `git log -1` で正しくコミットされているか確認\n",
                                    "3. `git push` が成功しているか確認（必要なら実行）\n",
                                    "4. デプロイが必要な場合（clasp push 等）、実行済みか確認\n",
                                    "5. 問題があれば修正してください。問題なければそのまま最終報告してください。\n",
                                    "\n最終報告には OPS_RESULT マーカーを含めてください。"
                                ).to_string(),
                            }],
                        });
                        continue; // 検証ターンを実行
                    }
                }
                break;
            }
        }
    }

    // 最終テキスト出力を抽出
    let final_text = extract_final_text(&messages);

    Ok(AgentLoopResult {
        final_text,
        total_usage,
        turn_count,
    })
}

/// builtin ツールか MCP ツールかを判定してディスパッチ
async fn dispatch_tool(
    name: &str,
    input: &serde_json::Value,
    ctx: &ToolExecutionContext,
    mcp: Option<&McpManager>,
) -> ToolExecutionResult {
    if parse_mcp_tool_name(name).is_some() {
        // MCP ツール — bash 系ツールには safeguard を適用
        if let Some(reason) = check_mcp_safeguard(name, input) {
            return ToolExecutionResult::err(format!("Blocked by safeguard: {}", reason));
        }
        match mcp {
            Some(mgr) => match mgr.call_tool(name, input).await {
                Ok((output, is_error)) => {
                    if is_error {
                        ToolExecutionResult::err(output)
                    } else {
                        ToolExecutionResult::ok(output)
                    }
                }
                Err(e) => ToolExecutionResult::err(format!("MCP tool '{}' failed: {}", name, e)),
            },
            None => ToolExecutionResult::err(format!(
                "MCP tool '{}' called but no MCP manager available",
                name
            )),
        }
    } else {
        // builtin ツール
        execute_tool(name, input, ctx).await
    }
}

/// MCP ツール呼び出し前のセーフガード
/// Serena の bash_command / execute_command に builtin Bash と同じ safeguard を適用
fn check_mcp_safeguard(name: &str, input: &serde_json::Value) -> Option<String> {
    if name.ends_with("__bash_command") || name.ends_with("__execute_command") {
        if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
            return super::tool_impls::check_dangerous_command(cmd).map(|r| r.to_string());
        }
    }
    None
}

/// 最後の assistant メッセージからテキストを抽出
fn extract_final_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_final_text() {
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Hello".to_string(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "First part.".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".to_string(),
                        name: "Read".to_string(),
                        input: serde_json::json!({}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: ToolResultContent::Text("file content".to_string()),
                    is_error: None,
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Final answer.".to_string(),
                }],
            },
        ];
        assert_eq!(extract_final_text(&messages), "Final answer.");
    }

    #[test]
    fn test_extract_final_text_empty() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        }];
        assert_eq!(extract_final_text(&messages), "");
    }

    #[test]
    fn test_check_mcp_safeguard_blocks_dangerous() {
        let input = serde_json::json!({"command": "rm -rf /"});
        assert!(check_mcp_safeguard("mcp__serena__bash_command", &input).is_some());
        assert!(check_mcp_safeguard("mcp__serena__execute_command", &input).is_some());
    }

    #[test]
    fn test_check_mcp_safeguard_allows_safe() {
        let input = serde_json::json!({"command": "ls -la"});
        assert!(check_mcp_safeguard("mcp__serena__bash_command", &input).is_none());
    }

    #[test]
    fn test_check_mcp_safeguard_ignores_non_bash() {
        let input = serde_json::json!({"command": "rm -rf /"});
        // bash 以外のツールは safeguard を通さない
        assert!(check_mcp_safeguard("mcp__serena__find_symbol", &input).is_none());
    }
}
