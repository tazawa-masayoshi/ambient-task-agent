# Security Reviewer Memory

## Project Threat Model
- Rust + Axum Web サーバー、Slack Socket Mode (主) + Webhook routes (条件付き mount)、SQLite (rusqlite)、agent-harness 経由の Anthropic/Bedrock API 直接呼び出し、Asana API、Google Calendar API (Service Account JWT)
- 信頼境界: Slack 署名検証は **fail-closed 化済み** (2026-04-07) — secret 未設定なら webhook ルート自体が mount されず 404
- 攻撃面: Slack Socket Mode 経由のイベント、ツール実行 (Bash/Write/Edit)、ファイル書き出し、LLM プロンプトへの外部データ埋め込み（Asana/GCal/Slack ファイル名）
- EC2 SG `sg-0f6143890dad5fc90`: port 3100 は inbound rule なし → AWS レベルで外部到達不可（depth-in-defense として http.rs の fail-closed 修正済み）

## Known Patterns
- **webhook ルートは fail-closed** (http.rs:51-): `slack_signing_secret.is_some()` の場合のみ `/webhook/slack` + `/slack/actions` を mount。`asana_webhook_secret.is_some()` の場合のみ `/webhook/asana` を mount。secret 未設定なら 404 で安全 (2026-04-07 修正済み)
- 過去の fail-open 状態 (Option<String> + ハンドラ内でスキップ) は解消済み。レビュー時に「signing_secret=None で fail-open」を再指摘しないこと
- HMAC 比較は `constant_time_eq()` による定数時間比較に修正済み (slack_webhook.rs:127-136) — タイミング攻撃リスク解消済み
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
- `build_ops_prompt()` (ops.rs:165) の `req.message_text` に boundary marker なし — `## 見出し` を含む入力でプロンプト構造を上書き可能 (Warning, openclaw #5 相当)
- Plan mode の `--resume` は session_id が Claude CLI の内部ID → 攻撃者がそのIDを知るには DB アクセスが必要、直接の外部入力経路はない
- Bedrock バックエンド (`bedrock.rs`) に `bash -c <LLM生成コマンド>` が実装されている。`execute_ops_with_tools` では `allowed_tools("")` で無効化済みだが、他の呼び出しパスでは有効になりうる (Warning)
- `OpsToolDispatcher`: LLM生成パラメータを `PARAM_xxx` 環境変数としてシェルスクリプトに渡す。値の長さ・内容バリデーションなし。シェルスクリプト側の実装に依存 (Warning)
- `--bare` + `--add-dir` の追加 (claude.rs:368-372): `cmd.args(["--add-dir", &add_dir_str])` 経由で安全。ただし cwd は設定ファイル由来であることが前提 — 外部入力が cwd に入ると任意 CLAUDE.md ロードの経路になりうる (Warning)
- `OPS_RESULT` マーカー判定 (runner_ops.rs:319-328): 全文検索のため、Slack 入力にマーカー文字列が含まれると LLM 出力に反射して is_no_action が偽装される可能性あり (Warning)。末尾 N 行のみ検索すると軽減可能。
- `route_ops` プロンプト (runner_ops.rs:547-553): `item.message_text` を無加工埋め込み。ただし `json_schema` + `allowed_tools("")` で出力形式が `{"scope": N}` に制約されており実害は限定的 (Warning)

## Accepted Risks
- subprocess `claude -p` はユーザー入力をプロンプトとして渡す。シェル展開なし (args()使用) のためコマンドインジェクションは非該当だが、プロンプトインジェクションは設計上の受容リスク
- GCal イベント名・Asana タスク名のプロンプト埋め込みによるプロンプトインジェクションは設計上の受容リスク（自分のカレンダー/タスクのみが対象）

## Calibration
- 過去に `/slack/actions` の署名検証なしを Critical と判定したが、今回確認したところ署名検証コードは実装済み（fail-open だが Critical ではなく Warning に降格）。Critical 判定には実際のコードを確認してから行う必要がある。
- worktree 系の引数は設定ファイル / DB 由来が多く、ユーザー入力直接流入は少ない → コマンドインジェクション過剰検知に注意。
- Socket Mode はクライアント側 WebSocket → cross-site WebSocket hijacking は構造的に非該当。openclaw 系の WebSocket 脆弱性をそのまま適用しないこと。
- `json_schema` + `allowed_tools("")` の組み合わせはプロンプトインジェクションの実害を大きく限定する — ルーティング専用の LLM 呼び出しでこのパターンが使われている場合は Confidence を下げてよい。
