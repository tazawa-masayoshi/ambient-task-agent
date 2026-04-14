use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use claude_auth::{AnthropicClient, AnthropicLlmClient};

use crate::claude::{AgentBackend, AgentOutput, AgentRequest, TokenUsage};

use super::bedrock_client::BedrockClient;
use super::harness_adapter;
use super::harness_adapter::AmbientToolExecutor;
use super::mcp::McpManager;
use super::tools::build_tool_definitions;

/// Anthropic Messages API を直接叩くバックエンド
pub struct AnthropicApiBackend {
    client: Arc<AnthropicClient>,
    model: String,
}

impl AnthropicApiBackend {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: Arc::new(AnthropicClient::with_api_key(api_key, model.clone())),
            model,
        }
    }

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
        let llm_client = AnthropicLlmClient::new(self.client.clone());
        execute_with_harness_generic(&llm_client, &self.model, request).await
    }
}

// ============================================================================
// Bedrock Converse API バックエンド
// ============================================================================

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
        let llm_client = harness_adapter::BedrockLlmClient {
            client: self.client.clone(),
        };
        execute_with_harness_generic(&llm_client, &self.model, request).await
    }
}

// ============================================================================
// harness 経由の共通実行（AnthropicApiBackend 用）
// ============================================================================

async fn execute_with_harness_generic(
    llm_client: &dyn agent_harness::LlmClient,
    model: &str,
    request: AgentRequest,
) -> Result<AgentOutput> {
    let start = std::time::Instant::now();

    // MCP サーバー起動
    let mcp_manager = if !request.mcp_servers.is_empty() {
        match McpManager::start(&request.mcp_servers).await {
            Ok(mgr) if !mgr.is_empty() => Some(Arc::new(mgr)),
            Ok(_) => None,
            Err(e) => {
                tracing::error!("MCP startup failed: {}", e);
                None
            }
        }
    } else {
        None
    };

    let tools = resolve_tools(request.allowed_tools.as_deref(), mcp_manager.as_deref()).await;
    let cwd = request.cwd.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // dynamic context → system prompt に結合
    let system_prompt = {
        let mut parts = Vec::new();
        if let Some(ref sp) = request.system_prompt {
            parts.push(sp.clone());
        }
        if request.json_schema.is_none() {
            let ctx = super::context_builder::ProjectContext::discover(&cwd);
            let dynamic = super::context_builder::build_dynamic_context(&ctx, &tools);
            if !dynamic.is_empty() {
                parts.push(dynamic);
            }
        }
        if parts.is_empty() { None } else { Some(parts.join("\n\n")) }
    };

    // harness AgentConfig を構築
    let config = agent_harness::AgentConfig {
        model: model.to_string(),
        max_tokens_per_turn: 32000,
        max_turns: if request.json_schema.is_some() { 1 } else { request.max_turns },
        system_prompt,
        tools,
        cwd: cwd.clone(),
        timeout_secs: request.timeout_secs.unwrap_or(600),
        completion_marker: "OPS_RESULT: completed".to_string(),
        failure_marker: "OPS_RESULT: failed".to_string(),
        verify_prompt: Some(format!(
            "作業完了前の最終確認を行ってください（検証 {{n}}/{{max}}）:\n\
             \n\
             ## 依頼内容の完了確認\n\
             - 元の依頼で求められた作業を全て実行したか？\n\
             - 「手動で対応してください」と書いた項目は、本当に自動化できないか？\n\
             - gws（Google Workspace CLI）やBashで実行できるスプレッドシート操作、ファイルアップロードが残っていないか？\n\
             - やれることが残っているなら、完了と報告せず実行してください。\n\
             \n\
             ## デプロイ確認\n\
             - `git status` で未コミットの変更がないか\n\
             - `git push` が成功しているか（必要なら実行）\n\
             - clasp push 等のデプロイが必要なら実行済みか\n\
             \n\
             ## 最終報告の形式（重要・必ず守る）\n\
             **依頼者には非エンジニアが含まれるので、技術詳細と人間向けサマリを分けて書く**。\n\
             \n\
             **冒頭文**（3 セクションの前に 1 行）:\n\
             「対応しました」「完了しました」等、作業を実行した事実を述べる肯定文にする。\n\
             NG: 「追加対応が必要な項目はありません」「特に問題ありませんでした」— 何もしていないように読める。\n\
             OK: 「ご依頼の対応が完了しました。」\n\
             \n\
             報告は以下の **3 セクション構成** にしてください:\n\
             \n\
             ### 📋 依頼者向けサマリ\n\
             依頼者（非エンジニア含む）が一目で状況を理解できる言葉で 2〜5 行。\n\
             \n\
             {slack_rules}\n\
             \n\
             **書く内容**:\n\
             - 何が追加・変更されたか（固有名詞で）\n\
             - いつから使えるか（即時 / 次回送信 / 翌営業日 等）\n\
             - どこで確認できるか（必要なら）\n\
             \n\
             **良い例**:\n\
             『オリジナル取材にサブカテ「ゾンビ狩り取材」を追加しました。\n\
             アンケート自動送信システムとスプレッドシート両方に登録済みで、次回の送信から有効です。』\n\
             \n\
             **悪い例（技術詳細が混ざっている）**:\n\
             『行139に追加、gid=930606752、テンプレートID=1iSV7...』\n\
             \n\
             ### 🔧 実施内容（技術詳細）\n\
             エンジニアが後から監査・再現できるレベルで具体的に書く。\n\
             実際に追加・変更・実行した項目を **ID と名前を明示して箇条書き**:\n\
             - `269_173`: 推し活リサーチ を image_mappings.yaml に追加\n\
             - `images/269_173.png` を配置\n\
             - 03-constants.js の EXPECTED_PATTERNS[269] に \"D\" 追加\n\
             - git push 済み (commit: abc1234)\n\
             - スプレッドシート行追加 (gid=612606300, 行=42)\n\
             \n\
             **禁止表現**: 「全て完了」「6 件追加済み」「全項目 ✅」など抽象的な表現。\n\
             ユーザーが後から個別に検証できる粒度で、必ず固有名詞・ID・コミットハッシュ・ファイル名を入れる。\n\
             \n\
             ### ✅ 確認結果\n\
             検証チェックを ✅ / ⚠️ / ❌ で 1〜2 行の短い箇条書きで。\n\
             \n\
             最終行に OPS_RESULT: completed (または failed) を出力してください。",
            slack_rules = agent_harness::SLACK_FORMAT_RULES,
        )),
        retry_prompt: "検証で問題が見つかりました。修正して再度確認してください。\n最終報告は『📋 依頼者向けサマリ』『🔧 実施内容（技術詳細）』『✅ 確認結果』の3セクション構成で、最終行に OPS_RESULT マーカーを含めてください。".to_string(),
        max_verify_attempts: 3,
        recovery_detector: Some(Arc::new(super::recovery::AmbientRecoveryDetector)),
        tool_output_offload_threshold: None,
        tool_output_offload_dir: None,
        recent_actions_limit: None,
    };

    // ToolExecutor
    let executor = AmbientToolExecutor {
        mcp_manager: mcp_manager.clone(),
        timeout_secs: request.timeout_secs.unwrap_or(600),
        permission_mode: request.permission_mode,
        hook: Some(Arc::new(super::hook::AmbientHookHandler)),
        stale_tracker: super::tool_impls::StaleFileTracker::new(),
    };

    // harness のコアループ実行
    let timeout_dur = std::time::Duration::from_secs(request.timeout_secs.unwrap_or(600));
    let result = tokio::time::timeout(
        timeout_dur,
        agent_harness::run_agent_loop(
            llm_client,
            &executor,
            config,
            vec![agent_harness::Message::user_text(&request.prompt)],
        ),
    )
    .await;

    let duration = start.elapsed();

    if let Some(ref mgr) = mcp_manager {
        mgr.shutdown().await;
    }

    match result {
        Ok(Ok(harness_result)) => {
            let usage = TokenUsage {
                input_tokens: harness_result.total_usage.input_tokens,
                output_tokens: harness_result.total_usage.output_tokens,
                cache_creation_input_tokens: harness_result.total_usage.cache_creation_input_tokens,
                cache_read_input_tokens: harness_result.total_usage.cache_read_input_tokens,
            };
            let cost_usd = calculate_cost(&harness_result.total_usage, model);
            tracing::info!(
                "AnthropicApiBackend (harness): {} turns, in={} out={}, cost=${:.6}",
                harness_result.turn_count, usage.input_tokens, usage.output_tokens, cost_usd,
            );
            Ok(AgentOutput {
                success: true,
                stdout: harness_result.final_text,
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
            Ok(AgentOutput { success: false, stdout: String::new(), stderr: e.to_string(), duration, truncated: false, usage: None, cost_usd: None, session_id: None })
        }
        Err(_) => {
            Ok(AgentOutput { success: false, stdout: String::new(), stderr: format!("Timed out after {}s", timeout_dur.as_secs()), duration, truncated: false, usage: None, cost_usd: None, session_id: None })
        }
    }
}

// ============================================================================
// 共通ユーティリティ
// ============================================================================

fn calculate_cost(usage: &super::types::AggregatedUsage, model: &str) -> f64 {
    let (input_rate, output_rate, cache_write_rate, cache_read_rate) = match model {
        m if m.contains("opus") => (15.0, 75.0, 18.75, 1.5),
        m if m.contains("sonnet") => (3.0, 15.0, 3.75, 0.3),
        m if m.contains("haiku") => (0.25, 1.25, 0.3, 0.03),
        _ => (3.0, 15.0, 3.75, 0.3),
    };

    let million = 1_000_000.0;
    (usage.input_tokens as f64 / million) * input_rate
        + (usage.output_tokens as f64 / million) * output_rate
        + (usage.cache_creation_input_tokens as f64 / million) * cache_write_rate
        + (usage.cache_read_input_tokens as f64 / million) * cache_read_rate
}

async fn resolve_tools(
    allowed_tools: Option<&str>,
    mcp_manager: Option<&McpManager>,
) -> Vec<super::types::ToolDefinition> {
    let mut tools = Vec::new();

    if let Some(mgr) = mcp_manager {
        let mcp_tools = mgr.list_all_tools().await;
        let has_serena = mcp_tools.iter().any(|t| t.name.starts_with("mcp__serena__"));

        if has_serena {
            tools.extend(build_tool_definitions("Bash"));
        } else {
            tools.extend(
                allowed_tools
                    .filter(|t| !t.is_empty())
                    .map(build_tool_definitions)
                    .unwrap_or_default(),
            );
        }
        tools.extend(mcp_tools);
    } else {
        tools.extend(
            allowed_tools
                .filter(|t| !t.is_empty())
                .map(build_tool_definitions)
                .unwrap_or_default(),
        );
    }

    tools
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
        assert!((cost - 3.4875).abs() < 0.01);
    }
}
