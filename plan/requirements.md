# Requirements: Bedrock Converse API 対応

## 背景

現在 `AnthropicApiBackend` は Anthropic Messages API を直叩きしている。
ユーザーは Anthropic API キーが使えないため、AWS Bedrock 経由でのみ Claude を利用する必要がある。

## 機能要件

### FR-1: Bedrock Converse Stream クライアント
- `aws-sdk-bedrockruntime` の `converse_stream()` を使用
- 認証は AWS SDK デフォルトチェーン（IAM ロール / 環境変数 / プロファイル）
- `MessagesResponse` 互換のレスポンスを返す

### FR-2: 型変換レイヤー
- `MessagesRequest` → Converse API 形式への変換
- Converse ストリームイベント → `MessagesResponse` への組み立て
- ツール定義: `ToolDefinition` → `ToolSpecification`
- メッセージ変換: `ContentBlock` ↔ Converse `ContentBlock`
- StopReason マッピング: `EndTurn`/`ToolUse`/`MaxTokens`/`ModelContextWindowExceeded`
- 連続 ToolResult の1メッセージ集約

### FR-3: LlmClient trait による抽象化
- `AnthropicClient` と `BedrockClient` が共通 trait を実装
- `agent_loop.rs` の `run_agent_loop()` が trait 経由で LLM を呼び出す
- 既存の `AnthropicApiBackend` は影響なし

### FR-4: BedrockBackend
- `AgentBackend` trait を実装
- MCP サーバー管理は既存と同じロジック
- コスト計算は Bedrock 用の単価

### FR-5: バックエンド切り替え
- `AGENT_BACKEND=bedrock` で Bedrock 使用
- `AWS_REGION` でリージョン指定（デフォルト: `us-east-1`）
- `BEDROCK_MODEL` でモデル ID 指定（デフォルト: `us.anthropic.claude-sonnet-4-20250514-v1:0`）

## 非機能要件

### NFR-1: 既存互換性
- `AnthropicApiBackend` は変更なしで動作し続ける
- `ClaudeCliBackend` も影響なし

### NFR-2: エラーハンドリング
- Bedrock スロットリング（429）でリトライ
- 認証エラーは明確なメッセージ

## 制約
- `types.rs` の既存型は変更最小限
- 依存追加: `aws-sdk-bedrockruntime`, `aws-config`
