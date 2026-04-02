use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use super::types::{MessagesRequest, MessagesResponse};

/// ツール呼び出し開始時のコールバック型
pub type OnToolUseCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// LLM バックエンドの共通インターフェース。
/// Anthropic API 直叩きと Bedrock Converse API の両方が実装する。
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// ストリーミング呼び出し。全イベントを組み立てて MessagesResponse を返す。
    async fn send_streaming(
        &self,
        request: MessagesRequest,
        on_tool_use: Option<OnToolUseCallback>,
    ) -> Result<MessagesResponse>;
}
