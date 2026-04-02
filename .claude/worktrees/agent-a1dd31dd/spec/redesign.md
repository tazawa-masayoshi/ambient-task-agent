# ambient-task-agent v2 — 全体設計

## 1. ビジョン

**PM機能つきAI秘書** — 日常の開発作業を裏方で支えるアンビエントエージェント。

- 簡単な作業（typo修正、設定変更、定型保守）は **全自動で対応**
- 複雑な作業は **分析→分解→提案** し、承認後に実行
- プロジェクト全体を俯瞰し、**優先度判断・停滞検出・スケジュール提案** を行う
- タスクごとに **ワークスペースを隔離** し、安全に並列実行

```
「やっといて」→ やってくれる。
「これどうする？」→ 分析して提案してくれる。
「今日何やる？」→ カレンダーとタスクを見て教えてくれる。
```

---

## 2. 基本設計（openFang / ZeroClaw パターン）

### 2.1 設計原則

| 原則 | 説明 |
|------|------|
| **Trait 駆動** | LLM バックエンド、通知チャネル等を trait で抽象化 |
| **コンテキスト統一注入** | `RunnerContext` で設定・フック・バックエンドを一括管理 |
| **設定駆動** | TOML で実行ポリシー・スケジュール・リポジトリ定義 |
| **防御的実行** | ループ検出、セマフォ、タイムアウト、exec_mode で暴走防止 |
| **Soul/Skill 分離** | 性格（判断基準）と手順（具体的作業）を分離 |

### 2.2 レイヤー構造

```
┌─────────────────────────────────────────────────┐
│  Trigger Layer (入力)                            │
│  ├─ Asana Webhook          (タスク作成・更新)     │
│  ├─ Slack Events/Actions   (メッセージ・ボタン)   │
│  ├─ Claude Code Hooks      (セッション開始・終了) │
│  └─ Cron Scheduler         (朝ブリーフィング等)   │
├─────────────────────────────────────────────────┤
│  Orchestration Layer (制御)                      │
│  ├─ Worker Loop             (イベント駆動+HB)    │
│  ├─ Task Pipeline           (analyze→decompose→execute) │
│  ├─ Workspace Manager [NEW] (git worktree 管理)  │
│  └─ PM Engine               (優先度・停滞・提案)  │
├─────────────────────────────────────────────────┤
│  Execution Layer (実行) — ダブルエージェント       │
│  ├─ trait AgentBackend      (LLM 抽象)           │
│  │  ├─ ClaudeCliBackend     (claude -p: PM+coding)│
│  │  └─ BedrockBackend [NEW] (API直呼び: ops専用) │
│  ├─ RunnerContext           (環境・制約一括注入)   │
│  └─ ExecutionHook           (ループ検出等)        │
├─────────────────────────────────────────────────┤
│  Persistence Layer (永続化)                      │
│  ├─ SQLite (coding_tasks, ops_contexts, sessions)│
│  └─ File System (.agent/, tasks-cache.json)      │
├─────────────────────────────────────────────────┤
│  Output Layer (出力)                             │
│  ├─ Slack API         (Block Kit, スレッド返信)   │
│  ├─ Asana API         (タスク更新・コメント)      │
│  ├─ wez-sidebar       (tasks-cache.json / API)   │
│  └─ GitHub API [NEW]  (PR 作成)                  │
└─────────────────────────────────────────────────┘
```

### 2.3 ダブルエージェント構想

**`trait AgentBackend` を活用し、用途に応じて 2 つのバックエンドを使い分ける。**

