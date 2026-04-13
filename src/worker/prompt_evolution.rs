//! Prompt evolution: 過去の成功/失敗サンプルから system_prompt の改善案を生成し、
//! 採点して最良案を提示する最小 PoC。
//!
//! **責務** (Phase 1):
//! - 現 system_prompt + 成功 3 件 + 失敗 3 件 を LLM に渡して改善案 3 つ生成
//! - 各改善案を LLM で採点（0-100）
//! - 最高スコアの候補を `EvolutionResult` として返す
//!
//! **責務外** (Phase 2 以降):
//! - DB テーブル拡張 (`prompt_evolution_proposals`)
//! - scheduler ジョブとしての定期実行
//! - Slack 承認ボタン連携
//! - 採用後の system_prompt 差し替え / A/B 検証
//!
//! **コスト見積**: LLM 呼び出し 1 回（候補生成）+ 3 回（候補ごとに採点）= 4 回。
//! 週次実行なら月 16 回。

#![allow(dead_code)]

use agent_harness::types::{ContentBlock, Message};
use agent_harness::LlmClient;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// 1 件の実行サンプル（改善案生成の material）。
#[derive(Debug, Clone)]
pub struct ExecutionSample {
    /// ユーザー依頼のテキスト
    pub request: String,
    /// 最終的な ops outcome (`completed` / `failed` / `error` / `no_action`)
    pub outcome: String,
    /// 失敗した場合の要約（成功なら空）
    pub failure_summary: String,
}

/// LLM が生成した改善案 1 件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptCandidate {
    /// 改善後 system_prompt 全文
    pub prompt: String,
    /// LLM が書いた「何を変えたか / なぜ」の理由
    pub rationale: String,
    /// score_candidate の結果（0.0-100.0）
    #[serde(default)]
    pub score: f64,
    /// 採点 LLM による判定理由
    #[serde(default)]
    pub score_rationale: String,
}

/// evolution サイクルの最終結果。
#[derive(Debug)]
pub struct EvolutionResult {
    pub candidates: Vec<PromptCandidate>,
    /// `candidates` の中で最高スコアの index。tie の場合は最初の方。
    pub best_index: usize,
    /// 候補生成時に LLM が返した raw text（JSON parse に失敗したときのデバッグ用）
    pub raw_generation: String,
}

impl EvolutionResult {
    pub fn best(&self) -> &PromptCandidate {
        &self.candidates[self.best_index]
    }
}

/// 候補生成プロンプト（system）のテンプレート。
const GENERATOR_SYSTEM_PROMPT: &str = "You are a prompt engineer. Given a current system_prompt \
that guides an ops agent, plus recent success and failure samples from its runs, \
propose 3 distinct improved versions. Each version should address different failure \
patterns you observe. \
\n\n\
Respond with ONLY valid JSON in this exact shape (no markdown, no prose outside JSON):\n\
{\"candidates\": [{\"prompt\": \"...\", \"rationale\": \"...\"}, ...]}";

/// 採点プロンプト（system）のテンプレート。
const SCORER_SYSTEM_PROMPT: &str = "You are a prompt evaluator. Given a candidate system_prompt \
and recent failure samples, judge how well this candidate would avoid those failures \
if used going forward. \
\n\n\
Respond with ONLY valid JSON in this exact shape (no markdown):\n\
{\"score\": <0-100 integer>, \"rationale\": \"<why this score>\"}";

/// 改善案 3 つを生成する。LLM 呼び出し 1 回。
pub async fn generate_candidates(
    llm: &dyn LlmClient,
    model: &str,
    current_prompt: &str,
    samples: &[ExecutionSample],
) -> Result<(Vec<PromptCandidate>, String)> {
    let user_msg = build_generator_user_message(current_prompt, samples);
    let response = llm
        .send(
            model,
            4096,
            Some(GENERATOR_SYSTEM_PROMPT),
            &[Message::user_text(user_msg)],
            None,
        )
        .await
        .context("LLM send failed during candidate generation")?;

    let raw = extract_text(&response.content);
    let candidates = parse_generator_response(&raw)
        .with_context(|| format!("Failed to parse candidate JSON. Raw: {raw}"))?;
    Ok((candidates, raw))
}

