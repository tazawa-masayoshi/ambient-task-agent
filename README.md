# ambient-task-agent

**Slack でコーディング。** Slack に書くだけでコードが書かれ、PR が出る。

Rust 製の自律タスクエージェント。Asana / Slack からタスクを受け取り、[AI-DLC](https://zenn.dev/yumemi_inc/articles/ai-and-development-workflow) を参考にした会話フローで不明点を解消し、Claude (Anthropic API / Max OAuth / AWS Bedrock のいずれか) を使って自動実行する。

## 世界観

```
入口が違うだけで、中の処理は同じ

  Slack メッセージ ─┐
                    ├→ classify → 明確 → executing → PR
  Asana タスク    ─┘            → 曖昧  → conversing（Slack ラリー） → executing
                                → 詰まった → manual（人間が terminal で対応） → executing
                                → 返信なし → sleeping（5営業日タイムアウト）
```

### 3 つの入口

| 入口 | テーブル | 処理 |
|------|---------|------|
| Asana タスク | `coding_tasks` | `new → classify → executing / conversing → ci_pending → done` |
| Slack ops（質問・依頼） | `ops_queue` | 自動回答 or Inception で要件確定 → タスク昇格 |
| Slack ops（定型作業） | `ops_queue` | スキル実行 → Slack 返信で完了 |

コーディングだけでなく、PM スキル（朝会ブリーフィング、タスク優先度整理、停滞検知、Google Calendar 連携リマインド等）も備える。

## ステータスモデル

```
        ┌─────────────────────┐
        │       [new]          │
        └─────────┬────────────┘
                  │ classify (heuristics + few-shot LLM)
       ┌──────────┼──────────┐
       ▼                     ▼
 [executing] ◀──────────▶ [conversing]
   │  │  │                 │   │   │
   │  │  │ Stop            │   │   │ 5営業日無反応
   │  │  └──────────▶ [manual]  │   ▼
   │  │      "直した"       │    [sleeping]
   │  │  ◀──────────────────┘       │
   │  │                              │ reply
   │  │  ブロッカー                  │
   │  └──────────▶ [conversing] ◀───┘
   │
   │  PR 作成
   └──────────▶ [ci_pending] ──▶ [done]
```

| ステータス | 意味 | 遷移先 |
|-----------|-----|-------|
| `new` | 取り込み直後。未分類 | → `executing` / `conversing` |
| `conversing` | 要件不明瞭、Slack スレッドで要件確定中 | → `executing` / `manual` / `sleeping` / `done` |
| `executing` | worktree 上で claude 実行中 | → `done` / `ci_pending` / `manual` / `conversing`（ブロッカー検知） |
| `ci_pending` | PR 作成済み、CI 結果待ち | → `done` |
| `manual` | 人間が terminal で作業中。`直した` / ボタンで復帰 | → `executing` / `done` |
| `sleeping` | conversing で 5 営業日反応なし | → `new`（Slack 再開時） |
| `done` | 完了 | （最終状態） |
| `error` | 実行失敗 | （最終状態） |

### 分類の仕組み

- **Heuristics**（即決）: Slack 起点 + analysis_text あり → Execute、Asana + `auto_execute` → Execute、それ以外は Converse
- **Few-shot LLM**（`min_fewshot_examples` 以上の履歴があれば）: 過去 10 件の分類結果を few-shot で渡して Claude に分類させる
- **Fallback**: LLM 失敗時は heuristics

### ブロッカー検知

executor の stdout に `BLOCKED:` / `REQUIRES_CLARIFICATION:` 行が出ると、`executing → conversing` に戻して Slack で要件を詰め直す。`claude_session_id` を保存しているので、分類し直しで `--resume` により会話を継続できる。

## 設計思想: Heartbeat + DB ポーリング

他の claw 系エージェント（Devin, SWE-agent, Symphony 等）との最大の違いは、**LLM を常時回すのではなく、Rust プログラムが heartbeat で自前の SQLite をポーリングし、処理対象があるときだけ Claude を起動する**点。

```
┌──────────────────────────────────────────────────────────┐
│  Worker Heartbeat (15s)                                  │
│                                                          │
│  1. SELECT * FROM coding_tasks WHERE status IN (...)     │
│  2. SELECT * FROM ops_queue WHERE status = 'pending'     │
│  3. SELECT * FROM scheduled_jobs WHERE next_run_at < NOW │
│  4. check_ops_followups()（平日 9-18時のみ、1h cadence） │
│                                                          │
│  → 対象なし: sleep 15s（LLM コストゼロ）                  │
│  → 対象あり: tokio::spawn（Semaphore で同時実行数制限）  │
└──────────────────────────────────────────────────────────┘
```

- **コスト効率**: 待機中は純粋な DB クエリのみ。LLM は実際のタスク処理時だけ呼ぶ
- **Rust 判定**: ステータス遷移・タイムアウト・優先度ソートはすべてプログラムで決定
- **LLM 判定**: タスク分類・要件確認・タスク実行・プロンプト評価のみ LLM に委譲

## アーキテクチャ

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│   Asana     │────▶│              │────▶│   claude    │
│   Webhook   │     │   Worker     │     │  (worktree) │
└─────────────┘     │   Heartbeat  │     └──────┬──────┘
                    │   (15s)      │            │
┌─────────────┐     │              │     ┌──────▼──────┐
│   Slack     │────▶│  classify    │     │   GitHub    │
│   Socket    │     │  conversing  │     │   PR + CI   │
│   Mode      │     │  executing   │     └─────────────┘
└─────────────┘     │  manual      │
                    │  ci_monitor  │     ┌─────────────┐
┌─────────────┐     │  scheduler ──┼────▶│  Google     │
│   Google    │◀────│  (cron)      │     │  Calendar   │
│   Calendar  │     └──────────────┘     └─────────────┘
└─────────────┘
```

### 主要コンポーネント

| コンポーネント | ファイル | 役割 |
|---------------|---------|------|
| Worker | `src/worker/runner.rs` (904行) | heartbeat ループ、task 処理の spawn ガード、CI 監視 |
| Classify | `src/worker/classify.rs` | タスク分類（few-shot LLM + heuristics フォールバック） |
| Conversing | `src/worker/runner_conversing.rs` | conversing フロー（Slack ラリー・LLM ガイド） |
| Ops Dispatch | `src/worker/runner_ops.rs` | ops キュー処理（アトミック dequeue で競合回避） |
| CI Monitor | `src/worker/runner_ci.rs` | CI 監視・失敗時の worktree 再作成 |
| Ratchet | `src/worker/ratchet.rs` | git-ratchet（テスト数減少・clippy warnings 増加の品質ゲート） |
| Executor | `src/worker/executor.rs` | `claude --append-system-prompt` 起動、ブロッカー検出 |
| Ops | `src/worker/ops.rs` | Slack ops メッセージの処理（Execute / Plan / Inception） |
| Scheduler | `src/worker/scheduler.rs` | cron ジョブ（朝会 / 夕会 / リマインド / 自己改善 / prompt evolution） |
| Context | `src/worker/context.rs` | タスク完了記録、memory 蓄積、per-repo context.md マージ |
| Priority | `src/worker/priority.rs` | タスク優先度スコア計算 |
| Workspace | `src/worker/workspace.rs` | worktree 作成 / cleanup / パス解決 |
| Prompt Evolution | `src/worker/prompt_evolution.rs` | system prompt 候補生成・スコアリング・Slack 提案 |
| Slack Events | `src/server/slack_events.rs` | Slack イベント受信、テキストコマンド処理 |
| Slack Actions | `src/server/slack_actions.rs` | Block Kit ボタン・セレクトハンドラ |
| DB | `src/db.rs` | SQLite（coding_tasks / ops_queue / ops_contexts / skill_candidates / scheduled_jobs / prompt_evolution_proposals ほか） |

## Slack インタラクション

### ボタン（Block Kit）

**タスク操作**:

| action_id | 動作 |
|-----------|------|
| `task_execute` | conversing / manual → executing |
| `task_manual` | executing / conversing → manual（旧 `stop_task` と統合） |
| `task_skip` | → done（実行せず完了扱い） |
| `task_add_instruction` | conversing に追加指示を入力 |
| `task_resume` | manual → executing（terminal 作業完了後） |
| `task_done` | → done（完了マーク） |

**Ops 操作**:

| action_id | 動作 |
|-----------|------|
| `ops_inception_approve` | Inception 承認 → executing 直行（二重分析なし） |
| `ops_inception_asana` | Inception を Asana タスクに昇格 |
| `ops_inception_revise` | Inception 再生成要求 |
| `ops_approve_proposal` | ops Plan モード → Execute モード |
| `ops_escalate` | ops 実行失敗 → admin メンション |
| `ops_resolve` | ops 完了マーク |

**Prompt Evolution**:

| action_id | 動作 |
|-----------|------|
| `prompt_evolution_approve` | 提案 → approved（次回 ops 実行から適用） |
| `prompt_evolution_postpone` | 3日後にリマインド |
| `prompt_evolution_reject` | 拒否 |

### テキストコマンド

| コマンド | 動作 |
|---------|------|
| `go` / `実行` / `run` / `ok` / `承認` / `approve` | conversing → executing |
| `直した` / `fixed` / `修正完了` | manual → executing |
| `stop` / `cancel` / `中止` / `停止` | executing → manual |
| `archive` | → archived |
| `⚡` リアクション（ops メッセージに） | ops 実行トリガ |

## 自己改善ループ

```
タスク実行時の LLM 出力
  ├─ SUMMARY:           → context.md に完了記録
  ├─ MEMORY:            → memory.md に学習蓄積
  └─ SKILL_CANDIDATE:   → skill_candidates テーブルに蓄積

self_improvement ジョブ（毎週月曜 10:00）
  ├─ 分類精度分析（few-shot 学習で精度向上）
  │    ├─ classification_outcome = "correct"（成功）
  │    └─ classification_outcome = "needed_converse"（ブロッカーで戻された）
  ├─ エラーパターン分析（ops_failure_patterns）
  ├─ 成熟スキル候補通知（occurrences >= 2 で mature）
  └─ git-ratchet で品質保証（テスト数↓ or warnings↑ → PR 拒否）

prompt_evolution ジョブ
  ├─ 現行 system_prompt + 成功 3 + 失敗 3 サンプル
  ├─ 3 候補を LLM 生成 + スコアリング
  ├─ best 候補を Slack で approve / postpone / reject
  └─ approve → 次回 ops 実行から適用
```

## スケジュール

`config/repos.toml` の `[[schedule]]` で定義。すべて営業日（土日除外）ベース。

| ジョブ | デフォルト | 内容 |
|--------|-----------|------|
| `morning_briefing` | 平日 9:00 | Asana sync、当日タスク一覧 + タイムボクシング提案 |
| `evening_summary` | 平日 18:00 | 進捗サマリー、未完了タスクのレビュー |
| `meeting_reminder` | 平日 8-20時 / 5分 | Google Calendar 連携、15分前リマインド |
| `stagnation_check` | 平日 14:00 | 停滞タスク（`stagnation_threshold_hours` 超）を検知 |
| `weekly_pm_review` | 金曜 17:00 | 週次 PM レビュー |
| `self_improvement` | 月曜 10:00 | 分類精度・エラー分析・成熟スキル検知 |
| `context_rot_scan` | 日次 | 古くなった context ファイルを検知 |
| `prompt_evolution` | 週次 | system_prompt 候補生成・Slack 提案 |
| `prompt_evolution_reminder` | 随時 | postpone された提案の3日後リマインド |

cron は `cron` クレート記法。次回実行時刻 (`next_run_at`) は実行「前」に advance することで double-fire を防ぐ。

## CLI コマンド

```
ambient-task-agent <command>

  sync [--quiet]                   Asana → JSON キャッシュ同期（--quiet: cron用、変更時のみ出力）
  show [--mine] [--json]           キャッシュ済みタスク表示
  notify -m "msg" [-c channel]     Slack 送信
  done -t "task"                   完了通知
  status                           キャッシュ状態表示
  hook <event>                     Claude Code hook イベント処理
  start [query] [--gid id]         作業タスクを設定
  current                          現在の作業タスクを表示
  serve [--port 3000] [--config-dir path]
                                   heartbeat + HTTP サーバー + Socket Mode 起動
  task <id> [--start] [--done]     タスク詳細 / ステータス遷移
  prompt-evolution <action>        プロンプト候補管理（下記）
```

### `prompt-evolution` サブコマンド

```
  list [--status (pending|approved|rejected|postponed)] [--limit N] [--json]
                                  提案一覧
  show <id> [--json]              提案詳細（best_prompt + rationale）
  approve <id>                    提案を approved に
  reject <id>                     提案を rejected に
  postpone <id>                   提案を postponed に（3日後リマインド）
```

## 設定

### `config/repos.toml`

```toml
[defaults]
slack_channel = "C..."
repos_base_dir = "/path/to/repos"       # worktree 作成時のベースディレクトリ
claude_max_plan_turns = 10
claude_max_execute_turns = 20
worker_heartbeat_secs = 15              # heartbeat 間隔（最低 10s）
stagnation_threshold_hours = 24         # 停滞タスク検知の閾値
claude_timeout_secs = 600
claude_max_output_bytes = 100_000
claude_max_concurrent = 2               # tokio::spawn の Semaphore
min_fewshot_examples = 5                # LLM 分類の履歴最低数
google_calendar_id = "..."
ops_admin_user = "U..."                 # エスカレーション先
slack_user_map = { "U..." = "田澤" }

[[repo]]
key = "my-repo"
github = "https://github.com/owner/repo"
match = { section_name = "my-repo" }    # Asana セクション名とマッピング
ops_channel = "C..."
auto_execute = true                     # classify せず即 executing
max_execute_turns = 30
ops_mode = "execute"                    # execute | plan | inception
ops_skills = ["lint", "format", "test"]
mcp_servers = [{ command = "..." }]

[[schedule]]
key = "morning_briefing"
cron = "0 9 * * 1-5"
job_type = "morning_briefing"
slack_channel = "C..."
```

### 環境変数（`.env` / `~/.credentials/` から読み込み）

**必須**:
- `SLACK_BOT_TOKEN` — Slack Bot (xoxb-)
- `ASANA_PAT` — Asana Personal Access Token
- `ASANA_PROJECT_ID`

**任意**:
- `SLACK_APP_TOKEN` — Socket Mode（xapp-）。未設定時は HTTP Webhook モード
- `SLACK_SIGNING_SECRET` — リクエスト検証用
- `SLACK_TEST_CHANNEL` — 一部コマンドの送信先
- `SLACK_WORKSPACE` — ワークスペース名
- `ASANA_USER_NAME` — デフォルト `田澤`
- `GOOGLE_CALENDAR_ID` — meeting_reminder 用
- `GWS_PATH` — `gws` CLI バイナリパス
- `ANTHROPIC_API_KEY` — API Key 認証用
- `ANTHROPIC_MODEL` — デフォルト `claude-sonnet-4-20250514`
- `AGENT_BACKEND` — `bedrock` 指定時のみ Bedrock
- `AWS_REGION` / `AWS_DEFAULT_REGION` — Bedrock 用
- `BEDROCK_MODEL` — デフォルト `us.anthropic.claude-sonnet-4-20250514-v1:0`

ロード順: プロセス環境 → `~/.credentials/common.env` → `~/.credentials/ambient-task-agent.env` → `./.env`

### データファイル

- DB: `~/.agent/ambient-task-agent.db`（SQLite）
- per-repo コンテキスト: `<repo>/.agent/context.md` / `memory.md` / `soul.md` / `skill.md`
- Asana キャッシュ: `<repo>/.agent/tasks.json`

## バックエンド切替（`AGENT_BACKEND`）

優先順位:

1. `AGENT_BACKEND=bedrock` → **BedrockBackend**（AWS SDK、従量課金）
2. OAuth (Claude Max) あり → **AnthropicApiBackend**（サブスク定額）
3. `ANTHROPIC_API_KEY` あり → **AnthropicApiBackend**（API Key 認証）
4. いずれもなければ Bedrock へフォールバック

すべて `AgentBackend` トレイトを実装（`src/anthropic/backend.rs`, `src/claude.rs`）。

## セットアップ

```bash
# ビルド
cargo build --release

# 環境変数
cp .env.example .env
vim .env  # SLACK_BOT_TOKEN / ASANA_PAT / ASANA_PROJECT_ID 等

# 設定
cp config/repos.toml.example config/repos.toml
vim config/repos.toml  # repos / schedules / slack channel

# 起動
./target/release/ambient-task-agent serve --port 3000
```

## 開発

```bash
cargo clippy -- -D warnings       # 必ずクリーンに保つ
cargo test                        # 約 112 件
```

- 設計判断は [`docs/adr/`](docs/adr/) に記録
- 破壊的変更は [`CHANGELOG.md`](CHANGELOG.md) に記録
- 詳細な設計: [`plan/design.md`](plan/design.md), [`plan/requirements.md`](plan/requirements.md)
- DB テストは `Connection::open_in_memory()` でインメモリ（`Db::open` はファイルパス必須）

## 設計判断（ADR）

| ADR | 決定 |
|-----|------|
| [0001](docs/adr/0001-db-separate-tables.md) | coding_tasks と ops_queue を DB 統合しない |
| [0002](docs/adr/0002-conversing-manual-status.md) | plan/approve 廃止 → conversing/manual に統合 |
| [0003](docs/adr/0003-hybrid-session-management.md) | セッション管理のハイブリッド方式 |
| [0004](docs/adr/0004-append-system-prompt.md) | `--system-prompt` → `--append-system-prompt` |
| [0005](docs/adr/0005-self-improvement-loop.md) | 自己改善ループ + git-ratchet |
| [0006](docs/adr/0006-tokio-spawn-parallelization.md) | `process_tasks()` の tokio::spawn 並列化 |
| [0007](docs/adr/0007-llm-classification-learning.md) | LLM 分類学習（few-shot classify） |

## なぜ OpenClaw を使わないか

[OpenClaw](https://github.com/openclaw/openclaw) はマルチチャンネル対応の汎用パーソナルアシスタントとして完成度が高い。ただし、チーム業務自動化という用途では合わない部分があった。

**セキュリティ面**: OpenClaw はデフォルトでエージェントがホスト上でフルアクセスで動作する。チーム Slack からの任意メッセージがコード実行に繋がる経路を持つため、インプットを Asana タスク（承認済み）と ops_admin に限定したいチーム運用では攻撃面が広すぎる。

**コスト面**: OpenClaw は Gateway が常時起動し、セッションが持続する設計になっている。ambient-task-agent はタスクがあるときだけ Claude を起動するため、LLM 課金がタスク数に比例しコントロールしやすい。

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
