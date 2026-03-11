use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const WORKTREES_DIR: &str = ".worktrees";

pub struct Workspace {
    pub worktree_path: PathBuf,
    pub branch_name: String,
    pub main_repo_path: PathBuf,
}

/// git コマンドを実行して stdout を返す (タイムアウト + kill_on_drop)
async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .output();

    let output = tokio::time::timeout(GIT_TIMEOUT, output)
        .await
        .context("git command timed out")?
        .context("failed to spawn git")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// gh コマンドを実行して stdout を返す
async fn gh(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .output();

    let output = tokio::time::timeout(GIT_TIMEOUT, output)
        .await
        .context("gh command timed out")?
        .context("failed to spawn gh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// worktree を作成する
pub async fn create(
    repos_base_dir: &str,
    repo_key: &str,
    task_id: i64,
    base_branch: &str,
) -> Result<Workspace> {
    let main_repo_path = PathBuf::from(repos_base_dir).join(repo_key);
    let worktree_dir = PathBuf::from(repos_base_dir).join(WORKTREES_DIR);
    let dir_name = format!("{}-task-{}", repo_key, task_id);
    let worktree_path = worktree_dir.join(&dir_name);
    let branch_name = format!("agent/task-{}", task_id);

    // worktrees ディレクトリを作成
    std::fs::create_dir_all(&worktree_dir)
        .with_context(|| format!("Failed to create worktrees dir: {}", worktree_dir.display()))?;

    // 既存の worktree があれば削除
    if worktree_path.exists() {
        tracing::info!("Removing existing worktree: {}", worktree_path.display());
        remove(&Workspace {
            worktree_path: worktree_path.clone(),
            branch_name: branch_name.clone(),
            main_repo_path: main_repo_path.clone(),
        })
        .await
        .ok();
    }

    // fetch origin
    git(&main_repo_path, &["fetch", "origin", base_branch]).await?;

    // worktree add
    let wt_str = worktree_path.to_string_lossy().to_string();
    let origin_ref = format!("origin/{}", base_branch);
    git(
        &main_repo_path,
        &["worktree", "add", &wt_str, "-b", &branch_name, &origin_ref],
    )
    .await?;

    tracing::info!(
        "Created worktree: {} (branch: {})",
        worktree_path.display(),
        branch_name
    );

    Ok(Workspace {
        worktree_path,
        branch_name,
        main_repo_path,
    })
}

/// 変更をコミット、プッシュ、PR 作成して PR URL を返す
pub async fn finalize(
    workspace: &Workspace,
    task_name: &str,
    base_branch: &str,
    github_repo: &str,
) -> Result<String> {
    let wt = &workspace.worktree_path;

    // git add -A
    git(wt, &["add", "-A"]).await?;

    // 変更があるか確認
    let diff_result = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(wt)
        .kill_on_drop(true)
        .output();

    let diff_output = tokio::time::timeout(GIT_TIMEOUT, diff_result)
        .await
        .context("git diff timed out")?
        .context("failed to spawn git diff")?;

    if diff_output.status.success() {
        bail!("No changes to commit");
    }

    // commit
    let commit_msg = format!("feat: {}", task_name);
    git(wt, &["commit", "-m", &commit_msg]).await?;

    // push
    git(
        wt,
        &["push", "-u", "origin", &workspace.branch_name],
    )
    .await?;

    // gh pr create --draft
    let pr_title = format!("agent: {}", task_name);
    let pr_body = format!(
        "## Auto-generated PR\n\nTask: {}\nBranch: `{}`\n\nThis PR was created automatically by ambient-task-agent.",
        task_name, workspace.branch_name
    );
    let pr_url = gh(
        wt,
        &[
            "pr", "create",
            "--repo", github_repo,
            "--base", base_branch,
            "--head", &workspace.branch_name,
            "--title", &pr_title,
            "--body", &pr_body,
            "--draft",
        ],
    )
    .await?;

    tracing::info!("Created PR: {}", pr_url);
    Ok(pr_url)
}

/// worktree を削除する
pub async fn remove(workspace: &Workspace) -> Result<()> {
    // worktree remove --force
    let wt_str = workspace.worktree_path.to_string_lossy().to_string();
    let _ = git(
        &workspace.main_repo_path,
        &["worktree", "remove", &wt_str, "--force"],
    )
    .await;

    // branch -D (エラー無視)
    let _ = git(
        &workspace.main_repo_path,
        &["branch", "-D", &workspace.branch_name],
    )
    .await;

    tracing::info!("Removed worktree: {}", workspace.worktree_path.display());
    Ok(())
}

/// worktree が存在するか確認
#[allow(dead_code)]
pub fn exists(repos_base_dir: &str, repo_key: &str, task_id: i64) -> bool {
    let dir_name = format!("{}-task-{}", repo_key, task_id);
    PathBuf::from(repos_base_dir)
        .join(WORKTREES_DIR)
        .join(dir_name)
        .exists()
}

// ── CI ステータス確認 ──

#[derive(Debug)]
pub enum CiStatus {
    /// CI ワークフローがまだ実行中 or キュー待ち
    Pending,
    /// CI 全パス
    Passed,
    /// CI 失敗（エラーサマリー付き）
    Failed { summary: String },
    /// CI ワークフローが見つからない（まだトリガーされていない等）
    NotFound,
}

/// ブランチの最新 CI ステータスを確認する
///
/// `gh run list` で最新の workflow run を取得し、status/conclusion を判定する。
pub async fn check_ci(
    repos_base_dir: &str,
    repo_key: &str,
    github_repo: &str,
    branch_name: &str,
) -> Result<CiStatus> {
    let main_repo_path = PathBuf::from(repos_base_dir).join(repo_key);

    let output = gh(
        &main_repo_path,
        &[
            "run", "list",
            "--branch", branch_name,
            "--repo", github_repo,
            "--limit", "1",
            "--json", "status,conclusion,name",
        ],
    )
    .await?;

    let runs: Vec<serde_json::Value> = serde_json::from_str(&output)
        .context("Failed to parse gh run list output")?;

    if runs.is_empty() {
        return Ok(CiStatus::NotFound);
    }

    let run = &runs[0];
    let status = run["status"].as_str().unwrap_or("");
    let conclusion = run["conclusion"].as_str().unwrap_or("");
    let name = run["name"].as_str().unwrap_or("CI");

    match status {
        "completed" => match conclusion {
            "success" => Ok(CiStatus::Passed),
            _ => Ok(CiStatus::Failed {
                summary: format!("{}: {}", name, conclusion),
            }),
        },
        // queued, in_progress, waiting, requested, pending
        _ => Ok(CiStatus::Pending),
    }
}

/// CI 失敗時の詳細ログを取得する
///
/// `gh run list` で失敗した run ID を取得し、`gh run view --log-failed` でエラー詳細を返す。
pub async fn get_ci_failure_log(
    repos_base_dir: &str,
    repo_key: &str,
    github_repo: &str,
    branch_name: &str,
) -> Result<String> {
    let main_repo_path = PathBuf::from(repos_base_dir).join(repo_key);

    // 最新の失敗 run の ID を取得
    let output = gh(
        &main_repo_path,
        &[
            "run", "list",
            "--branch", branch_name,
            "--repo", github_repo,
            "--limit", "1",
            "--json", "databaseId",
        ],
    )
    .await?;

    let runs: Vec<serde_json::Value> = serde_json::from_str(&output)?;
    if runs.is_empty() {
        bail!("No CI runs found");
    }

    let run_id = runs[0]["databaseId"]
        .as_i64()
        .context("Missing databaseId")?;

    // 失敗ログを取得（--log-failed は失敗ステップのみ出力）
    let log = gh(
        &main_repo_path,
        &[
            "run", "view",
            &run_id.to_string(),
            "--repo", github_repo,
            "--log-failed",
        ],
    )
    .await
    .unwrap_or_else(|_| "Failed to retrieve CI logs".to_string());

    // ログが長すぎる場合は末尾を返す（エージェントのコンテキストに収まるように）
    let max_len = 3000;
    if log.len() > max_len {
        Ok(format!("...(truncated)\n{}", &log[log.len() - max_len..]))
    } else {
        Ok(log)
    }
}

/// CI リトライ用: 既存ブランチから worktree を作成する
///
/// `create()` と異なり、新しいブランチは作らず既存ブランチをチェックアウトする。
pub async fn create_for_retry(
    repos_base_dir: &str,
    repo_key: &str,
    task_id: i64,
    branch_name: &str,
) -> Result<Workspace> {
    let main_repo_path = PathBuf::from(repos_base_dir).join(repo_key);
    let worktree_dir = PathBuf::from(repos_base_dir).join(WORKTREES_DIR);
    let dir_name = format!("{}-task-{}", repo_key, task_id);
    let worktree_path = worktree_dir.join(&dir_name);

    // worktrees ディレクトリを作成
    std::fs::create_dir_all(&worktree_dir)?;

    // 既存の worktree があれば削除
    if worktree_path.exists() {
        remove(&Workspace {
            worktree_path: worktree_path.clone(),
            branch_name: branch_name.to_string(),
            main_repo_path: main_repo_path.clone(),
        })
        .await
        .ok();
    }

    // fetch して最新の remote ブランチを取得
    git(&main_repo_path, &["fetch", "origin", branch_name]).await?;

    // 既存ブランチから worktree を作成
    let wt_str = worktree_path.to_string_lossy().to_string();
    git(
        &main_repo_path,
        &["worktree", "add", &wt_str, branch_name],
    )
    .await?;

    tracing::info!(
        "Created retry worktree: {} (branch: {})",
        worktree_path.display(),
        branch_name
    );

    Ok(Workspace {
        worktree_path,
        branch_name: branch_name.to_string(),
        main_repo_path,
    })
}

/// CI リトライ後に変更を force push する
pub async fn push_retry(workspace: &Workspace) -> Result<()> {
    let wt = &workspace.worktree_path;

    // git add -A
    git(wt, &["add", "-A"]).await?;

    // 変更があるか確認
    let diff_output = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(wt)
        .kill_on_drop(true)
        .output();

    let diff_output = tokio::time::timeout(GIT_TIMEOUT, diff_output)
        .await
        .context("git diff timed out")?
        .context("failed to spawn git diff")?;

    if diff_output.status.success() {
        bail!("No changes to commit after CI fix attempt");
    }

    // commit --amend（同じブランチに積む）
    git(wt, &["commit", "-m", "fix: address CI failures"]).await?;

    // force push
    git(
        wt,
        &["push", "--force-with-lease", "origin", &workspace.branch_name],
    )
    .await?;

    tracing::info!("Pushed CI fix to branch: {}", workspace.branch_name);
    Ok(())
}
