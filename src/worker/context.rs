use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[allow(dead_code)]
const MAX_CONTEXT_ENTRIES: usize = 20;
#[allow(dead_code)]
const MAX_MEMORY_ENTRIES: usize = 30;

// --- システムプロンプト構築 ---

/// 各ワーカーモジュール共通のシステムプロンプト構築ヘルパー
pub fn build_system_prompt(
    soul: &str,
    fallback_soul: &str,
    rules: &str,
    skill: &str,
    extra: Option<&str>,
) -> String {
    let base = if soul.is_empty() { fallback_soul } else { soul };
    let mut parts = vec![base.to_string(), rules.to_string()];
    if !skill.is_empty() {
        parts.push(format!("## スキル・規約\n{}", skill));
    }
    if let Some(e) = extra {
        parts.push(e.to_string());
    }
    parts.join("\n\n")
}

// ============================================================================
// パス解決 — グローバル（オーケストレータ層: .agent/）
// ============================================================================

pub fn global_context_path(repos_base_dir: &str) -> PathBuf {
    PathBuf::from(repos_base_dir).join(".agent").join("context.md")
}

pub fn global_memory_path(repos_base_dir: &str) -> PathBuf {
    PathBuf::from(repos_base_dir).join(".agent").join("memory.md")
}

pub fn soul_path(repos_base_dir: &str) -> PathBuf {
    PathBuf::from(repos_base_dir).join(".agent").join("soul.md")
}

pub fn skill_path(repos_base_dir: &str) -> PathBuf {
    PathBuf::from(repos_base_dir).join(".agent").join("skill.md")
}

// ============================================================================
// パス解決 — リポジトリ層（per-repo: {repo}/.agent/）
// ============================================================================

pub fn repo_context_path(repo: &Path) -> PathBuf {
    repo.join(".agent").join("context.md")
}

pub fn repo_memory_path(repo: &Path) -> PathBuf {
    repo.join(".agent").join("memory.md")
}

// ============================================================================
// 読み込み（なければ空文字列）
// ============================================================================

pub fn read_context(repos_base_dir: &str) -> String {
    std::fs::read_to_string(global_context_path(repos_base_dir)).unwrap_or_default()
}

pub fn read_memory(repos_base_dir: &str) -> String {
    std::fs::read_to_string(global_memory_path(repos_base_dir)).unwrap_or_default()
}

pub fn read_soul(repos_base_dir: &str) -> String {
    std::fs::read_to_string(soul_path(repos_base_dir)).unwrap_or_default()
}

pub fn read_skill(repos_base_dir: &str) -> String {
    std::fs::read_to_string(skill_path(repos_base_dir)).unwrap_or_default()
}

fn read_repo_context(repo: &Path) -> String {
    std::fs::read_to_string(repo_context_path(repo)).unwrap_or_default()
}

fn read_repo_memory(repo: &Path) -> String {
    std::fs::read_to_string(repo_memory_path(repo)).unwrap_or_default()
}

// ============================================================================
// 合成関数（per-repo + global）
// ============================================================================

fn merge_layers(repo_content: &str, global_content: &str, repo_header: &str, global_header: &str) -> String {
    let mut parts = Vec::new();
    if !repo_content.is_empty() {
        parts.push(format!("## {}\n{}", repo_header, repo_content));
    }
    if !global_content.is_empty() {
        parts.push(format!("## {}\n{}", global_header, global_content));
    }
    parts.join("\n\n")
}

/// per-repo WORKFLOW.md の body を soul にマージして返す
pub fn merged_soul(repos_base_dir: &str, repo: Option<&Path>) -> String {
    let global_soul = read_soul(repos_base_dir);
    let workflow_body = repo
        .and_then(super::workflow::load)
        .map(|wf| wf.body)
        .unwrap_or_default();

    if workflow_body.is_empty() {
        global_soul
    } else if global_soul.is_empty() {
        workflow_body
    } else {
        format!("{}\n\n## リポジトリ固有ルール（WORKFLOW.md）\n{}", global_soul, workflow_body)
    }
}

