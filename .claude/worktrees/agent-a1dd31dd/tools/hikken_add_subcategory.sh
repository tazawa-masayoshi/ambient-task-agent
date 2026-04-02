#!/bin/bash
set -euo pipefail

# hikken_schedule の image_mappings.yaml にサブカテゴリを追加する
# 環境変数:
#   PARAM_EVENT_NAME    - イベント名 (例: "WORLD2 RE:MEMORY 取材")
#   PARAM_MANAGEMENT_NO - イベント管理No (例: "269")
#   PARAM_IMAGE_FILE    - 添付画像ファイル名 (任意、images/ にダウンロード済み)

YAML_FILE="hikken_schedule/src/config/image_mappings.yaml"
IMAGES_DIR="hikken_schedule/images"
LOCK_FILE="/tmp/amu-tazawa-scripts.lock"

: "${PARAM_EVENT_NAME:?EVENT_NAME is required}"
: "${PARAM_MANAGEMENT_NO:?MANAGEMENT_NO is required}"

# リポジトリ単位の排他ロック（同時実行防止）
exec 200>"$LOCK_FILE"
flock -w 60 200 || { echo "エラー: ロック取得タイムアウト（他の作業が実行中）"; exit 1; }

# リモート最新を取得してリベース
jj git fetch
jj rebase -d main@origin 2>/dev/null || true

# 既に同名エントリがあるか確認
if grep -qF "  ${PARAM_EVENT_NAME}:" "$YAML_FILE"; then
    echo "エラー: '${PARAM_EVENT_NAME}' は既に登録されています"
    grep -F "  ${PARAM_EVENT_NAME}:" "$YAML_FILE"
    exit 1
fi

# management_no の最大連番を取得
max_seq=$(grep -oP "\"${PARAM_MANAGEMENT_NO}_(\d+)\." "$YAML_FILE" \
    | grep -oP "${PARAM_MANAGEMENT_NO}_\K\d+" \
    | sort -n | tail -1)
next_seq=$(( ${max_seq:-0} + 1 ))

# 画像ファイルの処理
if [ -n "${PARAM_IMAGE_FILE:-}" ] && [ -f "${IMAGES_DIR}/${PARAM_IMAGE_FILE}" ]; then
    ext="${PARAM_IMAGE_FILE##*.}"
    new_filename="${PARAM_MANAGEMENT_NO}_${next_seq}.${ext}"
    mv "${IMAGES_DIR}/${PARAM_IMAGE_FILE}" "${IMAGES_DIR}/${new_filename}"
    echo "画像リネーム: ${PARAM_IMAGE_FILE} → ${new_filename}"
else
    new_filename="${PARAM_MANAGEMENT_NO}_${next_seq}.jpg"
    if [ -n "${PARAM_IMAGE_FILE:-}" ]; then
        echo "警告: 画像ファイル '${PARAM_IMAGE_FILE}' が見つかりません"
    fi
fi

# YAML にエントリ追加
echo "  ${PARAM_EVENT_NAME}: \"${new_filename}\"" >> "$YAML_FILE"

# jj でコミット & プッシュ
jj describe -m "feat: サブカテ追加 ${PARAM_EVENT_NAME} (管理No.${PARAM_MANAGEMENT_NO})"
jj bookmark set main -r @
jj git push

echo "追加完了: ${PARAM_EVENT_NAME}: \"${new_filename}\" (管理No.${PARAM_MANAGEMENT_NO}, 連番${next_seq}) — push済み"
