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
        let primary_model = resolve_model_for_purpose(&self.model, request.purpose);
        let fallback_model = resolve_fallback_model(&request, request.purpose);

        // fallback がある場合のみ retry 用に request を clone して保持する。
        let retry_request = fallback_model.as_ref().map(|_| request.clone());

        match execute_with_harness_generic(&llm_client, &primary_model, request).await {
            Ok(out) => Ok(out),
            Err(e) if is_overloaded_error(&e) => {
                let Some(retry_req) = retry_request else {
                    return Err(e);
                };
                let Some(fb) = fallback_model else {
                    return Err(e);
                };
                tracing::warn!(
                    "primary model {primary_model} overloaded; falling back to {fb} ({e:#})"
                );
                execute_with_harness_generic(&llm_client, &fb, retry_req).await
            }
            Err(e) => Err(e),
        }
    }
}

/// `request.fallback_model` が明示指定されていればそれ、
/// そうでなければ purpose 別の環境変数 (`ANTHROPIC_MODEL_OPS_FALLBACK` 等) を参照。
///
/// Classify / Conversing は軽量モデルで動いているので fallback は原則不要
/// (空なら None が返る)。OpsExecute はサブスク枠消費が重いので特に効く。
fn resolve_fallback_model(
    request: &AgentRequest,
    purpose: crate::claude::RequestPurpose,
) -> Option<String> {
    if let Some(ref m) = request.fallback_model {
        if !m.trim().is_empty() {
            return Some(m.clone());
        }
    }
    let env_key = match purpose {
        crate::claude::RequestPurpose::OpsExecute => "ANTHROPIC_MODEL_OPS_FALLBACK",
        crate::claude::RequestPurpose::Classify => "ANTHROPIC_MODEL_CLASSIFY_FALLBACK",
        crate::claude::RequestPurpose::Conversing => "ANTHROPIC_MODEL_CONVERSING_FALLBACK",
        crate::claude::RequestPurpose::Generic => return None,
    };
    std::env::var(env_key)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// 529 (overloaded) や "overloaded" 文言を error chain から検出する。
/// primary model の容量枯渇時のみ fallback させる判定。
fn is_overloaded_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    msg.contains("529") || msg.contains("overloaded") || msg.contains("Overloaded")
}