```
┌─────────────────────────────────────────────────────────┐
│                  trait AgentBackend                      │
│  async fn execute(&self, request) -> Result<AgentOutput> │
├────────────────────────┬────────────────────────────────┤
│   ClaudeCliBackend     │    BedrockBackend [NEW]        │
│   (claude -p)          │    (AWS Bedrock API)           │
│                        │                                │
│   用途:                │    用途:                        │
│   - PM 判断・分析      │    - ops (定型保守作業)         │
│   - タスク分解提案     │                                │
│   - 文章生成           │    特徴:                        │
│   - 朝ブリーフィング   │    - ハングしない (API直呼び)   │
│                        │    - 軽量ツールのみ             │
│   特徴:                │      (Read,Write,Edit,Bash,    │
│   - MCP ツール利用可   │       Glob,Grep)               │
│   - フルツールセット   │    - Slack 応答の確実性が最重要 │
│   - 既存インフラ       │    - マルチターン少 (軽量)      │
│   - サブスク内で動作   │    - 従量課金 → コスト効率      │
└────────────────────────┴────────────────────────────────┘

※ コーディング (executor) は当面 claude -p を使用。
  Anthropic がサブスク内でエージェント常駐を
  正式サポートする方向に寄せているため、
  executor の Bedrock 移行は状況を見て判断。
```

#### バックエンド選択ロジック

```rust
// RunnerContext に 2 つのバックエンドを保持
pub struct RunnerContext {
    pub defaults: Defaults,
    pub semaphore: Arc<Semaphore>,
    pub registry: Arc<ExecutionRegistry>,
    pub hooks: Arc<HookRegistry>,
    pub resolved_env: Vec<(String, String)>,
    // ダブルエージェント
    pub cli_backend: Arc<dyn AgentBackend>,      // claude -p (PM判断 + coding)
    pub ops_backend: Arc<dyn AgentBackend>,       // Bedrock (ops専用)
}

// モジュールごとに適切なバックエンドを選択
impl RunnerContext {
    pub fn backend_for(&self, module: &str) -> &Arc<dyn AgentBackend> {
        match module {
            // ops → Bedrock (ハングしない、確実)
            "ops" => &self.ops_backend,
            // それ以外 → claude -p (MCP利用可、フルツール)
            _ => &self.cli_backend,
        }
    }
}
```

#### BedrockBackend の実装方針

```rust
pub struct BedrockBackend {
    client: aws_sdk_bedrockruntime::Client,
    model_id: String,  // e.g. "anthropic.claude-sonnet-4-20250514"
    tools: Vec<ToolDefinition>,  // Read,Write,Edit,Bash,Glob,Grep
}

#[async_trait]
impl AgentBackend for BedrockBackend {
    async fn execute(&self, request: AgentRequest) -> Result<AgentOutput> {
        // Bedrock Converse API で実行
        // - system_prompt → system メッセージ
        // - prompt → user メッセージ
        // - ツール定義は self.tools から注入
        // - max_turns ループを自前管理
        // - cwd はサーバー側で制御
        // - ツール実行は自前 (std::fs, tokio::process)
    }
}
```

**段階的移行**:
1. まず全モジュール `ClaudeCliBackend` で動作（現状）
2. `BedrockBackend` を実装し **ops のみ** 切り替え
3. ops で安定したら、他モジュールへの展開は **Anthropic の方向次第で判断**
4. `claude -p` がサブスク内で安定して動くなら、executor は移行しない

### 2.4 その他の Trait

```rust
trait ExecutionHook: Send + Sync {
    fn before_run(&self, module: &str, prompt_summary: &str) -> HookDecision;
    fn after_run(&self, record: &ExecutionRecord);
}
```

追加の trait 抽象化は **現時点では不要**。
Slack only の環境で Channel 抽象や Tool 抽象を入れるのは過剰。

---

## 3. PM機能（AI秘書としての知性）

### 3.1 現行機能（実装済み）

| 機能 | 実装場所 | 説明 |
|------|----------|------|
| 朝ブリーフィング | scheduler.rs | 期限超過・ブロッカー・今日のタスク・会議予定を Slack に投稿 |
| 夕方サマリー | scheduler.rs | 本日の完了タスク + 明日の提案 |
| 停滞検出 | scheduler.rs | 24h 未更新タスクを検出 → 原因分析・分割提案 |
| 週次レビュー | scheduler.rs | 金曜に振り返り + 来週スケジュール |
| 会議リマインダー | scheduler.rs | Google Calendar 連携で15分前通知 |
| 優先度スコア | priority.rs | 停滞時間・進捗率・ステータス・blocked率で動的計算 |
| 適応型深度 | analyzer.rs + runner.rs | simple/standard/complex で分析・実行の深度を変える |
| タイムボクシング | scheduler.rs | カレンダー空き時間にタスクを配置提案 |

