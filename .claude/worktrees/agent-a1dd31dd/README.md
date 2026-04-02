# ambient-task-agent

**Slack でコーディング。** Slack に書くだけでコードが書かれ、PR が出る。

Rust 製の自律タスクエージェント。Asana/Slack からタスクを受け取り、[AI-DLC](https://zenn.dev/yumemi_inc/articles/ai-and-development-workflow) を参考にした会話フローで不明点を解消し、Claude Code (`claude -p`) で自動実行する。

## 世界観

```
入口が違うだけで、中の処理は同じ

  Slack メッセージ ─┐
                    ├→ classify → 明確 → そのまま実行 → PR
  Asana タスク    ─┘            → 曖昧 → Slack ラリーで要件確定 → 実行
                                → 詰まった → manual（人間が terminal で対応）
```

### 3つの入口

| 入口 | テーブル | 処理 |
|------|---------|------|
| Slack ops（質問・依頼） | `ops_queue` | 自動回答 or Inception で要件確定 → タスク昇格 |
| Slack ops（定型作業） | `ops_queue` | スキル実行 → Slack 返信で完了 |
| Asana タスク | `coding_tasks` | `new → classify → executing/conversing → PR → CI` |

コーディングだけでなく、PM スキル（朝会ブリーフィング、タスク優先度整理、停滞検知、Google Calendar 連携リマインド等）も備える。

### ステータスモデル

```
new → executing（明確）/ conversing（曖昧）
conversing → executing / manual / done / sleeping（5営業日タイムアウト）
manual → executing / done
executing → done / ci_pending / manual（stop） / conversing（ブロッカー検知）
```

- **conversing**: 曖昧なタスクを Slack スレッドで要件確定。LLM が few-shot 分類履歴を参照して判定
- **manual**: ブロッカー検知時や stop コマンドで人間が terminal で直接対応。`直した` で再開
- **sleeping**: conversing で5営業日返信なし → 自動休止

## 設計思想: Heartbeat + DB ポーリング

他の claw 系エージェント（Devin, SWE-agent, Symphony 等）との最大の違いは、**LLM を常時回すのではなく、Rust プログラムが heartbeat で自前の DB をポーリングし、処理対象があるときだけ `claude -p` を起動する**点にある。

```
┌──────────────────────────────────────────────────────────┐
│  Worker Heartbeat (15s)                                  │
│                                                          │
│  1. SELECT * FROM coding_tasks WHERE status IN (...)     │
│  2. SELECT * FROM ops_queue WHERE status = 'pending'     │
│  3. SELECT * FROM scheduled_jobs WHERE next_run_at < NOW │
│                                                          │
│  → 対象なし: sleep 15s（LLM コストゼロ）                    │
│  → 対象あり: classify → tokio::spawn で並列実行             │
└──────────────────────────────────────────────────────────┘
```

- **コスト効率**: 待機中は純粋な DB クエリのみ。LLM は実際のタスク処理時だけ呼ぶ
- **Rust 判定**: ステータス遷移・タイムアウト・優先度ソートはすべてプログラムで決定
- **LLM 判定**: タスク分類（execute/converse）とタスク実行のみ LLM に委譲

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
                    │              │     ┌─────────────┐
┌─────────────┐     │  scheduler ──┼────▶│  Google     │
│   Google    │◀────│  (cron)      │     │  Calendar   │
│   Calendar  │     └──────────────┘     └─────────────┘
└─────────────┘
```

### 主要コンポーネント

| コンポーネント | ファイル | 役割 |
|---------------|---------|------|
| Worker | `src/worker/runner.rs` | heartbeat ループ、spawn_task ガード |
| Classify | `src/worker/classify.rs` | タスク分類（few-shot LLM + heuristics フォールバック） |
| Conversing | `src/worker/runner_conversing.rs` | conversing フロー（Slack ラリーで要件確定） |
| Ops Dispatch | `src/worker/runner_ops.rs` | ops キュー処理（ルーティング・実行・結果投稿） |
| CI Monitor | `src/worker/runner_ci.rs` | CI 監視・自動リトライ |
| Ratchet | `src/worker/ratchet.rs` | git-ratchet（テスト数・warnings の品質ゲート） |
| Executor | `src/worker/executor.rs` | `claude -p --append-system-prompt` でタスク実行 |
| Ops | `src/worker/ops.rs` | Slack ops メッセージの処理（Execute/Plan/Inception） |
| Scheduler | `src/worker/scheduler.rs` | cron ジョブ（朝会/夕会/リマインド/自己改善） |
| Context | `src/worker/context.rs` | タスク完了記録、メモリ蓄積、per-repo context.md |
| Priority | `src/worker/priority.rs` | タスク優先度スコア計算・ソート |
| Workspace | `src/worker/workspace.rs` | worktree 作成・cleanup・パス解決 |
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

## スケジュール

`config/repos.toml` の `[[schedule]]` で定義。すべて営業日（土日除外）ベース。

| ジョブ | スケジュール | 内容 |
|--------|------------|------|
| `morning_briefing` | 平日 9:00 | 当日タスク一覧 + タイムボクシング提案 |
| `evening_summary` | 平日 18:00 | 進捗サマリー |
| `meeting_reminder` | 平日 8-20時/5分 | Google Calendar 連携リマインド |
| `stagnation_check` | 平日 14:00 | 停滞タスク検知 |
| `weekly_pm_review` | 毎週金曜 17:00 | PM レビュー |
| `self_improvement` | 毎週月曜 10:00 | 分類精度・エラー分析 → 改善 PR 提案 |

## CLI コマンド

```
ambient-task-agent <command>

  sync [--quiet]           Asana → JSON キャッシュ同期（--quiet: cron用、変更時のみ出力）
  show [--mine] [--json]   キャッシュ済みタスク表示
  notify -m "msg"          Slack 送信
  done -t "task"           完了通知
  status                   キャッシュ状態表示
  hook <event>             Claude Code hook イベント処理
  start [query] [--gid]    作業タスクを設定
  current                  現在の作業タスクを表示
  serve [--port] [--config-dir]  サーバー起動（heartbeat + Socket Mode）
  task <id> [--start] [--done]   タスク詳細・ステータス遷移
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

## なぜ OpenClaw を使わないか

[OpenClaw](https://github.com/openclaw/openclaw) はマルチチャンネル対応の汎用パーソナルアシスタントとして完成度が高い。ただし、チーム業務自動化という用途では合わない部分があった。

**セキュリティ面**: OpenClaw はデフォルトでエージェントがホスト上でフルアクセスで動作する。チーム Slack からの任意メッセージがコード実行に繋がる経路を持つため、インプットを Asana タスク（承認済み）と ops_admin に限定したいチーム運用では攻撃面が広すぎる。

**コスト面**: OpenClaw は Gateway が常時起動し、セッションが持続する設計になっている。ambient-task-agent はタスクがあるときだけ `claude -p` を起動するため、LLM 課金がタスク数に比例しコントロールしやすい。

**自作の理由**: 既存のものを使うより、自分で作った方が面白い。マネでもいい。Rust でバイブコーディングするのが好きなので、TypeScript 製の OpenClaw をそのまま使うより Rust で書き直す方が自分にとって自然だった。社内チーム向けに閉じた用途なので、汎用性より業務フィット（Asana + Slack + GitHub）を優先した作りにできる。

## 参考プロジェクト

| プロジェクト | 採用したパターン |
|-------------|----------------|
| [AI-DLC](https://zenn.dev/yumemi_inc/articles/ai-and-development-workflow) | 会話フローで不明点を解消してから実行（Inception モード） |
| [Symphony](https://arxiv.org/abs/2506.01579) | DB ポーリング + プログラム判定 → 必要時のみ LLM 起動 |
| [autoresearch](https://github.com/karpathy/autoresearch) | git-ratchet、NEVER STOP directive |
| [multi-agent-shogun](https://github.com/yohey-w/multi-agent-shogun) | Bottom-up Skill Discovery |
| [lossless-claw](https://github.com/Martian-Engineering/lossless-claw) | agent self-retrieval（将来検討） |
| [prompt-review](https://github.com/tokoroten/prompt-review) | 分析テンプレート外部ファイル化 |
| [OpenClaw](https://github.com/openclaw/openclaw) | 参考にしたが採用しなかった（理由は上記） |

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

# 起動（サーバーモード）
./target/release/ambient-task-agent serve
```

## 設定

`config/repos.toml` でリポジトリ、スケジュール、Slack ユーザーマッピングを設定。詳細は同ファイルのコメント参照。
