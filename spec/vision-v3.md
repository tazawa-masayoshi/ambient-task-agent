# ambient-task-agent v3 ビジョン

> **このドキュメントは `spec/redesign.md` を置き換える最新設計書です。**
> 以前の設計（`coding_tasks` 単一テーブル）は廃止し、`tasks / task_attempts / task_events / task_reviews` の4テーブル構成に移行します。

## 参考プロジェクト

### オーケストレーション / タスク管理

| プロジェクト | 概要 | 参考ポイント |
|-------------|------|-------------|
| [openai/symphony](https://github.com/openai/symphony) | Issue Tracker → 自律 Agent ディスパッチャー（Elixir） | Tracker ポーリング → 自律ディスパッチ、per-issue workspace isolation、harness engineering（CI=完了証拠）、WORKFLOW.md 統一設定（YAML+Markdown）、動的リロード、DB レス設計（Tracker が SoT）、Tracker アダプタパターン、blocker 依存解決 |
| [BloopAI/vibe-kanban](https://github.com/BloopAI/vibe-kanban) | コーディングエージェント向け Plan→Execute→Review 環境（Rust+TS） | Plan→Execute→Review の3段階統合、per-issue workspace（ブランチ+ターミナル+devサーバー）、マルチエージェント対応（Claude Code/Codex/Gemini 等10+）、インライン diff レビュー、カンバン UI、PR 自動生成 → GitHub マージ |

### Agent ランタイム / フレームワーク

| プロジェクト | 概要 | 参考ポイント |
|-------------|------|-------------|
| [openclaw/openclaw](https://github.com/openclaw/openclaw) | パーソナル AI アシスタント — Gateway + マルチチャネル + デバイスノード（Node.js/TS） | Gateway WS コントロールプレーン（セッション・プレゼンス・cron・webhook 統合管理）、20+ チャネル（Slack Bolt/Discord/Telegram/WhatsApp/iMessage 等）、クロスエージェント通信（sessions_list/send/history）、per-session state（model/thinking level 切替）、Skills プラットフォーム（bundled/managed/workspace）、デバイスノード（macOS/iOS/Android）統合、cron + webhook + Gmail Pub/Sub、DM pairing セキュリティ |
| [RightNow-AI/openfang](https://github.com/RightNow-AI/openfang) | Agent OS — 自律エージェントの完全なランタイム環境（Rust, 14 crate, 138K LOC） | 「Hands」= 自律実行パッケージ（スケジュール駆動、24/7稼働）、HAND.toml マニフェスト + SKILL.md、40チャネルアダプタ（Slack/Discord/Telegram等）、WASM サンドボックス + 16段セキュリティ、SQLite + ベクトル埋め込み、Merkle hash-chain 監査証跡、承認ゲート（購買等の危険操作に必須承認） |
| [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw) | 軽量自律 AI アシスタント基盤（Rust, <5MB RAM） | トレイト駆動アーキテクチャ（Provider/Channel/Memory/Tool/Runtime 全て差し替え可能）、設定変更だけで実装切替（zero code changes）、SQLite ハイブリッド検索 + PostgreSQL 対応、15+ チャネル対応、ガイダンスシステム（TOML マニフェスト + SKILL.md）、daemon モード常時稼働 |
| [sipeed/picoclaw](https://github.com/sipeed/picoclaw) | 超軽量パーソナル AI アシスタント（Go, <10MB RAM, $10 HW で動作） | Gateway モード（永続 Webhook サーバー）+ Agent モード（CLI）+ Launcher モード（Web UI）、RISC-V/ARM/MIPS/x86 マルチアーキテクチャ、チャット統合（Telegram/Discord/WhatsApp/LINE等）、model_list 形式でゼロコードプロバイダ追加 |

### プロンプト設計

| プロジェクト | 概要 | 参考ポイント |
|-------------|------|-------------|
| [phuryn/pm-skills](https://github.com/phuryn/pm-skills) | PM 向けプロンプトスキル集 | ロール設定 → Think Step by Step → 構造化テンプレートのパターン、Tiger 分類フレームワーク |

## アーキテクチャ思想

```
┌──────────────────────────────────┐
│  Tracker（Asana / Linear）       │ ← タスクが入ってくるパイプ（汎用・交換可能）
│  = Source of Truth for 存在      │
└──────────────┬───────────────────┘
               │ sync（Webhook + ポーリング）
┌──────────────▼───────────────────┐
│  SQLite DB                       │ ← Agent の脳（状態管理・分析・レビュー・連携情報）
│  = Source of Truth for 知性      │    Tracker にない全てのデータがここにある
└──────────────┬───────────────────┘
               │ Worker ループ
┌──────────────▼───────────────────┐
│  Agent Engine                    │ ← 分析・実行・CI確認・リトライ
│  + Scheduler + Ops Monitor       │
└──────────────┬───────────────────┘
               │
┌──────────────▼───────────────────┐
│  Slack                           │ ← ユーザーインターフェース（依頼・承認・軌道修正）
│  = 開発コックピット              │    ターミナルでもOK（同じ DB を共有）
└──────────────────────────────────┘
```

**Tracker は何でもいい。** Asana でも Linear でも GitHub Issues でも、
入り口が変わるだけで DB 以降のパイプラインは同じ。

## コンセプト

**「Slack を開発コックピットにする」**

ターミナルを開かずに、Slack 上でタスクの依頼・監視・承認・軌道修正ができる。
腰を据えたいときはターミナルで直接作業。どちらでも同じタスク状態を共有する。

## 3 本柱

### 1. Slack プログラミング

Slack チャンネルでタスクを依頼し、エージェントが自律的にコーディングする。
承認・軌道修正・完了確認まで Slack 上で完結。

```
田澤: 「@bot 認証機能を JWT で追加して」
  ↓
bot: 📋 タスク作成しました（Asana #123）
bot: 🔍 要件分析中...
bot: [要件定義] こういう設計で進めます → ok / ng / 修正指示
  ↓
田澤: 「ok」
  ↓
bot: 🔨 実装中...（workspace: /tmp/workspaces/ABC-123/）
bot: ✅ 実装完了。PR: https://github.com/...
bot: CI: ✅ 全テスト通過 / カバレッジ 85%
  ↓
田澤: 「:+1:」 → マージ
```

**ポイント:**
- スレッド内で会話を続けることで軌道修正できる
- CI/テスト結果が「完了の証拠」（harness engineering）
- per-issue workspace で他の作業に影響しない

### 2. 自動監視 + 定型作業

固定の作業パターンを監視し、トリガー条件を満たしたら自動実行する。

| パターン | トリガー | アクション |
|---------|---------|-----------|
| ops チャンネル | `:zap:` / @bot メンション | スキル/ツール自動実行 |
| 定時ジョブ | cron | ブリーフィング、振り返り等 |
| Webhook | Asana タスク変更 | DB 同期、ステータス更新 |

### 3. PM / 秘書機能

Slack を通じて自分をマネジメントしてくれる。

| 機能 | タイミング | 内容 |
|------|----------|------|
| 朝のブリーフィング | 平日 9:00 | タイムボクシング + 優先度整理 + GCal 自動配置 |
| 停滞チェック | 平日 14:00 | Tiger 分類で停滞原因を診断 |
| 夕方の振り返り | 平日 18:00 | 成果サマリー + 明日への引き継ぎ |
| 週次レビュー | 金曜 17:00 | ベロシティ + ボトルネック + 来週戦略 |
| 会議リマインダー | 5分間隔 | 15分前アラート + Meet リンク |

## コアループ

```
Asana (Source of Truth)
  ↕ sync（Webhook + ポーリング）
SQLite DB (ローカル状態 + 分析結果 + 会話履歴)
  ↕ Worker ループ（常時稼働）
  ├─ ディスパッチャー: 新規タスク検出 → 分析 → 提案 → 承認待ち
  ├─ エグゼキューター: 承認済みタスク → workspace 作成 → 実行 → PR
  ├─ スケジューラー: cron ジョブ実行
  └─ モニター: ops チャンネル監視
  ↕
Slack (ユーザーインターフェース)
  ├─ タスク依頼・承認・軌道修正
  ├─ ブリーフィング・振り返り受信
  └─ ops コマンド実行
```

## Symphony から取り入れる設計

### 1. Per-Issue Workspace Isolation

タスクごとに独立した作業ディレクトリを作成する。

```
workspace_root/
  ├── ABC-123/          # git worktree or clone
  │   └── (作業ファイル)
  ├── ABC-124/
  └── ABC-125/
```

**実装方針:** `git worktree` を活用。メインリポジトリを汚さない。

```rust
// workspace 作成
fn create_workspace(repo_path: &Path, issue_id: &str, branch: &str) -> Result<PathBuf> {
    let ws_root = workspace_root();
    let ws_path = ws_root.join(sanitize(issue_id));
    // git worktree add <ws_path> -b <branch>
    // → after_create hook 実行
    Ok(ws_path)
}

// workspace 削除（タスク完了/キャンセル時）
fn remove_workspace(issue_id: &str) -> Result<()> {
    // before_remove hook 実行
    // git worktree remove <ws_path>
    Ok(())
}
```

### 2. Harness Engineering

CI/テストが通ることを「完了の証拠」にする。

```
実装完了
  → PR 作成
  → CI 実行（自動）
  → CI 結果を Slack スレッドに投稿
  → CI 通過 → 承認待ちに遷移
  → CI 失敗 → エージェントが自動修正を試行（max_retry 回）
```

**ステータス遷移の変更:**
```
現在: proposed → approved → executing → done
変更: proposed → approved → executing → ci_pending → ci_passed → done
                                       → ci_failed → executing（リトライ）
```

### 3. 統一設定ファイル（WORKFLOW.md 方式）

repos.toml + soul.md を統合し、リポジトリごとに `WORKFLOW.md` を持てるようにする。

```markdown
---
# config/workflows/slack-task-runner.md
tracker:
  kind: asana
  project_gid: "1209044193035773"

workspace:
  isolation: worktree    # worktree | clone | none
  root: /tmp/ambient-workspaces

agent:
  max_concurrent: 2
  max_turns: 20
  timeout_secs: 900

hooks:
  after_create: |
    npm install
  before_run: |
    npm test -- --bail
  after_run: |
    npm run lint

ci:
  check_command: "gh run list --limit 1 --json conclusion -q '.[0].conclusion'"
  required_status: "success"
  max_retry: 3
---

# System Prompt

あなたは田澤のプロジェクトマネージャー兼エグゼクティブアシスタントです。
（現在の soul.md の内容がここに入る）

# 作業指示テンプレート

タスク: {{ issue.title }}
説明: {{ issue.description }}
ブランチ: {{ workspace.branch }}
```

**メリット:**
- 1ファイルで設定 + プロンプトが完結
- リポジトリごとにカスタマイズ可能
- 動的リロード対応しやすい（1ファイル監視するだけ）

### 4. 動的設定リロード

設定ファイルの変更を検知し、再起動なしで反映する。

```rust
// ファイル監視（notify crate）
// 変更検知 → パース → Orchestrator に通知
// 反映対象: polling interval, max_concurrent, プロンプト, hooks
// 非反映（再起動必要）: DB パス, Slack トークン, ポート番号
```

## Slack → ターミナルのシームレス切り替え

```
Slack で依頼 → エージェントが workspace で作業中
  ↓
田澤「やっぱ自分でやる」
  ↓
bot: ワークスペースはここです → /tmp/workspaces/ABC-123/
     現在の進捗: src/auth.rs に JWT 認証を実装中（60%）
  ↓
田澤: cd /tmp/workspaces/ABC-123/ && claude
  ↓
（ターミナルで直接作業）
  ↓
完了したら Slack で「done」→ PR 作成 + CI 確認
```

## DB 設計の方針

### 設計原則

- **Tracker (Asana/Linear) = Source of Truth** — タスクの存在・タイトル・期限・担当
- **DB = Agent の脳** — Tracker にない情報を全て吸収する
- **Tracker に依存しない状態管理** — Asana→Linear 移行時に DB スキーマは変えない

### Linear vs Asana vs DB のフィールド比較

以下の表で、各フィールドが「どこに保存されるか」を整理する。

| 概念 | Linear | Asana | DB で吸収 |
|------|--------|-------|-----------|
| **ID** | `id` | `gid` | `tracker_id` (統一キー) |
| **識別子** | `identifier` (ABC-123) | なし | `tracker_identifier` |
| **タイトル** | `title` | `name` | sync でキャッシュ |
| **説明** | `description` | `notes` | sync でキャッシュ |
| **状態** | `state` (Triage→Backlog→Todo→InProgress→Done→Canceled) | `completed` + section（手動） | `agent_status` (独自状態機械) |
| **優先度** | `priority` (0-4 整数) | なし（カスタムフィールド要） | `priority_score` (float) |
| **見積もり** | `estimate` (ポイント/Tシャツ) | なし（カスタムフィールド要） | `estimated_minutes` |
| **担当者** | `assignee` | `assignee` | — (Tracker で管理) |
| **期限** | `dueDate` | `due_on` / `due_at` | — (Tracker で管理) |
| **依存関係** | `relations` (blocks/blocked-by + 各状態) | `dependencies` (リスト) | `blocked_by_json` (状態付き) |
| **ラベル** | `labels` (プロジェクトスコープ) | `tags` | `labels_json` |
| **サイクル** | `cycle` (スプリント) | なし | — (将来検討) |
| **ブランチ名** | `branchName` (自動生成) | なし | `branch_name` |
| **SLA** | `slaBreachesAt` | なし | — (将来検討) |
| **作成/更新日** | `createdAt`/`updatedAt` | `created_at`/`modified_at` | `created_at`/`updated_at` |
| **完了日** | `completedAt` | `completed_at` | `completed_at` |

### テーブル設計（vibe-kanban の task/attempt 分離 + Symphony のリトライ + 独自拡張）

現在の `coding_tasks` は「タスクとは何か」と「実行状態」が混在している。
これを **tasks（何を）/ attempts（どうやった）/ events（何が起きた）** に分離する。

参考: vibe-kanban の `tasks` → `task_attempts` → `task_attempt_activities`

```
tasks (タスク定義 = 不変の「何をやるか」)
  ├── task_attempts (実行試行 = 「n回目の挑戦」)
  │     └── task_events (活動ログ = 「各試行で何が起きたか」)
  └── task_reviews (人間レビュー = 「承認/却下の履歴」)
```

> **必須: DB接続時に `PRAGMA foreign_keys = ON` を実行すること。**
> SQLite はデフォルトで外部キー制約が無効。ON DELETE CASCADE / SET NULL はこれがないと動作しない。
> 現行の `src/db.rs` では `PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;` として実装済み。

#### tasks テーブル（タスク定義）

```sql
CREATE TABLE tasks (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,

    -- Tracker 連携（Asana/Linear 両対応）
    tracker_kind        TEXT NOT NULL DEFAULT 'asana',  -- 'asana' | 'linear'
    tracker_id          TEXT NOT NULL,                  -- Asana GID or Linear ID
    tracker_identifier  TEXT,                           -- 'ABC-123' (Linear のみ)
    title               TEXT NOT NULL,
    description         TEXT,

    -- Agent 状態
    -- agent_status は「タスク全体のパイプライン進行」を表す粗い状態。
    -- 各試行の細かい状態（ci_passed/ci_failed 等）は task_attempts.status で管理。
    -- ci_pending: 少なくとも1つの attempt が CI 待ち中
    -- done: 最新 attempt が ci_passed かつマージ済み
    agent_status        TEXT NOT NULL DEFAULT 'new'
        CHECK (agent_status IN (
            'new',            -- Tracker から取り込み済み、未着手
            'analyzing',      -- LLM が要件分析中
            'proposed',       -- 分析完了、人間レビュー待ち
            'approved',       -- 人間が承認、実行待ち
            'executing',      -- Agent が実装中
            'ci_pending',     -- PR 作成済み、CI 待ち（attempt 側で ci_passed/ci_failed を追跡）
            'done',           -- 完了（最新 attempt が ci_passed + マージ済み）
            'rejected',       -- 人間が却下
            'archived',       -- アーカイブ
            'sleeping'        -- スヌーズ中
        )),

    -- リポジトリ
    repo_key            TEXT,
    -- branch_name: タスク全体のデフォルトブランチ名（試行ごとのブランチは task_attempts.branch_name）
    branch_name         TEXT,

    -- LLM 生成物（再分析で上書きされることに注意）
    -- 変化履歴を残したい場合は task_events に event_type='analysis_result' で記録する
    analysis_text       TEXT,           -- 最新の要件分析結果
    subtasks_json       TEXT,           -- 最新の分解サブタスク

    -- PM メタデータ
    priority_score      REAL,           -- 動的優先度スコア (float)
    estimated_minutes   INTEGER,        -- 見積もり時間（分）
    complexity          TEXT,           -- simple / standard / complex
    -- blocked_by_json: JSON配列 [{id, identifier, state}]
    -- ⚠️ SQL側でフィルタ不可。ディスパッチャーは「全 approved タスク取得 → Rust側でJSON解析 → blocker状態確認」の2段階フィルタで処理する。
    --    将来的に依存数が多くなれば task_dependencies テーブルへ別出しを検討。
    blocked_by_json     TEXT,           -- 依存タスク + 各状態
    labels_json         TEXT,           -- ラベル/タグ

    -- Slack: 依頼元（origin）
    requester_slack_id  TEXT,           -- 依頼者の Slack UID
    origin_channel      TEXT,           -- 依頼が来たチャンネル
    origin_thread_ts    TEXT,           -- 依頼メッセージのスレッド ts
    origin_message_ts   TEXT,           -- 依頼メッセージ自体の ts

    -- Slack: 応答先（response）
    slack_channel       TEXT,           -- Agent が応答するチャンネル
    slack_thread_ts     TEXT,           -- Agent の進捗スレッド ts
    slack_plan_ts       TEXT,           -- 要件定義メッセージの ts

    -- 結果
    pr_url              TEXT,
    summary             TEXT,           -- 完了サマリー
    retrospective_note  TEXT,           -- ふりかえりメモ
    memory_note         TEXT,           -- 次回実行への引き継ぎ

    -- 時間計測
    progress_percent    INTEGER,        -- 0-100
    started_at          TEXT,
    completed_at        TEXT,
    actual_minutes      INTEGER,

    -- タイムスタンプ
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
```

#### task_attempts テーブル（実行試行）

1つのタスクに対して複数回の実行試行がありえる。
CI 失敗 → リトライ、人間が changes_requested → 再実行、など。

```sql
CREATE TABLE task_attempts (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id             INTEGER NOT NULL,

    -- 実行環境
    workspace_path      TEXT,           -- per-issue workspace パス
    executor            TEXT,           -- 'claude-code' | 'codex' | 'manual'
    branch_name         TEXT,           -- この試行で使ったブランチ

    -- 結果
    status              TEXT NOT NULL DEFAULT 'running'
        CHECK (status IN (
            'running',        -- 実行中
            'ci_pending',     -- CI 待ち
            'ci_passed',      -- CI 通過
            'ci_failed',      -- CI 失敗
            'completed',      -- 正常完了
            'failed',         -- エラー終了
            'cancelled'       -- キャンセル
        )),
    pr_url              TEXT,           -- この試行で作った PR
    ci_url              TEXT,           -- CI run の URL
    error_message       TEXT,

    -- stall 検出 (Symphony 由来)
    last_agent_event_at TEXT,           -- 最後の Agent アクティビティ

    -- タイムスタンプ
    started_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    finished_at         TEXT,

    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
);
```

#### task_events テーブル（活動ログ）

各試行で何が起きたかの時系列ログ。デバッグ・ふりかえりに使う。

```sql
CREATE TABLE task_events (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id          INTEGER NOT NULL,

    event_type          TEXT NOT NULL,   -- 'status_change' | 'agent_output' | 'ci_result' | 'human_input' | 'error'
    detail              TEXT,            -- イベントの詳細（JSON or テキスト）

    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),

    FOREIGN KEY (attempt_id) REFERENCES task_attempts(id) ON DELETE CASCADE
);
```

#### task_reviews テーブル（人間レビュー履歴）

```sql
CREATE TABLE task_reviews (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id             INTEGER NOT NULL,
    attempt_id          INTEGER,         -- 特定の試行に対するレビュー（NULL = タスク全体）

    reviewer_slack_id   TEXT NOT NULL,    -- レビューした人
    decision            TEXT NOT NULL     -- 'approved' | 'rejected' | 'changes_requested'
        CHECK (decision IN ('approved', 'rejected', 'changes_requested')),
    comment             TEXT,            -- フィードバック内容

    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),

    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
    FOREIGN KEY (attempt_id) REFERENCES task_attempts(id) ON DELETE SET NULL
);
```

#### テーブル関連図

```
tasks (何をやるか)
  │
  ├──< task_attempts (何回目の実行か)
  │       │
  │       └──< task_events (各実行で何が起きたか)
  │
  └──< task_reviews (人間がどう判断したか)

ops_contexts (ops チャンネルの会話履歴 — タスクとは独立)
sessions (Claude Code セッション状態 — wez-sidebar 向け)
scheduled_jobs (cron ジョブ定義)
meeting_reminders (会議通知済み記録)
webhook_events (Asana Webhook ログ)
```

#### 実例: 認証機能追加タスクの流れ

```
tasks #42: 「JWT認証機能を追加」
  agent_status: new → analyzing → proposed → approved → executing → ci_pending → done
  requester_slack_id: U012ABC
  origin_channel: C_OPS

  task_reviews:
    #1: U012ABC, approved, "LGTM、JWTでいこう"

  task_attempts:
    #1: workspace=/tmp/ws/42/, executor=claude-code
        status: ci_failed
        pr_url: https://github.com/.../pull/10
        events:
          - status_change: running (10:00)
          - agent_output: "src/auth.rs を実装中" (10:15)
          - status_change: ci_pending (10:30)
          - ci_result: failure "test_auth_jwt FAILED" (10:35)

    #2: workspace=/tmp/ws/42/, executor=claude-code
        status: ci_passed → completed
        pr_url: https://github.com/.../pull/10  (force-push で更新)
        events:
          - status_change: running (10:36)
          - agent_output: "テスト修正中" (10:40)
          - ci_result: success (10:50)
          - status_change: completed (10:50)

→ Slack: ops チャンネルで <@U012ABC> に完了通知
→ Tracker: Asana タスクを完了に更新
```

### 状態機械の設計

**Tracker の状態**（Asana section / Linear state）と **Agent の状態** を分離する。

```
Tracker 側（Source of Truth for 存在）:
  Asana:  section 移動 + completed フラグ
  Linear: Todo → In Progress → Done → Canceled

Agent 側（DB: tasks.agent_status）:
  new → analyzing → proposed → [task_reviews で分岐]
                                 ├─ approved → executing → ci_pending → done
                                 ├─ changes_requested → analyzing（再分析）
                                 ├─ rejected → archived
                                 └─ (auto_approved → executing → ...)

Attempt 側（DB: task_attempts.status）:
  running → ci_pending → ci_passed → completed
                       → ci_failed → (新しい attempt を作成)
         → failed → (新しい attempt を作成)
         → cancelled
```

**ポイント:**
- `tasks.agent_status` = パイプライン全体の進行状態
- `task_attempts.status` = 各実行試行の結果
- `task_reviews` = 人間の判断履歴（誰が・いつ・何と言ったか）
- `task_events` = 詳細な活動ログ（デバッグ・ふりかえり用）
- Tracker の状態と Agent の状態は独立。Agent が done になったら Tracker も完了にする

### Tracker アダプタパターン

将来 Asana → Linear に移行しても DB スキーマを変えない設計。

```rust
trait TrackerClient {
    async fn fetch_issues(&self) -> Result<Vec<TrackerIssue>>;
    async fn update_state(&self, id: &str, state: &str) -> Result<()>;
    async fn add_comment(&self, id: &str, text: &str) -> Result<()>;
    async fn add_attachment(&self, id: &str, url: &str, title: &str) -> Result<()>;
}

struct TrackerIssue {
    id: String,             // gid or Linear id
    identifier: Option<String>,  // "ABC-123" (Linear only)
    title: String,
    description: Option<String>,
    priority: Option<i32>,  // Linear: 0-4, Asana: custom field → 正規化
    due_date: Option<String>,
    labels: Vec<String>,
    blocked_by: Vec<BlockerRef>,
    state: String,          // Asana: section name, Linear: state name
}
```

DB の `tracker_kind` + `tracker_id` で、
どちらの Tracker でも同じテーブル構造で管理する。

## 実装ロードマップ

### Phase 1: Workspace Isolation（最小変更）
- `git worktree` による per-issue workspace 作成/削除
- executor が workspace 内で claude -p を実行するよう変更
- workspace パスを DB に保存

### Phase 2: Harness Engineering
- CI チェック機能（`gh run list` でステータス確認）
- CI 失敗時の自動リトライ
- ステータス遷移に `ci_pending` / `ci_passed` を追加

### Phase 3: WORKFLOW.md 統合設定
- YAML front matter + Markdown パーサー実装
- repos.toml → WORKFLOW.md マイグレーション
- 動的リロード（notify crate でファイル監視）

### Phase 4: Slack プログラミング強化
- チャンネル内での自由なタスク依頼（@bot + 自然言語）
- スレッド内での軌道修正（会話継続）
- リアルタイム進捗更新（実行中のログをスレッドに投稿）
- `:+1:` でマージ承認
