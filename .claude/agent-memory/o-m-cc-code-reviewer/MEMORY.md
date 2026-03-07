# Code Reviewer Memory

## Project Patterns

### 繰り返し見つかる品質問題
- `truncate_str` 系のユーティリティ関数がモジュールをまたいで重複しやすい (`claude.rs` と `runner.rs` で確認済み)
- ビルダーパターンのメソッドで "後方互換" と書かれた `#[allow(dead_code)]` メソッドが残留しやすい
- `unsafe impl Send/Sync` が本当に必要かコメントが不正確なケースがある (trait bound で自動導出されるケース)
- 複数フロー分岐（early return など）でサイドエフェクト（ファイル書き出し等）の呼び出しが片方だけ抜けやすい → `write_task_file` で確認済み
- LLM 出力パーサーの `contains()` 部分一致は単語境界未チェックになりがち → `split_whitespace().any(|w| w == keyword)` が安全

### プロジェクト固有の OK パターン
- `ClaudeRunner` のビルダーチェーン末尾 `.with_context(runner_ctx).run()` は各 worker モジュールで統一されており正常
- `build_system_prompt()` の共通化は analyzer/decomposer/executor/scheduler で適切に実施済み
- `HookDecision` の `before_run` チェーンは設計として OK (短絡評価)

### 効率・ホットパス上の既知パターン
- `load_credentials_env()` が `load_slack_config/asana/server` ごとに独立呼び出しされ、起動時に3ファイルを複数回読む → `OnceLock` またはまとめて1回呼び出しにすべき
- `cmd_task` のように複数フラグがある関数で DB を毎フラグ open するパターンが発生しやすい → 先頭で1回だけ open して使い回す
- ops_monitor チャンネルで全トップレベルメッセージを LLM 分類するパターン → 高頻度チャンネルではコスト増大リスク
- `load_credentials_env` の `set_var` ループは「プロセス env が最優先」になっており、`.env` 優先というコメントと矛盾しやすい
- 二重 spawn（外側 spawn → 内側 spawn）は機能問題なし → 過剰検知寄り、デバッグ複雑性の指摘にとどめる

### 繰り返し見つかるコード再利用問題
- `AsanaConfig { pat: state.asana_pat.clone(), ... }` の inline 構築が `slack_events.rs` / `webhook.rs` / `hooks.rs` で繰り返し発生 → `AppState::asana_config()` メソッド化が有効
- `OpsContext` 構造体が `http.rs` に定義されているが実際には未使用（会話履歴は DB 経由）→ デッドコードの可能性大
- `classify_ops_message` の `answer.contains("YES")` は部分一致問題（memory 既記の `.split_whitespace().any(|w| w == keyword)` が安全）
- `log_dir_from_state` はモジュールプライベートで `slack_events.rs` 内に閉じており、他ファイルに同じロジックが生まれる余地がある → `AppState` のメソッドに移動が望ましい
- `reqwest::Client::new()` が `slack_socket.rs` と `SlackClient` / `AsanaClient` / `GoogleCalendarClient` に分散 → Socket Mode は `SlackClient` を共有できる可能性あり

### Blast Radius が大きかった変更の傾向
- `RunnerContext` の導入: 11ファイルにシグネチャ変更が波及 (これは意図的なリファクタリング)
- `truncate_for_slack` が `runner.rs` から `slack_events.rs` に参照されており、モジュール境界が曖昧になっている
- `CalendarEvent` にメソッドを追加せず利用側で再パースするパターンが発生しやすい → `end_time()` 不在が原因

### 重複ステート パターン (2回目確認)
- `RunnerContext` 導入後も `AppState.semaphore` と `SchedulerContext.defaults/semaphore` が残留
  → `#[allow(dead_code)]` を付けて移行期に放置するパターンは定着している。レビュー時は必ず残留フィールドを確認すること
- `ClaudeRunner.registry` のように「書き込まれるが読み取られない」フィールドが発生しやすい

## Calibration
- `#[allow(dead_code)]` 付きフィールドは移行期の意図的残留と誤用の区別が必要 → Context を読んで判断
- `unsafe impl Send/Sync` の誤用は見逃しリスクが高い → 今後も重点確認する
- 過剰検知: `else { if ... }` 構造を常に Warning にするのは厳しすぎる可能性。本体の return が絡む場合は許容
- `i32 as u32` キャストは DB 由来の Optional フィールドで発生しやすい → simple パス特有のバイパスに注意
- `compute_free_slots` のような区間演算は、一見バグに見えるコードが実は正しいことがある → テスト確認を先に行うこと（過剰検知防止）
- `CalendarEvent::end_time()` の欠如は繰り返し指摘パターン → 新規コードが追加されるたびに再発する構造的問題。早期に追加を促す
- `if result.is_empty() { result } else { result }` パターン（両分岐が同値）はコンパイラが通すが、意図不明瞭として必ず指摘する
