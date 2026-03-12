# ops Inception モード 要件定義

**作成日**: 2026-03-12
**対象**: ambient-task-agent（現 HEAD: f77ebf0）
**担当**: Analyst（Discovery Council）

---

## 背景と目的

現在の `ops_mode` は `execute`（実行）と `plan`（分析のみ）の2種類。
いずれも「要件が明確な前提」で動作する。

新たに `inception` モードを追加し、**要件が曖昧な状態からSlackスレッド上で対話的に要件定義→タスク分解まで自律完結**させる。
AI-DLC（Amazon Inception Workflow）のInception Phaseのエッセンスを2ターンに圧縮して実装する。

---

## フロー概要

```
[ターン1] ユーザーの要求メッセージ受信
  → ops_queue: pending → processing
  → Claude: Intent分析 + 不明点3〜5問をSlackスレッドに投稿
  → ops_queue: mark_ops_done（ターン1完了、会話履歴はops_contextsに保存済み）

[ユーザー回答] スレッドへの @bot メンション返信（例: "@bot 〇〇です"）
  → 既存の「スレッド返信 + @bot メンション → admin のみ ops 実行」ルートで再エンキュー
  → ops_queue: 新エントリ（同一 thread_ts、ops_contexts に履歴あり）

[ターン2] 会話履歴付きで再実行（ops_contexts.len() > 0 で判定）
  → Claude: 要件整理 + タスク分解 → Slackスレッドに投稿
  → Block Kit 3ボタン承認ゲート投稿
  → ops_queue: mark_ops_done

[承認ゲート]
  ✅ inception_approve → coding_tasks 登録 + wake_worker（Asana APIは将来拡張）
  🔧 inception_revise  → 「修正点をスレッドで @bot に返信してください」→ ユーザー再回答待ち
  ❌ inception_cancel  → resolve_ops（ops_queue を done に）
```

---

## 機能要件 (FR)

### FR-1: `ops_mode = "inception"` の設定対応

**ファイル**: `src/repo_config.rs`

`OpsMode` enum に `Inception` バリアントを追加:

```rust
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OpsMode {
    #[default]
    Execute,
    Plan,
    Inception,  // 追加
}
```

`#[serde(rename_all = "snake_case")]` により `ops_mode = "inception"` が自動デシリアライズされる。

**設定例（`config/repos.toml`）:**
```toml
[[repo]]
key = "my-project"
ops_mode = "inception"
ops_channel = "my-ops-channel"
```

---

### FR-2: ターン判定ロジック（DBスキーマ変更なし）

**ファイル**: `src/worker/runner.rs`

DBスキーマの変更は不要。`ops_contexts` テーブルに保存された**会話履歴の件数**でターンを判定する:

```rust
let history = self.db.get_ops_context(&item.channel, reply_ts)?;

match repo_entry.ops_mode {
    OpsMode::Inception => {
        if history.is_empty() {
            // ターン1: 初回メッセージ → Intent分析 + 質問生成
            self.run_ops_inception_turn1(&item, &repo_entry, &req, &history).await
        } else {
            // ターン2: 会話履歴あり → 要件整理 + タスク分解
            self.run_ops_inception_turn2(&item, &repo_entry, &req, &history).await
        }
    }
    OpsMode::Plan => { /* 既存 */ }
    OpsMode::Execute => { /* 既存 */ }
}
```

**理由**: `ops_contexts` は `channel + thread_ts` をキーに蓄積される。
ターン1完了後に user/assistant の2エントリが保存されるため、ターン2では `history.len() >= 2` となる。
`inception_turn` カラムをDBに追加するより、既存の仕組みを最大限活用するこの方針を採用する。

---

### FR-3: `run_ops_item()` の分岐リファクタリング

**ファイル**: `src/worker/runner.rs`

現在の `plan_only: bool` フラグによる分岐を `OpsMode` の match 式に置き換える。
既存の `execute_ops()` シグネチャは変更せず、inception 専用の `execute_ops_inception_turn1/2()` を別関数として追加する（既存コードへの影響を最小化）。

---

### FR-4: Inception ターン1（Intent分析 + 質問生成）

**ファイル**: `src/worker/ops.rs`

以下の定数と関数を追加:

