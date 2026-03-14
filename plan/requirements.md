# coding_tasks + ops_queue 統合設計 — 要件定義

**作成日**: 2026-03-14
**対象**: ambient-task-agent
**担当**: Discovery Council（analyst / researcher / scout）

---

## 背景と目的

現在、システムは2つの独立したパイプラインで動作している:

1. **`coding_tasks` パイプライン**: Asana 同期 → `new → planning → proposed → approved → executing → done`
2. **`ops_queue` パイプライン**: Slack → `pending → processing → done/failed`

この二重構造により、以下の問題が生じている:
- Slack から来たタスクは `ops_escalate` ボタンで手動的に `coding_tasks` に「昇格」させる必要がある
- 入口（Asana/Slack）によってフローが異なり、ユーザー体験が非一貫
- `coding_tasks` の `new → planning → proposed` は常に人間承認を経るため、明確なタスクでも無駄な待ち時間が発生

**目的**: 入口に関わらず同一の処理フローで動作させ、明確なタスクは即実行・曖昧なタスクは Slack ラリーで要件確定してから実行する。

---

## コードベース現状の制約（Discovery Council 調査結果）

### 既存のブリッジ実装（researcher 指摘）

`create_task_from_ops()` は**既に実装済み**。以下2ルートから呼ばれている:
- `ops_escalate` ボタン → `create_task_from_ops()` → `coding_tasks` に `new` で挿入 → worker が `planning` へ
- `ops_inception_approve` ボタン → タスク分解結果から `create_task_from_ops()` → 複数タスクが `new` で挿入

**解消すべき問題**: Inception ターン2で要件定義が完了しているのに、`coding_tasks` 登録後に再び `planning`（Claude分析）→ `proposed`（人間承認）を経る。この**二重分析を廃止**することが統合の核心。

### DB構造の制約（scout 指摘）

- `coding_tasks.asana_task_gid` は `NOT NULL`（スキーマ制約）
- `coding_tasks` は28フィールド、`ops_queue` は12フィールドと乖離が大きく **DB統合はしない**
- 共通処理は Rust の `WorkItem` enum で抽象化する

### セッション管理の2モデル（researcher 指摘）

- **coding_tasks**: `claude_session_id` + `--resume` でセッション継続
- **ops**: `ops_contexts` テーブルにテキスト履歴を蓄積してプロンプト注入

統合後は **conversing フェーズは ops_contexts モデル、executing フェーズは claude_session_id モデル** を使うハイブリッド方式を採用する。

### 静的 vs 動的ルーティング（scout 指摘）

現在、ops の `execute/plan/inception` 判定は `repos.toml` の `ops_mode` 設定で**静的**に決まる。動的な明確/曖昧判定には追加の LLM 呼び出しコストが発生する。

### `auto_execute` フラグとの整合（researcher 指摘）

現在 `repo_config` の `auto_execute: bool` が `new → auto_approved`（人間承認スキップ）を制御している。新しい `new → executing` の自動遷移とこのフラグは概念的に重複するため、統合後は `auto_execute = true` を「`new → executing` の直行を許可する」フラグとして再定義する。

---

## 新しいステータスモデル

### `coding_tasks.status` の変更

```
new → (自動判定) → executing     ← 明確なタスク、即実行
                 → conversing   ← 要件ラリー中（Slack スレッド）

conversing → executing          ← 要件確定、実行開始
           → manual             ← 人間が terminal で対応

manual → executing              ← 「直した」でエージェント再開
       → done                   ← 人間が完了宣言

executing → done / ci_pending   ← 正常完了
          → conversing          ← 実行中にブロッカー発生
```

### 廃止するステータス（既存との互換性）

- `planning` → `conversing` に統合（要件確認中の状態として再定義）
- `proposed` → `conversing` に統合（人間返信待ちの状態として再定義）
- `approved` / `auto_approved` → 廃止（`conversing → executing` への直接遷移で代替）

**マイグレーション方針**:
```sql
-- executing 中のタスクはそのまま維持
UPDATE coding_tasks SET status = 'conversing'
  WHERE status IN ('planning', 'proposed')
  AND status NOT IN ('executing', 'ci_pending');

UPDATE coding_tasks SET status = 'executing'
  WHERE status IN ('approved', 'auto_approved');
```

既存の `done`, `error`, `sleeping`, `archived`, `ci_pending` は変更なし。

---

## 機能要件

### FR-1: `coding_tasks` への新ステータス対応

