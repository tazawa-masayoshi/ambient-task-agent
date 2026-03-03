use anyhow::{Context, Result};
use std::path::PathBuf;

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

// --- パス解決 ---

pub fn context_path(repos_base_dir: &str) -> PathBuf {
    PathBuf::from(repos_base_dir).join("context.md")
}

pub fn memory_path(repos_base_dir: &str) -> PathBuf {
    PathBuf::from(repos_base_dir).join("memory.md")
}

pub fn soul_path(repos_base_dir: &str) -> PathBuf {
    PathBuf::from(repos_base_dir).join("soul.md")
}

pub fn skill_path(repos_base_dir: &str) -> PathBuf {
    PathBuf::from(repos_base_dir).join("skill.md")
}

// --- 読み込み（なければ空文字列） ---

pub fn read_context(repos_base_dir: &str) -> String {
    std::fs::read_to_string(context_path(repos_base_dir)).unwrap_or_default()
}

pub fn read_memory(repos_base_dir: &str) -> String {
    std::fs::read_to_string(memory_path(repos_base_dir)).unwrap_or_default()
}

pub fn read_soul(repos_base_dir: &str) -> String {
    std::fs::read_to_string(soul_path(repos_base_dir)).unwrap_or_default()
}

pub fn read_skill(repos_base_dir: &str) -> String {
    std::fs::read_to_string(skill_path(repos_base_dir)).unwrap_or_default()
}

// --- 追記（ローテーション付き） ---

#[allow(dead_code)]
fn append_rotated(path: &PathBuf, entry: &str, max: usize) -> Result<()> {
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

#[allow(dead_code)]
/// context.md に1行追記（20件ローテーション）
pub fn append_context(repos_base_dir: &str, entry: &str) -> Result<()> {
    append_rotated(&context_path(repos_base_dir), entry, MAX_CONTEXT_ENTRIES)
}

#[allow(dead_code)]
/// memory.md に1行追記（30件ローテーション）
pub fn append_memory(repos_base_dir: &str, entry: &str) -> Result<()> {
    append_rotated(&memory_path(repos_base_dir), entry, MAX_MEMORY_ENTRIES)
}

// --- 完了タスク記録 ---

/// context.md にタスク完了記録を追記
pub fn append_completed_task(repos_base_dir: &str, task: &crate::db::CodingTask) {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let entry = format!(
        "[DONE] #{} {} ({})",
        task.id, task.asana_task_name, date
    );
    if let Err(e) = append_context(repos_base_dir, &entry) {
        tracing::error!("Failed to append completed task to context: {}", e);
    }
}

// --- 出力パース ---

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
}