```rust
const FALLBACK_OPS_INCEPTION_SOUL: &str = "\
あなたは要件定義を支援するエージェントです。
ユーザーの要求を分析し、実装前に明確にすべき点を質問してください。
スキルファイルがなくても動作します。";

const OPS_INCEPTION_TURN1_RULES: &str = "\
## Intent分析の観点
以下の4軸でユーザーの要求を評価してください:
1. **種別**: 新機能 / 既存機能改善 / バグ修正 / インフラ / その他
2. **スコープ**: 影響範囲（単一モジュール / 複数モジュール / システム横断）
3. **複雑度**: 低（数時間）/ 中（1日前後）/ 高（複数日）
4. **明確さ**: 要件が十分明確か、曖昧な点があるか

## 質問生成（3〜5問に絞ること）
以下のカテゴリから必要な質問を選択:
- **機能要件**: 何をするか、何をしないか、ユーザーストーリー
- **非機能要件**: パフォーマンス、セキュリティ、可用性、互換性
- **ビジネスコンテキスト**: なぜ今必要か、優先度、期限
- **境界条件**: エッジケース、エラー処理

## 出力形式（厳守）
1. **Intent分析サマリー**（1〜2行）
2. **確認したい点**（箇条書き、3〜5問）

最後に SUMMARY: <種別> | <スコープ> | <複雑度> の形式で1行出力すること。";
```

**関数シグネチャ:**
```rust
pub async fn execute_ops_inception_turn1(
    req: &OpsRequest,
    repo_path: &Path,
    soul: &str,
    max_turns: u32,
    log_dir: Option<&Path>,
    runner_ctx: &RunnerContext,
    history: &[OpsMessage],
    download_dir: Option<&str>,
) -> Result<String>
```

**ツール権限**: `OPS_PLAN_ALLOWED_TOOLS`（`"Read,Glob,Grep,Bash"`）を流用。
**スキルファイル**: 不要（空でも動作）。

---

### FR-5: Inception ターン2（要件整理 + タスク分解）

**ファイル**: `src/worker/ops.rs`

```rust
const OPS_INCEPTION_TURN2_RULES: &str = "\
## 要件定義ドキュメントの構造
会話履歴に基づいて、以下の形式で要件定義を作成してください:

### 概要
（何を実装するか、1〜3行）

### 機能要件
- FR-1: ...

### 非機能要件
- NFR-1: ...

### 制約・前提条件
- ...

### 受け入れ基準
- AC-1: ...

## タスク分解
| # | タスク名 | 説明 | 依存 | 工数見積 |
|---|---------|------|------|---------|
| 1 | ... | ... | なし | 2h |

## 出力
上記のドキュメントとタスク分解表を出力すること。
最後に REQUIREMENTS: <要件1行サマリー> の形式で出力すること。";
```

**関数シグネチャ:**
```rust
pub async fn execute_ops_inception_turn2(
    req: &OpsRequest,
    repo_path: &Path,
    soul: &str,
    max_turns: u32,
    log_dir: Option<&Path>,
    runner_ctx: &RunnerContext,
    history: &[OpsMessage],
    download_dir: Option<&str>,
) -> Result<String>
```

**ツール権限**: `OPS_PLAN_ALLOWED_TOOLS`（ターン1と同じ）。

---

### FR-6: Block Kit 承認ゲート（Inception専用3ボタン）

**ファイル**: `src/worker/runner.rs`（ターン2完了後）

```rust
let blocks = serde_json::json!([
    {
        "type": "section",
        "text": {
            "type": "mrkdwn",
            "text": format!(":bulb: *要件定義完了*\n```\n{}\n```", truncated)
        }
    },
    {
        "type": "actions",
        "elements": [
            {
                "type": "button",
                "text": { "type": "plain_text", "text": "\u{2705} 承認（タスク登録）" },
                "style": "primary",
                "action_id": "inception_approve",
                "value": item.id.to_string()
            },
            {
                "type": "button",
                "text": { "type": "plain_text", "text": "\u{1f527} 修正して" },
                "action_id": "inception_revise",
                "value": item.id.to_string()
            },
            {
                "type": "button",
                "text": { "type": "plain_text", "text": "\u{274c} キャンセル" },
                "style": "danger",
                "action_id": "inception_cancel",
                "value": item.id.to_string()
            }
        ]
    }
]);
```

