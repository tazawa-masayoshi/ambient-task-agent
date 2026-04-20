# send_survey_mail ops

GAS ベースのアンケート送信システムの保守作業を行う。

## 作業ディレクトリ

`send_survey_mail/` 配下で作業すること。

## システム構成

- Google Apps Script で構成
- Kintone からデータ取得 → メール送信 → 回収管理を自動化
- ファイルは番号付き（`01-main.js` 〜）で役割分担

### 主要ファイル

| ファイル | 役割 |
|---------|------|
| `01-main.js` | メインエントリポイント |
| `05-kintoneAPI.js` | Kintone 連携 |
| `13-メール送信.js` | メール送信処理 |
| `14-催促メール送信.js` | 催促メール処理 |
| `16-メール回収判定.js` | 回収チェック |

## 対応可能な保守作業

- メールテンプレートの修正（`メール本文.html` 等）
- フィルタリング条件の変更（`08-フィルタリング判定.js`）
- 定数・設定値の変更（`03-constants.js`）
- バグ修正・機能改善

## 取材設定シートへの行追加ルール

取材設定シート（スプレッドシートID: `1DbQv09X9XVslKl8q3-1oEHwntZLYsZQIVlDnPQ-hcZM`）にサブカテゴリを追加する際は、**同じ管理No（A列）のまとまりの末尾に挿入すること**。最終行に追記してはいけない。

手順:
1. 対象管理No の行を A列で全て検索し、その中で最大の行番号を特定
2. その行の直下に `insertDimension` で行を挿入（`sheets.batchUpdate` の `insertRange` または `insertDimension`）
3. 挿入した行に値を書き込む

理由: 管理No ごとに行がまとまっている前提で運用しているため、末尾追加だと並びが崩れる。

## 注意

- GAS のファイルなので `appsscript.json` のマニフェストを壊さないこと
- シート名やカラム構成を変更する場合は影響範囲を確認すること
- 不明な要件は確認が必要な内容を報告すること

## 禁止事項（厳守）

**kintone アプリの仕様変更は絶対に行わないこと。** 次の操作は全て禁止:

- フィールドの追加・削除・名称変更
- `required` / `unique` / `defaultValue` など属性の変更
- `/k/v1/preview/app/form/fields.json` の PUT/POST/DELETE
- `/k/v1/preview/app/deploy.json` への POST（アプリデプロイ）
- ルックアップ・関連レコード設定の変更
- ビュー・グラフ・通知設定の変更

kintone 側で対応が必要と判断した場合は、**変更を実行せず** `OPS_RESULT: proposal` で
「kintone 側でこの仕様変更が必要です」と admin に提案のみ行うこと。admin が手動で対応する。

許可されている kintone 操作はレコード CRUD（GET/POST/PUT/DELETE `/k/v1/records.json` 等）のみ。
