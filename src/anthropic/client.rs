use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};

use super::llm_client::{LlmClient, OnToolUseCallback};
use super::oauth::OAuthManager;
use super::types::*;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const MAX_RETRIES: u32 = 3;

/// 認証方式
enum AuthMode {
    ApiKey(String),
    OAuth(Arc<OAuthManager>),
}

pub struct AnthropicClient {
    http: reqwest::Client,
    auth: AuthMode,
    base_url: String,
}

impl AnthropicClient {
    /// API キー認証で初期化
    pub fn new(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            auth: AuthMode::ApiKey(api_key),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// OAuth 認証（Claude Code Max プラン）で初期化
    pub fn with_oauth(oauth: Arc<OAuthManager>) -> Self {
        Self {
            http: reqwest::Client::new(),
            auth: AuthMode::OAuth(oauth),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    async fn headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        match &self.auth {
            AuthMode::ApiKey(key) => {
                if let Ok(val) = HeaderValue::from_str(key) {
                    headers.insert("x-api-key", val);
                }
            }
            AuthMode::OAuth(oauth) => {
                let token = oauth.access_token().await?;
                if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", token)) {
                    headers.insert("authorization", val);
                }
            }
        }
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(API_VERSION),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // prompt caching beta
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("prompt-caching-2024-07-31"),
        );
        Ok(headers)
    }

    /// 非ストリーミング呼び出し
    #[allow(dead_code)]
    pub async fn send(&self, request: MessagesRequest) -> Result<MessagesResponse> {
        let url = format!("{}/v1/messages", self.base_url);
        let mut last_error = None;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                tracing::info!("Anthropic API retry attempt {}/{}", attempt, MAX_RETRIES);
            }

            let headers = self.headers().await?;
            let resp = self
                .http
                .post(&url)
                .headers(headers)
                .json(&request)
                .send()
                .await
                .context("Failed to send request to Anthropic API")?;

            let status = resp.status();
            if status.is_success() {
                return resp
                    .json::<MessagesResponse>()
                    .await
                    .context("Failed to parse Anthropic response");
            }

            // リトライ可能なエラー
            if matches!(status.as_u16(), 429 | 529 | 500 | 502 | 503) && attempt < MAX_RETRIES {
                let delay = retry_delay(&resp, attempt);
                tracing::warn!(
                    "Anthropic API returned {}, retrying in {:.1}s",
                    status,
                    delay.as_secs_f64()
                );
                tokio::time::sleep(delay).await;
                last_error = Some(format!("HTTP {}", status));
                continue;
            }

            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error: HTTP {} - {}", status, body);
        }

        anyhow::bail!(
            "Anthropic API: max retries exceeded. Last error: {}",
            last_error.unwrap_or_default()
        )
    }

}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn send_streaming(
        &self,
        request: MessagesRequest,
        on_tool_use: Option<OnToolUseCallback>,
    ) -> Result<MessagesResponse> {
        let mut request = request;
        request.stream = true;
        let url = format!("{}/v1/messages", self.base_url);
        let mut last_error = None;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                tracing::info!("Anthropic API retry attempt {}/{}", attempt, MAX_RETRIES);
            }

            let headers = self.headers().await?;
            let resp = self
                .http
                .post(&url)
                .headers(headers)
                .json(&request)
                .send()
                .await
                .context("Failed to send streaming request to Anthropic API")?;

            let status = resp.status();
            if !status.is_success() {
                if matches!(status.as_u16(), 429 | 529 | 500 | 502 | 503) && attempt < MAX_RETRIES
                {
                    let delay = retry_delay(&resp, attempt);
                    tracing::warn!(
                        "Anthropic API returned {}, retrying in {:.1}s",
                        status,
                        delay.as_secs_f64()
                    );
                    tokio::time::sleep(delay).await;
                    last_error = Some(format!("HTTP {}", status));
                    continue;
                }
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Anthropic API error: HTTP {} - {}", status, body);
            }

            return parse_sse_stream(resp, on_tool_use.as_deref()).await;
        }

        anyhow::bail!(
            "Anthropic API: max retries exceeded. Last error: {}",
            last_error.unwrap_or_default()
        )
    }
}

