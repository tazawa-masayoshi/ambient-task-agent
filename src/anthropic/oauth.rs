//! Claude Code OAuth 認証 — ~/.claude/.credentials.json のトークン管理

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
/// リフレッシュの余裕（5分前）
const REFRESH_MARGIN_MS: u64 = 300_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub subscription_type: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialsFile {
    claude_ai_oauth: OAuthCredentials,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}

/// OAuth トークンマネージャー
pub struct OAuthManager {
    credentials_path: PathBuf,
    http: reqwest::Client,
    current: Arc<Mutex<OAuthCredentials>>,
}

impl OAuthManager {
    /// ~/.claude/.credentials.json からトークンを読み込んで初期化
    pub fn load() -> Result<Self> {
        let home = dirs::home_dir().context("Cannot determine home directory")?;
        let path = home.join(".claude/.credentials.json");
        let content =
            std::fs::read_to_string(&path).context("Failed to read ~/.claude/.credentials.json")?;
        let file: CredentialsFile =
            serde_json::from_str(&content).context("Failed to parse credentials JSON")?;

        let creds = file.claude_ai_oauth;
        tracing::info!(
            "OAuth credentials loaded: subscription={}, expires_at={}",
            creds.subscription_type.as_deref().unwrap_or("unknown"),
            creds.expires_at
        );

        Ok(Self {
            credentials_path: path,
            http: reqwest::Client::new(),
            current: Arc::new(Mutex::new(creds)),
        })
    }

    /// 有効な access_token を返す。期限切れならリフレッシュする。
    pub async fn access_token(&self) -> Result<String> {
        let mut creds = self.current.lock().await;

        if Self::is_expired(&creds) {
            tracing::info!("OAuth token expired, refreshing...");
            let new_creds = self.refresh(&creds).await?;
            *creds = new_creds;
            // ファイルに書き戻し
            if let Err(e) = self.save_credentials(&creds) {
                tracing::warn!("Failed to save refreshed credentials: {}", e);
            }
        }

        Ok(creds.access_token.clone())
    }

    fn is_expired(creds: &OAuthCredentials) -> bool {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        now_ms + REFRESH_MARGIN_MS >= creds.expires_at
    }

    async fn refresh(&self, creds: &OAuthCredentials) -> Result<OAuthCredentials> {
        let resp = self
            .http
            .post(TOKEN_URL)
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": creds.refresh_token,
                "client_id": CLIENT_ID,
                "scope": SCOPES,
            }))
            .send()
            .await
            .context("OAuth token refresh request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OAuth token refresh failed: HTTP {} - {}", status, body);
        }

        let token_resp: TokenResponse = resp
            .json()
            .await
            .context("Failed to parse token refresh response")?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let new_creds = OAuthCredentials {
            access_token: token_resp.access_token,
            refresh_token: token_resp
                .refresh_token
                .unwrap_or_else(|| creds.refresh_token.clone()),
            expires_at: now_ms + (token_resp.expires_in * 1000),
            scopes: creds.scopes.clone(),
            subscription_type: creds.subscription_type.clone(),
        };

        tracing::info!(
            "OAuth token refreshed, new expiry: {}",
            new_creds.expires_at
        );
        Ok(new_creds)
    }

    fn save_credentials(&self, creds: &OAuthCredentials) -> Result<()> {
        let file = CredentialsFile {
            claude_ai_oauth: creds.clone(),
        };
        let json = serde_json::to_string_pretty(&file)?;
        std::fs::write(&self.credentials_path, json)?;
        Ok(())
    }
}
