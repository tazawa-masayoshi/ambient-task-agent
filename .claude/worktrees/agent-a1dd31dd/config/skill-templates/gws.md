## Google Workspace CLI (gws)

Google Workspace の操作には `gws` コマンドを使用する。
認証は事前設定済み（OAuth2 / keyring）。出力は JSON 形式。

### 構文

```
gws <service> <resource> [sub-resource] <method> [flags]
```

- `--params '<JSON>'` — URL/クエリパラメータ
- `--json '<JSON>'` — リクエストボディ（POST/PATCH/PUT）
- `--upload <PATH>` — ファイルアップロード（multipart）
- `--output <PATH>` — バイナリレスポンスの保存先
- `--format <FMT>` — 出力形式: json（デフォルト）, table, yaml, csv
- `--page-all` — 自動ページネーション（NDJSON 出力）
- `--dry-run` — API に送信せずバリデーションのみ
- `gws schema <service.resource.method>` — API スキーマ確認

### 主要コマンド

```bash
# Calendar
gws calendar events list --params '{"calendarId":"primary","timeMin":"2026-04-01T00:00:00+09:00","timeMax":"2026-04-02T00:00:00+09:00","maxResults":10}'
gws calendar events insert --params '{"calendarId":"primary"}' --json '{"summary":"会議","start":{"dateTime":"2026-04-01T10:00:00+09:00"},"end":{"dateTime":"2026-04-01T11:00:00+09:00"}}'

# Gmail
gws gmail users messages list --params '{"userId":"me","maxResults":5}'
gws gmail users messages get --params '{"userId":"me","id":"MSG_ID","format":"metadata","metadataHeaders":["Subject","From","Date"]}'
gws gmail users messages send --params '{"userId":"me"}' --json '{"raw":"BASE64_ENCODED_RFC2822"}'

# Drive
gws drive files list --params '{"pageSize":10}'
gws drive files list --params '{"q":"name contains '\''keyword'\''","pageSize":10}'
gws drive files get --params '{"fileId":"FILE_ID"}'
gws drive files create --params '{"uploadType":"multipart"}' --json '{"name":"file.pdf"}' --upload file.pdf

# Sheets
gws sheets spreadsheets get --params '{"spreadsheetId":"SHEET_ID"}'
gws sheets spreadsheets values get --params '{"spreadsheetId":"SHEET_ID","range":"Sheet1!A1:D10"}'
gws sheets spreadsheets values append --params '{"spreadsheetId":"SHEET_ID","range":"Sheet1","valueInputOption":"USER_ENTERED"}' --json '{"values":[["A","B","C"]]}'

# Docs
gws docs documents get --params '{"documentId":"DOC_ID"}'
gws docs documents create --json '{"title":"ドキュメント名"}'
```

### 注意事項

- 日時は必ず ISO 8601 + タイムゾーン（`+09:00`）で指定
- Calendar の `calendarId` は通常 `"primary"`
- Gmail の `userId` は通常 `"me"`
- `--page-all` でページネーション自動処理（NDJSON 出力、最大10ページ）
- `--dry-run` で実行前プレビュー可能
- 大量操作時は API quota に注意（Gmail: 250通/日、Sheets: 300リクエスト/分）
