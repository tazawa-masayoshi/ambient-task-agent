# ops_queue 追加 要件定義

**作成日**: 2026-03-11
**対象バージョン**: ambient-task-agent (現 HEAD: 9567cc8)
**担当**: Analyst

---

## 背景と課題

現状、Slack の ops チャンネルにメッセージが来ると、
`handle_message()` 内で即座に `tokio::spawn` → `dispatch_ops_request()` → `execute_ops()` (claude -p) を呼んでいる。
レートリミット等の一時エラーが発生した場合、リトライ機構がなくメッセージが消失する。

```
[現状フロー]
Slack event → handle_message/handle_reaction_added
  → tokio::spawn(dispatch_ops_request)
      → execute_ops (claude -p)  ← 失敗したら消える
```

```
[目標フロー]
Slack event → handle_message/handle_reaction_added
  → DB INSERT into ops_queue (即返却)

Worker heartbeat (15s) → process_ops_queue()
  → pick pending item → execute_ops (claude -p)
      → 成功: done
      → 一時エラー: retry (retry_count++)
      → 上限超過: failed
```

---

## 機能要件 (FR)

### FR-1: ops_queue テーブル
- ops_monitor チャンネルの新着メッセージ、および ⚡ リアクション手動トリガーを DB に INSERT する
- INSERT 後は即座に HTTP ハンドラーへ返却し、Slack に `:hourglass_flowing_sand: キューに追加しました` を返信する
- キューアイテムはステータス管理される: `pending` → `processing` → `done` / `failed` / `retry`

### FR-2: ops_queue スキーマ
```sql
CREATE TABLE IF NOT EXISTS ops_queue (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    channel       TEXT NOT NULL,
    thread_ts     TEXT NOT NULL,       -- トップレベルメッセージの ts (スレッド ID)
    repo_key      TEXT NOT NULL,
    message_text  TEXT NOT NULL,
    files_json    TEXT NOT NULL DEFAULT '[]',  -- SlackFile のシリアライズ
    status        TEXT NOT NULL DEFAULT 'pending',
    retry_count   INTEGER NOT NULL DEFAULT 0,
    max_retries   INTEGER NOT NULL DEFAULT 3,
    error_message TEXT,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_ops_queue_status
    ON ops_queue(status, created_at);
```

**ステータス遷移**:
```
pending → processing → done
                     → retry   (retry_count < max_retries)
                     → failed  (retry_count >= max_retries)
retry   → pending    (次のハートビートで再試行)
```

### FR-3: process_ops_queue() の実装
- `runner.rs` の `process_tasks()` に続けて呼ばれる同期関数として実装する
- `pending` または `retry` ステータスのアイテムを 1 件 FIFO (created_at ASC) で取得する
- 取得したら即座に `processing` にステータスを更新してから `tokio::spawn` する（二重処理防止）
- 実行後の成功/失敗に応じて `done` / `retry` / `failed` を更新する

**エラー分類**:
- レートリミット (`rate_limit` 相当のエラー文字列を含む): `retry`
- その他一時エラー (ネットワーク等): `retry`
- 上限超過後の任意エラー: `failed`
- スキルファイルなし等の永続エラー: `failed` (リトライ不要、max_retries=0 相当で即 failed)

### FR-4: slack_events.rs の変更（即時実行→キュー INSERT）

**handle_message() の ops 分岐**:
- 変更前: `tokio::spawn(classify → dispatch_ops_request)`
- 変更後: `tokio::spawn(classify → db.enqueue_ops(...))`
  - classify の結果が true の場合のみ INSERT
  - INSERT 後にワーカーを `wake_worker()` で起こす

**handle_reaction_added() の `"zap"` 分岐**:
- 変更前: `dispatch_ops_request(...)` を直接 await
- 変更後: `db.enqueue_ops(...)` で INSERT → `wake_worker()`

**スレッド返信 (followup)**:
- 既存 ops スレッドへの返信も同様にキュー経由に統一する
- `dispatch_ops_request()` への直接 call を `db.enqueue_ops()` に置き換える

### FR-5: dispatch_ops_request() の扱い
- 関数自体は残す（ロジックを ops.rs の `execute_ops()` 呼び出しに集約するため）
- ただし `dispatch_ops_request()` を直接呼ぶパスをなくし、
  代わりに `process_ops_queue()` からのみ呼ばれる形にする
- 具体的には `dispatch_ops_request()` を非公開の `run_ops_item()` として runner.rs 側に移動するか、
  または ops.rs 側に `execute_ops_item(item: OpsQueueItem)` として整理する

**設計方針（推奨）**:
```
slack_events.rs:  Slack event → enqueue_ops()
db.rs:            enqueue_ops() → INSERT ops_queue
runner.rs:        process_ops_queue() → pick item → execute_ops_item()
worker/ops.rs:    execute_ops_item() → execute_ops() (既存ロジックをそのまま使う)
```

### FR-6: DB メソッド追加 (db.rs)

