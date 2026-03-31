## Google Workspace CLI (gws)

Google Workspace の操作には `gws` コマンドを使用する。
認証は事前設定済み（Service Account）。出力は JSON 形式。

### 主要コマンド

```bash
# Gmail
gws gmail send --to "user@example.com" --subject "件名" --body "本文"
gws gmail send --to "user@example.com" --subject "件名" --body "本文" --attachment "file.pdf"

# Calendar
gws calendar agenda                    # 今日の予定
gws calendar agenda --days 7           # 今後7日間
gws calendar insert --summary "会議" --start "2026-04-01T10:00:00+09:00" --end "2026-04-01T11:00:00+09:00"

# Drive
gws drive upload file.pdf              # ファイルアップロード
gws drive list                         # ファイル一覧

# Sheets
gws sheets get --spreadsheet-id "SHEET_ID" --range "Sheet1!A1:D10"
gws sheets append --spreadsheet-id "SHEET_ID" --range "Sheet1" --values '[["A","B","C"]]'

# Docs
gws docs create --title "ドキュメント名" --body "内容"
```

### 注意事項

- 日時は必ず ISO 8601 + タイムゾーン（`+09:00`）で指定
- `--page-all` でページネーション自動処理（NDJSON 出力）
- `--dry-run` で実行前プレビュー可能
- 大量操作時は API quota に注意（Gmail: 250通/日、Sheets: 300リクエスト/分）
