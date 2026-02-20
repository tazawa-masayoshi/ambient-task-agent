use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

const SYSTEM_PROMPT: &str = "\
あなたは自律コーディングエージェントのプランナーです。
Asanaタスクを受け取り、コードベースを調査して実装プランを作成します。

## ルール
- コードベースを十分に読んでからプランを作成すること
- ファイルの変更は一切行わないこと（読み取り専用）
- CLAUDE.md があれば必ず読み、プロジェクトの規約に従うこと
- 既存のコードパターンや命名規則を尊重すること

## 出力フォーマット
以下の構造で出力してください:

### 概要
タスクの要約と方針（2-3文）

### 調査結果
- 関連ファイルと現状の実装
- 影響範囲

### 実装ステップ
1. 具体的な変更内容（ファイルパス付き）
2. ...

### リスク・注意点
- 既存機能への影響、エッジケースなど";

/// claude -p でプランを生成（read-only）
pub async fn generate_plan(
    task_name: &str,
    task_notes: &str,
    repo_path: &Path,
    max_turns: u32,
) -> Result<String> {
    let notes_section = if task_notes.is_empty() {
        String::new()
    } else {
        format!("\n\n## 詳細\n{}", task_notes)
    };

    let prompt = format!("## タスク\n{}{}", task_name, notes_section);

    tracing::info!(
        "Running claude -p in {} (max_turns={})",
        repo_path.display(),
        max_turns
    );

    let output = Command::new("claude")
        .args([
            "-p",
            &prompt,
            "--system-prompt",
            SYSTEM_PROMPT,
            "--max-turns",
            &max_turns.to_string(),
        ])
        .current_dir(repo_path)
        .output()
        .await
        .context("Failed to execute claude -p")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude -p failed (exit {}): {}", output.status, stderr);
    }

    let plan = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(plan)
}
