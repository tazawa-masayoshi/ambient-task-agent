//! Git Ratchet（品質ラチェット）
//!
//! PR 作成前にテスト数・clippy warnings の悪化を検知してブロックする。

use std::path::{Path, PathBuf};

use anyhow::Result;

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct QualityBaseline {
    test_count: u32,
    clippy_warnings: u32,
    #[serde(default)]
    updated_at: String,
}

/// ベースラインファイルのパス（worktree の親リポジトリの .agent/ に保存）
///
/// `.agent/` ディレクトリが見つからない場合（setup_repo_dirs 未実行時）は
/// worktree 直下に `.agent/` を作成してそこに保存する。
fn baseline_path(worktree_path: &Path) -> PathBuf {
    // worktree_path は .claude/worktrees/xxx/ の下にあるので、
    // ancestors を辿って main repo の .agent/ を見つける
    let base = worktree_path
        .ancestors()
        .find(|p| p.join(".agent").is_dir())
        .unwrap_or(worktree_path);
    let agent_dir = base.join(".agent");
    // .agent/ が存在しない場合は作成（フォールバック時）
    std::fs::create_dir_all(&agent_dir).ok();
    agent_dir.join("quality-baseline.json")
}

/// worktree で cargo test + clippy を並列実行してメトリクスを取得
async fn capture_quality_metrics(worktree_path: &Path) -> Result<(u32, u32)> {
    let test_fut = tokio::process::Command::new("cargo")
        .arg("test")
        .arg("--")
        .arg("--format=terse")
        .current_dir(worktree_path)
        .output();

    let clippy_fut = tokio::process::Command::new("cargo")
        .args(["clippy", "--message-format=short"])
        .current_dir(worktree_path)
        .output();

    let (test_output, clippy_output) = tokio::join!(test_fut, clippy_fut);

    let test_output = test_output
        .map_err(|e| anyhow::anyhow!("cargo test failed to start: {}", e))?;
    let clippy_output = clippy_output
        .map_err(|e| anyhow::anyhow!("cargo clippy failed to start: {}", e))?;

    // "test result: ok. 36 passed; 0 failed" → "passed" の直前の数字を取得
    let test_stdout = String::from_utf8_lossy(&test_output.stdout);
    let test_count = test_stdout.lines()
        .find(|l| l.contains("test result:"))
        .and_then(|l| {
            let words: Vec<&str> = l.split_whitespace().collect();
            words.windows(2)
                .find(|w| w[1] == "passed" || w[1] == "passed;")
                .and_then(|w| w[0].parse::<u32>().ok())
        })
        .unwrap_or(0);

    let clippy_stderr = String::from_utf8_lossy(&clippy_output.stderr);
    let clippy_warnings = clippy_stderr.lines()
        .filter(|l| l.contains("warning:") && !l.contains("warning: `"))
        .count() as u32;

    Ok((test_count, clippy_warnings))
}

/// ラチェット検証: テスト数が減少 or clippy warnings が増加 → エラー
pub async fn quality_ratchet_check(worktree_path: &Path) -> Result<()> {
    let bp = baseline_path(worktree_path);
    let baseline = if bp.exists() {
        let content = std::fs::read_to_string(&bp)
            .map_err(|e| anyhow::anyhow!("Failed to read baseline: {}", e))?;
        serde_json::from_str::<QualityBaseline>(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse baseline: {}", e))?
    } else {
        tracing::info!("No quality baseline found, skipping ratchet check");
        return Ok(());
    };

    let (test_count, clippy_warnings) = capture_quality_metrics(worktree_path).await?;

    tracing::info!(
        "Ratchet check: tests={}/{} (baseline), clippy_warnings={}/{} (baseline)",
        test_count, baseline.test_count, clippy_warnings, baseline.clippy_warnings
    );

    let mut violations = Vec::new();
    if test_count < baseline.test_count {
        violations.push(format!(
            "テスト数が減少: {} → {} ({}件減)",
            baseline.test_count, test_count, baseline.test_count - test_count
        ));
    }
    if clippy_warnings > baseline.clippy_warnings {
        violations.push(format!(
            "clippy warnings が増加: {} → {} ({}件増)",
            baseline.clippy_warnings, clippy_warnings, clippy_warnings - baseline.clippy_warnings
        ));
    }

    if violations.is_empty() {
        tracing::info!("Ratchet check passed");
        Ok(())
    } else {
        Err(anyhow::anyhow!("{}", violations.join("\n")))
    }
}

/// ベースラインを更新（PR 作成成功後に呼ぶ）
pub async fn update_quality_baseline(worktree_path: &Path) {
    match capture_quality_metrics(worktree_path).await {
        Ok((test_count, clippy_warnings)) => {
            let baseline = QualityBaseline {
                test_count,
                clippy_warnings,
                updated_at: chrono::Utc::now().to_rfc3339(),
            };
            let bp = baseline_path(worktree_path);
            if let Some(parent) = bp.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            match serde_json::to_string_pretty(&baseline) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&bp, json) {
                        tracing::warn!("Failed to write quality baseline: {}", e);
                    } else {
                        tracing::info!("Quality baseline updated: tests={}, clippy={}", test_count, clippy_warnings);
                    }
                }
                Err(e) => tracing::warn!("Failed to serialize quality baseline: {}", e),
            }
        }
        Err(e) => tracing::warn!("Failed to capture quality metrics for baseline: {}", e),
    }
}