### 3.2 soul.md（性格定義）

```
サーバント型プロジェクトマネージャー
- 優先度: 期限超過 > ブロッカー除去 > 今日期限 > 高インパクト
- 安全性 > 正確性 > 効率性
- 提案はするが最終判断はユーザーに委ねる
- スコープクリープ防止
```

PM機能は **現行で十分な基盤がある**。
強化ポイントは「ワークスペース隔離による並列実行の安全性向上」と「PR自動作成」。

### 3.3 週報自動集約 [NEW]

`.agent/context.md` に蓄積された完了タスク情報を、金曜の週次レビューで
**週報フォーマットに整形して Slack 投稿**する。

```
現行:
  夕方サマリー: 「今日の完了タスク」を日次で報告
  週次レビュー: 振り返り + 来週提案（フリーフォーマット）

改善:
  週次レビュー時に以下を自動生成:
  ┌────────────────────────────────┐
  │ ## 今週のサマリー               │
  │ - 完了: 5件 (42pt)             │
  │ - 進行中: 3件                  │
  │ - ブロッカー: 1件              │
  │                                │
  │ ## 完了タスク                   │
  │ - [hikken] ログイン画面実装     │
  │ - [hikken] API認証修正          │
  │ - [infra] CI パイプライン整備   │
  │                                │
  │ ## 学び・気づき                 │
  │ - context.md / memory.md から抽出│
  │                                │
  │ ## 来週の計画                   │
  │ - 優先度順にタスクを配置        │
  └────────────────────────────────┘
```

**実装**: 週次レビューの system_prompt にフォーマット指示を追加するだけ。
データソースは SQLite (coding_tasks) + `.agent/context.md`。

### 3.4 Epic / カテゴリ分類 [NEW]

Asana のセクション・プロジェクト情報をタスク分類に活用する。

```
現行:
  CodingTask に complexity (simple/standard/complex) と
  priority_score (動的計算) のみ

改善:
  Asana Webhook から受け取るセクション情報を DB に保存し、
  以下に活用:

  1. 朝ブリーフィングでカテゴリ別グルーピング
     「インフラ: 2件、機能開発: 3件、バグ修正: 1件」

  2. 優先度計算の重み付け
     section_weights in repos.toml:
       [defaults.section_weights]
       "ビジネス" = 1.5    # 優先度 1.5 倍
       "インフラ" = 0.8    # 優先度 0.8 倍
       "技術的負債" = 0.5  # 優先度 0.5 倍

  3. 週報のカテゴリ別集計
```

**実装**:
- `CodingTask` に `section` フィールド追加（Webhook から取得済み、DB保存を追加）
- `priority.rs` で section_weights を加味
- 朝ブリーフィング・週報のプロンプトにカテゴリ情報を含める

---

## 4. 簡単な作業の全自動対応

### 4.1 現行の自動実行パス

```
適応型深度: simple
  → 分解スキップ（サブタスク1つ自動生成）
  → Slack で「simpleタスクのため分解をスキップしました」

自動承認:
  → Slack で提案に :robot: リアクション → auto_approved
  → executor が自動実行
```

### 4.2 ops チャンネル（定型保守）

```
Slack ops_channel にメッセージ投稿
  → ops_skills (.claude/commands/ops.md) を読み込み
  → claude -p で実行
  → 結果をスレッドに返信
  → 会話履歴は ops_contexts テーブルに永続化（スレッド返信で文脈継続）
```

### 4.3 完全自動モード（検討中）

simple タスクで `auto_execute: true` が設定されている場合:
```
new → analyzing → proposed → [自動承認] → executing → done
                             ^^^^^^^^
                             人間の承認をスキップ
```

**判断基準**:
- complexity = simple
- repo の `auto_execute` フラグが true
- 影響範囲が1ファイル以下

→ **リスク**: 安全性とのバランス。まずは simple + :robot: リアクション運用を続けて、信頼度が上がったら段階的に自動化。

