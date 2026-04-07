//! Ambient ops 用の PreToolUse hook 実装。
//!
//! bash_validation の手前で走る user 定義チェック。bash_validation は generic な
//! 検出を担当し、こちらは ambient 固有のドメインルール (絶対禁止操作) を担当する。
//!
//! 設計判断: bash_validation で warn になる程度のものはそのまま warn のままにし、
//! ここでは「やらかしたら本番が壊れる」レベルの操作だけ deny する。

use std::path::Path;

use agent_harness::{HookDecision, ToolHook};

use super::harness_adapter::is_bash_tool;

/// 自動運用対象のサービス。systemctl stop/restart の許可リスト。
const MANAGED_SERVICES: &[&str] = &["ambient-task-agent", "knowledge-bot"];

pub struct AmbientHookHandler;

impl ToolHook for AmbientHookHandler {
    fn pre_tool_use(
        &self,
        name: &str,
        input: &serde_json::Value,
        _cwd: &Path,
    ) -> HookDecision {
        if !is_bash_tool(name) {
            return HookDecision::Allow;
        }
        let Some(command) = input.get("command").and_then(|v| v.as_str()) else {
            return HookDecision::Allow;
        };

        // ── 絶対禁止リスト（bash_validation の warn では不十分なもの） ──

        // .git ディレクトリの破壊（リポジトリが死ぬ）
        if command.contains("rm -rf .git") || command.contains("rm -r .git") {
            return HookDecision::Deny {
                reason: ".git ディレクトリの削除は禁止されています".into(),
            };
        }

        // git push --force / -f に main / master を組み合わせた場合
        if (command.contains("--force") || command.contains(" -f"))
            && command.contains("git push")
            && (command.contains(" main") || command.contains(" master"))
        {
            return HookDecision::Deny {
                reason: "main/master ブランチへの force push は禁止です。\
                    別ブランチ + PR 経由で進めてください"
                    .into(),
            };
        }

        // git reset --hard HEAD~ 系（コミット消失のリスク）
        if command.contains("git reset --hard HEAD~") || command.contains("git reset --hard origin") {
            return HookDecision::Deny {
                reason: "git reset --hard でコミットを消す操作は禁止です。\
                    revert で進めてください"
                    .into(),
            };
        }

        // ~/.credentials/ への書き込み・削除
        if (command.contains("rm ") || command.contains(" > ") || command.contains(" >> "))
            && command.contains(".credentials")
        {
            return HookDecision::Deny {
                reason: "~/.credentials/ への書き込み・削除は禁止です（手動で更新してください）"
                    .into(),
            };
        }

        // systemctl で managed 以外のサービスを stop / restart
        if command.contains("systemctl")
            && (command.contains("stop ") || command.contains("restart "))
            && !MANAGED_SERVICES.iter().any(|s| command.contains(s))
        {
            return HookDecision::Deny {
                reason: format!(
                    "{} 以外のサービスの stop/restart は禁止です",
                    MANAGED_SERVICES.join(" / ")
                ),
            };
        }

        HookDecision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn allow(cmd: &str) {
        let h = AmbientHookHandler;
        let d = h.pre_tool_use("Bash", &json!({"command": cmd}), Path::new("/tmp"));
        assert!(matches!(d, HookDecision::Allow), "expected Allow for: {cmd}");
    }
    fn deny(cmd: &str) {
        let h = AmbientHookHandler;
        let d = h.pre_tool_use("Bash", &json!({"command": cmd}), Path::new("/tmp"));
        assert!(matches!(d, HookDecision::Deny { .. }), "expected Deny for: {cmd}");
    }

    #[test]
    fn denies_git_dir_destruction() {
        deny("rm -rf .git");
        deny("cd repo && rm -rf .git");
    }

    #[test]
    fn denies_force_push_to_main() {
        deny("git push --force origin main");
        deny("git push -f origin master");
    }

    #[test]
    fn allows_force_push_to_feature_branch() {
        allow("git push --force origin feature/x");
    }

    #[test]
    fn denies_hard_reset_destructive() {
        deny("git reset --hard HEAD~3");
        deny("git reset --hard origin/main");
    }

    #[test]
    fn allows_hard_reset_to_known_commit() {
        // We only block the specific HEAD~ / origin patterns. SHA-based reset is allowed.
        allow("git reset --hard abc123");
    }

    #[test]
    fn denies_credentials_write() {
        deny("rm ~/.credentials/common.env");
        deny("echo TOKEN=xyz > ~/.credentials/common.env");
        deny("cat foo >> ~/.credentials/common.env");
    }

    #[test]
    fn denies_systemctl_stop_other_services() {
        deny("systemctl stop nginx");
        deny("systemctl --user restart sdtab-other-service");
    }

    #[test]
    fn allows_systemctl_self_restart() {
        allow("systemctl --user restart sdtab-ambient-task-agent.service");
        allow("systemctl --user restart sdtab-knowledge-bot.service");
    }

    #[test]
    fn allows_safe_commands() {
        allow("ls -la");
        allow("git status");
        allow("git push origin feature/foo");
        allow("cargo test");
    }

    #[test]
    fn ignores_non_bash_tools() {
        let h = AmbientHookHandler;
        let d = h.pre_tool_use(
            "Read",
            &json!({"file_path": "/etc/passwd"}),
            Path::new("/tmp"),
        );
        assert!(matches!(d, HookDecision::Allow));
    }
}