```rust
// INSERT
pub fn enqueue_ops(
    &self,
    channel: &str,
    thread_ts: &str,
    repo_key: &str,
    message_text: &str,
    files_json: &str,  // serde_json::to_string(&files)
) -> Result<i64>

// FIFO で 1 件取得 (pending or retry)
pub fn dequeue_ops_item(&self) -> Result<Option<OpsQueueItem>>

// ステータス更新 (processing)
pub fn mark_ops_processing(&self, id: i64) -> Result<()>

// 成功
pub fn mark_ops_done(&self, id: i64) -> Result<()>

// リトライ (retry_count++ して pending に戻す、上限超過なら failed)
pub fn mark_ops_retry(&self, id: i64, error: &str) -> Result<()>

// 失敗
pub fn mark_ops_failed(&self, id: i64, error: &str) -> Result<()>
```

**OpsQueueItem 構造体**:
```rust
pub struct OpsQueueItem {
    pub id: i64,
    pub channel: String,
    pub thread_ts: String,
    pub repo_key: String,
    pub message_text: String,
    pub files_json: String,
    pub status: String,
    pub retry_count: i32,
    pub max_retries: i32,
    pub created_at: String,
}
```

---

## 非機能要件 (NFR)

### NFR-1: メッセージ消失ゼロ
- Slack イベント受信後、DB INSERT に成功すれば以降の処理失敗でメッセージは消失しない

### NFR-2: ハートビートへの影響最小化
- `process_ops_queue()` は既存 `process_tasks()` と同様に非同期 `tokio::spawn` するため、
  ハートビートループのブロッキングは発生しない
- ops 処理が長くても 15 秒間隔のハートビートに影響しない

### NFR-3: 既存 ops 動作の維持
- ops_monitor=false のリポジトリでは ⚡ 手動トリガー経由のキューのみ使う（既存動作と同等）
- ops スレッド返信（followup）も同様にキュー経由に統一して動作を保つ

### NFR-4: 同時実行制御
- `dequeue_ops_item()` は `mark_ops_processing()` を即座に呼ぶことで二重 pickup を防ぐ
- `coding_tasks` の既存パターン（`update_status` で claim してから spawn）と同一の設計とする

### NFR-5: DB スキーマ後方互換
- `add_missing_columns` パターンは使えない（新テーブルのため `migrate()` に `CREATE TABLE IF NOT EXISTS` を追加する）
- 既存テーブルへの影響なし

---

## 制約条件

- **ランタイム**: Tokio async、rusqlite（WAL モード）
- **追加依存クレート不要**: serde_json は既存で使用中、追加クレートなしで実装する
- **既存 `dispatch_ops_request()` の署名変更は最小限**:
  event ペイロードから `SlackFile` を抽出する処理を `enqueue_ops` 呼び出し前に行い、
  files_json としてシリアライズして保存する
- **スキルファイル未設定の場合**: `execute_ops` が bail するため、classify=true でも enqueue せず
  即座に Slack に `:x: スキル未設定` を返すことで永続 failed 蓄積を防ぐ

---

## 前提条件

- SQLite WAL モードは既に有効（`PRAGMA journal_mode=WAL`）
- `ops_contexts` テーブルは変更不要（会話履歴の管理は従来通り）
- `worker_notify: Arc<Notify>` は `AppState` に存在し、`wake_worker()` で即時起床できる

---

## リスク

| リスク | 影響 | 対策 |
|--------|------|------|
| R-1: processing のまま放置（プロセス再起動等） | キューが詰まる | 起動時または定期的に `processing` → `pending` に戻すリカバリを追加（将来対応でも可） |
| R-2: retry ループが長時間続く | キューが肥大化 | `max_retries` のデフォルト 3 回で抑止。`failed` 後は Slack に通知 |
| R-3: enqueue_ops が `files_json` のシリアライズに失敗 | ファイル情報が欠落する | `unwrap_or_else(|_| "[]".to_string())` でフォールバックし、ログ出力 |
| R-4: classify 呼び出しの間にレートリミット発生 | classify 自体が失敗 | classify エラーはログのみ（enqueue しない）。既存動作と同じため許容 |

---

## 変更ファイル一覧と変更内容サマリー

| ファイル | 変更種別 | 変更内容 |
|----------|----------|----------|
| `src/db.rs` | 追加 | `OpsQueueItem` 構造体、`ops_queue` テーブル DDL、`enqueue_ops` / `dequeue_ops_item` / `mark_ops_*` メソッド群 |
| `src/worker/runner.rs` | 追加 | `process_ops_queue()` メソッド、ハートビートループからの呼び出し |
| `src/server/slack_events.rs` | 変更 | `handle_message()` の ops 分岐: `dispatch_ops_request` → `enqueue_ops` + `wake_worker`。`handle_reaction_added()` の `"zap"` 分岐も同様 |
| `src/worker/ops.rs` | 変更(小) | `OpsQueueItem` を受け取る `execute_ops_item()` ラッパーを追加（または runner.rs に持つ） |

---

## 受け入れ基準

- AC-1: ops チャンネルに投稿した後すぐに `:hourglass_flowing_sand: キューに追加しました` が返信される
- AC-2: claude -p がレートリミットで失敗した場合、次のハートビート（最大 15 秒後）に自動リトライされる
- AC-3: `max_retries` 回を超えた場合は `:x: ops 失敗（リトライ上限）` が Slack に通知される
- AC-4: ⚡ リアクション手動トリガーも同じキューを経由して処理される
- AC-5: ops スレッド返信（followup）も同様にキュー経由で処理される
- AC-6: 既存の `coding_tasks` 処理（plan/execute/CI サイクル）に影響が出ない