/// per-repo context → global context を結合して返す
pub fn merged_context(repos_base_dir: &str, repo: Option<&Path>) -> String {
    let global = read_context(repos_base_dir);
    let repo_ctx = repo.map(read_repo_context).unwrap_or_default();
    merge_layers(&repo_ctx, &global, "リポジトリ作業履歴", "横断作業履歴")
}

/// per-repo memory → global memory を結合して返す
pub fn merged_memory(repos_base_dir: &str, repo: Option<&Path>) -> String {
    let global = read_memory(repos_base_dir);
    let repo_mem = repo.map(read_repo_memory).unwrap_or_default();
    merge_layers(&repo_mem, &global, "リポジトリ学習メモ", "横断学習メモ")
}

// ============================================================================
// WorkContext — AI モジュール共通の作業コンテキスト
// ============================================================================

/// analyzer / decomposer / executor に渡す共通コンテキスト
pub struct WorkContext {
    pub repo_path: std::path::PathBuf,
    pub max_turns: u32,
    pub soul: String,
    pub skill: String,
    pub context: String,
    pub memory: String,
}

// ============================================================================
// 追記（ローテーション付き）
// ============================================================================

fn append_rotated(path: &PathBuf, entry: &str, max: usize) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create dir {}", parent.display()))?;
    }

    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<&str> = existing.lines().filter(|l| !l.is_empty()).collect();
    lines.push(entry);

    if lines.len() > max {
        lines = lines[lines.len() - max..].to_vec();
    }

    let content = lines.join("\n") + "\n";
    std::fs::write(path, content)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// グローバル context.md に1行追記（20件ローテーション）
pub fn append_context(repos_base_dir: &str, entry: &str) -> Result<()> {
    append_rotated(&global_context_path(repos_base_dir), entry, MAX_CONTEXT_ENTRIES)
}

/// グローバル memory.md に1行追記（30件ローテーション）
#[allow(dead_code)]
pub fn append_memory(repos_base_dir: &str, entry: &str) -> Result<()> {
    append_rotated(&global_memory_path(repos_base_dir), entry, MAX_MEMORY_ENTRIES)
}

/// per-repo context.md に1行追記（20件ローテーション）
pub fn append_repo_context(repo: &Path, entry: &str) -> Result<()> {
    append_rotated(&repo_context_path(repo), entry, MAX_CONTEXT_ENTRIES)
}

/// per-repo memory.md に1行追記（30件ローテーション）
#[allow(dead_code)]
pub fn append_repo_memory(repo: &Path, entry: &str) -> Result<()> {
    append_rotated(&repo_memory_path(repo), entry, MAX_MEMORY_ENTRIES)
}

// ============================================================================
// 完了タスク記録（global + per-repo）
// ============================================================================

/// context.md にタスク完了記録を追記（global + per-repo）
pub fn append_completed_task(
    repos_base_dir: &str,
    task: &crate::db::CodingTask,
    repo_path: Option<&Path>,
) {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let entry = format!(
        "[DONE] #{} {} ({})",
        task.id, task.asana_task_name, date
    );
    if let Err(e) = append_context(repos_base_dir, &entry) {
        tracing::error!("Failed to append completed task to global context: {}", e);
    }
    if let Some(repo) = repo_path {
        if let Err(e) = append_repo_context(repo, &entry) {
            tracing::error!("Failed to append completed task to repo context: {}", e);
        }
    }
}

// ============================================================================
// マイグレーション
// ============================================================================

