# ambient-task-agent

Asanaをバックエンドとしたアンビエントタスクエージェント。
Claude Codeのhook/skillをトリガーにタスク状態を自動管理し、
wez-sidebarなどのUIからタスク一覧を参照できるようにする。

## Development Guidelines

- Follow the user's instructions precisely
- Asana MCP (`asana@claude-plugins-official`) は認証済み
- 詳細は `spec/context.md` を参照
