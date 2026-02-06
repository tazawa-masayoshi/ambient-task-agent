use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::skills::registry::SkillRegistry;

const OPENAI_API_URL: &str = "https://api.openai.com/v1/chat/completions";
const DEFAULT_MODEL: &str = "gpt-4o";
const MAX_ITERATIONS: usize = 10;

/// Agent実行コンテキスト
pub struct AgentContext {
    pub current_time: String,
    pub additional_context: Option<String>,
}

/// Agentオーケストレーター
pub struct AgentOrchestrator {
    client: Client,
    api_key: String,
    model: String,
    system_prompt: String,
    registry: SkillRegistry,
}

// OpenAI API structures
#[derive(Debug, Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    tools: Vec<OpenAITool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct OpenAITool {
    #[serde(rename = "type")]
    tool_type: String,
    function: OpenAIFunction,
}

#[derive(Debug, Serialize)]
struct OpenAIFunction {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponse {
    choices: Vec<OpenAIChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    message: OpenAIMessage,
    finish_reason: String,
}

impl AgentOrchestrator {
    pub fn new(api_key: String, registry: SkillRegistry) -> Self {
        let system_prompt = r#"あなたはタスク管理エージェントです。
ユーザーのタスクを効率的に管理し、適切なアクションを取ります。

## 判断基準
- urgentタスクは即対応を検討
- 期限が近いものを優先
- カレンダーの空きを確認してからスケジュール
- 進捗があればSlackで報告

## 行動方針
1. まず現状を把握（タスク一覧、カレンダー確認）
2. 状況を分析して優先度を判断
3. 適切なアクションを実行（ステータス更新、Slack通知等）
4. 必要に応じて追加のアクションを検討

各ツールを適切に組み合わせて、自律的に判断・行動してください。
日本語で応答してください。"#
            .to_string();

        Self {
            client: Client::new(),
            api_key,
            model: DEFAULT_MODEL.to_string(),
            system_prompt,
            registry,
        }
    }

    #[allow(dead_code)]
    pub fn with_model(mut self, model: &str) -> Self {
        self.model = model.to_string();
        self
    }

    #[allow(dead_code)]
    pub fn with_system_prompt(mut self, prompt: &str) -> Self {
        self.system_prompt = prompt.to_string();
        self
    }

    /// エージェントを実行
    pub async fn run(&self, user_message: &str, context: &AgentContext) -> Result<String> {
        let context_info = format!(
            "現在時刻: {}\n{}",
            context.current_time,
            context.additional_context.as_deref().unwrap_or("")
        );

        let full_message = format!("{}\n\n{}", context_info, user_message);

        let mut messages = vec![
            OpenAIMessage {
                role: "system".to_string(),
                content: Some(self.system_prompt.clone()),
                tool_calls: None,
                tool_call_id: None,
            },
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(full_message),
                tool_calls: None,
                tool_call_id: None,
            },
        ];

        let mut final_response = String::new();
        let mut iteration = 0;

        loop {
            iteration += 1;
            if iteration > MAX_ITERATIONS {
                tracing::warn!("Max iterations reached");
                break;
            }

            let response = self.call_openai(&messages).await?;
            let choice = response.choices.first().context("No response from OpenAI")?;

            tracing::info!(
                "OpenAI response (iter {}): finish_reason={}",
                iteration,
                choice.finish_reason
            );

            // テキスト応答を保存
            if let Some(content) = &choice.message.content {
                if !content.is_empty() {
                    final_response = content.clone();
                }
            }

            // assistantメッセージを追加
            messages.push(choice.message.clone());

            // tool_callsがあれば実行
            if let Some(tool_calls) = &choice.message.tool_calls {
                if tool_calls.is_empty() {
                    break;
                }

                for tool_call in tool_calls {
                    let name = &tool_call.function.name;
                    let args: Value = serde_json::from_str(&tool_call.function.arguments)
                        .unwrap_or(Value::Object(serde_json::Map::new()));

                    tracing::info!("Executing skill: {}", name);
                    let result = self.registry.execute(name, args).await?;
                    let result_json = serde_json::to_string(&result)?;

                    // tool応答を追加
                    messages.push(OpenAIMessage {
                        role: "tool".to_string(),
                        content: Some(result_json),
                        tool_calls: None,
                        tool_call_id: Some(tool_call.id.clone()),
                    });
                }
            } else {
                // tool_callsがなければ終了
                break;
            }

            // finish_reasonがstopなら終了
            if choice.finish_reason == "stop" {
                break;
            }
        }

        Ok(final_response)
    }

    async fn call_openai(&self, messages: &[OpenAIMessage]) -> Result<OpenAIResponse> {
        let tools = self.build_openai_tools();

        let request = OpenAIRequest {
            model: self.model.clone(),
            messages: messages.to_vec(),
            tools,
            tool_choice: None,
        };

        let resp = self
            .client
            .post(OPENAI_API_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .context("OpenAI API request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI API error ({}): {}", status, body);
        }

        let response: OpenAIResponse = resp.json().await.context("Failed to parse OpenAI response")?;
        Ok(response)
    }

    fn build_openai_tools(&self) -> Vec<OpenAITool> {
        let claude_tools = self.registry.to_claude_tools();

        if let Value::Array(tools) = claude_tools {
            tools
                .into_iter()
                .filter_map(|t| {
                    let name = t.get("name")?.as_str()?.to_string();
                    let description = t.get("description")?.as_str()?.to_string();
                    let parameters = t.get("input_schema")?.clone();

                    Some(OpenAITool {
                        tool_type: "function".to_string(),
                        function: OpenAIFunction {
                            name,
                            description,
                            parameters,
                        },
                    })
                })
                .collect()
        } else {
            Vec::new()
        }
    }
}
