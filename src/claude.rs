use anyhow::Result;
use tokio::process::Command;

/// claude -p でプロンプトを実行して出力を得る
pub async fn run_claude_prompt(prompt: &str, max_turns: u32) -> Result<String> {
    tracing::info!(
        "Running claude -p (max_turns={})",
        max_turns
    );

    let output = Command::new("claude")
        .args(["-p", prompt, "--max-turns", &max_turns.to_string()])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to execute claude -p: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude -p failed (exit {}): {}", output.status, stderr);
    }

    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(result)
}
