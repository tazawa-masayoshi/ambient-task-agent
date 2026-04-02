//! MCP (Model Context Protocol) クライアント — JSON-RPC 2.0 over stdio
//!
//! MCP サーバープロセスを起動し、initialize → tools/list → tools/call の
//! ライフサイクルを管理する。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// read_line の最大バイト数（10 MiB）— OOM 防止
const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;

/// 危険な環境変数キー（ライブラリインジェクション）
const DANGEROUS_ENV_KEYS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
];

use super::types::ToolDefinition;

/// MCP の名前（サーバー名・ツール名）として有効か検証。
/// 英数字・ハイフン・アンダースコアのみ許可、`__` は禁止（区切り文字と衝突）。
fn is_valid_mcp_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("__")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

// ============================================================================
// MCP サーバー設定（repos.toml から読む）
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl McpServerConfig {
    /// env と args 内の `${VAR_NAME}` を実際の環境変数値で展開する
    #[allow(dead_code)]
    pub fn resolve_env(&mut self) {
        for value in self.env.values_mut() {
            *value = resolve_template(value);
        }
        for arg in &mut self.args {
            *arg = resolve_template(arg);
        }
    }
}

fn resolve_template(s: &str) -> String {
    let mut result = s.to_string();
    while let Some(start) = result.find("${") {
        if let Some(end) = result[start..].find('}') {
            let var_name = &result[start + 2..start + end];
            let var_value = std::env::var(var_name).unwrap_or_default();
            result = format!("{}{}{}", &result[..start], var_value, &result[start + end + 1..]);
        } else {
            break;
        }
    }
    result
}

// ============================================================================
// JSON-RPC 2.0
// ============================================================================

#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcNotification {
    jsonrpc: &'static str,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    id: u64,
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

// ============================================================================
// MCP プロトコル型
// ============================================================================

#[derive(Debug, Deserialize)]
struct McpToolInfo {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    input_schema: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ToolsListResult {
    tools: Vec<McpToolInfo>,
}

#[derive(Debug, Deserialize)]
struct ToolCallResult {
    content: Vec<ToolCallContent>,
    #[serde(default)]
    is_error: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ToolCallContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(other)]
    Other,
}

// ============================================================================
// McpClient — 1 サーバープロセスに対応
// ============================================================================

pub struct McpClient {
    server_name: String,
    child: Mutex<Child>,
    stdin: Mutex<tokio::process::ChildStdin>,
    reader: Mutex<BufReader<tokio::process::ChildStdout>>,
    next_id: AtomicU64,
    /// JSON-RPC call をシリアライズ（並列呼び出しでのレスポンス消失防止）
    call_lock: Mutex<()>,
    /// shutdown 済みフラグ（Drop での二重 kill 防止）
    shutdown_called: AtomicBool,
}

impl McpClient {
    /// MCP サーバーを起動して initialize ハンドシェイクを完了する
    pub async fn start(config: &McpServerConfig) -> Result<Self> {
        // C-1: server_name バリデーション
        if !is_valid_mcp_name(&config.name) {
            anyhow::bail!(
                "Invalid MCP server name '{}': must be alphanumeric/hyphen/underscore, no '__'",
                config.name
            );
        }

        tracing::info!("Starting MCP server: {} ({})", config.name, config.command);

        // W-1: 危険な環境変数キーの警告
        for key in config.env.keys() {
            if DANGEROUS_ENV_KEYS.iter().any(|&k| k.eq_ignore_ascii_case(key)) {
                tracing::warn!(
                    "MCP server '{}': dangerous env var '{}' set — potential library injection risk",
                    config.name,
                    key
                );
            }
        }

        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::inherit()); // W-2: 診断情報を保持

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server: {}", config.command))?;

        let stdin = child.stdin.take().expect("stdin should be piped");
        let stdout = child.stdout.take().expect("stdout should be piped");
        let reader = BufReader::new(stdout);

        let client = Self {
            server_name: config.name.clone(),
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            reader: Mutex::new(reader),
            next_id: AtomicU64::new(1),
            call_lock: Mutex::new(()),
            shutdown_called: AtomicBool::new(false),
        };

        // initialize
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "ambient-task-agent",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        let _init_result = client
            .call("initialize", Some(init_params))
            .await
            .context("MCP initialize failed")?;

        // initialized notification
        client.notify("notifications/initialized", None).await?;

