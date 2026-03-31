## kintone REST API

kintone の操作には REST API を使用する。
認証は Basic認証 + API トークンの2段階。

### 認証

- Basic認証: `KINTONE_BASIC_AUTH_USERNAME` / `KINTONE_BASIC_AUTH_PASSWORD`
- API トークン: `KINTONE_TOKEN_{appId}`（アプリごとに異なる）
- ドメイン: `KINTONE_DOMAIN`（例: `dmm-amu` → `https://dmm-amu.cybozu.com`）

### 登録済みアプリ

| アプリ ID | 用途 |
|-----------|------|
| 3617 | 新イベント依頼（案件データ） |
| 3351 | ぱちタウン管理マスタ（店舗データ） |
| 3352 | 社員マスタ |

### 主要 API

```bash
DOMAIN="https://${KINTONE_DOMAIN}.cybozu.com"
AUTH="Authorization: Basic $(echo -n "${KINTONE_BASIC_AUTH_USERNAME}:${KINTONE_BASIC_AUTH_PASSWORD}" | base64)"
TOKEN="X-Cybozu-API-Token: ${KINTONE_TOKEN_3617}"

# レコード取得（1件）
curl -s -H "$AUTH" -H "$TOKEN" \
  "${DOMAIN}/k/v1/record.json?app=3617&id=90918"

# レコード一覧取得（クエリ）
curl -s -H "$AUTH" -H "$TOKEN" \
  "${DOMAIN}/k/v1/records.json?app=3617" \
  --data-urlencode 'query=ステータス in ("未対応") limit 10'

# レコード登録
curl -s -X POST -H "$AUTH" -H "$TOKEN" \
  -H "Content-Type: application/json" \
  "${DOMAIN}/k/v1/record.json" \
  -d '{"app":3617,"record":{"フィールド名":{"value":"値"}}}'

# レコード更新
curl -s -X PUT -H "$AUTH" -H "$TOKEN" \
  -H "Content-Type: application/json" \
  "${DOMAIN}/k/v1/record.json" \
  -d '{"app":3617,"id":90918,"record":{"フィールド名":{"value":"新しい値"}}}'

# フィールド情報取得（スキーマ確認）
curl -s -H "$AUTH" -H "$TOKEN" \
  "${DOMAIN}/k/v1/app/form/fields.json?app=3617"
```

### URL からの ID 抽出

kintone URL: `https://dmm-amu.cybozu.com/k/{appId}/show#record={recordId}`

- アプリ ID: パス `/k/{appId}/` の数値部分
- レコード ID: フラグメント `record={recordId}` の数値部分

### 注意事項

- API トークンはアプリごとに異なる（`KINTONE_TOKEN_{appId}`）
- クエリの文字列フィールドは日本語名で指定（例: `ステータス`）
- 一括取得は最大500件/リクエスト（`limit 500`）
- レコード更新時は `revision` を指定すると楽観ロック可能
- フィールド名が不明な場合は `form/fields.json` で確認
