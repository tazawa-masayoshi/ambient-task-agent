use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::claude::{AgentBackend, AgentOutput, AgentRequest, TokenUsage};

use super::agent_loop::{run_agent_loop, AgentLoopConfig};
use super::bedrock_client::BedrockClient;
use super::client::AnthropicClient;
use super::llm_client::LlmClient;
use super::mcp::McpManager;
use super::tools::build_tool_definitions;

/// Anthropic Messages API を直接叩くバックエンド
pub struct AnthropicApiBackend {
    client: Arc<AnthropicClient>,
    model: String,
}

impl AnthropicApiBackend {
    /// API キー認証で初期化
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: Arc::new(AnthropicClient::new(api_key)),
            model,
        }
    }

    /// Claude Code Max プランの OAuth 認証で初期化
    pub fn from_env(model: String) -> Result<Self> {
        Ok(Self {
            client: Arc::new(AnthropicClient::from_env()?),
            model,
        })
    }
}

#[async_trait]
impl AgentBackend for AnthropicApiBackend {
    async fn execute(&self, request: AgentRequest) -> Result<AgentOutput> {
        let start = std::time::Instant::now();

        // resume_session_id は API 直叩きでは非対応
        if let Some(ref sid) = request.resume_session_id {
            tracing::warn!(
                "AnthropicApiBackend: resume_session_id={} ignored (not supported in direct API mode)",
                sid
            );
        }

        // ツール定義を構築（builtin）
        let mut tools = request
            .allowed_tools
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(build_tool_definitions)
            .unwrap_or_default();

        // MCP サーバー起動 + ツール収集
        let mcp_manager = if !request.mcp_servers.is_empty() {
            let mgr = McpManager::start(&request.mcp_servers).await?;
            if !mgr.is_empty() {
                let mcp_tools = mgr.list_all_tools().await;
                tracing::info!("MCP tools loaded: {} total", mcp_tools.len());
                tools.extend(mcp_tools);
                Some(Arc::new(mgr))
            } else {
                None
            }
        } else {
            None
        };

        let cwd = request
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        let config = AgentLoopConfig {
            model: self.model.clone(),
            max_tokens_per_turn: 8192,
            // json_schema 指定時は単発生成に強制（ツールループ不要）
            max_turns: if request.json_schema.is_some() { 1 } else { request.max_turns },
            system_prompt: request.system_prompt.clone(),
            tools,
            cwd,
            timeout_secs: request.timeout_secs.unwrap_or(600),
            json_schema: request.json_schema.clone(),
            progress: request.progress.clone(),
            mcp_manager: mcp_manager.clone(),
        };

        // タイムアウト付きでエージェントループ実行
        let timeout_dur = std::time::Duration::from_secs(request.timeout_secs.unwrap_or(600));

        let client: &dyn LlmClient = self.client.as_ref();
        let result =
            tokio::time::timeout(timeout_dur, run_agent_loop(client, config, &request.prompt))
                .await;

        let duration = start.elapsed();

        // MCP サーバーをシャットダウン
        if let Some(ref mgr) = mcp_manager {
            mgr.shutdown().await;
        }

        match result {
            Ok(Ok(loop_result)) => {
                let usage = TokenUsage {
                    input_tokens: loop_result.total_usage.input_tokens,
                    output_tokens: loop_result.total_usage.output_tokens,
                    cache_creation_input_tokens: loop_result
                        .total_usage
                        .cache_creation_input_tokens,
                    cache_read_input_tokens: loop_result.total_usage.cache_read_input_tokens,
                };
                let cost_usd = calculate_cost(&loop_result.total_usage, &self.model);

                tracing::info!(
                    "AnthropicApiBackend: {} turns, in={} out={} cache_create={} cache_read={}, cost=${:.6}",
                    loop_result.turn_count,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_creation_input_tokens,
                    usage.cache_read_input_tokens,
                    cost_usd,
                );

                Ok(AgentOutput {
                    success: true,
                    stdout: loop_result.final_text,
                    stderr: String::new(),
                    duration,
                    truncated: false,
                    usage: Some(usage),
                    cost_usd: Some(cost_usd),
                    session_id: None,
                })
            }
            Ok(Err(e)) => {
                tracing::error!("AnthropicApiBackend error: {}", e);
                Ok(AgentOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: e.to_string(),
                    duration,
                    truncated: false,
                    usage: None,
                    cost_usd: None,
                    session_id: None,
                })
            }
            Err(_) => {
                tracing::error!(
                    "AnthropicApiBackend: timed out after {}s",
                    timeout_dur.as_secs()
                );
                Ok(AgentOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: format!("Process timed out after {}s", timeout_dur.as_secs()),
                    duration,
                    truncated: false,
                    usage: None,
                    cost_usd: None,
                    session_id: None,
                })
            }
        }
    }
}

// ============================================================================
// Bedrock Converse API バックエンド
// ============================================================================

/// AWS Bedrock Converse API を使うバックエンド
pub struct BedrockBackend {
    client: Arc<BedrockClient>,
    model: String,
}

impl BedrockBackend {
    pub async fn new(region: String, model: String) -> Result<Self> {
        let client = BedrockClient::new(&region, model.clone()).await?;
        Ok(Self {
            client: Arc::new(client),
            model,
        })
    }
}