        tracing::info!("MCP server '{}' initialized", config.name);
        Ok(client)
    }

    /// tools/list を呼んで ToolDefinition のリストを返す。
    /// ツール名は `mcp__<server_name>__<tool_name>` に名前空間化。
    pub async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let result = self.call("tools/list", None).await?;
        let list: ToolsListResult =
            serde_json::from_value(result).context("Failed to parse tools/list result")?;

        let tools = list
            .tools
            .into_iter()
            .filter(|t| {
                if is_valid_mcp_name(&t.name) {
                    true
                } else {
                    tracing::warn!(
                        "MCP server '{}': skipping tool with invalid name '{}'",
                        self.server_name,
                        t.name.chars().take(100).collect::<String>()
                    );
                    false
                }
            })
            .map(|t| {
                let namespaced = format!("mcp__{}__{}", self.server_name, t.name);
                ToolDefinition {
                    name: namespaced,
                    description: t.description.unwrap_or_default(),
                    input_schema: t.input_schema.unwrap_or_else(|| {
                        serde_json::json!({
                            "type": "object",
                            "properties": {}
                        })
                    }),
                }
            })
            .collect();

        Ok(tools)
    }

    /// tools/call を呼んでテキスト結果を返す。
    /// `tool_name` は名前空間なしの元のツール名。
    /// 戻り値: (output_text, is_error)
    pub async fn call_tool(&self, tool_name: &str, arguments: &Value) -> Result<(String, bool)> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments
        });
        let result = self.call("tools/call", Some(params)).await?;
        let call_result: ToolCallResult =
            serde_json::from_value(result).context("Failed to parse tools/call result")?;

        let is_error = call_result.is_error.unwrap_or(false);
        let text: String = call_result
            .content
            .into_iter()
            .filter_map(|c| match c {
                ToolCallContent::Text { text } => Some(text),
                ToolCallContent::Other => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok((text, is_error))
    }

    /// プロセスを停止する
    pub async fn shutdown(&self) {
        self.shutdown_called.store(true, Ordering::Relaxed);
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        tracing::info!("MCP server '{}' shut down", self.server_name);
    }

    // ========================================================================
    // JSON-RPC I/O
    // ========================================================================

    async fn call(&self, method: &str, params: Option<Value>) -> Result<Value> {
        // C1: 同一サーバーへの並列呼び出しでレスポンス消失を防ぐため、
        // write + read を一括でシリアライズする
        let _guard = self.call_lock.lock().await;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        // write
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }

        // read response — skip notifications (no id field matching)
        let response = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let mut reader = self.reader.lock().await;
            loop {
                let mut buf = String::new();
                let n = reader.read_line(&mut buf).await?;
                if n == 0 {
                    anyhow::bail!("MCP server '{}' closed stdout", self.server_name);
                }
                // C-3: バッファ上限チェック（OOM 防止）
                if buf.len() > MAX_LINE_BYTES {
                    anyhow::bail!(
                        "MCP server '{}': response line exceeds {} bytes",
                        self.server_name,
                        MAX_LINE_BYTES
                    );
                }
                let buf = buf.trim();
                if buf.is_empty() {
                    continue;
                }

                // notifications（id なし）はスキップ
                if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(buf) {
                    if resp.id == id {
                        return Ok(resp);
                    }
                }
                // id が違う or パースできない行はスキップ
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("MCP call '{}' timed out (30s)", method))??;

        if let Some(err) = response.error {
            anyhow::bail!("MCP {}: {}", method, err);
        }

        response.result.ok_or_else(|| {
            anyhow::anyhow!("MCP {}: response has neither result nor error", method)
        })
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
        };

        let mut line = serde_json::to_string(&notification)?;
        line.push('\n');

        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // shutdown() 済みなら二重 kill しない
        if self.shutdown_called.load(Ordering::Relaxed) {
            return;
        }
        // best-effort synchronous kill
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
    }
}

// ============================================================================
// McpManager — 複数 MCP サーバーの管理 + ツールルーティング
// ============================================================================

