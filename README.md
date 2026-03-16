# ambient-task-agent

Rust 製の自律タスクエージェント。Asana/Slack からタスクを受け取り、Claude Code (`claude -p`) で自動実行する。

## 世界観

```
入口が違うだけで、中の処理は同じ

  Asana タスク ─┐
                ├→ classify → 明確 → そのまま実行 → PR
  Slack ops   ─┘            → 曖昧 → Slack ラリー → 明確になったら実行
                             → 詰まった → manual（人間が terminal で対応）
```

### 3つの入口

| 入口 | テーブル | 処理 |
|------|---------|------|
| Asana タスク | `coding_tasks` | `new → classify → executing/conversing → PR → CI` |
| Slack ops（質問・依頼） | `ops_queue` | 自動回答 or Inception で要件確定 → タスク昇格 |
| Slack ops（定型作業） | `ops_queue` | スキル実行 → Slack 返信で完了 |

### ステータスモデル

```
new → executing（明確）/ conversing（曖昧）
conversing → executing / manual / done
manual → executing / done
executing → done / ci_pending / conversing（ブロッカー検知）
```

## アーキテクチャ

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│   Asana     │────▶│              │────▶│  claude -p  │
│   Webhook   │     │   Worker     │     │  (worktree) │
└─────────────┘     │   Heartbeat  │     └──────┬──────┘
                    │   (15s)      │            │
┌─────────────┐     │              │     ┌──────▼──────┐
│   Slack     │────▶│  classify    │     │   GitHub    │
│   Socket    │     │  conversing  │     │   PR + CI   │
│   Mode      │     │  executing   │     └─────────────┘
└─────────────┘     │  manual      │
                    └──────────────┘
```

### 主要コンポーネント

| コンポーネント | ファイル | 役割 |
|---------------|---------|------|
| Worker | `src/worker/runner.rs` | heartbeat ループ、タスク分類、tokio::spawn 並列実行 |
| Executor | `src/worker/executor.rs` | `claude -p --append-system-prompt` でタスク実行 |
| Ops | `src/worker/ops.rs` | Slack ops メッセージの処理（Execute/Plan/Inception） |
| Scheduler | `src/worker/scheduler.rs` | cron ジョブ（朝会/夕会/リマインド/自己改善） |
| Slack Events | `src/server/slack_events.rs` | Slack イベント受信、コマンド処理 |
| Slack Actions | `src/server/slack_actions.rs` | Block Kit ボタンハンドラ |
| DB | `src/db.rs` | SQLite（coding_tasks + ops_queue + ops_contexts + skill_candidates） |

## 自己改善ループ

```
タスク実行
  ├─ SUMMARY: → context.md に完了記録
  ├─ MEMORY: → memory.md に学習蓄積
  └─ SKILL_CANDIDATE: → skill_candidates テーブルに蓄積

self_improvement ジョブ（毎週月曜 10:00）
  ├─ 分類精度分析（few-shot 学習で精度向上）
  ├─ エラーパターン分析 → 改善タスク提案
  ├─ 成熟スキル候補通知（occurrences >= 2）
  └─ git-ratchet で品質保証（テスト数↓ or warnings↑ → PR 拒否）
```

## 設計判断

主要な設計判断は [`docs/adr/`](docs/adr/) に記録。

| ADR | 決定 |
|-----|------|
| [0001](docs/adr/0001-db-separate-tables.md) | coding_tasks と ops_queue を DB 統合しない |
| [0002](docs/adr/0002-conversing-manual-status.md) | plan/approve 廃止 → conversing/manual に統合 |
| [0003](docs/adr/0003-hybrid-session-management.md) | セッション管理のハイブリッド方式 |
| [0004](docs/adr/0004-append-system-prompt.md) | --system-prompt → --append-system-prompt |
| [0005](docs/adr/0005-self-improvement-loop.md) | 自己改善ループ + git-ratchet |
| [0006](docs/adr/0006-tokio-spawn-parallelization.md) | process_tasks() の tokio::spawn 並列化 |
| [0007](docs/adr/0007-llm-classification-learning.md) | LLM 分類学習（few-shot classify） |

## 参考プロジェクト

| プロジェクト | 採用したパターン |
|-------------|----------------|
| [autoresearch](https://github.com/karpathy/autoresearch) | git-ratchet、NEVER STOP directive |
| [multi-agent-shogun](https://github.com/yohey-w/multi-agent-shogun) | Bottom-up Skill Discovery |
| [lossless-claw](https://github.com/Martian-Engineering/lossless-claw) | agent self-retrieval（将来検討） |
| [prompt-review](https://github.com/tokoroten/prompt-review) | 分析テンプレート外部ファイル化 |

## セットアップ

```bash
# ビルド
cargo build --release

# 環境変数（.env）
SLACK_BOT_TOKEN=xoxb-...
SLACK_APP_TOKEN=xapp-...  # Socket Mode
SLACK_SIGNING_SECRET=...
ASANA_PAT=...
ASANA_PROJECT_ID=...

# 起動
./target/release/ambient-task-agent
```

## 設定

`config/repos.toml` でリポジトリとスケジュールを設定。詳細は同ファイルのコメント参照。