/// 1 候補を採点する。LLM 呼び出し 1 回。
pub async fn score_candidate(
    llm: &dyn LlmClient,
    model: &str,
    candidate: &str,
    failure_samples: &[ExecutionSample],
) -> Result<(f64, String)> {
    let user_msg = build_scorer_user_message(candidate, failure_samples);
    let response = llm
        .send(
            model,
            1024,
            Some(SCORER_SYSTEM_PROMPT),
            &[Message::user_text(user_msg)],
            None,
        )
        .await
        .context("LLM send failed during scoring")?;

    let raw = extract_text(&response.content);
    parse_scorer_response(&raw)
        .with_context(|| format!("Failed to parse score JSON. Raw: {raw}"))
}

/// 生成 → 採点 → 最良選択を一気通貫で実行。
/// LLM 呼び出しは候補生成 1 回 + 採点 3 回 = 合計 4 回。
pub async fn run_evolution(
    llm: &dyn LlmClient,
    model: &str,
    current_prompt: &str,
    samples: &[ExecutionSample],
) -> Result<EvolutionResult> {
    let (mut candidates, raw_generation) =
        generate_candidates(llm, model, current_prompt, samples).await?;
    if candidates.is_empty() {
        anyhow::bail!("Generator returned zero candidates");
    }

    let failure_samples: Vec<ExecutionSample> = samples
        .iter()
        .filter(|s| s.outcome == "failed" || s.outcome == "error")
        .cloned()
        .collect();

    for c in &mut candidates {
        match score_candidate(llm, model, &c.prompt, &failure_samples).await {
            Ok((score, rationale)) => {
                c.score = score;
                c.score_rationale = rationale;
            }
            Err(e) => {
                // 1 候補の採点失敗で全体を落とさない。スコア 0 で継続。
                tracing::warn!("Candidate scoring failed, marking 0: {e:#}");
                c.score = 0.0;
                c.score_rationale = format!("scoring failed: {e:#}");
            }
        }
    }

    let best_index = candidates
        .iter()
        .enumerate()
        .max_by(|a, b| {
            a.1.score
                .partial_cmp(&b.1.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(0);

    Ok(EvolutionResult {
        candidates,
        best_index,
        raw_generation,
    })
}

/// Slack 通知用フォーマット。diff は出さない（Phase 2 で実装）。
pub fn format_evolution_proposal(result: &EvolutionResult, repo_key: &str) -> String {
    let best = result.best();
    let others: Vec<String> = result
        .candidates
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != result.best_index)
        .map(|(i, c)| format!("  - 候補{} (score {:.1}): {}", i + 1, c.score, c.rationale))
        .collect();

    format!(
        ":sparkles: *Prompt Evolution 提案* (repo: `{}`)\n\n\
         *採用候補* (score {:.1}):\n\
         理由: {}\n\
         採点理由: {}\n\n\
         *他の候補:*\n{}\n\n\
         _この提案は自動生成です。採用前に prompt 内容を確認してください。_",
        repo_key,
        best.score,
        best.rationale,
        best.score_rationale,
        if others.is_empty() {
            "  (なし)".to_string()
        } else {
            others.join("\n")
        },
    )
}

// ============================================================================
// Internals
// ============================================================================

fn build_generator_user_message(current_prompt: &str, samples: &[ExecutionSample]) -> String {
    let mut buf = String::new();
    buf.push_str("## Current system_prompt\n\n");
    buf.push_str(current_prompt);
    buf.push_str("\n\n## Recent execution samples\n\n");
    for (i, s) in samples.iter().enumerate() {
        buf.push_str(&format!(
            "### Sample {} (outcome: {})\nRequest: {}\n",
            i + 1,
            s.outcome,
            s.request
        ));
        if !s.failure_summary.is_empty() {
            buf.push_str(&format!("Failure: {}\n", s.failure_summary));
        }
        buf.push('\n');
    }
    buf.push_str(
        "Propose 3 improved versions that address the observed failures. \
         Keep the overall structure but revise instructions where warranted.",
    );
    buf
}

fn build_scorer_user_message(candidate: &str, failure_samples: &[ExecutionSample]) -> String {
    let mut buf = String::new();
    buf.push_str("## Candidate system_prompt\n\n");
    buf.push_str(candidate);
    buf.push_str("\n\n## Failure samples to judge against\n\n");
    if failure_samples.is_empty() {
        buf.push_str("(No failure samples available — score based on prompt quality alone.)\n");
    } else {
        for (i, s) in failure_samples.iter().enumerate() {
            buf.push_str(&format!(
                "### Failure {}\nRequest: {}\nObserved failure: {}\n\n",
                i + 1,
                s.request,
                s.failure_summary
            ));
        }
    }
    buf.push_str("Score 0-100 on how well this candidate would likely prevent these failures.");
    buf
}

fn extract_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Deserialize)]
struct GeneratorResponse {
    candidates: Vec<PromptCandidate>,
}