---

## 5. タスク設定（Symphony 的 vs 現行）

### 5.1 現行フロー

```
Asana タスク作成/更新
  → Asana Webhook → ambient-task-agent
  → repos.toml の match ルールでリポジトリ紐付け
  → analyze → propose (Slack承認) → decompose → execute
```

**利点**: Asana がタスク管理の source of truth。既存の Asana ワークフローと共存。
**課題**: Asana Webhook の設定が複雑。タスクの粒度が Asana に依存。

### 5.2 Symphony 的フロー

```
チケットボード (Linear/Asana) にタスク投入
  → ポーリング or Webhook で検知
  → git worktree でワークスペース隔離
  → WORKFLOW.md (= soul.md + skill.md) でプロンプト構築
  → エージェント実行 → PR 作成
  → CI/テスト検証 → レビュー
  → Rework 時は worktree 全削除 → 新規作成
```

**利点**: ワークスペース隔離で安全。PR ベースのレビューフロー。
**課題**: 現行の Slack 承認フローとの共存。

### 5.3 ハイブリッド方針（推奨）

**現行の Asana + Slack フローを維持しつつ、Symphony の良いところを取り入れる。**

| Symphony の要素 | 取り入れ方 |
|---|---|
| ワークスペース隔離 | **git worktree** — executor 実行時に自動作成 |
| WORKFLOW.md | 既存の **soul.md + skill.md + .claude/rules/** で代替（十分） |
| PR 自動作成 | executor 完了後に **gh pr create** |
| Rework | worktree 削除 → 新規作成（再生成ボタンと連動） |
| チケットポーリング | 既存の **Asana Webhook** を維持（ポーリング不要） |

**変えないもの**:
- Asana = source of truth（変更なし）
- Slack 承認フロー（変更なし）
- repos.toml 設定駆動（変更なし）

**追加するもの**:
- git worktree によるワークスペース隔離
- PR 自動作成
- Rework 時の worktree リセット

---

## 6. ワークスペース隔離（git worktree）[NEW]

### 6.1 ディレクトリ構成

```
{repos_base_dir}/
  {repo_key}/                    # メインブランチ（既存、読み取り専用として扱う）
  .worktrees/
    {repo_key}-task-{id}/        # タスクごとの隔離ワークスペース
    {repo_key}-task-{id2}/
```

### 6.2 ライフサイクル

```
タスク承認 (approved / auto_approved)
  ↓
workspace::create(repo_key, task_id, base_branch)
  → git worktree add .worktrees/{repo_key}-task-{id} -b agent/task-{id}
  ↓
executor 実行（cwd = worktree パス）
  ↓
成功時:
  → git add + commit (worktree 内)
  → git push origin agent/task-{id}
  → gh pr create --base {base_branch} --head agent/task-{id}
  → PR URL を Slack に通知 + DB 保存
  → worktree は PR マージまで保持

失敗時 / Rework:
  → git worktree remove .worktrees/{repo_key}-task-{id} --force
  → ブランチ削除
  → 新しい worktree を作成して再実行
```

### 6.3 実装: `src/worker/workspace.rs` [NEW]

```rust
pub struct Workspace {
    pub worktree_path: PathBuf,
    pub branch_name: String,
}

/// タスク用の隔離ワークスペースを作成
pub fn create(
    repos_base_dir: &Path,
    repo_key: &str,
    task_id: i64,
    base_branch: &str,
) -> Result<Workspace>

/// ワークスペースを削除
pub fn remove(workspace: &Workspace) -> Result<()>

/// ワークスペースが存在するか確認
pub fn exists(repos_base_dir: &Path, repo_key: &str, task_id: i64) -> bool

/// commit + push + PR 作成
pub async fn finalize(
    workspace: &Workspace,
    task: &CodingTask,
    base_branch: &str,
) -> Result<String>  // PR URL
```

### 6.4 runner.rs への統合

```rust
// execute_auto_approved_task() の変更

// Step 1: worktree 作成
let ws = workspace::create(
    &base_dir, repo_key, task.id, &repo_entry.default_branch
)?;

// Step 2: executor を worktree パスで実行
let result = executor::execute_task(
    ...,
    repo_path: Some(&ws.worktree_path),  // worktree パスを使う
    ...
).await?;

// Step 3: 成功時は PR 作成
if result.success {
    match workspace::finalize(&ws, &task, &repo_entry.default_branch).await {
        Ok(pr_url) => {
            db.update_pr_url(task.id, &pr_url)?;
            slack.reply_thread(channel, thread_ts, &format!(
                ":pull_request: PR を作成しました: {}", pr_url
            )).await.ok();
        }
        Err(e) => { /* PR 作成失敗はエラーだがタスク自体は成功 */ }
    }
}