---

### FR-7: Inception ボタンハンドラ

**ファイル**: `src/server/slack_actions.rs`

`process_action()` の冒頭（`ops_resolve` / `ops_escalate` 判定の直前）に追加:

```rust
if action_id == "inception_approve" {
    return process_inception_approve(state, action_value, channel, message_ts, thread_ts).await;
}
if action_id == "inception_revise" {
    return process_inception_revise(state, action_value, channel, message_ts, thread_ts).await;
}
if action_id == "inception_cancel" {
    return process_inception_cancel(state, action_value, channel, message_ts).await;
}
```

**`process_inception_approve()` の処理:**
1. `db.get_ops_context(channel, thread_ts)` で最後の assistant エントリ（ターン2出力）を取得
2. タスク分解テーブルをパースして各行の `task_name`・`description` を抽出
3. 各タスクを `create_task_from_ops(ops_id, task_name, description, repo_key, channel, thread_ts)` で `coding_tasks` に登録
4. `state.wake_worker()` で既存タスク実行フローへ引き渡し
5. Slackスレッドに「N件のタスクを登録しました (task #X 〜 #Y)」を通知
6. ボタンメッセージを「承認済み」テキストに更新
7. パースに失敗した場合は要件テキスト全体を1タスクとして登録（フォールバック）

**`process_inception_revise()` の処理:**
1. ボタンを「修正リクエスト受付」テキストに更新
2. Slackスレッドに「修正のポイントをこのスレッドで `@bot` に返信してください」を投稿
3. ops_queue は変更しない（mark_ops_done済み）→ 次の @bot 返信で新エントリとして自動再エンキュー

**`process_inception_cancel()` の処理:**
1. ボタンを「キャンセル済み」テキストに更新
2. Slackスレッドに「要件定義をキャンセルしました」を投稿
3. `state.db.resolve_ops(ops_id)` で完了扱い（既存パターンと同一）

---

### FR-8: スレッド返信のルーティング（ユーザー回答の受け取り）

**ファイル**: `src/server/slack_events.rs`

**現状の制約**: スレッドへのメンションなし返信は `(Some(_), false) => {}` で無視される。

**Inception でのユーザー回答トリガー方法（Phase 1）**: 既存の admin @bot メンションルートを活用する。
- ユーザーは `@bot 〇〇です` の形式でスレッドに返信
- 既存の `(Some(tts), true) if is_admin` ルートで `enqueue_ops_request()` が呼ばれる
- `get_ops_context(channel, thread_ts)` でターン1の履歴が取得され、`history.len() > 0` → ターン2判定

**ファイル変更なし**（Phase 1では既存ルートをそのまま利用）。

**Phase 2 拡張（スコープ外）**: `inception` モードではメンションなしのスレッド返信も受け付けるよう、`(Some(tts), false)` 分岐に `ops_mode == Inception` の条件を追加する。

---

### FR-9: Asana タスク登録（段階的実装）

**Phase 1（今回実装）**:
- `create_task_from_ops()` で `coding_tasks` テーブルにローカル登録
- `asana_task_gid = "inception_{ops_id}_{index}"` のダミーGIDを使用
- 既存の `ops_escalate` ボタン処理と同じパターン

**Phase 2（将来拡張）**:
- `src/asana/client.rs` に `create_task(project_id, name, notes)` メソッドを追加
- `inception_approve` ハンドラから呼び出し、実際のAsana GIDを取得して `coding_tasks` に反映

---

## 非機能要件 (NFR)

### NFR-1: 既存モードへの影響なし
- `OpsMode::Execute` / `OpsMode::Plan` の処理パスは変更しない
- `execute_ops()` 既存関数のシグネチャを維持し、inception 専用の関数を別途追加する

### NFR-2: 会話履歴の整合性
- ターン1・ターン2ともに `ops_contexts` に user/assistant 両方を保存（既存フロー）
- 修正ループ（inception_revise 後の再エンキュー）でも同一スレッドの履歴が引き継がれる
- `history.len() >= 2`（user + assistant）でターン2と判定するため、修正ループの3回目以降も正しくターン2として処理される

### NFR-3: DBスキーマ変更なし
- `ops_queue.status` の拡張不要（ターン完了後は `done` のまま）
- `ops_contexts` の既存構造で会話履歴管理が完結する

### NFR-4: ボタンaction_idの一意性
- `inception_approve/revise/cancel` は既存の `ops_resolve/ops_escalate/approve_task/reject_task` と重複しない
- value には `ops_queue.id` を使い、`coding_task.id` と混在しない

### NFR-5: スキルファイル不要
- inception モードはスキルファイルなしで動作（plan モードと同様）
- `execute_ops_inception_turn1/2()` はスキルファイルを参照しない

---

## 変更ファイル一覧

| ファイル | 変更種別 | 変更内容 |
|----------|---------|---------|
| `src/repo_config.rs` | 変更（小） | `OpsMode::Inception` バリアント追加（3行） |
| `src/worker/ops.rs` | 追加 | inception 用 soul/rules 定数 × 3、`execute_ops_inception_turn1()` / `execute_ops_inception_turn2()` 関数 |
| `src/worker/runner.rs` | 変更 | `run_ops_item()` に `OpsMode::Inception` 分岐追加、`history.len()` によるターン判定、ターン2完了後の3ボタン投稿 |
| `src/server/slack_actions.rs` | 追加 | `process_inception_approve/revise/cancel()` ハンドラ3本、`process_action()` に3分岐追加 |

DBスキーマ変更なし。`src/server/slack_events.rs` の変更なし（Phase 1）。

---

## 受け入れ基準 (AC)

- **AC-1**: `ops_mode = "inception"` のリポジトリでメッセージを受信すると、Claudeが Intent分析サマリーと3〜5問の確認事項をスレッドに投稿する
- **AC-2**: ユーザーが `@bot 回答` 形式でスレッドに返信すると、ターン2が実行され要件定義＋タスク分解テーブルが投稿される
- **AC-3**: ✅承認ボタン押下後に `coding_tasks` にタスクが登録され、Slackに「N件のタスクを登録しました」が通知される
- **AC-4**: 🔧修正ボタン押下後に「@bot に返信してください」と表示され、次の返信でターン2が再実行される（会話履歴は引き継ぎ）
- **AC-5**: ❌キャンセルボタン押下後に「キャンセルしました」が表示されて終了する
- **AC-6**: `OpsMode::Execute` / `OpsMode::Plan` のリポジトリの動作に変化がない
- **AC-7**: inception 中に claude -p が失敗した場合、既存の `mark_ops_retry/failed` が正しく動作する

---

## リスク

| リスク | 影響 | 対策 |
|--------|------|------|
| R-1: `history.len()` によるターン判定のズレ | ターン2以降が誤ってターン1として処理される可能性 | `history.is_empty()` でターン1判定のため、1件でも履歴があればターン2。修正ループも意図通り機能する |
| R-2: タスク分解テキストのパース失敗 | coding_tasks が0件登録 | パースエラー時は要件テキスト全体を1タスクとして登録するフォールバックを実装 |
| R-3: admin 制限によるユーザビリティ低下 | inception 対話がadminしか使えない | Phase 1は admin @bot メンション方式とし、Phase 2で全ユーザー対応を追加 |
| R-4: Asana登録なし（Phase 1）でのユーザー期待のズレ | 「Asanaにタスクが登録されていない」という混乱 | Slack通知で「ローカル登録のみ、Asana連携は後続フェーズ」を明示 |

---

## 設計判断の記録

1. **DBスキーマ変更なし（researcher提案採用）**: `ops_contexts` の履歴件数でターン判定できるため、`inception_turn` カラム（scout提案）は不要。シンプルさを優先。

2. **`execute_ops()` シグネチャ維持**: inception 専用の `execute_ops_inception_turn1/2()` を別関数として追加することで、既存の execute/plan パスへの影響をゼロに抑える。

3. **Asana API 直接作成はフェーズ2**: `AsanaClient::create_task()` が存在せず、スコープを絞って初回実装を確実に完了させる。`create_task_from_ops()` のローカル登録パターンを再利用。

4. **スレッド返信の受け取りは既存ルートを活用（Phase 1）**: Inception専用のスレッド返信検出ロジックを追加するより、既存の「admin @bot メンション → エンキュー」ルートを使う。ユーザー体験の改善はPhase 2。
