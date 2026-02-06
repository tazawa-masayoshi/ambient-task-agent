pub mod registry;
pub mod slack_skills;
pub mod mock_skills;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Skill実行結果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillResult {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl SkillResult {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
            data: None,
        }
    }

    pub fn ok_with_data(message: impl Into<String>, data: Value) -> Self {
        Self {
            success: true,
            message: message.into(),
            data: Some(data),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
            data: None,
        }
    }
}

/// Skill定義（Claude Tool Use用）
#[derive(Debug, Clone, Serialize)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Skillトレイト
#[async_trait]
pub trait Skill: Send + Sync {
    /// Skill名
    fn name(&self) -> &str;

    /// 説明（LLMが使用タイミングを判断する材料）
    fn description(&self) -> &str;

    /// パラメータのJSON Schema
    fn parameters_schema(&self) -> Value;

    /// 実行
    async fn execute(&self, params: Value) -> Result<SkillResult>;

    /// Claude Tool Use形式の定義を取得
    fn to_tool_definition(&self) -> Value {
        serde_json::json!({
            "name": self.name(),
            "description": self.description(),
            "input_schema": self.parameters_schema()
        })
    }
}