// Step 4: 失敗時は worktree 削除
if !result.success {
    workspace::remove(&ws).ok();
}
```

---

## 7. 全体データフロー（v2）

```
                    ┌──────────────┐
                    │   Asana      │
                    │  (タスク管理) │
                    └──────┬───────┘
                           │ Webhook
                           ▼
┌──────────────────────────────────────────────────────┐
│              ambient-task-agent (EC2)                 │
│                                                      │
│  ┌────────────────────────────────────────────────┐  │
│  │  HTTP Server (Axum)                            │  │
│  │  /webhook/asana, /webhook/slack, /slack/actions │  │
│  │  /hooks/event, /api/tasks/*                    │  │
│  └────────────────┬───────────────────────────────┘  │
│                   │                                  │
│                   ▼                                  │
│  ┌────────────────────────────────────────────────┐  │
│  │  Worker Loop (event-driven + heartbeat)        │  │
│  │                                                │  │
│  │  Task Pipeline:                                │  │
│  │  ┌─────────┐  ┌───────────┐  ┌─────────────┐  │  │
│  │  │Analyzer │→│Decomposer │→│  Executor   │  │  │
│  │  │(read-   │  │(subtask   │  │(worktree内  │  │  │
│  │  │ only)   │  │ 分解)     │  │ 実行+PR)   │  │  │
│  │  └─────────┘  └───────────┘  └──────┬──────┘  │  │
│  │                                      │         │  │
│  │  ┌────────────────┐  ┌───────────────┘         │  │
│  │  │ Workspace Mgr  │←─┘ git worktree            │  │
│  │  │ (create/remove │    create → execute → PR   │  │
│  │  │  /finalize)    │                             │  │
│  │  └────────────────┘                             │  │
│  │                                                │  │
│  │  PM Engine:                                    │  │
│  │  ┌──────────┐ ┌──────────┐ ┌────────────────┐  │  │
│  │  │Priority  │ │Stagnation│ │Morning Briefing│  │  │
│  │  │Scoring   │ │Detection │ │+ Timeboxing    │  │  │
│  │  └──────────┘ └──────────┘ └────────────────┘  │  │
│  │                                                │  │
│  │  Ops Channel:                                  │  │
│  │  ┌──────────────────────────────────────────┐  │  │
│  │  │ Slack msg → skill 読込 → claude -p → 返信│  │  │
│  │  │ (ops_contexts で会話履歴永続化)           │  │  │
│  │  └──────────────────────────────────────────┘  │  │
│  └────────────────────────────────────────────────┘  │
│                                                      │
│  ┌───────────┐  ┌──────────────┐                     │
│  │ SQLite DB │  │ .agent/ files│                     │
│  └───────────┘  └──────────────┘                     │
└───────────┬──────────┬───────────┬───────────────────┘
            │          │           │
            ▼          ▼           ▼
       ┌────────┐ ┌────────┐ ┌──────────┐ ┌────────┐
       │ Slack  │ │ Asana  │ │wez-sidebar│ │ GitHub │
       │(通知・ │ │(更新)  │ │(TUI表示) │ │(PR作成)│
       │ 承認)  │ │        │ │          │ │        │
       └────────┘ └────────┘ └──────────┘ └────────┘
```

---

## 8. 設定ファイル構成

```
config/
  repos.toml        # リポジトリ定義、スケジュール、実行ポリシー
  soul.md           # PM 性格（判断基準・行動指針）
  skill.md          # 作業手順（グローバル規約）

{repos_base_dir}/
  .agent/
    soul.md          # (config/ からコピーされる)
    skill.md
    context.md       # 横断作業履歴（自動追記）
    memory.md        # 横断学習メモ（自動追記）
    logs/            # 実行ログ（100件ローテーション）
    tasks/{id}.md    # タスクファイル（YAML frontmatter + Markdown）

  {repo_key}/
    .agent/
      context.md     # リポ固有の作業履歴
      memory.md      # リポ固有の学習メモ
    .claude/
      rules/agent.md # エージェント向けルール（自動生成）
```

---

## 9. タスクのライフサイクル（v2）

```
                          ┌──────────────────┐
                          │  Asana Webhook   │
                          │  or Slack trigger│
                          └────────┬─────────┘
                                   │
                                   ▼
                              ┌─────────┐
                              │   new   │
                              └────┬────┘
                                   │ analyzer (read-only, claude -p)
                                   ▼
                             ┌──────────┐
                             │analyzing │
                             └────┬─────┘
                                  │ 要件定義完成
                                  ▼
                             ┌──────────┐
                        ┌────│proposed  │────┐
                        │    └──────────┘    │
                   :robot:リアクション     Slack ボタン
                        │                    │
                        ▼                    ▼
                ┌───────────────┐    ┌────────────┐
                │auto_approved  │    │  approved  │
                └───────┬───────┘    └─────┬──────┘
                        │                  │
                        │   ┌──────────────┘
                        │   │
                        ▼   ▼
              ┌────────────────────┐
              │   decomposing     │  (simple → スキップ)
              └────────┬──────────┘
                       │ サブタスク分解完了
                       ▼
                  ┌──────────┐
                  │  ready   │
                  └────┬─────┘
                       │ 実行ボタン or 自動
                       ▼
              ┌────────────────────┐
              │   executing       │
              │                   │
              │ [NEW] worktree内  │
              │ で隔離実行        │
              └────────┬──────────┘
                       │
              ┌────────┴────────┐
              │                 │
              ▼                 ▼
         ┌─────────┐      ┌─────────┐
         │  done   │      │ failed  │
         │         │      │         │
         │ [NEW]   │      │ [NEW]   │
         │ PR作成  │      │ worktree│
         │ Slack   │      │ 削除    │
         │ 通知    │      │         │
         └─────────┘      └─────────┘
```

---

## 10. 実装ロードマップ

### Phase 1: ワークスペース隔離 (git worktree)

**目標**: タスク実行を安全に隔離し、メインブランチを汚さない

| ファイル | 変更内容 |
|---------|---------|
| `src/worker/workspace.rs` [NEW] | create/remove/exists/finalize |
| `src/worker/runner.rs` | execute_auto_approved_task で worktree 使用 |
| `src/worker/mod.rs` | workspace モジュール追加 |
| `src/db.rs` | `branch_name`, `pr_url` フィールドの活用 |

### Phase 2: PR 自動作成

**目標**: executor 完了後にドラフト PR を作成し、Slack に通知

| ファイル | 変更内容 |
|---------|---------|
| `src/worker/workspace.rs` | finalize() で gh pr create |
| `src/worker/runner.rs` | PR URL の DB 保存 + Slack 通知 |

### Phase 3: Rework 改善

**目標**: 再生成時に worktree を全削除して clean state から再実行

| ファイル | 変更内容 |
|---------|---------|
| `src/server/slack_actions.rs` | regenerate 時に workspace::remove() |
| `src/worker/runner.rs` | 再実行フロー |

### Phase 4: BedrockBackend for ops

**目標**: ops を Bedrock API 直呼びに切り替え、ハングリスクを排除

| ファイル | 変更内容 |
|---------|---------|
| `src/claude.rs` | `BedrockBackend` 実装（Converse API + ツール実行ループ） |
| `src/execution.rs` | `RunnerContext` に `cli_backend` / `ops_backend` 分離 |
| `src/main.rs` | Bedrock クライアント初期化 + 注入 |
| `src/worker/ops.rs` | `runner_ctx.backend_for("ops")` でバックエンド選択 |
| `Cargo.toml` | `aws-sdk-bedrockruntime` 依存追加 |
| `config/repos.toml` | `bedrock_model_id`, `bedrock_region` 設定追加 |

**スコープ**: ops のみ。executor (coding) は claude -p を継続。
Anthropic のサブスク内エージェント方向次第で将来判断。

### Phase 5: 完全自動モード（将来）

**目標**: simple タスクの承認スキップ

| ファイル | 変更内容 |
|---------|---------|
| `src/repo_config.rs` | `auto_execute: bool` フラグ追加 |
| `src/worker/runner.rs` | simple + auto_execute → 承認スキップ |

---

## 11. 要相談ポイント

### A. タスクの入口

| 選択肢 | 説明 | Pros | Cons |
|--------|------|------|------|
| **現行維持** (Asana Webhook + Slack) | Asana でタスク作成 → Webhook → agent | 既存ワークフロー維持、Asana が source of truth | Webhook 設定が面倒 |
| **Symphony 的** (ポーリング) | 定期的に Asana/Linear をポーリング | Webhook 不要、シンプル | ポーリング間隔の遅延 |
| **Slack 直接投入** | Slack メッセージでタスク作成 | 最も手軽 | Asana との同期が必要 |

→ **推奨**: 現行の Asana Webhook を維持。Slack からの ops は既に動いている。

### B. 並列実行の粒度

| 選択肢 | 説明 |
|--------|------|
| **タスク単位** | 1タスク = 1 worktree。現行の semaphore (max=2) で制御 |
| **サブタスク単位** | 1サブタスク = 1 worktree。依存関係のないサブタスクを並列実行 |

→ **推奨**: まずタスク単位。サブタスク並列は将来の拡張。

### C. worktree の cleanup タイミング

| 選択肢 | 説明 |
|--------|------|
| **PR マージ後に自動削除** | GitHub Webhook でマージ検知 → cleanup |
| **タスク done 後に即削除** | executor 完了後すぐ |
| **定期 GC** | スケジューラーで古い worktree を定期削除 |

→ **推奨**: タスク done 後に即削除 + 定期 GC（安全ネット）

---

## 12. 現行コードベースの統計

| 項目 | 数値 |
|------|------|
| 総行数 (Rust) | ~11,600 |
| SQLite テーブル数 | 6 |
| API エンドポイント | 12 |
| Scheduled Job Type | 5 |
| タスクステータス | 11 |

v2 での追加見込み:
- Phase 1-3 (worktree + PR): ~300行
- Phase 4 (BedrockBackend for ops): ~400行
- 合計: ~700行

## 13. ダブルエージェント — なぜ ops だけ分けるのか

```
claude -p (PM + coding):
  - MCP ツール (Asana, Slack 等) をそのまま使える
  - Claude Code の hook/skill/rules 資産を活用
  - サブスク定額 → コーディングに最適
  - Anthropic が常駐方向に寄せている → 将来さらに安定

claude -p の問題 (ops で顕在化):
  - in_process_teammate 等でハングするリスク
  - Slack 応答がハングすると UX 最悪
  - プロセス起動コストが ops の軽量さに見合わない

Bedrock API (ops 専用):
  - API 直呼び → ハングしない、確実
  - ops のツールセットは少ない (6個) → 自前実装コスト低
  - 従量課金だが ops は軽量 → コスト微小
  - 完全制御 → タイムアウト・リトライも自前
```

**結論**:
- **PM 判断 + コーディング** → `claude -p`。MCP・フルツール・サブスク定額の恩恵。
- **ops (Slack 駆動の定型作業)** → Bedrock。ハングリスク排除が最優先。
- **コーディングの Bedrock 移行** → Anthropic のサブスク方向次第で将来判断。急がない。

この分離により:
1. ops の Slack 応答が確実になる（ハングしない）
2. `claude -p` の同時実行枠を PM + coding に集中
3. trait AgentBackend のおかげで段階的に移行可能
