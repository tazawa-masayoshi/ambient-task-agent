use anyhow::Result;
use std::path::Path;

use crate::claude::ClaudeRunner;
use crate::db::OpsMessage;
use crate::execution::RunnerContext;

const OPS_ALLOWED_TOOLS: &str = "Read,Write,Edit,Bash,Glob,Grep";
const OPS_PLAN_ALLOWED_TOOLS: &str = "Read,Glob,Grep,Bash";
const OPS_INCEPTION_ALLOWED_TOOLS: &str = "Read,Glob,Grep,Bash";

const FALLBACK_OPS_SOUL: &str = "\
あなたは定型保守作業を実行する自律エージェントです。
スキルファイルの手順に従い、正確に作業を完了してください。";

const FALLBACK_OPS_PLAN_SOUL: &str = "\
あなたは依頼内容を分析し、作業計画を立てるエージェントです。
コードを読んで問題を特定し、何をどう修正すべきかを具体的に報告してください。";

const FALLBACK_OPS_INCEPTION_SOUL: &str = "\
あなたはプロダクトオーナーとして要件定義を支援するエージェントです。
ユーザーの依頼意図を正確に把握し、具体的な要件とタスクに落とし込んでください。";

const OPS_RULES: &str = "\
## ルール
- 作業手順に従って処理すること
- 不明な点があれば作業を中断し、確認が必要な内容を報告すること

## 出力（重要）
最後に必ずテキストで作業結果を出力すること。ツール操作だけで終了してはいけない。
以下の形式で要約を出力:
- 何を確認/実行したか
- 結果（成功/失敗/対応不要）
- 対応不要の場合はその理由";

const OPS_PLAN_RULES: &str = "\
## ルール
- ファイルの読み取り・検索のみ行い、書き込み・編集は一切行わないこと
- 問題の特定と原因分析を行うこと
- 具体的な修正方針（どのファイルのどの箇所をどう変更すべきか）を報告すること
- 不明な点があれば、その旨を明記すること

## 出力（重要）
最後に必ずテキストで分析結果を出力すること。ツール操作だけで終了してはいけない。
日本語で箇条書きにまとめ、以下を含めること:
- 問題の特定結果
- 原因分析
- 具体的な修正方針（対応不要の場合はその理由）";

const OPS_INCEPTION_TURN1_RULES: &str = "\
## Inception モード — ターン1: Intent 分析と質問生成

依頼を以下の4軸で分析し、不明点を質問してください。

### Intent 分析
- **明確さ**: clear / vague / incomplete のいずれか
- **種別**: feature / bug / refactoring / migration / investigation / その他
- **スコープ**: single-file / module / cross-system のいずれか
- **複雑度**: trivial / simple / moderate / complex のいずれか

### 質問生成の観点（該当するものを質問）
1. 機能的動作・ユーザー操作（何ができるべきか）
2. 性能・セキュリティ・スケーラビリティ要件
3. ユースケース・エッジケース
4. ビジネス目標・成功指標
5. 外部連携・データフロー
6. 品質属性（信頼性・保守性など）

### ルール
- 明確さが clear で複雑度が trivial の場合、質問は最小限（1〜2問）にすること
- 質問が不要な場合は「質問なし」と明記し、ターン2で直接要件整理に進むよう案内すること
- ファイルの読み取り・検索のみ行い、書き込み・編集は一切行わないこと

## 出力（重要）
最後に必ずテキストで以下を出力すること:

### Intent 分析結果
- 明確さ: [clear/vague/incomplete]
- 種別: [...]
- スコープ: [...]
- 複雑度: [...]

### 確認したい点
（質問がある場合）
1. [質問1]
2. [質問2]
...

（質問がない場合）
質問なし。上記の内容で理解できました。返信不要で、そのまま要件整理に進みます。

---
*上記の質問にご回答後、このスレッドに返信してください。*";

const OPS_INCEPTION_TURN2_RULES: &str = "\
## Inception モード — ターン2: 要件整理とタスク分解

会話履歴を踏まえて要件を整理し、Asana 登録用のタスクに分解してください。

### 要件ドキュメントの構造
以下の形式で出力すること:

```
## Intent Summary
- Request type: [feature/bug/refactoring/...]
- Complexity: [trivial/simple/moderate/complex]
- Scope: [single-file/module/cross-system]

## Functional Requirements
- FR-1: ...
- FR-2: ...

## Non-Functional Requirements
- NFR-1: ...（該当する場合のみ）

## Architectural Considerations
- ...（該当する場合のみ）
```

### タスク分解のルール
- INVEST 基準（Independent / Negotiable / Valuable / Estimable / Small / Testable）を満たすこと
- 依存関係がある場合は `depends_on` に先行タスクの番号を記載すること
- ファイルの読み取り・検索のみ行い、書き込み・編集は一切行わないこと

## 出力（重要）
最後に必ずテキストで以下を出力すること。ツール操作だけで終了してはいけない。

[要件ドキュメント（上記形式）]

