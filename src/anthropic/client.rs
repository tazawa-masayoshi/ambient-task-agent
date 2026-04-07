//! Anthropic API クライアント — claude-auth crate のラッパー

use anyhow::Result;

/// claude-auth crate の AnthropicClient をラップ
pub struct AnthropicClient {
    inner: claude_auth::AnthropicClient,
}

impl AnthropicClient {
    pub fn inner_client(&self) -> &claude_auth::AnthropicClient {
        &self.inner
    }

    pub fn new(api_key: String) -> Self {
        let model = std::env::var("ANTHROPIC_MODEL")
            .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string());
        Self {
            inner: claude_auth::AnthropicClient::with_api_key(api_key, model),
        }
    }

    pub fn from_env() -> Result<Self> {
        Ok(Self {
            inner: claude_auth::AnthropicClient::from_env()?,
        })
    }
}