**ファイル**: `src/db.rs`（マイグレーション追加）

新規カラム追加（ALTER TABLE による後方互換追加）:
```sql
-- conversing フェーズの会話識別用（ops_contexts と同じキーで引く）
ALTER TABLE coding_tasks ADD COLUMN converse_thread_ts TEXT;
```

`converse_thread_ts` は `conversing` 状態での Slack スレッド TS を保持し、
`ops_contexts(channel, thread_ts)` との紐付けに使う。

既存の `slack_thread_ts` と重複するが、`slack_thread_ts` はタスク全体のスレッド、`converse_thread_ts` は conversing フェーズの会話履歴スレッドとして意味が異なる（Asana 入口タスクは `slack_thread_ts` が未設定の場合がある）。

### FR-2: `new → 自動判定` ロジック

**ファイル**: `src/worker/runner.rs`

**入口別デフォルト（researcher 推奨の選択肢C をベース）**:
- **Asana 入口** (`asana_task_gid` が `"slack_"` プレフィックスでない): 人間がチケット化済みとして「明確」とみなす
- **Slack 入口** (`asana_task_gid` が `"slack_"` プレフィックス): Inception 経由で要件確定済みなら `executing`、未確定なら `conversing`

**具体的な判定ロジック**:
```rust
fn classify_new_task(task: &CodingTask) -> TaskClassification {
    let is_slack_origin = task.asana_task_gid.starts_with("slack_");
    let has_repo = task.repo_key.is_some();
    let auto_execute = /* repo_config から取得 */;

    if is_slack_origin {
        // Slack 入口: analysis_text に Inception 要件定義が入っていれば即実行
        // なければ conversing でヒアリング開始
        if task.analysis_text.is_some() {
            TaskClassification::Execute
        } else {
            TaskClassification::Converse
        }
    } else {
        // Asana 入口: auto_execute フラグ（旧 auto_execute）を尊重
        if auto_execute && has_repo {
            TaskClassification::Execute
        } else {
            TaskClassification::Converse
        }
    }
}
```

`auto_execute` フラグの再定義: 旧来の `new → auto_approved` スキップから「`new → executing` 直行を許可する」に意味を変更（後方互換: `auto_execute = true` の既存設定は動作継続）。

### FR-3: `conversing` フェーズの実装

**ファイル**: `src/worker/runner.rs`, `src/server/slack_events.rs`

#### conversing 開始時（`start_conversing_task()` メソッド）

1. `coding_tasks.status = 'conversing'` に遷移
2. Slack スレッドを作成（まだなければ `slack_thread_ts` を設定）
3. `ops_contexts` に user エントリを追加（`channel + thread_ts` をキー）
4. Inception Turn1 相当のプロンプトで Claude を実行（要件ヒアリング質問を生成）
5. 質問を Slack スレッドに投稿
6. `coding_tasks.converse_thread_ts = thread_ts` を保存

#### conversing 中のスレッド返信受信

**ファイル**: `src/server/slack_events.rs` の `handle_message()`

`find_task_by_thread_ts()` で `conversing` 状態のタスクを発見した場合:
- `ops_contexts` に user メッセージを追記
- `wake_worker()` でワーカーを起床
- （既存の sleep/wake/archive コマンドはそのまま維持）

#### conversing ターン継続（ワーカー側）

`process_tasks()` に `process_conversing_tasks()` サブルーティンを追加:
```rust
fn process_conversing_tasks(self: &Arc<Self>) -> bool {
    // conversing 状態のタスクを取得
    // ops_contexts の最新エントリが "user" ロール → 次の Claude ターンを実行
    // ops_contexts の最新エントリが "assistant" ロール → まだ返信待ち、スキップ
}
```

#### conversing → executing 遷移トリガー

以下のいずれかで `executing` に遷移:
- Slack Block Kit ボタン「実行開始」（`action_id = "task_execute"`）
- スレッドテキスト「go」「実行」「run」（既存コマンドを `conversing` にも対応）
- Claude の返答に `REQUIREMENTS_CONFIRMED:` プレフィックスが含まれる場合（自動判定）

### FR-4: `manual` ステータスの実装

**ファイル**: `src/server/slack_events.rs`, `src/server/slack_actions.rs`

#### manual への遷移トリガー

**Slack Block Kit ボタン主体**（researcher 推奨）:
- `conversing` 状態のボタン「手動修正」（`action_id = "task_manual"`）
- `executing` 状態の `stop_task` ボタンを拡張: `error` ではなく `manual` に遷移（既存 `stop_task` の代替として実装）

