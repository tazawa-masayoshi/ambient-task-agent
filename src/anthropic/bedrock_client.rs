//! Bedrock Converse Stream クライアント
//!
//! AWS Bedrock の ConverseStream API を使い、`LlmClient` trait を実装する。

use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_bedrockruntime::types as br;
use aws_sdk_bedrockruntime::Client as BrClient;

use super::bedrock_convert;
use super::llm_client::{LlmClient, OnToolUseCallback};
use super::types::*;

const MAX_RETRIES: u32 = 3;

pub struct BedrockClient {
    client: BrClient,
    model_id: String,
}

impl BedrockClient {
    pub async fn new(region: &str, model_id: String) -> Result<Self> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_string()))
            .load()
            .await;
        let client = BrClient::new(&config);

        tracing::info!(
            "BedrockClient initialized: region={}, model={}",
            region,
            model_id
        );
        Ok(Self { client, model_id })
    }
}

#[async_trait]
impl LlmClient for BedrockClient {
    async fn send_streaming(
        &self,
        request: MessagesRequest,
        on_tool_use: Option<OnToolUseCallback>,
    ) -> Result<MessagesResponse> {
        let mut last_error = None;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = std::time::Duration::from_secs(2u64.pow(attempt));
                tracing::info!(
                    "Bedrock API retry attempt {}/{}, waiting {:.1}s",
                    attempt,
                    MAX_RETRIES,
                    delay.as_secs_f64()
                );
                tokio::time::sleep(delay).await;
            }

            match self.do_converse_stream(&request, &on_tool_use).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    let err_str = e.to_string();
                    let is_retryable = err_str.contains("ThrottlingException")
                        || err_str.contains("ServiceUnavailableException")
                        || err_str.contains("InternalServerException");

                    if is_retryable && attempt < MAX_RETRIES {
                        tracing::warn!("Bedrock API error (retryable): {}", err_str);
                        last_error = Some(err_str);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        anyhow::bail!(
            "Bedrock API: max retries exceeded. Last error: {}",
            last_error.unwrap_or_default()
        )
    }
}

impl BedrockClient {
    async fn do_converse_stream(
        &self,
        request: &MessagesRequest,
        on_tool_use: &Option<OnToolUseCallback>,
    ) -> Result<MessagesResponse> {
        // 1. リクエスト変換
        let br_messages = bedrock_convert::convert_messages(&request.messages);
        let br_system = request
            .system
            .as_ref()
            .map(|s| bedrock_convert::convert_system_blocks(s));
        let br_tool_config =
            bedrock_convert::convert_tools(request.tools.as_deref().unwrap_or(&[]), &request.tool_choice);
        let inference_config = br::InferenceConfiguration::builder()
            .max_tokens(request.max_tokens as i32)
            .build();

        // 2. ConverseStream 呼び出し
        let mut builder = self
            .client
            .converse_stream()
            .model_id(&self.model_id)
            .set_messages(Some(br_messages))
            .inference_config(inference_config);

        if let Some(system) = br_system {
            for block in system {
                builder = builder.system(block);
            }
        }
        if let Some(tool_config) = br_tool_config {
            builder = builder.tool_config(tool_config);
        }

        let output = builder
            .send()
            .await
            .context("Bedrock ConverseStream call failed")?;

        // 3. ストリームイベント処理 → MessagesResponse 組み立て
        let mut stream = output.stream;
        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        let mut stop_reason: Option<StopReason> = None;
        let mut usage = Usage::default();

        // 現在のコンテンツブロックの蓄積
        let mut current_text = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut current_json_parts = String::new();
        let mut current_block_type: Option<BlockType> = None;

        while let Some(event) = stream
            .recv()
            .await
            .context("Error reading Bedrock stream")?
        {
            match event {
                br::ConverseStreamOutput::ContentBlockStart(e) => {
                    if let Some(start) = e.start() {
                        match start {
                            br::ContentBlockStart::ToolUse(tool) => {
                                if let Some(ref cb) = on_tool_use {
                                    cb(tool.name());
                                }
                                current_tool_id = tool.tool_use_id().to_string();
                                current_tool_name = tool.name().to_string();
                                current_json_parts.clear();
                                current_block_type = Some(BlockType::ToolUse);
                            }
                            _ => {
                                // Text ブロック開始
                                current_text.clear();
                                current_block_type = Some(BlockType::Text);
                            }
                        }
                    }
                }
                br::ConverseStreamOutput::ContentBlockDelta(e) => {
                    if let Some(delta) = e.delta() {
                        match delta {
                            br::ContentBlockDelta::Text(text) => {
                                current_text.push_str(text);
                            }
                            br::ContentBlockDelta::ToolUse(d) => {
                                current_json_parts.push_str(d.input());
                            }
                            _ => {}
                        }
                    }
                }
                br::ConverseStreamOutput::ContentBlockStop(_) => {
                    match current_block_type.take() {
                        Some(BlockType::Text) => {
                            if !current_text.is_empty() {
                                content_blocks.push(ContentBlock::Text {
                                    text: std::mem::take(&mut current_text),
                                });
                            }
                        }
                        Some(BlockType::ToolUse) => {
                            let input: serde_json::Value =
                                serde_json::from_str(&current_json_parts).unwrap_or_else(|_| {
                                    serde_json::Value::Object(serde_json::Map::new())
                                });
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
                br::ConverseStreamOutput::MessageStop(e) => {
                    stop_reason = Some(bedrock_convert::map_stop_reason(e.stop_reason()));
                }
                br::ConverseStreamOutput::Metadata(e) => {
                    if let Some(u) = e.usage() {
                        usage = Usage {
                            input_tokens: u.input_tokens() as u64,
                            output_tokens: u.output_tokens() as u64,
                            cache_creation_input_tokens: u
                                .cache_write_input_tokens()
                                .map(|v| v as u64),
                            cache_read_input_tokens: u
                                .cache_read_input_tokens()
                                .map(|v| v as u64),
                        };
                    }
                }
                _ => {} // MessageStart, Unknown
            }
        }

        Ok(MessagesResponse {
            id: String::new(), // Bedrock Converse はメッセージ ID を返さない
            model: self.model_id.clone(),
            role: Role::Assistant,
            content: content_blocks,
            stop_reason,
            usage,
        })
    }
}

enum BlockType {
    Text,
    ToolUse,
}