TASKS_JSON:
[
  {\"title\": \"タスク名\", \"description\": \"詳細説明\", \"depends_on\": [], \"estimate\": \"小/中/大\"},
  ...
]";

pub struct OpsRequest {
    pub message_text: String,
    pub files: Vec<SlackFile>,
}

pub struct SlackFile {
    pub name: String,
    pub url_private_download: String,
}

/// ops モードの種別
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OpsExecMode {
    Execute,
    PlanOnly,
    InceptionTurn1,
    InceptionTurn2,
}

/// ops プロンプトを構築（履歴 + メッセージ + 添付ファイル）
fn build_ops_prompt(req: &OpsRequest, history: &[OpsMessage], download_dir: Option<&str>) -> String {
    let mut parts = Vec::new();
    if !history.is_empty() {
        let history_text: Vec<String> = history
            .iter()
            .map(|m| format!("[{}] {}", m.role, m.content))
            .collect();
        parts.push(format!(
            "## 前回の会話履歴\n<conversation_history>\n{}\n</conversation_history>",
            history_text.join("\n\n")
        ));
    }
    parts.push(format!(
        "## Slackメッセージ\n<user_input>\n{}\n</user_input>",
        req.message_text
    ));
    if !req.files.is_empty() {
        if let Some(dir) = download_dir {
            let file_list: Vec<String> = req
                .files
                .iter()
                .map(|f| {
                    let safe = std::path::Path::new(&f.name)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("download");
                    format!("- {}/{}", dir, safe)
                })
                .collect();
            parts.push(format!(
                "## 添付ファイル（{}/ にダウンロード済み）\n{}",
                dir,
                file_list.join("\n")
            ));
        } else {
            let file_list: Vec<String> = req
                .files
                .iter()
                .map(|f| {
                    let safe = std::path::Path::new(&f.name)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("download");
                    format!("- {}", safe)
                })
                .collect();
            parts.push(format!(
                "## 添付ファイル（Slackに添付、ローカル未保存）\n{}",
                file_list.join("\n")
            ));
        }
    }
    parts.join("\n\n")
}

/// スキルファイルを読み込んで結合
fn read_ops_skills(repo_path: &Path, skill_paths: &[String]) -> String {
    skill_paths
        .iter()
        .filter_map(|p| {
            let full = repo_path.join(p);
            match std::fs::read_to_string(&full) {
                Ok(content) => Some(content),
                Err(e) => {
                    tracing::warn!("Failed to read skill file {}: {}", full.display(), e);
                    None
                }
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// Slack イベント JSON から SlackFile を抽出
pub fn extract_slack_files_from_json(event: &serde_json::Value) -> Vec<SlackFile> {
    event
        .get("files")
        .and_then(|f| f.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|f| {
                    let name = f.get("name")?.as_str()?.to_string();
                    let url = f.get("url_private_download")?.as_str()?.to_string();
                    Some(SlackFile {
                        name,
                        url_private_download: url,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// TASKS_JSON: ブロックを出力から抽出してパース
pub fn extract_tasks_json(output: &str) -> Vec<serde_json::Value> {
    let marker = "TASKS_JSON:";
    if let Some(pos) = output.find(marker) {
        let json_str = output[pos + marker.len()..].trim();
        // 最初の '[' から最後の ']' までを抽出
        if let (Some(start), Some(end)) = (json_str.find('['), json_str.rfind(']')) {
            let slice = &json_str[start..=end];
            if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str(slice) {
                return arr;
            }
        }
    }
    Vec::new()
}

/// ops タスクを claude -p で実行
#[allow(clippy::too_many_arguments)]
pub async fn execute_ops(
    req: &OpsRequest,
    repo_path: &Path,
    skill_paths: &[String],
    soul: &str,
    max_turns: u32,
    log_dir: Option<&Path>,
    runner_ctx: &RunnerContext,
    history: &[OpsMessage],
    download_dir: Option<&str>,
    exec_mode: OpsExecMode,
) -> Result<String> {
    let skill_content = read_ops_skills(repo_path, skill_paths);

    let (base_soul, rules, tools) = match exec_mode {
        OpsExecMode::PlanOnly => {
            let soul = if soul.is_empty() { FALLBACK_OPS_PLAN_SOUL } else { soul };
            (soul, OPS_PLAN_RULES, OPS_PLAN_ALLOWED_TOOLS)
        }
        OpsExecMode::InceptionTurn1 => {
            let soul = if soul.is_empty() { FALLBACK_OPS_INCEPTION_SOUL } else { soul };
            (soul, OPS_INCEPTION_TURN1_RULES, OPS_INCEPTION_ALLOWED_TOOLS)
        }
        OpsExecMode::InceptionTurn2 => {
            let soul = if soul.is_empty() { FALLBACK_OPS_INCEPTION_SOUL } else { soul };
            (soul, OPS_INCEPTION_TURN2_RULES, OPS_INCEPTION_ALLOWED_TOOLS)
        }
        OpsExecMode::Execute => {
            if skill_content.is_empty() {
                anyhow::bail!("No skill files found for ops execution");
            }
            let soul = if soul.is_empty() { FALLBACK_OPS_SOUL } else { soul };
            (soul, OPS_RULES, OPS_ALLOWED_TOOLS)
        }
    };

    // システムプロンプト構築
    let system_prompt = if skill_content.is_empty() {
        format!("{}\n\n{}", base_soul, rules)
    } else {
        format!("{}\n\n## 作業手順\n{}\n\n{}", base_soul, skill_content, rules)
    };

    let prompt = build_ops_prompt(req, history, download_dir);

    let result = ClaudeRunner::new("ops", &prompt)
        .system_prompt(&system_prompt)
        .max_turns(max_turns)
        .allowed_tools(tools)
        .cwd(repo_path)
        .optional_log_dir(log_dir)
        .with_context(runner_ctx)
        .run()
        .await?;

    if !result.success {
        anyhow::bail!("claude -p ops failed: {}", result.error_output());
    }

    Ok(result.stdout.trim().to_string())
}