/// 旧パス（repos_base_dir/soul.md 等）→ 新パス（repos_base_dir/.agent/soul.md 等）に移行
pub fn migrate_context_files(repos_base_dir: &str) -> Result<()> {
    let base = PathBuf::from(repos_base_dir);
    let agent_dir = base.join(".agent");

    let files = [
        ("soul.md", "soul.md"),
        ("skill.md", "skill.md"),
        ("context.md", "context.md"),
        ("memory.md", "memory.md"),
    ];

    // 移行対象の有無を先にチェック
    let needs_migration = files
        .iter()
        .any(|(old, new)| base.join(old).exists() && !agent_dir.join(new).exists());

    if needs_migration {
        std::fs::create_dir_all(&agent_dir)
            .with_context(|| format!("Failed to create .agent dir: {}", agent_dir.display()))?;
    }

    let mut migrated = false;
    for (old_name, new_name) in &files {
        let old_path = base.join(old_name);
        let new_path = agent_dir.join(new_name);

        if old_path.exists() && !new_path.exists() {
            std::fs::rename(&old_path, &new_path).with_context(|| {
                format!(
                    "Failed to migrate {} → {}",
                    old_path.display(),
                    new_path.display()
                )
            })?;
            tracing::info!(
                "Migrated {} → {}",
                old_path.display(),
                new_path.display()
            );
            migrated = true;
        }
    }

    if !migrated {
        tracing::debug!("No context files to migrate");
    }

    Ok(())
}

// ============================================================================
// 出力パース
// ============================================================================

#[allow(dead_code)]
/// executor 出力から SUMMARY: 行を抽出（最後に見つかったものを採用）
pub fn extract_summary(output: &str) -> Option<String> {
    output
        .lines()
        .rev()
        .find(|line| line.starts_with("SUMMARY:"))
        .map(|line| line.trim_start_matches("SUMMARY:").trim().to_string())
}

