use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use super::{Skill, SkillResult};

/// Skill登録・実行を管理するレジストリ
pub struct SkillRegistry {
    skills: HashMap<String, Arc<dyn Skill>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
        }
    }

    /// Skillを登録
    pub fn register(&mut self, skill: impl Skill + 'static) {
        let name = skill.name().to_string();
        self.skills.insert(name, Arc::new(skill));
    }

    /// 登録済みSkill一覧を取得
    pub fn list(&self) -> Vec<&str> {
        self.skills.keys().map(|s| s.as_str()).collect()
    }

    /// Claude Tool Use形式のtools配列を生成
    pub fn to_claude_tools(&self) -> Value {
        let tools: Vec<Value> = self
            .skills
            .values()
            .map(|s| s.to_tool_definition())
            .collect();
        serde_json::json!(tools)
    }

    /// Skillを名前で実行
    pub async fn execute(&self, name: &str, params: Value) -> Result<SkillResult> {
        let skill = self
            .skills
            .get(name)
            .context(format!("Skill '{}' not found", name))?;

        skill.execute(params).await
    }

    /// Skillが存在するか確認
    pub fn has(&self, name: &str) -> bool {
        self.skills.contains_key(name)
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}
