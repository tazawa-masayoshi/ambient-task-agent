use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::config::AsanaConfig;

const ASANA_API_URL: &str = "https://app.asana.com/api/1.0";

pub struct AsanaClient {
    config: AsanaConfig,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct AsanaResponse<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
pub struct AsanaTask {
    pub gid: String,
    pub name: String,
    pub assignee: Option<AsanaUser>,
    pub due_on: Option<String>,
    pub completed: bool,
    pub notes: Option<String>,
    pub memberships: Option<Vec<AsanaMembership>>,
}

#[derive(Debug, Deserialize)]
pub struct AsanaUser {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct AsanaMembership {
    pub section: Option<AsanaSection>,
    pub project: Option<AsanaProject>,
}

#[derive(Debug, Deserialize)]
pub struct AsanaProject {
    pub gid: String,
}

#[derive(Debug, Deserialize)]
pub struct AsanaSection {
    pub name: String,
}

impl AsanaClient {
    pub fn new(config: AsanaConfig) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self { config, client }
    }

    /// プロジェクトのタスク一覧を取得
    pub async fn fetch_project_tasks(&self) -> Result<Vec<AsanaTask>> {
        let url = format!("{}/tasks", ASANA_API_URL);

        let opt_fields = "name,due_on,assignee.name,completed,notes,memberships.section.name";

        let mut all_tasks = Vec::new();
        let mut offset: Option<String> = None;

        loop {
            let mut req = self
                .client
                .get(&url)
                .header("Authorization", format!("Bearer {}", self.config.pat))
                .query(&[
                    ("project", self.config.project_id.as_str()),
                    ("opt_fields", opt_fields),
                    ("limit", "100"),
                ]);

            if let Some(ref off) = offset {
                req = req.query(&[("offset", off.as_str())]);
            }

            let resp = req.send().await.context("Asana API request failed")?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Asana API error ({}): {}", status, body);
            }

            let body: serde_json::Value = resp.json().await.context("Failed to parse Asana response")?;

            let tasks: Vec<AsanaTask> = serde_json::from_value(
                body.get("data").context("No data field")?.clone()
            )?;

            all_tasks.extend(tasks);

            // ページネーション
            if let Some(next_page) = body.get("next_page") {
                if next_page.is_null() {
                    break;
                }
                offset = next_page.get("offset").and_then(|v| v.as_str()).map(|s| s.to_string());
                if offset.is_none() {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(all_tasks)
    }

    /// タスクにコメントを投稿
    pub async fn post_comment(&self, task_gid: &str, text: &str) -> Result<()> {
        let url = format!("{}/tasks/{}/stories", ASANA_API_URL, task_gid);

        let body = serde_json::json!({
            "data": {
                "text": text
            }
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.pat))
            .json(&body)
            .send()
            .await
            .context("Asana API request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Asana API error ({}): {}", status, body);
        }

        Ok(())
    }

    /// 個別タスクの詳細を取得
    pub async fn get_task(&self, task_gid: &str) -> Result<AsanaTask> {
        let url = format!("{}/tasks/{}", ASANA_API_URL, task_gid);
        let opt_fields = "name,due_on,assignee.name,completed,notes,memberships.section.name,memberships.project.gid";

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.pat))
            .query(&[("opt_fields", opt_fields)])
            .send()
            .await
            .context("Asana API request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Asana API error ({}): {}", status, body);
        }

        let body: AsanaResponse<AsanaTask> = resp.json().await.context("Failed to parse Asana response")?;
        Ok(body.data)
    }

}
