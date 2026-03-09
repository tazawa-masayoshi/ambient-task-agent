# favorite_pop ops

新規取材タイプの追加作業を行う。

## 作業ディレクトリ

`favorite_pop/` 配下で作業すること。

## 手順

### 1. テンプレート画像の配置

- Slack に添付された画像を `assets/templates/` に保存
- 命名規則: 小文字英数字とアンダースコア（例: `shirokuro.png`）
- 画像サイズ: 1080x1920

### 2. Python 設定追加

`src/config/interview_types.py` の `INTERVIEW_TYPES` 辞書に新しいエントリを追加する。

既存エントリを参考に以下を設定:
- `sheet_name`: Google Sheets のタブ名
- `image_path`: テンプレート画像パス
- `title_format`: 生成ファイル名の形式
- `date_position`: 日付表示座標 (X, Y)
- `text_positions`: テキスト配置座標
- `font_size`: フォントサイズ
- `text_color`: テキスト色（白 or 黒）
- `display_settings`: 配信タイミング設定
- `custom_url_generator`: URL 設定
- `column_mapping`: 結果書き込み列

### 3. GAS 設定追加

- `gas-scripts/config.js` の `ORIGINAL_RULES` にルール追加
- `gas-scripts/kintone.js` の `SHEET_NAMES` にマッピング追加
- 固定 URL の場合は `URL_MAPPING` にも追加

## 注意

- Slack メッセージに取材タイプ名・管理No・配信設定などが記載されている
- 既存の設定パターンに合わせること
- 不明な項目がある場合は確認が必要な内容を報告すること
