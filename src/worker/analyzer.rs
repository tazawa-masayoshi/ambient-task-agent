use anyhow::Result;
use std::path::Path;

use crate::claude::ClaudeRunner;
use crate::execution::RunnerContext;
use super::context::WorkContext;

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
- 確認方法（どうテストするか）

### 複雑度
simple / standard / complex のいずれか1語のみ記載。
判定基準: 変更ファイル数、影響範囲、要件の明確さ、推定作業時間
- simple: 明確な1ファイル修正、typo、設定変更（30分以内）
- standard: 通常の機能追加・バグ修正（30分〜3時間）
- complex: 複数リポジトリ、設計変更、不明確なスコープ（3時間超）";

fn build_system_prompt(soul: &str, skill: &str) -> String {
    super::context::build_system_prompt(soul, FALLBACK_SOUL, ANALYZER_RULES, skill, None)
}

/// 分析テキストから複雑度（simple/standard/complex）を抽出
pub fn extract_complexity(analysis_text: &str) -> Option<String> {
    // "### 複雑度" セクションを探す
    let marker = "### 複雑度";
    let section_start = analysis_text.find(marker)?;
    let after = &analysis_text[section_start + marker.len()..];

    // 次のセクション（### ）までの範囲を取得
    let section_end = after.find("\n### ").unwrap_or(after.len());
    let section = &after[..section_end];

    // simple / standard / complex を探す
    for keyword in &["simple", "standard", "complex"] {
        if section.contains(keyword) {
            return Some(keyword.to_string());
        }
    }

    None
}

/// claude -p で要件定義を生成（read-only）
/// 戻り値: (分析テキスト, 複雑度)
pub async fn analyze_task(
    task_name: &str,
    description: &str,
    wc: &WorkContext,
    log_dir: Option<&Path>,
    runner_ctx: &RunnerContext,
) -> Result<(String, Option<String>)> {
    let system_prompt = build_system_prompt(&wc.soul, &wc.skill);

    let mut prompt_parts = vec![format!("## タスク\n{}", task_name)];

    if !description.is_empty() {
        prompt_parts.push(format!("## 詳細\n{}", description));
    }
    if !wc.context.is_empty() {
        prompt_parts.push(format!("## 直近の作業履歴\n{}", wc.context));
    }
    if !wc.memory.is_empty() {
        prompt_parts.push(format!("## 過去の学び・メモ\n{}", wc.memory));
    }

    let prompt = prompt_parts.join("\n\n");

    let result = ClaudeRunner::new("analyzer", &prompt)
        .system_prompt(&system_prompt)
        .max_turns(wc.max_turns)
        .cwd(&wc.repo_path)
        .optional_log_dir(log_dir)
        .with_context(runner_ctx)
        .run()
        .await?;

    if !result.success {
        anyhow::bail!("claude -p analysis failed: {}", result.error_output());
    }

    let complexity = extract_complexity(&result.stdout);
    Ok((result.stdout, complexity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_complexity_simple() {
        let text = "### 概要\ntypo修正\n\n### 複雑度\nsimple\n\n### 成功指標\nテスト通過";
        assert_eq!(extract_complexity(text), Some("simple".to_string()));
    }

    #[test]
    fn test_extract_complexity_standard() {
        let text = "### 複雑度\nstandard";
        assert_eq!(extract_complexity(text), Some("standard".to_string()));
    }

    #[test]
    fn test_extract_complexity_complex() {
        let text = "some text\n### 複雑度\ncomplex\n### 次のセクション\nfoo";
        assert_eq!(extract_complexity(text), Some("complex".to_string()));
    }

    #[test]
    fn test_extract_complexity_not_found() {
        let text = "### 概要\nタスクの説明";
        assert_eq!(extract_complexity(text), None);
    }

    #[test]
    fn test_extract_complexity_priority_order() {
        // simple が先にマッチする
        let text = "### 複雑度\nsimple (standard のようにも見えるが simple)";
        assert_eq!(extract_complexity(text), Some("simple".to_string()));
    }
}
