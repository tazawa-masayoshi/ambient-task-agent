use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

/// soul.md が無い場合のフォールバック
const FALLBACK_SOUL: &str = "\
あなたは自律コーディングエージェントの要件定義アナリストです。
Asanaタスクを受け取り、コードベースを調査して要件を具体化します。";

/// アナリスト固有のルール（常に付与）
const ANALYZER_RULES: &str = "\
## ルール
- コードベースを十分に読んでから要件を整理すること
- ファイルの変更は一切行わないこと（読み取り専用）
- CLAUDE.md があれば必ず読み、プロジェクトの規約に従うこと
- 既存のコードパターンや命名規則を尊重すること

## 出力フォーマット
Slack に投稿される前提で、以下の構造で出力してください（Markdown形式）:

### 概要
タスクの要約と方針（2-3文）

### 要件
- 具体的な要件を箇条書き

### 影響範囲
- 関連ファイルと現状の実装
- 変更が必要なファイル一覧

### 実装方針
1. 具体的なアプローチ（ファイルパス付き）
2. ...

### リスク・注意点
- 既存機能への影響、エッジケースなど

### なぜこのタスクをやるのか
- ビジネス価値・ユーザー影響
- やらない場合のリスク

### 成功指標
- 完了の定義（何ができたら Done か）
- 確認方法（どうテストするか）";

fn build_system_prompt(soul: &str, skill: &str) -> String {
    super::context::build_system_prompt(soul, FALLBACK_SOUL, ANALYZER_RULES, skill, None)
}

/// claude -p で要件定義を生成（read-only）
pub async fn analyze_task(
    task_name: &str,
    description: &str,
    repo_path: &Path,
    max_turns: u32,
    soul: &str,
    skill: &str,
    context: &str,
    memory: &str,
) -> Result<String> {
    let system_prompt = build_system_prompt(soul, skill);

    let mut prompt_parts = vec![format!("## タスク\n{}", task_name)];

    if !description.is_empty() {
        prompt_parts.push(format!("## 詳細\n{}", description));
    }
    if !context.is_empty() {
        prompt_parts.push(format!("## 直近の作業履歴\n{}", context));
    }
    if !memory.is_empty() {
        prompt_parts.push(format!("## 過去の学び・メモ\n{}", memory));
    }

    let prompt = prompt_parts.join("\n\n");

    tracing::info!(
        "Running analyzer claude -p in {} (max_turns={})",
        repo_path.display(),
        max_turns
    );

    let output = Command::new("claude")
        .args([
            "-p",
            &prompt,
            "--system-prompt",
            &system_prompt,
            "--max-turns",
            &max_turns.to_string(),
        ])
        .current_dir(repo_path)
        .output()
        .await
        .context("Failed to execute claude -p for analysis")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "claude -p analysis failed (exit {}): {}",
            output.status,
            stderr
        );
    }

    let analysis = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(analysis)
}
