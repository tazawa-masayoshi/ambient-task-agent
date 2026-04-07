//! Bedrock Converse API クライアント

use anyhow::Result;
use aws_sdk_bedrockruntime::Client as BrClient;

pub struct BedrockClient {
    client: BrClient,
    model_id: String,
}

impl BedrockClient {
    pub fn raw_client(&self) -> &BrClient {
        &self.client
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub async fn new(region: &str, model_id: String) -> Result<Self> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_string()))
            .load()
            .await;
        let client = BrClient::new(&config);

        tracing::info!("BedrockClient initialized: region={}, model={}", region, model_id);
        Ok(Self { client, model_id })
    }
}