/// MCP ツール名から (server_name, original_tool_name) を抽出
pub fn parse_mcp_tool_name(namespaced: &str) -> Option<(&str, &str)> {
    let rest = namespaced.strip_prefix("mcp__")?;
    let sep = rest.find("__")?;
    let server = &rest[..sep];
    let tool = &rest[sep + 2..];
    if tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

pub struct McpManager {
    clients: HashMap<String, McpClient>,
}

impl McpManager {
    /// 設定リストから全 MCP サーバーを並列起動する
    pub async fn start(configs: &[McpServerConfig]) -> Result<Self> {
        let futures: Vec<_> = configs
            .iter()
            .map(|config| async move {
                match McpClient::start(config).await {
                    Ok(client) => Some((config.name.clone(), client)),
                    Err(e) => {
                        tracing::error!("Failed to start MCP server '{}': {}", config.name, e);
                        None
                    }
                }
            })
            .collect();

        let results = join_all(futures).await;
        let clients: HashMap<_, _> = results.into_iter().flatten().collect();

        Ok(Self { clients })
    }

    /// 全 MCP サーバーからツール定義を収集（名前順でソート済み）
    pub async fn list_all_tools(&self) -> Vec<ToolDefinition> {
        let mut all_tools = Vec::new();

        for (name, client) in &self.clients {
            match client.list_tools().await {
                Ok(tools) => {
                    tracing::info!("MCP server '{}': {} tools available", name, tools.len());
                    all_tools.extend(tools);
                }
                Err(e) => {
                    tracing::error!("Failed to list tools from MCP server '{}': {}", name, e);
                }
            }
        }

        // W1: ツール順序を安定化（プロンプトキャッシュヒット率向上）
        all_tools.sort_by(|a, b| a.name.cmp(&b.name));
        all_tools
    }

    /// MCP ツールを呼び出す。名前空間付きツール名から適切なサーバーにルーティング。
    /// 戻り値: (output_text, is_error)
    pub async fn call_tool(&self, namespaced_name: &str, arguments: &Value) -> Result<(String, bool)> {
        let (server, tool) = parse_mcp_tool_name(namespaced_name)
            .ok_or_else(|| anyhow::anyhow!("Invalid MCP tool name: {}", namespaced_name))?;

        let client = self
            .clients
            .get(server)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not found", server))?;

        client.call_tool(tool, arguments).await
    }

    /// 全サーバーをシャットダウン
    pub async fn shutdown(&self) {
        for client in self.clients.values() {
            client.shutdown().await;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mcp_tool_name() {
        assert_eq!(
            parse_mcp_tool_name("mcp__kintone__get_records"),
            Some(("kintone", "get_records"))
        );
        assert_eq!(
            parse_mcp_tool_name("mcp__google_calendar__list_events"),
            Some(("google_calendar", "list_events"))
        );
        // builtin ツールはマッチしない
        assert_eq!(parse_mcp_tool_name("Read"), None);
        assert_eq!(parse_mcp_tool_name("Bash"), None);
        // 不正な形式
        assert_eq!(parse_mcp_tool_name("mcp__"), None);
        assert_eq!(parse_mcp_tool_name("mcp__server__"), None);
        assert_eq!(parse_mcp_tool_name("mcp__server"), None);
    }

    #[test]
    fn test_is_valid_mcp_name() {
        assert!(is_valid_mcp_name("kintone"));
        assert!(is_valid_mcp_name("google-calendar"));
        assert!(is_valid_mcp_name("my_server"));
        assert!(is_valid_mcp_name("server123"));
        // 無効なケース
        assert!(!is_valid_mcp_name(""));
        assert!(!is_valid_mcp_name("my__server")); // __ は区切りと衝突
        assert!(!is_valid_mcp_name("my server")); // スペース
        assert!(!is_valid_mcp_name("server\n")); // 制御文字
        assert!(!is_valid_mcp_name("server/path")); // スラッシュ
    }

    #[test]
    fn test_mcp_server_config_deserialize() {
        let toml_str = r#"
            name = "kintone"
            command = "npx"
            args = ["-y", "@anthropic/mcp-kintone"]
            [env]
            KINTONE_BASE_URL = "https://example.cybozu.com"
        "#;
        let config: McpServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.name, "kintone");
        assert_eq!(config.command, "npx");
        assert_eq!(config.args, vec!["-y", "@anthropic/mcp-kintone"]);
        assert_eq!(
            config.env.get("KINTONE_BASE_URL").unwrap(),
            "https://example.cybozu.com"
        );
    }

    #[test]
    fn test_mcp_server_config_minimal() {
        let toml_str = r#"
            name = "test"
            command = "echo"
        "#;
        let config: McpServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.name, "test");
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
    }

    #[test]
    fn test_json_rpc_request_serialize() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "tools/list".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"method\":\"tools/list\""));
        // params が None なら出力されない
        assert!(!json.contains("params"));
    }

    #[test]
    fn test_json_rpc_response_parse() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, 1);
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_json_rpc_error_parse() {
        let json = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.error.is_some());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
    }

    #[test]
    fn test_tools_list_result_parse() {
        let json = r#"{
            "tools": [
                {
                    "name": "get_records",
                    "description": "Get records from kintone",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "app_id": { "type": "integer" }
                        },
                        "required": ["app_id"]
                    }
                }
            ]
        }"#;
        let result: ToolsListResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "get_records");
    }

    #[test]
    fn test_tool_call_result_parse() {
        let json = r#"{
            "content": [
                { "type": "text", "text": "Found 3 records" }
            ]
        }"#;
        let result: ToolCallResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ToolCallContent::Text { text } => assert_eq!(text, "Found 3 records"),
            _ => panic!("Expected Text content"),
        }
    }
}