/// purpose に応じて model を解決。環境変数 override 優先、未設定なら `base`。
///
/// 環境変数:
/// - `ANTHROPIC_MODEL_CLASSIFY` (Classify)
/// - `ANTHROPIC_MODEL_CONVERSING` (Conversing)
/// - `ANTHROPIC_MODEL_OPS` (OpsExecute)
/// - Generic は常に `base` (`ANTHROPIC_MODEL`) を返す
///
/// 空文字列 / 未設定は「未指定」扱いで base にフォールバック。
fn resolve_model_for_purpose(
    base: &str,
    purpose: crate::claude::RequestPurpose,
) -> String {
    let env_key = match purpose {
        crate::claude::RequestPurpose::Classify => "ANTHROPIC_MODEL_CLASSIFY",
        crate::claude::RequestPurpose::Conversing => "ANTHROPIC_MODEL_CONVERSING",
        crate::claude::RequestPurpose::OpsExecute => "ANTHROPIC_MODEL_OPS",
        crate::claude::RequestPurpose::Generic => return base.to_string(),
    };
    match std::env::var(env_key) {
        Ok(v) if !v.trim().is_empty() => v,
        Ok(_) => {
            // typo / シェル側で空になってる等の事故を可視化
            tracing::warn!(
                "{env_key} is set but empty/whitespace; falling back to base model"
            );
            base.to_string()
        }
        Err(_) => base.to_string(),
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
        user_prompt_hook: None,
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

    // ---- resolve_model_for_purpose ---------------------------------------

    /// パニック発生時でも env を clean up するための RAII ガード。
    /// Rust のテストは並列実行されるので、他テストに env を残すと flaky になる。
    struct EnvGuard {
        keys: &'static [&'static str],
    }

    impl EnvGuard {
        fn new(keys: &'static [&'static str]) -> Self {
            for k in keys {
                std::env::remove_var(k);
            }
            Self { keys }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for k in self.keys {
                std::env::remove_var(k);
            }
        }
    }

    // 環境変数を触るテストは serial に実行する必要があるため 1 関数にまとめる。
    #[test]
    fn resolve_model_for_purpose_env_and_fallback() {
        use crate::claude::RequestPurpose;

        let _guard = EnvGuard::new(&[
            "ANTHROPIC_MODEL_CLASSIFY",
            "ANTHROPIC_MODEL_CONVERSING",
            "ANTHROPIC_MODEL_OPS",
        ]);

        let base = "claude-sonnet-base";

        // 未設定 → base
        assert_eq!(
            resolve_model_for_purpose(base, RequestPurpose::Classify),
            base
        );
        assert_eq!(
            resolve_model_for_purpose(base, RequestPurpose::Conversing),
            base
        );
        assert_eq!(
            resolve_model_for_purpose(base, RequestPurpose::OpsExecute),
            base
        );
        // Generic は常に base (環境変数参照しない)
        assert_eq!(
            resolve_model_for_purpose(base, RequestPurpose::Generic),
            base
        );

        // 設定あり → env 値
        std::env::set_var("ANTHROPIC_MODEL_CLASSIFY", "haiku-test");
        std::env::set_var("ANTHROPIC_MODEL_CONVERSING", "haiku-test");
        std::env::set_var("ANTHROPIC_MODEL_OPS", "opus-test");
        assert_eq!(
            resolve_model_for_purpose(base, RequestPurpose::Classify),
            "haiku-test"
        );
        assert_eq!(
            resolve_model_for_purpose(base, RequestPurpose::Conversing),
            "haiku-test"
        );
        assert_eq!(
            resolve_model_for_purpose(base, RequestPurpose::OpsExecute),
            "opus-test"
        );
        // Generic は env に引きずられない
        assert_eq!(
            resolve_model_for_purpose(base, RequestPurpose::Generic),
            base
        );

        // 空文字列 → base にフォールバック
        std::env::set_var("ANTHROPIC_MODEL_CLASSIFY", "");
        std::env::set_var("ANTHROPIC_MODEL_CONVERSING", "   ");
        assert_eq!(
            resolve_model_for_purpose(base, RequestPurpose::Classify),
            base
        );
        assert_eq!(
            resolve_model_for_purpose(base, RequestPurpose::Conversing),
            base
        );
        // _guard が scope 抜けると env が自動 clean up される
    }

    #[test]
    fn is_overloaded_error_matches_common_patterns() {
        assert!(is_overloaded_error(&anyhow::anyhow!("HTTP 529 overloaded")));
        assert!(is_overloaded_error(&anyhow::anyhow!(
            "Anthropic stream error: overloaded_error - Overloaded"
        )));
        assert!(is_overloaded_error(&anyhow::anyhow!("status: 529")));
        assert!(!is_overloaded_error(&anyhow::anyhow!("timeout")));
        assert!(!is_overloaded_error(&anyhow::anyhow!("HTTP 500")));
    }

    // env を触る fallback テストは並列実行で干渉するので 1 関数に統合する。
    #[test]
    fn resolve_fallback_model_all_cases() {
        use crate::claude::RequestPurpose;

        let _guard = EnvGuard::new(&[
            "ANTHROPIC_MODEL_OPS_FALLBACK",
            "ANTHROPIC_MODEL_CLASSIFY_FALLBACK",
            "ANTHROPIC_MODEL_CONVERSING_FALLBACK",
        ]);

        // 1. request field が設定されていれば env より優先
        std::env::set_var("ANTHROPIC_MODEL_OPS_FALLBACK", "env-fallback");
        let req = make_test_request(Some("request-fallback".to_string()));
        assert_eq!(
            resolve_fallback_model(&req, RequestPurpose::OpsExecute),
            Some("request-fallback".to_string())
        );

        // 2. request field が None の時は env をチェック
        std::env::set_var("ANTHROPIC_MODEL_OPS_FALLBACK", "sonnet-fallback");
        std::env::set_var("ANTHROPIC_MODEL_CLASSIFY_FALLBACK", "haiku-fallback");
        let req = make_test_request(None);
        assert_eq!(
            resolve_fallback_model(&req, RequestPurpose::OpsExecute),
            Some("sonnet-fallback".to_string())
        );
        assert_eq!(
            resolve_fallback_model(&req, RequestPurpose::Classify),
            Some("haiku-fallback".to_string())
        );

        // 3. Generic は env を読まない
        assert_eq!(
            resolve_fallback_model(&req, RequestPurpose::Generic),
            None
        );

        // 4. request field が空文字 → env へフォールバック
        let req_empty = make_test_request(Some(String::new()));
        assert_eq!(
            resolve_fallback_model(&req_empty, RequestPurpose::OpsExecute),
            Some("sonnet-fallback".to_string())
        );

        // 5. env が空白のみ → None
        std::env::set_var("ANTHROPIC_MODEL_OPS_FALLBACK", "   ");
        let req = make_test_request(None);
        assert_eq!(
            resolve_fallback_model(&req, RequestPurpose::OpsExecute),
            None
        );

        // 6. 未設定の purpose → None
        std::env::remove_var("ANTHROPIC_MODEL_CONVERSING_FALLBACK");
        assert_eq!(
            resolve_fallback_model(&req, RequestPurpose::Conversing),
            None
        );
    }

    fn make_test_request(fallback_model: Option<String>) -> AgentRequest {
        AgentRequest {
            prompt: String::new(),
            system_prompt: None,
            max_turns: 1,
            allowed_tools: None,
            cwd: None,
            env: vec![],
            timeout_secs: None,
            max_output_bytes: None,
            resume_session_id: None,
            json_schema: None,
            fallback_model,
            progress: None,
            mcp_servers: vec![],
            permission_mode: agent_harness::PermissionMode::default(),
            purpose: crate::claude::RequestPurpose::Generic,
        }
    }

    #[test]
    fn request_purpose_from_module_mapping() {
        use crate::claude::RequestPurpose;
        assert_eq!(RequestPurpose::from_module("classify"), RequestPurpose::Classify);
        assert_eq!(RequestPurpose::from_module("route"), RequestPurpose::Classify);
        assert_eq!(RequestPurpose::from_module("dm_format"), RequestPurpose::Classify);
        assert_eq!(RequestPurpose::from_module("conversing"), RequestPurpose::Conversing);
        assert_eq!(RequestPurpose::from_module("ops"), RequestPurpose::OpsExecute);
        assert_eq!(RequestPurpose::from_module("ops_summary"), RequestPurpose::OpsExecute);
        assert_eq!(RequestPurpose::from_module("executor"), RequestPurpose::OpsExecute);
        // 未知の module は Generic
        assert_eq!(
            RequestPurpose::from_module("scheduler:morning_briefing"),
            RequestPurpose::Generic
        );
        assert_eq!(RequestPurpose::from_module("unknown"), RequestPurpose::Generic);
    }
}