#[allow(dead_code)]
/// executor 出力から MEMORY: 行を抽出（最後に見つかったものを採用）
pub fn extract_memory(output: &str) -> Option<String> {
    output
        .lines()
        .rev()
        .find(|line| line.starts_with("MEMORY:"))
        .map(|line| line.trim_start_matches("MEMORY:").trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_extract_summary_found() {
        let output = "some output\nSUMMARY: フォームバリデーションを実装\n";
        assert_eq!(
            extract_summary(output),
            Some("フォームバリデーションを実装".to_string())
        );
    }

    #[test]
    fn test_extract_summary_not_found() {
        let output = "some output without summary\n";
        assert_eq!(extract_summary(output), None);
    }

    #[test]
    fn test_extract_summary_last_wins() {
        let output = "SUMMARY: 最初の要約\nother\nSUMMARY: 最終の要約\n";
        assert_eq!(extract_summary(output), Some("最終の要約".to_string()));
    }

    #[test]
    fn test_extract_memory_found() {
        let output = "output\nMEMORY: このリポジトリはjjを使う\n";
        assert_eq!(
            extract_memory(output),
            Some("このリポジトリはjjを使う".to_string())
        );
    }

    #[test]
    fn test_extract_memory_not_found() {
        let output = "no memory line\n";
        assert_eq!(extract_memory(output), None);
    }

    #[test]
    fn test_append_context_and_read() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();

        append_context(base, "[2026-03-03 15:00] repo | task1 | completed | summary1").unwrap();
        append_context(base, "[2026-03-03 16:00] repo | task2 | completed | summary2").unwrap();

        let ctx = read_context(base);
        let lines: Vec<&str> = ctx.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("task1"));
        assert!(lines[1].contains("task2"));
    }

    #[test]
    fn test_append_context_max_entries() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();

        for i in 0..25 {
            append_context(base, &format!("entry {}", i)).unwrap();
        }

        let ctx = read_context(base);
        let lines: Vec<&str> = ctx.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), MAX_CONTEXT_ENTRIES);
        assert_eq!(lines[0], "entry 5");
        assert_eq!(lines[19], "entry 24");
    }

    #[test]
    fn test_append_memory_max_entries() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();

        for i in 0..35 {
            append_memory(base, &format!("memory {}", i)).unwrap();
        }

        let mem = read_memory(base);
        let lines: Vec<&str> = mem.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), MAX_MEMORY_ENTRIES);
        assert_eq!(lines[0], "memory 5");
        assert_eq!(lines[29], "memory 34");
    }

    #[test]
    fn test_read_no_file() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();
        assert_eq!(read_context(base), "");
        assert_eq!(read_memory(base), "");
        assert_eq!(read_soul(base), "");
        assert_eq!(read_skill(base), "");
    }

    #[test]
    fn test_merged_context() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();
        let repo = dir.path().join("my-repo");
        std::fs::create_dir_all(&repo).unwrap();

        // global context
        append_context(base, "global entry 1").unwrap();

        // repo context
        append_repo_context(&repo, "repo entry 1").unwrap();

        let merged = merged_context(base, Some(&repo));
        assert!(merged.contains("リポジトリ作業履歴"));
        assert!(merged.contains("repo entry 1"));
        assert!(merged.contains("横断作業履歴"));
        assert!(merged.contains("global entry 1"));
    }

    #[test]
    fn test_merged_context_no_repo() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();

        append_context(base, "global only").unwrap();

        let merged = merged_context(base, None);
        assert!(merged.contains("横断作業履歴"));
        assert!(merged.contains("global only"));
        assert!(!merged.contains("リポジトリ作業履歴"));
    }

    #[test]
    fn test_merged_memory() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();
        let repo = dir.path().join("my-repo");
        std::fs::create_dir_all(&repo).unwrap();

        append_memory(base, "global memo").unwrap();
        append_repo_memory(&repo, "repo memo").unwrap();

        let merged = merged_memory(base, Some(&repo));
        assert!(merged.contains("リポジトリ学習メモ"));
        assert!(merged.contains("repo memo"));
        assert!(merged.contains("横断学習メモ"));
        assert!(merged.contains("global memo"));
    }

    #[test]
    fn test_migrate_context_files() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();

        // 旧パスにファイルを作成
        std::fs::write(dir.path().join("soul.md"), "soul content").unwrap();
        std::fs::write(dir.path().join("skill.md"), "skill content").unwrap();
        std::fs::write(dir.path().join("context.md"), "context content").unwrap();
        std::fs::write(dir.path().join("memory.md"), "memory content").unwrap();

        // マイグレーション実行
        migrate_context_files(base).unwrap();

        // 旧パスは消えている
        assert!(!dir.path().join("soul.md").exists());
        assert!(!dir.path().join("skill.md").exists());
        assert!(!dir.path().join("context.md").exists());
        assert!(!dir.path().join("memory.md").exists());

        // 新パスに移動している
        let agent_dir = dir.path().join(".agent");
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("soul.md")).unwrap(),
            "soul content"
        );
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("skill.md")).unwrap(),
            "skill content"
        );
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("context.md")).unwrap(),
            "context content"
        );
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("memory.md")).unwrap(),
            "memory content"
        );

        // 2回目は何もしない
        migrate_context_files(base).unwrap();
    }

    #[test]
    fn test_migrate_no_old_files() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();

        // 旧ファイルがなければ何もしない
        migrate_context_files(base).unwrap();
        assert!(!dir.path().join(".agent").exists());
    }

    #[test]
    fn test_migrate_skips_if_new_exists() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_str().unwrap();

        // 旧パスと新パスの両方にファイルがある場合
        std::fs::write(dir.path().join("soul.md"), "old soul").unwrap();
        let agent_dir = dir.path().join(".agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("soul.md"), "new soul").unwrap();

        migrate_context_files(base).unwrap();

        // 新パスは上書きされない
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("soul.md")).unwrap(),
            "new soul"
        );
        // 旧パスは残る（衝突回避）
        assert!(dir.path().join("soul.md").exists());
    }

    #[test]
    fn test_repo_context_append_and_read() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("test-repo");
        std::fs::create_dir_all(&repo).unwrap();

        append_repo_context(&repo, "repo task done").unwrap();
        let content = read_repo_context(&repo);
        assert!(content.contains("repo task done"));
    }
}
