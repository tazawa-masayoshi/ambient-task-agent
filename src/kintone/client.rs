use anyhow::{Context, Result};
use reqwest::Client;

use crate::config::KintoneConfig;
use crate::kintone::models::{
    KintoneAddResponse, KintoneRawRecord, KintoneRecordsResponse, TaskRecord,
};
use crate::kintone::query::urlencode;

#[derive(Debug, Clone)]
pub struct KintoneClient {
    config: KintoneConfig,
    client: Client,
}

impl KintoneClient {
    pub fn new(config: KintoneConfig) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build reqwest client");
        Self { config, client }
    }

    /// Fetch records matching a kintone query string.
    pub async fn fetch_records(&self, query: &str) -> Result<Vec<TaskRecord>> {
        let url = format!(
            "https://{}/k/v1/records.json?app={}&query={}",
            self.config.domain,
            self.config.app_id,
            urlencode(query)
        );

        let resp = self
            .client
            .get(&url)
            .header("X-Cybozu-API-Token", &self.config.api_token)
            .send()
            .await
            .context("kintone GET request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("kintone API error ({}): {}", status, body);
        }

        let data: KintoneRecordsResponse = resp.json().await.context("failed to parse response")?;
        Ok(data.records.iter().map(TaskRecord::from_raw).collect())
    }

    /// Update fields on a record.
    pub async fn update_record(
        &self,
        record_id: &str,
        fields: serde_json::Value,
    ) -> Result<()> {
        let url = format!("https://{}/k/v1/record.json", self.config.domain);

        let body = serde_json::json!({
            "app": self.config.app_id,
            "id": record_id,
            "record": fields,
        });

        let resp = self
            .client
            .put(&url)
            .header("X-Cybozu-API-Token", &self.config.api_token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("kintone PUT request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("kintone update error ({}): {}", status, body);
        }

        Ok(())
    }

    /// Update the status of a record. Sets completed_at when status is "done".
    pub async fn update_status(&self, record_id: &str, new_status: &str) -> Result<()> {
        let mut fields = serde_json::json!({
            "status": { "value": new_status }
        });

        if new_status == "done" {
            fields["completed_at"] = serde_json::json!({
                "value": chrono::Utc::now().to_rfc3339()
            });
        }

        self.update_record(record_id, fields).await
    }

    /// Add a new record. Returns the new record ID.
    pub async fn add_record(&self, fields: serde_json::Value) -> Result<String> {
        let url = format!("https://{}/k/v1/record.json", self.config.domain);

        let body = serde_json::json!({
            "app": self.config.app_id,
            "record": fields,
        });

        let resp = self
            .client
            .post(&url)
            .header("X-Cybozu-API-Token", &self.config.api_token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("kintone POST request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("kintone add error ({}): {}", status, body);
        }

        let data: KintoneAddResponse = resp.json().await.context("failed to parse add response")?;
        Ok(data.id)
    }
}
