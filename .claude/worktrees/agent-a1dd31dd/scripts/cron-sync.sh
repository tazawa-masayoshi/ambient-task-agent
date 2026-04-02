#!/bin/bash
# Asanaタスク同期 - cronから定期実行用
#
# 使い方: crontab -e で以下を追加
#   */5 9-19 * * 1-5 /path/to/ambient-task-agent/scripts/cron-sync.sh
#   (平日 9:00-19:00 の間、5分ごとに実行)
#
# 動作:
#   1. ambient-task-agent sync --quiet でAsanaからタスク取得+ハッシュ比較
#   2. 変更なし → 何もしない（コスト0）
#   3. 変更あり → claude -p で差分を渡してLLM判断（Slack通知等）

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
CACHE_DIR="$HOME/.config/wez-sidebar"
LOG_FILE="$CACHE_DIR/sync.log"
AGENT_BIN="$PROJECT_DIR/target/release/ambient-task-agent"

mkdir -p "$CACHE_DIR"

# リリースビルドがなければdevを使う
if [ ! -f "$AGENT_BIN" ]; then
    AGENT_BIN="$PROJECT_DIR/target/debug/ambient-task-agent"
fi

# 1. Asana同期 (PAT直接、軽い)
DIFF_OUTPUT=$("$AGENT_BIN" sync --quiet 2>/dev/null || true)

if [ -z "$DIFF_OUTPUT" ]; then
    # 変更なし - 何もしない
    exit 0
fi

echo "[$(date '+%Y-%m-%d %H:%M:%S')] changes detected" >> "$LOG_FILE"
echo "$DIFF_OUTPUT" >> "$LOG_FILE"

# 2. 変更あり → claude -p でLLM判断
claude -p "あなたはタスク管理アシスタントです。以下のAsanaタスク変更を確認し、適切な対応をしてください。

## 変更内容
$DIFF_OUTPUT

## 対応ルール
- タスクが完了になった → Slackテストチャンネルに報告（ambient-task-agent notify -m '...' を使用）
- 新規タスクが自分(田澤)に割り当てられた → Slackに通知
- 期限超過タスクがある → 警告通知
- それ以外 → 特にアクションなし、ログのみ

必ず日本語で対応してください。" \
  --cwd "$PROJECT_DIR" \
  --output-format text \
  --max-turns 5 \
  >> "$LOG_FILE" 2>&1

echo "[$(date '+%Y-%m-%d %H:%M:%S')] done" >> "$LOG_FILE"

# ログが大きくなりすぎないよう最新1000行に制限
tail -1000 "$LOG_FILE" > "$LOG_FILE.tmp" && mv "$LOG_FILE.tmp" "$LOG_FILE"