#### manual 中の Slack 通知（Block Kit ボタン付き）

```
:wrench: *手動対応モード*
ターミナルで作業を行い、完了後に以下のボタンを押してください:
[再開]  [完了]
```

Block Kit ボタンで一貫性を担保。ターミナルでのファイル編集後に Slack で「再開」を押すフロー。

#### manual → executing 遷移

「再開」ボタン（`action_id = "task_resume"`）押下:
- `coding_tasks.status = 'executing'` に更新
- `wake_worker()` で実行再開

#### manual → done 遷移

「完了」ボタン（`action_id = "task_done"`）押下:
- `coding_tasks.status = 'done'` に更新

### FR-5: `executing → conversing` ブロッカー検知

**ファイル**: `src/worker/executor.rs`（または実行完了後の出力解析）

Claude の実行出力に以下のパターンが含まれる場合、`conversing` に遷移:
- `BLOCKED:` プレフィックスを含む行
- `REQUIRES_CLARIFICATION:` プレフィックスを含む行

遷移時:
1. `coding_tasks.status = 'conversing'` に更新
2. `claude_session_id` は **保持**（再開時に `--resume` で継続できるよう）
3. ブロッカー内容を Slack スレッドに投稿
4. ユーザー返信待ち

### FR-6: Slack 承認ボタンの再設計

**ファイル**: `src/server/slack_actions.rs`

#### `conversing` 状態でのボタン

```
[実行開始]  [指示追加]  [手動修正]  [スキップ]
```

| ボタン | action_id | 遷移先 | 処理 |
|--------|-----------|--------|------|
| 実行開始 | `task_execute` | `executing` | `wake_worker()` |
| 指示追加 | `task_add_instruction` | `conversing`（継続）| 追加指示をプロンプトに追記して再実行 |
| 手動修正 | `task_manual` | `manual` | 手動対応通知を投稿 |
| スキップ | `task_skip` | `done` | スキップ理由を記録 |

#### `executing` 状態でのボタン（実行中通知メッセージ）

```
[中止]  [手動修正]
```

#### `manual` 状態でのボタン

```
[再開]  [完了]
```

action_id: `task_resume` / `task_done`

#### 既存ボタンとの関係

- `approve_task` / `reject_task` / `regenerate_task`: **廃止**（`proposed` 廃止に伴い）
  - ただし `proposed` タスクが残存する期間は**並存**（マイグレーション完了後に削除）
- `stop_task`: **`task_manual` に置き換え**（`executing → error` から `executing → manual` に変更）
- `ops_resolve` / `ops_escalate` / `ops_inception_*`: **維持**（ops_queue パイプラインは独立）

### FR-6b: Inception 後の二重分析廃止

**ファイル**: `src/server/slack_actions.rs`（`process_ops_inception_approve()`）

**現状の問題**: `ops_inception_approve` → `create_task_from_ops()` → `coding_tasks` に `status = 'new'` で登録 → worker が `planning`（Claude 再分析）→ `proposed`（人間再承認）を実行する。Inception ターン2で要件定義が完了しているのに二重で実行される。

**修正方針**: `create_task_from_ops()` に `initial_status` 引数を追加し、Inception 承認経由の登録は `status = 'executing'` で挿入する:

```rust
// 修正前
state.db.create_task_from_ops(ops_id, title, description, &item.repo_key, channel, reply_ts)?;

// 修正後
state.db.create_task_from_ops_with_status(
    ops_id, title, description, &item.repo_key, channel, reply_ts,
    "executing",  // Inception 要件定義完了済みのため即実行
)?;
```

`analysis_text` に Inception ターン2の出力（要件定義）を格納して引き継ぐ。

### FR-6c: `conversing` タイムアウト設計

**ファイル**: `src/worker/runner.rs`（`check_ops_followups()` に準じて追加）

現在 `ops_queue` には `check_ops_followups()` がある（営業日1/3/5日後にリマインド）。`coding_tasks` の `conversing` 状態も同様のフォローアップが必要。

**実装方針**: 既存の `check_ops_followups()` を拡張して `coding_tasks` の `conversing` 状態も対象に含める:

- 営業日1日後: リマインド投稿（「引き続きヒアリング中です。返信をお待ちしています」）
- 営業日5日後: `conversing → sleeping` に遷移（`ops_queue` の `on_hold` 相当）

`updated_at` カラムを最終メッセージ時刻として利用（追加カラム不要）。

