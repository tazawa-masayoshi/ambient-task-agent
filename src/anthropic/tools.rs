use crate::anthropic::types::ToolDefinition;
use serde_json::json;

/// allowed_tools カンマ区切り文字列をパースして ToolDefinition のベクタを返す
pub fn build_tool_definitions(allowed_tools: &str) -> Vec<ToolDefinition> {
    allowed_tools
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(build_single_tool)
        .collect()
}

fn build_single_tool(name: &str) -> Option<ToolDefinition> {
    match name {
        "Read" => Some(read_tool()),
        "Write" => Some(write_tool()),
        "Edit" => Some(edit_tool()),
        "Bash" => Some(bash_tool()),
        "Glob" => Some(glob_tool()),
        "Grep" => Some(grep_tool()),
        other => {
            tracing::warn!("Unknown tool: {}, skipping", other);
            None
        }
    }
}

fn read_tool() -> ToolDefinition {
    ToolDefinition {
        name: "Read".to_string(),
        description: "Read a file from the filesystem. Returns the file contents with line numbers.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (0-based). Optional."
                },
                "limit": {
                    "type": "integer",
                    "description": "Number of lines to read. Optional."
                }
            },
            "required": ["file_path"]
        }),
    }
}

fn write_tool() -> ToolDefinition {
    ToolDefinition {
        name: "Write".to_string(),
        description: "Write content to a file. Creates the file and parent directories if they don't exist. Overwrites existing content.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["file_path", "content"]
        }),
    }
}

fn edit_tool() -> ToolDefinition {
    ToolDefinition {
        name: "Edit".to_string(),
        description: "Edit a file by replacing an exact string match with new content. The old_string must appear exactly once in the file.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact string to find and replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement string"
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        }),
    }
}

fn bash_tool() -> ToolDefinition {
    ToolDefinition {
        name: "Bash".to_string(),
        description: "Execute a bash command and return its output (stdout + stderr). Use for running tests, git commands, and other shell operations.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds. Optional, defaults to 120."
                }
            },
            "required": ["command"]
        }),
    }
}

fn glob_tool() -> ToolDefinition {
    ToolDefinition {
        name: "Glob".to_string(),
        description: "Find files matching a glob pattern. Returns matching file paths sorted by modification time.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files (e.g., '**/*.rs', 'src/**/*.ts')"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in. Optional, defaults to cwd."
                }
            },
            "required": ["pattern"]
        }),
    }
}

fn grep_tool() -> ToolDefinition {
    ToolDefinition {
        name: "Grep".to_string(),
        description: "Search file contents using a regex pattern. Returns matching lines with file paths and line numbers.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in. Optional, defaults to cwd."
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g., '*.rs'). Optional."
                }
            },
            "required": ["pattern"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_tool_definitions_full() {
        let tools = build_tool_definitions("Read,Write,Edit,Bash,Glob,Grep");
        assert_eq!(tools.len(), 6);
        assert_eq!(tools[0].name, "Read");
        assert_eq!(tools[5].name, "Grep");
    }

    #[test]
    fn test_build_tool_definitions_partial() {
        let tools = build_tool_definitions("Read,Bash");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "Read");
        assert_eq!(tools[1].name, "Bash");
    }

    #[test]
    fn test_build_tool_definitions_empty() {
        let tools = build_tool_definitions("");
        assert!(tools.is_empty());
    }

    #[test]
    fn test_build_tool_definitions_with_spaces() {
        let tools = build_tool_definitions("Read, Write, Edit");
        assert_eq!(tools.len(), 3);
    }

    #[test]
    fn test_build_tool_definitions_unknown_skipped() {
        let tools = build_tool_definitions("Read,Unknown,Bash");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "Read");
        assert_eq!(tools[1].name, "Bash");
    }

    #[test]
    fn test_tool_schema_has_required_fields() {
        let tools = build_tool_definitions("Read,Write,Edit,Bash,Glob,Grep");
        for tool in &tools {
            assert!(!tool.name.is_empty());
            assert!(!tool.description.is_empty());
            let schema = &tool.input_schema;
            assert_eq!(schema["type"], "object");
            assert!(schema["properties"].is_object());
            assert!(schema["required"].is_array());
        }
    }
}