#[async_trait]
impl AgentBackend for BedrockBackend {
    async fn execute(&self, request: AgentRequest) -> Result<AgentOutput> {
        let start = std::time::Instant::now();

        if let Some(ref sid) = request.resume_session_id {
            tracing::warn!(
                "BedrockBackend: resume_session_id={} ignored (not supported)",
                sid
            );
        }

        let mut tools = request
            .allowed_tools
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(build_tool_definitions)
            .unwrap_or_default();

        let mcp_manager = if !request.mcp_servers.is_empty() {
            let mgr = McpManager::start(&request.mcp_servers).await?;
            if !mgr.is_empty() {
                let mcp_tools = mgr.list_all_tools().await;
                tracing::info!("MCP tools loaded: {} total", mcp_tools.len());
                tools.extend(mcp_tools);
                Some(Arc::new(mgr))
            } else {
                None
            }
        } else {
            None
        };

        let cwd = request
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        let config = AgentLoopConfig {
            model: self.model.clone(),
            max_tokens_per_turn: 8192,
            max_turns: if request.json_schema.is_some() {
                1
            } else {
                request.max_turns
            },
            system_prompt: request.system_prompt.clone(),
            tools,
            cwd,
            timeout_secs: request.timeout_secs.unwrap_or(600),
            json_schema: request.json_schema.clone(),
            progress: request.progress.clone(),
            mcp_manager: mcp_manager.clone(),
        };

        let timeout_dur = std::time::Duration::from_secs(request.timeout_secs.unwrap_or(600));

        let client: &dyn LlmClient = self.client.as_ref();
        let result =
            tokio::time::timeout(timeout_dur, run_agent_loop(client, config, &request.prompt))
                .await;

        let duration = start.elapsed();

        if let Some(ref mgr) = mcp_manager {
            mgr.shutdown().await;
        }

        match result {
            Ok(Ok(loop_result)) => {
                let usage = TokenUsage {
                    input_tokens: loop_result.total_usage.input_tokens,
                    output_tokens: loop_result.total_usage.output_tokens,
                    cache_creation_input_tokens: loop_result
                        .total_usage
                        .cache_creation_input_tokens,
                    cache_read_input_tokens: loop_result.total_usage.cache_read_input_tokens,
                };
                let cost_usd = calculate_cost(&loop_result.total_usage, &self.model);

                tracing::info!(
                    "BedrockBackend: {} turns, in={} out={} cache_create={} cache_read={}, cost=${:.6}",
                    loop_result.turn_count,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_creation_input_tokens,
                    usage.cache_read_input_tokens,
                    cost_usd,
                );

                Ok(AgentOutput {
                    success: true,
                    stdout: loop_result.final_text,
                    stderr: String::new(),
                    duration,
                    truncated: false,
                    usage: Some(usage),
                    cost_usd: Some(cost_usd),
                    session_id: None,
                })
            }
            Ok(Err(e)) => {
                tracing::error!("BedrockBackend error: {}", e);
                Ok(AgentOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: e.to_string(),
                    duration,
                    truncated: false,
                    usage: None,
                    cost_usd: None,
                    session_id: None,
                })
            }
            Err(_) => {
                tracing::error!(
                    "BedrockBackend: timed out after {}s",
                    timeout_dur.as_secs()
                );
                Ok(AgentOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: format!("Process timed out after {}s", timeout_dur.as_secs()),
                    duration,
                    truncated: false,
                    usage: None,
                    cost_usd: None,
                    session_id: None,
                })
            }
        }
    }
}

// ============================================================================
// 共通ユーティリティ
// ============================================================================

/// モデル別のトークン単価からコストを算出 (USD)
fn calculate_cost(usage: &super::types::AggregatedUsage, model: &str) -> f64 {
    // Anthropic pricing (per million tokens, 2025-05)
    let (input_rate, output_rate, cache_write_rate, cache_read_rate) = match model {
        m if m.contains("opus") => (15.0, 75.0, 18.75, 1.5),
        m if m.contains("sonnet") => (3.0, 15.0, 3.75, 0.3),
        m if m.contains("haiku") => (0.25, 1.25, 0.3, 0.03),
        _ => (3.0, 15.0, 3.75, 0.3), // default to sonnet pricing
    };

    let million = 1_000_000.0;
    (usage.input_tokens as f64 / million) * input_rate
        + (usage.output_tokens as f64 / million) * output_rate
        + (usage.cache_creation_input_tokens as f64 / million) * cache_write_rate
        + (usage.cache_read_input_tokens as f64 / million) * cache_read_rate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::AggregatedUsage;

    #[test]
    fn test_calculate_cost_sonnet() {
        let usage = AggregatedUsage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let cost = calculate_cost(&usage, "claude-sonnet-4-20250514");
        // 1M input * $3/M + 100K output * $15/M = $3 + $1.5 = $4.5
        assert!((cost - 4.5).abs() < 0.01);
    }

    #[test]
    fn test_calculate_cost_opus() {
        let usage = AggregatedUsage {
            input_tokens: 100_000,
            output_tokens: 10_000,
            cache_creation_input_tokens: 50_000,
            cache_read_input_tokens: 200_000,
        };
        let cost = calculate_cost(&usage, "claude-opus-4-20250514");
        // 100K * $15/M + 10K * $75/M + 50K * $18.75/M + 200K * $1.5/M
        // = $1.5 + $0.75 + $0.9375 + $0.3 = $3.4875
        assert!((cost - 3.4875).abs() < 0.01);
    }
}
