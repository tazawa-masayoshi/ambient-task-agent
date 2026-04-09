//! コンテンツベースルーター
//!
//! メッセージ内容からスコープ判定 + MCP サーバー選択を行う。
//! runner_ops.rs の route_ops() / resolve_ops_repo_entry() を構造体に集約。

use anyhow::Result;

use crate::anthropic::mcp::McpServerConfig;
use crate::anthropic::mcp_config;
use crate::db::OpsQueueItem;
use crate::execution::RunnerContext;
use crate::repo_config::{RepoEntry, ReposConfig};

/// ルーティング結果
pub struct RouteResult {
    pub repo_entry: RepoEntry,
    pub mcp_configs: Vec<McpServerConfig>,
}

/// コンテンツベースルーター
pub struct ContentRouter<'a> {
    repos_config: &'a ReposConfig,
    runner_ctx: &'a RunnerContext,
}

impl<'a> ContentRouter<'a> {
    pub fn new(repos_config: &'a ReposConfig, runner_ctx: &'a RunnerContext) -> Self {
        Self {
            repos_config,
            runner_ctx,
        }
    }

    /// メッセージ内容からスコープ + MCP サーバー群を決定
    pub async fn route(&self, item: &OpsQueueItem) -> Result<Option<RouteResult>> {
        // 1. repo_key で直接マッチ（ready ステータスのみ）
        //
        // pending ステータスの場合は直接マッチをスキップして必ず LLM 分類に回す。
        // 理由: pending は「自動拾い」（@admin メンション or スレッド返信）で enqueue
        // されたもので、会話締め (「対応済み」「ありがとう」) や雑談が混ざる可能性が
        // あるため、few-shot examples ベースで actionable 判定する必要がある。
        // ready は @bot メンション / ⚡ リアクション等で明示的にトリガーされたものなので
        // 直接マッチで信頼していい。
        if item.status != "pending" {
            if let Some(entry) = self.repos_config.find_repo_by_key(&item.repo_key) {
                tracing::info!(
                    "ContentRouter: key-matched to scope: {} ({})",
                    entry.key,
                    entry.ops_description.as_deref().unwrap_or("no description")
                );
                let repo_path = self.repos_config.repo_local_path(entry);
                let mcp_configs = mcp_config::build_mcp_configs(entry, &repo_path);
                return Ok(Some(RouteResult {
                    repo_entry: entry.clone(),
                    mcp_configs,
                }));
            }
        }

        // 2. コンテンツベースルーティング（LLM スコープ判定）
        match self.route_by_content(item).await {
            Ok(Some(idx)) => {
                let entry = self.repos_config.repo[idx].clone();
                tracing::info!(
                    "ContentRouter: routed to scope: {} ({})",
                    entry.key,
                    entry.ops_description.as_deref().unwrap_or("no description")
                );
                let repo_path = self.repos_config.repo_local_path(&entry);
                let mcp_configs = mcp_config::build_mcp_configs(&entry, &repo_path);
                Ok(Some(RouteResult {
                    repo_entry: entry,
                    mcp_configs,
                }))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// LLM を使ったコンテンツベースルーティング
    async fn route_by_content(&self, item: &OpsQueueItem) -> Result<Option<usize>> {
        if item.message_text.trim().len() < 5 {
            tracing::debug!("ContentRouter: message too short, skipping");
            return Ok(None);
        }

        let ops_entries = self.repos_config.get_all_ops_entries();
        if ops_entries.is_empty() {
            tracing::warn!("ContentRouter: no ops entries found in config");
            return Ok(None);
        }

        // スコープが1つしかない場合は分類不要
        if ops_entries.len() == 1 {
            tracing::info!(
                "ContentRouter: single scope, auto-selecting: {}",
                ops_entries[0].1.key
            );
            return Ok(Some(ops_entries[0].0));
        }

        let scopes: Vec<String> = ops_entries
            .iter()
            .enumerate()
            .map(|(i, (_, entry))| {
                let desc = entry.ops_description.as_deref().unwrap_or(&entry.key);
                let mut block = format!("{}. **{}**\n   説明: {}", i + 1, entry.key, desc);
                if let Some(examples) = &entry.ops_request_examples {
                    if !examples.is_empty() {
                        block.push_str("\n   依頼例:");
                        for ex in examples.iter().take(3) {
                            // 1 例 200 文字でクランプして prompt 膨張を防ぐ
                            let truncated: String = ex.chars().take(200).collect();
                            // 改行を ' / ' に圧縮して 1 行で表示
                            let oneliner = truncated.replace('\n', " / ");
                            block.push_str(&format!("\n   - 「{}」", oneliner));
                        }
                    }
                }
                block
            })
            .collect();

        tracing::info!(
            "ContentRouter: classifying across {} scopes",
            ops_entries.len()
        );

        let prompt = format!(
            "以下のSlackメッセージがどの作業スコープに該当するか判定してください。\n\
             各スコープの「依頼例」と文体・トピック・使用語彙が近いものを選んでください。\n\
             \n\
             ## 0 (out-of-scope) を返すべきパターン\n\
             以下のような「作業依頼ではないメッセージ」は必ず 0 を返すこと:\n\
             - 会話の締めくくり: 「対応済みです」「ありがとうございます」「お疲れ様でした」「了解しました」「確認しました」\n\
             - 報告のみ: 「A さんに連絡済み」「共有しました」「完了報告です」\n\
             - 質問・確認依頼: 「どうなってますか？」「進捗教えてください」「これで合ってますか？」\n\
             - 雑談・挨拶・絵文字のみ: 「お疲れ様です」「👍」「🙏」\n\
             - 他者間のやり取り: 特定の人宛のメッセージで作業指示でないもの\n\
             \n\
             ## スコープ 1〜N を返すべきパターン\n\
             各スコープの「依頼例」と同じ文体・構造の **作業依頼** のみ。\n\
             \n\
             ## 作業スコープ一覧\n{}\n\n\
             ## メッセージ\n{}\n\n\
             該当するスコープの番号を scope フィールドに返してください。\n\
             作業依頼ではない会話・報告・確認は 0 を返してください。",
            scopes.join("\n\n"),
            item.message_text
        );

        let schema = r#"{"type":"object","properties":{"scope":{"type":"integer"}},"required":["scope"]}"#;

        let log_dir = std::path::PathBuf::from(&self.repos_config.defaults.repos_base_dir)
            .join(".agent")
            .join("logs");

        let result = crate::claude::ClaudeRunner::new("route", &prompt)
            .max_turns(1)
            .allowed_tools("")
            .json_schema(schema)
            .log_dir(&log_dir)
            .with_context(self.runner_ctx)
            .run()
            .await?;

        if !result.success {
            anyhow::bail!("ContentRouter: LLM routing failed: {}", result.stderr);
        }

        let answer = result.stdout.trim();
        tracing::info!("ContentRouter: LLM answer='{}' for item {}", answer, item.id);

        let num: usize = serde_json::from_str::<serde_json::Value>(answer)
            .ok()
            .and_then(|v| v.get("scope")?.as_u64())
            .unwrap_or(0) as usize;

        if num == 0 || num > ops_entries.len() {
            tracing::info!("ContentRouter: no match (answer='{}', parsed={})", answer, num);
            return Ok(None);
        }

        let selected = &ops_entries[num - 1].1;
        tracing::info!(
            "ContentRouter: selected scope {} '{}' for item {}",
            num,
            selected.ops_description.as_deref().unwrap_or(&selected.key),
            item.id
        );

        Ok(Some(ops_entries[num - 1].0))
    }
}