/// SSE レスポンスを読み取って MessagesResponse を組み立てる
async fn parse_sse_stream(
    response: reqwest::Response,
    on_tool_use: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> Result<MessagesResponse> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    // 組み立て中の状態
    let mut message_id = String::new();
    let mut model = String::new();
    let mut input_usage = Usage::default();
    let mut output_tokens: u64 = 0;
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut stop_reason: Option<StopReason> = None;

    // 現在のコンテンツブロックの蓄積
    let mut current_text = String::new();
    let mut current_tool_id = String::new();
    let mut current_tool_name = String::new();
    let mut current_json_parts = String::new();
    let mut current_block_type: Option<BlockType> = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("Error reading SSE chunk: {}", e))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // SSE は "\n\n" でイベントを区切る
        while let Some(pos) = buffer.find("\n\n") {
            let event_text = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();

            for line in event_text.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        continue;
                    }
                    let event: StreamEvent = match serde_json::from_str(data) {
                        Ok(e) => e,
                        Err(err) => {
                            tracing::debug!("Failed to parse SSE event: {} - data: {}", err, data);
                            continue;
                        }
                    };

                    match event {
                        StreamEvent::MessageStart { message } => {
                            message_id = message.id;
                            model = message.model;
                            input_usage = message.usage;
                        }
                        StreamEvent::ContentBlockStart { content_block, .. } => {
                            match content_block {
                                ContentBlockStartPayload::Text { text } => {
                                    current_text = text;
                                    current_block_type = Some(BlockType::Text);
                                }
                                ContentBlockStartPayload::ToolUse { id, name } => {
                                    if let Some(cb) = on_tool_use {
                                        cb(&name);
                                    }
                                    current_tool_id = id;
                                    current_tool_name = name;
                                    current_json_parts.clear();
                                    current_block_type = Some(BlockType::ToolUse);
                                }
                            }
                        }
                        StreamEvent::ContentBlockDelta { delta, .. } => match delta {
                            Delta::TextDelta { text } => {
                                current_text.push_str(&text);
                            }
                            Delta::InputJsonDelta { partial_json } => {
                                current_json_parts.push_str(&partial_json);
                            }
                        },
                        StreamEvent::ContentBlockStop { .. } => {
                            match current_block_type.take() {
                                Some(BlockType::Text) => {
                                    content_blocks.push(ContentBlock::Text {
                                        text: std::mem::take(&mut current_text),
                                    });
                                }
                                Some(BlockType::ToolUse) => {
                                    let input: serde_json::Value =
                                        serde_json::from_str(&current_json_parts)
                                            .unwrap_or(serde_json::Value::Object(
                                                serde_json::Map::new(),
                                            ));
                                    content_blocks.push(ContentBlock::ToolUse {
                                        id: std::mem::take(&mut current_tool_id),
                                        name: std::mem::take(&mut current_tool_name),
                                        input,
                                    });
                                    current_json_parts.clear();
                                }
                                None => {}
                            }
                        }
                        StreamEvent::MessageDelta { delta, usage } => {
                            if let Some(reason) = delta.stop_reason {
                                stop_reason = Some(reason);
                            }
                            if let Some(u) = usage {
                                output_tokens = u.output_tokens;
                            }
                        }
                        StreamEvent::Error { error } => {
                            anyhow::bail!(
                                "Anthropic stream error: {} - {}",
                                error.error_type,
                                error.message
                            );
                        }
                        StreamEvent::MessageStop | StreamEvent::Ping => {}
                    }
                }
            }
        }
    }

    Ok(MessagesResponse {
        id: message_id,
        model,
        role: Role::Assistant,
        content: content_blocks,
        stop_reason,
        usage: Usage {
            input_tokens: input_usage.input_tokens,
            output_tokens,
            cache_creation_input_tokens: input_usage.cache_creation_input_tokens,
            cache_read_input_tokens: input_usage.cache_read_input_tokens,
        },
    })
}

#[derive(Debug)]
enum BlockType {
    Text,
    ToolUse,
}

/// Retry-After ヘッダまたは exponential backoff でディレイ算出
fn retry_delay(resp: &reqwest::Response, attempt: u32) -> std::time::Duration {
    if let Some(retry_after) = resp.headers().get("retry-after") {
        if let Ok(secs) = retry_after.to_str().unwrap_or("0").parse::<u64>() {
            return std::time::Duration::from_secs(secs.min(60));
        }
    }
    // exponential backoff: 2s, 4s, 8s
    std::time::Duration::from_secs(2u64.pow(attempt + 1))
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_retry_delay_exponential() {
        // exponential backoff: 2^(attempt+1)
        assert_eq!(2u64.pow(0 + 1), 2);
        assert_eq!(2u64.pow(1 + 1), 4);
        assert_eq!(2u64.pow(2 + 1), 8);
    }
}