### FR-7: 成果物の自動判断

**ファイル**: `src/worker/executor.rs`（完了後処理）

実行完了時の成果物判断:
- `task.repo_key.is_some()` かつ ファイル変更あり → PR 作成フロー（既存ロジック）
- `task.repo_key.is_none()` または ファイル変更なし → Slack 返信で完了報告

worktree 作成の有無も同じ基準で切り替え（`repo_key` ありの場合のみ作成）。

### FR-8: Slack 入口タスクの `asana_task_gid` 統一

**ファイル**: `src/server/slack_actions.rs`（`create_task_from_ops()` 呼び出し側）

Slack 入口タスクの GID フォーマットを `slack_{channel}_{message_ts}` に統一:
```rust
let dummy_gid = format!("slack_{}_{}", channel, message_ts.replace('.', "_"));
```

---

## 非機能要件

### NFR-1: 後方互換性

- `ops_queue` テーブルは**廃止しない**（ops パイプラインは引き続き独立動作）
- `coding_tasks` の既存カラムは変更しない（`ALTER TABLE ADD COLUMN` のみ）
- 既存の `planning` / `proposed` ステータスのタスクは DB マイグレーションで変換

### NFR-2: 既存 ops フローへの影響なし

- `OpsMode::Execute` / `OpsMode::Plan` / `OpsMode::Inception` の処理パスは変更しない
- `process_ops_queue()` は独立して動作し続ける

### NFR-3: セッション管理（ハイブリッド方式）

- `conversing` フェーズ: `ops_contexts` テーブルで会話履歴管理（変更なし）
- `executing` フェーズ: `claude_session_id` + `--resume` で継続（変更なし）
- `executing → conversing` 遷移時: `claude_session_id` を保持して再開に備える

### NFR-4: 明確/曖昧判定の精度

- Phase 1: heuristics（description 文字数 + repo_key 有無）
- Phase 2: Claude による intent 分析（LLM コスト評価後に導入）

---

## 変更ファイル一覧

| ファイル | 変更種別 | 内容 |
|----------|---------|------|
| `src/db.rs` | 変更（小）| `converse_thread_ts` カラム追加、ステータスマイグレーション、`create_task_from_ops_with_status()` 追加 |
| `src/worker/runner.rs` | 変更（中）| `process_tasks()` に conversing ループ追加、`new → 自動判定` ロジック追加、`check_ops_followups()` を conversing タイムアウトに拡張 |
| `src/server/slack_events.rs` | 変更（小）| `handle_message()` に `conversing`/`manual` ステータス対応追加 |
| `src/server/slack_actions.rs` | 変更（中）| 新ボタンハンドラ追加（`task_execute`, `task_manual`, `task_skip`, `task_add_instruction`, `task_resume`, `task_done`）、`approve_task` 等を段階的廃止、`stop_task` を `task_manual` に置き換え、Inception 承認後の `create_task_from_ops_with_status()` 呼び出し変更 |

**DBスキーマ変更**: `coding_tasks` に `converse_thread_ts TEXT` カラム追加のみ。テーブル統合なし。

---

## 受け入れ基準

- **AC-1**: Asana から `description >= 100文字` かつ `repo_key` ありのタスクが来ると、自動的に `executing` に遷移して実行開始する
- **AC-2**: Asana から `description < 100文字` または `repo_key` なしのタスクが来ると、`conversing` に遷移して Slack に質問が投稿される
- **AC-3**: Slack Inception 経由で承認されたタスクは `conversing` 状態から開始される
- **AC-4**: `conversing` 状態のタスクスレッドに返信すると、ワーカーが起床して次の会話ターンを実行する
- **AC-5**: 「実行開始」ボタン押下で `executing` に遷移し、実行が開始される
- **AC-6**: 「指示追加」ボタン押下後に追加指示を入力すると、それをコンテキストに加えて `conversing` が継続する
- **AC-7**: 実行中に `BLOCKED:` 出力があると `conversing` に遷移してブロッカーを Slack に投稿する
- **AC-8**: 「手動修正」ボタンで `manual` に遷移し、「直した」返信で `executing` に戻る
- **AC-9**: `manual` 状態で「done」返信するとタスクが完了する
- **AC-10**: `repo_key` ありのタスクは PR 作成、`repo_key` なしは Slack 返信で完了する
- **AC-11**: 既存の `ops_queue` パイプライン（Execute/Plan/Inception モード）の動作に変化がない
- **AC-12**: 既存の `done` / `error` / `sleeping` ステータスのタスクは正常に参照・表示される

