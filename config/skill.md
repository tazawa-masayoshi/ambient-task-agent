## バージョン管理
- Jujutsu (jj) を使用。git コマンドは使わない
- コミットメッセージは Conventional Commits 形式（feat:, fix:, chore:, docs:, refactor:）
- `jj describe -m "msg"` で必ず -m を付ける（エディタを開かせない）

## 言語・ツール
- Rust プロジェクト: cargo build / cargo test で確認
- mise 経由のコマンド: `~/.local/share/mise/shims/` を使用

## コーディング規約
- エラーは `anyhow::Result` で返す
- ログは `tracing` マクロを使用
- 不要な `unwrap()` は避ける。`?` または適切なフォールバック
