//! Ambient ops 用の RecoveryDetector 実装。
//!
//! 既知の失敗パターン (git push rejected, gws auth expired, etc.) を
//! 直近の tool result から検出して、scenario 別の修正手順を返す。
//! agent_loop はこれを最後の user メッセージとして注入し、もう1ターン回す。

use agent_harness::{Message, RecoveryDetector, RecoveryScenario};
use agent_harness::types::{ContentBlock, ToolResultContent, ToolResultBlock};

pub struct AmbientRecoveryDetector;

impl RecoveryDetector for AmbientRecoveryDetector {
    fn detect(&self, messages: &[Message]) -> Option<RecoveryScenario> {
        // 直近 5 メッセージから tool result テキストを集める
        let recent_outputs = collect_recent_tool_outputs(messages, 5);
        let combined = recent_outputs.join("\n");
        let lower = combined.to_ascii_lowercase();

        // GitPushRejected: combined window 内で git push と git 固有の拒否パターンが共起。
        // tool_use の command (git push) と tool_result の error が別ブロックに分かれるため
        // window 全体で AND 評価する。"rejected" 単独は PR review rejected 等の
        // false positive を起こすので git 固有パターン (non-fast-forward / "! [rejected]" /
        // fetch first) のみを使う。
        if lower.contains("git push")
            && (lower.contains("non-fast-forward")
                || lower.contains("! [rejected]")
                || lower.contains("fetch first"))
        {
            return Some(RecoveryScenario {
                name: "GitPushRejected".into(),
                instruction: "git push が non-fast-forward で拒否されました。\
                    以下を順に実行してください:\n\
                    1. `git fetch origin`\n\
                    2. `git pull --rebase origin <現在のブランチ>`\n\
                    3. conflict があれば解消してから `git rebase --continue`\n\
                    4. `git push` を再実行\n\
                    \n\
                    最終報告には OPS_RESULT マーカーを含めてください。"
                    .into(),
            });
        }

        // GwsAuthExpired: invalid_grant / token expired / auth required (gws CLI)
        if lower.contains("invalid_grant")
            || lower.contains("token has been expired")
            || lower.contains("token expired")
            || (lower.contains("gws") && lower.contains("unauthorized"))
        {
            return Some(RecoveryScenario {
                name: "GwsAuthExpired".into(),
                instruction: "gws (Google Workspace CLI) の認証トークンが期限切れです。\
                    以下を実行してから操作を再試行してください:\n\
                    1. `gws auth login` を実行（ブラウザ認証の場合は手動対応をユーザーに依頼）\n\
                    2. 認証完了後、失敗した操作を再実行\n\
                    \n\
                    認証が手動対応必須なら『手動対応必要』と報告し OPS_RESULT: failed を出力してください。\
                    自動再試行できれば実施してから OPS_RESULT: completed を出力してください。"
                    .into(),
            });
        }

        // ClaspAuthExpired: clasp の認証失敗
        if lower.contains("clasp") && (lower.contains("not logged in") || lower.contains("login required")) {
            return Some(RecoveryScenario {
                name: "ClaspAuthExpired".into(),
                instruction: "clasp の認証が必要です。\
                    `clasp login --no-localhost` をユーザーに実行依頼するか、\
                    既に `~/.clasprc.json` がある場合はそれが期限切れの可能性があるので報告してください。\
                    最終報告には OPS_RESULT マーカーを含めてください。"
                    .into(),
            });
        }

        None
    }
}

/// 直近 N メッセージから ToolResult のテキストと ToolUse のコマンドを収集。
/// シナリオ判定には「何を実行して何が起きたか」両方の文脈が必要。
fn collect_recent_tool_outputs(messages: &[Message], count: usize) -> Vec<String> {
    let mut outputs = Vec::new();
    for msg in messages.iter().rev().take(count) {
        for block in &msg.content {
            match block {
                ContentBlock::ToolResult { content, .. } => {
                    let text = match content {
                        ToolResultContent::Text(t) => t.clone(),
                        ToolResultContent::Blocks(blocks) => blocks
                            .iter()
                            .map(|b| match b {
                                ToolResultBlock::Text { text } => text.as_str(),
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                    };
                    outputs.push(text);
                }
                ContentBlock::ToolUse { input, .. } => {
                    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                        outputs.push(cmd.to_string());
                    }
                }
                ContentBlock::Text { .. } => {}
            }
        }
    }
    outputs
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_harness::types::Role;
    use serde_json::json;

    fn user_tool_result(output: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "x".to_string(),
                content: ToolResultContent::Text(output.to_string()),
                is_error: None,
            }],
        }
    }

    fn assistant_tool_use(name: &str, command: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "x".to_string(),
                name: name.to_string(),
                input: json!({"command": command}),
            }],
        }
    }

    #[test]
    fn detects_git_push_rejected() {
        let messages = vec![
            assistant_tool_use("Bash", "git push origin main"),
            user_tool_result(
                "To github.com:org/repo.git\n\
                ! [rejected]        main -> main (non-fast-forward)\n\
                error: failed to push some refs",
            ),
        ];
        let detector = AmbientRecoveryDetector;
        let scenario = detector.detect(&messages).expect("expected scenario");
        assert_eq!(scenario.name, "GitPushRejected");
        assert!(scenario.instruction.contains("git pull --rebase"));
    }

    #[test]
    fn detects_gws_auth_expired() {
        let messages = vec![
            assistant_tool_use("Bash", "gws sheets list"),
            user_tool_result("Error: invalid_grant: Token has been expired or revoked."),
        ];
        let detector = AmbientRecoveryDetector;
        let scenario = detector.detect(&messages).expect("expected scenario");
        assert_eq!(scenario.name, "GwsAuthExpired");
    }

    #[test]
    fn detects_clasp_auth_expired() {
        let messages = vec![
            assistant_tool_use("Bash", "clasp push"),
            user_tool_result("clasp: not logged in. run clasp login first."),
        ];
        let detector = AmbientRecoveryDetector;
        let scenario = detector.detect(&messages).expect("expected scenario");
        assert_eq!(scenario.name, "ClaspAuthExpired");
    }

    #[test]
    fn returns_none_for_unknown_failures() {
        let messages = vec![
            assistant_tool_use("Bash", "ls -la"),
            user_tool_result("file1\nfile2\nfile3"),
        ];
        let detector = AmbientRecoveryDetector;
        assert!(detector.detect(&messages).is_none());
    }

    #[test]
    fn ignores_assistant_messages() {
        let messages = vec![Message::assistant_text("non-fast-forward in my plan")];
        let detector = AmbientRecoveryDetector;
        assert!(detector.detect(&messages).is_none());
    }

    #[test]
    fn looks_at_recent_messages_only() {
        // Old git push failure outside the recent window should be ignored
        let mut messages = Vec::new();
        messages.push(assistant_tool_use("Bash", "git push origin main"));
        messages.push(user_tool_result("non-fast-forward error in git push"));
        // Pad with 6 unrelated messages so the window doesn't reach the failure
        for i in 0..6 {
            messages.push(Message::assistant_text(&format!("ack {i}")));
            messages.push(Message::user_text(&format!("ok {i}")));
        }
        let detector = AmbientRecoveryDetector;
        // Window is 5 messages, the git failure is 12 messages back → not detected
        assert!(detector.detect(&messages).is_none());
    }
}
