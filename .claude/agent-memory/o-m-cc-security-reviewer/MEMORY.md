# Security Reviewer Memory

## Project Threat Model
- Rust + Axum Web サーバー、Slack Webhook/Events API、SQLite (rusqlite)、subprocess `claude -p`、Asana API、Google Calendar API (Service Account JWT)
- 信頼境界: Slack 署名検証 (/webhook/slack は実装済み、/slack/actions は未実装)
- 攻撃面: Slack Interactivity エンドポイント、subprocess コマンド実行、ファイル書き出し、LLM プロンプトへの外部データ埋め込み（Asana/GCal）

## Known Patterns
- `slack_signing_secret` は `Option<String>` — None の場合は署名検証をスキップする実装 (fail-open)
- `/slack/actions` の署名検証コードは追加済みだが、signing_secret=None 時は fail-open のまま (Warning)
- HMAC 比較が `computed == expected` (文字列比較) — タイミング攻撃リスクがある (Warning)
- `Command::new("claude").args(["-p", &prompt, ...])` — args() 経由なのでシェルインジェクションは発生しない
- プロンプトは stdin 経由で渡す (`cmd.stdin(piped)` + `write_all`) — 引数長制限もなし
- `workspace.rs` の git/gh 実行も全て `run_cmd("git/gh", cwd, args)` 経由 → シェルインジェクションなし
- worktree パスは `PathBuf::join(repo_key).join(task_id_number)` で構築 → repo_key は設定定数、task_id は i64 数値 → パストラバーサルなし
- `task_name` (Asana 由来) が `git commit -m` に展開される → args() 経由のためインジェクション不可だが長さ制限未実施 (Warning)
- `claude_session_id` は Claude CLI レスポンス JSON から DB に保存 → `--resume <id>` に展開。バリデーションなし (Warning, args()経由のためシェルinj不可だが異常値混入の余地あり)
- `task.branch_name.as_deref().unwrap_or("")` が worktree 再利用に使われる → 空文字でコマンド失敗リスク (Warning)
- `TASK_COLUMNS` は定数 → format!() 内 SQL インジェクションは発生しない
- `add_missing_columns` の table/col 名は全てハードコード定数 → 動的入力なし
- GCal `urlencoded()` は @ と # のみエンコード — calendar_id は設定ファイル由来で外部入力ではないため許容
- GCal `delete_event()` の event_id は API 応答から取得したもの → 外部ユーザー直接制御ではないが GCal 側のデータ汚染経路あり
- GCal `token_uri` は Service Account JSON から読み込み → SSRF の潜在経路 (Warning)
- プロンプトへの Asana タスク名・GCal イベント名の無加工埋め込み → プロンプトインジェクション経路
- Slack ファイルダウンロード: `Path::new(&f.name).file_name()` でパストラバーサル修正済み (slack_events.rs:815-818)。ただし `build_ops_prompt` 内のプロンプトへの埋め込みは `f.name` のまま → ファイル名経由プロンプトインジェクション (Warning)
- ops 実行で `OPS_ALLOWED_TOOLS` に `Write,Edit,Bash` が含まれる — Slack ユーザーからの入力が LLM プロンプトに直接流入する経路がある (プロンプトインジェクション→ファイル書き換え/コマンド実行)
- Plan mode の `--resume` は session_id が Claude CLI の内部ID → 攻撃者がそのIDを知るには DB アクセスが必要、直接の外部入力経路はない
- Bedrock バックエンド (`bedrock.rs`) に `bash -c <LLM生成コマンド>` が実装されている。`execute_ops_with_tools` では `allowed_tools("")` で無効化済みだが、他の呼び出しパスでは有効になりうる (Warning)
- `OpsToolDispatcher`: LLM生成パラメータを `PARAM_xxx` 環境変数としてシェルスクリプトに渡す。値の長さ・内容バリデーションなし。シェルスクリプト側の実装に依存 (Warning)

## Accepted Risks
- subprocess `claude -p` はユーザー入力をプロンプトとして渡す。シェル展開なし (args()使用) のためコマンドインジェクションは非該当だが、プロンプトインジェクションは設計上の受容リスク
- GCal イベント名・Asana タスク名のプロンプト埋め込みによるプロンプトインジェクションは設計上の受容リスク（自分のカレンダー/タスクのみが対象）

## Calibration
- 過去に `/slack/actions` の署名検証なしを Critical と判定したが、今回確認したところ署名検証コードは実装済み（fail-open だが Critical ではなく Warning に降格）。Critical 判定には実際のコードを確認してから行う必要がある。
- worktree 系の引数は設定ファイル / DB 由来が多く、ユーザー入力直接流入は少ない → コマンドインジェクション過剰検知に注意。
