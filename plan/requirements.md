# Requirements: MCP ベース自律エージェントアーキテクチャへの移行

## 背景

ambient-task-agent は現在、チャンネル→リポジトリの固定マッピングとハードコードされたツール実装（tool_impls.rs）に依存している。これを MCP (Model Context Protocol) ベースのプラグイン可能なアーキテクチャに移行し、自律性と拡張性を向上させる。

### 現状の問題

1. **チャンネル固定ルーティング**: 同じチャンネルに異なる種類の依頼が来ると正しくルーティングできない
2. **ハードコードツール**: tool_impls.rs に Read/Write/Edit/Bash/Grep/Glob を自前実装（セマンティック理解なし）
3. **リポジトリ固定**: 依頼がどのリポジトリに属するか事前定義が必要
4. **claude -p 依存の残骸**: ClaudeCliBackend やストリーミング関連のデッドコードが残存

## 機能要件

### FR-1: MCP サーバーによるツール提供

ハードコードされた tool_impls.rs を MCP サーバーに置き換える:

- **Serena** (`--context autonomous-agent`): セマンティックコード解析・編集（find_symbol, replace_symbol_body, rename_symbol 等 21ツール）。30+ 言語対応（Rust: rust-analyzer, Python: pylsp）
- **GitHub MCP Server**: Issue/PR/Actions 操作
- **kintone MCP Server**: kintone 公式 MCP（レコード CRUD）
- **gws CLI**: Google Workspace 操作（既存の Bash 経由 → MCP 化検討）
- **filesystem / bash**: ファイル操作・コマンド実行（Serena の autonomous-agent モードでカバー可能）

### FR-2: コンテンツベースルーティング

チャンネル固定マッピングを廃止し、メッセージ内容から動的にスコープを判定:

- LLM がメッセージを分析し、適切な MCP ツール群を選択
- リポジトリ/プロジェクトは Serena の `--project` で動的切り替え
- 同じチャンネルの異なる依頼（favorite_pop, send_survey_mail, 汎用調査）を正しく振り分け

### FR-3: 動的プロジェクトコンテキスト

repos.toml のリポジトリ定義は残すが、ルーティングの基準をチャンネルから内容に変更:

- リポジトリごとの MCP サーバー設定（既存の `mcp_servers` フィールドを活用）
- Serena のプロジェクトパスを動的に設定
- スキルファイルは Serena の memory 機能で管理も検討

### FR-4: セーフガードの MCP 対応

tool_impls.rs の Bash safeguard を MCP レベルで実現:

- agent_loop.rs でツール呼び出し前にフィルタリング（既存の仕組みを維持）
- MCP サーバー側のサンドボックス機能の活用（Serena の filesystem 制限等）

### FR-5: claude-auth crate による LLM 呼び出し

`claude -p` を完全廃止し、claude-auth crate (OAuth + API Key) で直接 API を呼ぶ:

- `AGENT_BACKEND=max` (OAuth) をデフォルトに
- Bedrock はフォールバックとして維持
- `ClaudeCliBackend` は削除候補

## 非機能要件

### NFR-1: 既存 ops の動作維持

移行中も既存の ops（hikken_schedule, send_survey_mail, favorite_pop）が動作し続けること。

### NFR-2: 段階的移行

一括移行ではなく、MCP サーバーを1つずつ追加し、tool_impls.rs を段階的に縮小する。

### NFR-3: トークン効率

Serena のシンボルレベル操作により、ファイル全体を読む必要がなくなり、トークン消費を削減。

## 制約

- Serena は `uv` + 対象言語の Language Server が必要
- kintone MCP は Docker or npx で提供（既に repos.toml に設定パターンあり）
- MCP サーバーの起動・管理は McpManager（既存実装）で対応可能
- Max プランのレートリミットを考慮した設計

## 既知の不足

- Google Workspace の MCP 化方針（gws CLI のまま vs Google 公式 MCP）
- Slack MCP の統合方針（slack-socket crate のまま vs Slack 公式 MCP）
- Claude Code Max プランのレートリミット上限の正確な値