fn parse_generator_response(raw: &str) -> Result<Vec<PromptCandidate>> {
    let trimmed = strip_json_fence(raw);
    let parsed: GeneratorResponse =
        serde_json::from_str(trimmed).map_err(|e| anyhow!("JSON parse error: {e}"))?;
    Ok(parsed.candidates)
}

#[derive(Deserialize)]
struct ScorerResponse {
    score: f64,
    #[serde(default)]
    rationale: String,
}

fn parse_scorer_response(raw: &str) -> Result<(f64, String)> {
    let trimmed = strip_json_fence(raw);
    let parsed: ScorerResponse =
        serde_json::from_str(trimmed).map_err(|e| anyhow!("JSON parse error: {e}"))?;
    Ok((parsed.score.clamp(0.0, 100.0), parsed.rationale))
}

/// LLM が `” ```json ... ``` ”` で囲んでくることがあるので外す。
fn strip_json_fence(raw: &str) -> &str {
    let t = raw.trim();
    if let Some(rest) = t.strip_prefix("```json") {
        rest.trim_start_matches('\n')
            .trim_end_matches("```")
            .trim_end_matches('\n')
            .trim()
    } else if let Some(rest) = t.strip_prefix("```") {
        rest.trim_start_matches('\n')
            .trim_end_matches("```")
            .trim_end_matches('\n')
            .trim()
    } else {
        t
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use agent_harness::types::{LlmResponse, StopReason, ToolDefinition, Usage};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Mock LLM: 送信されたメッセージを順に記録し、事前にセットした応答を返す。
    struct MockLlm {
        responses: Mutex<Vec<String>>,
        calls: Mutex<Vec<String>>,
    }

    impl MockLlm {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().map(String::from).rev().collect()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn send(
            &self,
            _model: &str,
            _max_tokens: u32,
            _system_prompt: Option<&str>,
            messages: &[Message],
            _tools: Option<&[ToolDefinition]>,
        ) -> Result<LlmResponse> {
            let last = messages
                .last()
                .and_then(|m| m.content.first())
                .map(|b| match b {
                    ContentBlock::Text { text } => text.clone(),
                    _ => String::new(),
                })
                .unwrap_or_default();
            self.calls.lock().unwrap().push(last);
            let next = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| "{}".to_string());
            Ok(LlmResponse {
                content: vec![ContentBlock::Text { text: next }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            })
        }
    }

    fn sample(outcome: &str, request: &str, failure: &str) -> ExecutionSample {
        ExecutionSample {
            request: request.to_string(),
            outcome: outcome.to_string(),
            failure_summary: failure.to_string(),
        }
    }

    #[test]
    fn strip_json_fence_plain_passthrough() {
        assert_eq!(strip_json_fence(r#"{"a":1}"#), r#"{"a":1}"#);
    }

    #[test]
    fn strip_json_fence_removes_markdown_fence() {
        assert_eq!(
            strip_json_fence("```json\n{\"a\":1}\n```"),
            r#"{"a":1}"#
        );
        assert_eq!(strip_json_fence("```\n{\"a\":1}\n```"), r#"{"a":1}"#);
    }

    #[test]
    fn parse_generator_response_ok() {
        let raw = r#"{"candidates":[{"prompt":"p1","rationale":"r1"},{"prompt":"p2","rationale":"r2"},{"prompt":"p3","rationale":"r3"}]}"#;
        let cs = parse_generator_response(raw).unwrap();
        assert_eq!(cs.len(), 3);
        assert_eq!(cs[0].prompt, "p1");
        assert_eq!(cs[2].rationale, "r3");
    }

    #[test]
    fn parse_generator_response_with_fence() {
        let raw = "```json\n{\"candidates\":[{\"prompt\":\"x\",\"rationale\":\"y\"}]}\n```";
        let cs = parse_generator_response(raw).unwrap();
        assert_eq!(cs.len(), 1);
    }

    #[test]
    fn parse_scorer_response_ok() {
        let (s, r) = parse_scorer_response(r#"{"score": 72, "rationale": "solid"}"#).unwrap();
        assert!((s - 72.0).abs() < 0.001);
        assert_eq!(r, "solid");
    }

    #[test]
    fn parse_scorer_response_clamps_out_of_range() {
        let (s, _) = parse_scorer_response(r#"{"score": 150, "rationale": ""}"#).unwrap();
        assert!((s - 100.0).abs() < 0.001);
        let (s2, _) = parse_scorer_response(r#"{"score": -10, "rationale": ""}"#).unwrap();
        assert!(s2.abs() < 0.001);
    }

    #[tokio::test]
    async fn run_evolution_end_to_end_picks_highest_score() {
        let llm = MockLlm::new(vec![
            r#"{"candidates":[{"prompt":"A","rationale":"ra"},{"prompt":"B","rationale":"rb"},{"prompt":"C","rationale":"rc"}]}"#,
            r#"{"score": 40, "rationale": "weak"}"#,
            r#"{"score": 85, "rationale": "strong"}"#,
            r#"{"score": 60, "rationale": "ok"}"#,
        ]);

        let samples = vec![
            sample("completed", "do X", ""),
            sample("failed", "do Y", "timed out"),
            sample("error", "do Z", "permission denied"),
        ];

        let result = run_evolution(&llm, "claude-opus", "current prompt", &samples)
            .await
            .unwrap();

        assert_eq!(result.candidates.len(), 3);
        assert_eq!(result.best_index, 1);
        assert!((result.best().score - 85.0).abs() < 0.001);
        // 生成 1 回 + 採点 3 回
        assert_eq!(llm.call_count(), 4);
    }

    #[tokio::test]
    async fn run_evolution_scoring_failure_does_not_abort() {
        // 2 件目の採点応答を壊す
        let llm = MockLlm::new(vec![
            r#"{"candidates":[{"prompt":"A","rationale":"ra"},{"prompt":"B","rationale":"rb"}]}"#,
            r#"{"score": 50, "rationale": ""}"#,
            r#"not even json"#,
        ]);

        let samples = vec![sample("failed", "req", "boom")];
        let result = run_evolution(&llm, "m", "cur", &samples).await.unwrap();
        assert_eq!(result.candidates.len(), 2);
        // 候補 1 は 50、候補 2 は scoring 失敗で 0
        assert!((result.candidates[0].score - 50.0).abs() < 0.001);
        assert!(result.candidates[1].score.abs() < 0.001);
        assert!(result.candidates[1].score_rationale.contains("scoring failed"));
        assert_eq!(result.best_index, 0);
    }

    #[tokio::test]
    async fn run_evolution_generator_empty_is_error() {
        let llm = MockLlm::new(vec![r#"{"candidates":[]}"#]);
        let r = run_evolution(&llm, "m", "cur", &[]).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn failure_samples_only_passed_to_scorer() {
        let llm = MockLlm::new(vec![
            r#"{"candidates":[{"prompt":"A","rationale":"r"}]}"#,
            r#"{"score": 50, "rationale": "ok"}"#,
        ]);
        let samples = vec![
            sample("completed", "success req", ""),
            sample("failed", "fail req", "boom"),
        ];
        run_evolution(&llm, "m", "cur", &samples).await.unwrap();

        let calls = llm.calls.lock().unwrap();
        // Generator call: both samples included
        assert!(calls[0].contains("success req"));
        assert!(calls[0].contains("fail req"));
        // Scorer call: only failure sample
        assert!(!calls[1].contains("success req"));
        assert!(calls[1].contains("fail req"));
    }

    #[test]
    fn format_evolution_proposal_includes_repo_and_scores() {
        let result = EvolutionResult {
            candidates: vec![
                PromptCandidate {
                    prompt: "A".into(),
                    rationale: "ra".into(),
                    score: 40.0,
                    score_rationale: "weak".into(),
                },
                PromptCandidate {
                    prompt: "B".into(),
                    rationale: "rb".into(),
                    score: 85.0,
                    score_rationale: "strong".into(),
                },
            ],
            best_index: 1,
            raw_generation: String::new(),
        };
        let msg = format_evolution_proposal(&result, "myrepo");
        assert!(msg.contains("myrepo"));
        assert!(msg.contains("85"));
        assert!(msg.contains("strong"));
        assert!(msg.contains("候補1"));
    }
}
