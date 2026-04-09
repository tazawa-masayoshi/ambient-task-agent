# hikken_schedule ops

必見スケジュールのサブカテゴリ追加・クローズ作業を行う。

## 作業ディレクトリ

`hikken_schedule/` 配下で作業すること。

## 手順

1. **残骸チェック & 隔離 → 最新取得 → 新しい change 作成**
   ```bash
   # 前の ops の残骸があれば wip として commit して隔離
   if jj status 2>&1 | grep -q "Working copy changes"; then
     jj describe -m "wip: stale work before hikken ops $(date +%s)"
     jj new
   fi

   # リモート最新取得 & 新しい @ を main@origin の子に rebase
   jj git fetch
   jj rebase -r @ -d main@origin 2>/dev/null || true
   ```

   これで `@` は空の change で main@origin の直接の子。他の作業の残骸が混ざらない。

2. **Slack メッセージから作業内容を抽出**
   - 「▼サブカテ追加」→ 追加作業
   - 「▼クローズ対応」→ クローズ作業
   - イベント管理No とイベント名・サブカテ名を全て抽出する
   - 1つの依頼に複数の管理No・複数のサブカテゴリが含まれることがある
   - kintone 側の指示（あいうえお順フィールド、配置位置等）は無視する

3. **画像ファイルのリネーム**（添付画像がある場合）
   - `images/` にダウンロード済みの画像を `{管理No}_{連番}.{ext}` にリネーム
   - 連番は `src/config/image_mappings.yaml` 内の該当管理No の最大連番 + 1

4. **`src/config/image_mappings.yaml` を編集**

   **追加の場合:**
   - **サブカテゴリあり**: `  イベント名: "{管理No}_{連番}.jpg"` を末尾に追加
   - **サブカテゴリなし**: `  イベント名: "{管理No}.jpg"` を末尾に追加
   - 複数エントリがある場合は全て一括で追加する
   - 重複チェック: 同名エントリが既にある場合はスキップして報告

   **クローズの場合:**
   - 該当するエントリの行頭に `#` を追加してコメントアウトする
   - 例: `  VIP: "269_1.jpg"` → `  #VIP: "269_1.jpg"`
   - 該当エントリが見つからない場合は報告する

5. **コミット & プッシュ**（全エントリ編集後に1回だけ）
   ```bash
   # 編集した change に description を付ける
   jj describe -m "feat(hikken_schedule): サブカテ追加 {イベント名の要約}"
   # クローズの場合:
   # jj describe -m "feat(hikken_schedule): サブカテクローズ {イベント名の要約}"

   # push 直前にもう1度 fetch & rebase（並列 ops による remote 進行に対する race 対策）
   jj git fetch
   jj rebase -r @ -d main@origin 2>/dev/null || true

   jj bookmark set main -r @
   jj git push
   ```

   **注意:** `jj status` で変更対象が `hikken_schedule/` 配下だけになっていることを必ず確認してから describe すること。他プロジェクト（knowledge-bot, crates/, psp_ocr 等）の変更が混ざっていたら異常。その場合は描写せずに報告して中断する。

## 出力フォーマット

作業完了後、以下の形式で結果を報告:
```
追加完了:
- {イベント名}: "{ファイル名}" (管理No.{管理No})
...
push済み
```

クローズの場合:
```
クローズ完了:
- {サブカテ名} (管理No.{管理No})
...
push済み
```

## 注意

- 既存エントリのフォーマットに合わせること
- 不明な場合は確認が必要な内容を報告すること
- 依頼内容が自分たちのスコープ外（kintone側のみの作業等）の場合は「対応不要」と報告