---

## フェーズ分割

### Phase 1（今回実装）

- FR-1: `converse_thread_ts` カラム追加 + ステータスマイグレーション
- FR-2: `new → 自動判定`（heuristics版）
- FR-3: `conversing` フェーズ基本実装（スレッド返信受信 + ワーカー継続）
- FR-4: `manual` ステータス基本実装（ボタン + テキストコマンド）
- FR-6: 新ボタン追加（`task_execute`, `task_manual`, `task_skip`, `task_add_instruction`）

### Phase 2（後続）

- FR-5: `executing → conversing` ブロッカー検知
- FR-2 精度向上: Claude による intent 分析
- FR-7 完全実装: 成果物タイプの動的判断
- BedrockBackend 統合後: セッション管理の完全統一

---

## リスク

| リスク | 影響 | 対策 |
|--------|------|------|
| R-1: `planning`/`proposed` → `conversing` マイグレーション中のタスク消失 | 実行中タスクのステータス不整合 | `executing`/`ci_pending` 以外のタスクのみ変換 |
| R-2: `approve_task` ボタン廃止による既存 `proposed` タスクの操作不能 | ユーザーが承認できない | マイグレーション完了まで並存、完了後に廃止 |
| R-3: `converse_thread_ts` が NULL のタスクが `conversing` に遷移 | `ops_contexts` と紐付けできない | `conversing` 遷移時に `slack_thread_ts` があればそちらを使うフォールバック |
| R-4: 動的判定のコスト増 | Phase 2 で LLM 呼び出しが増加 | Phase 1 は heuristics のみ、Phase 2 は評価後に導入 |
| R-5: worktree なし ops タスクが PR フローに入る | エラー発生 | `repo_key` チェックを worktree 作成前に必須化（scout 指摘） |
| R-6: `auto_execute` フラグ再定義による既存動作変化 | `auto_execute = true` のリポジトリで挙動が変わる | 既存フラグの意味を「`new → executing` 直行許可」として再定義。後方互換を維持しつつ `planning`/`proposed` をスキップするだけなので実質同等 |
| R-7: `conversing` タイムアウト → sleeping 遷移の誤検知 | 積極的に会話中のタスクが sleeping になる | `updated_at` だけでなく `ops_contexts` の最終エントリ時刻も参照してタイムアウト判定する |

---

## 設計判断の記録

1. **DB統合しない（scout 推奨採用）**: `coding_tasks` と `ops_queue` のカラム差異が大きく、統合によるメリットより移行コストが高い。`WorkItem` enum による論理抽象化で対処。

2. **`planning`/`proposed` 廃止（ユーザー合意済みモデルに従う）**: 新しいステータスモデルでは `conversing` が要件ラリーと承認待ちを兼ねる。既存タスクはマイグレーションで変換。`approved`/`auto_approved` も `executing` に統合。

3. **Phase 1 の `new → 自動判定` は heuristics（LLM コスト回避）**: Claude による動的判定は追加コストが発生するため Phase 1 は description 文字数と repo_key 有無で判定。精度向上は Phase 2。

4. **`ops_contexts` を coding_tasks の conversing にも流用（researcher 提案採用）**: 同一テーブルで `channel + thread_ts` をキーとして管理。`converse_thread_ts` カラムで紐付けを明示。

5. **`executing → conversing` 時に `claude_session_id` 保持（researcher 提案採用）**: ブロッカー解消後に `--resume` で実行再開できるよう session_id をクリアしない。

6. **`manual` はボタン主体（researcher 推奨採用）**: テキストコマンドは `app_mention` のルーティングが複雑になるため、Block Kit ボタンで統一。`stop_task`（`executing → error`）を `task_manual`（`executing → manual`）に置き換え。

7. **Inception 後の二重分析廃止（researcher 指摘を要件に追加）**: `ops_inception_approve` 経由のタスクは `status = 'executing'` で挿入し、`planning → proposed` をスキップ。`create_task_from_ops_with_status()` で実現。

8. **`auto_execute` フラグの再定義**: 旧来の `new → auto_approved` スキップから「`new → executing` 直行許可」に意味を更新。既存設定は後方互換を維持。

9. **`conversing` タイムアウトは `check_ops_followups()` 拡張で対応（researcher 指摘採用）**: `ops_queue` の既存フォローアップ機構を `coding_tasks.conversing` 状態にも適用。新規コードを最小化。
