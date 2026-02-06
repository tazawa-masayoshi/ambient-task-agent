use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::{Skill, SkillResult};

/// モック: タスク一覧取得
pub struct MockFetchTasks;

#[async_trait]
impl Skill for MockFetchTasks {
    fn name(&self) -> &str {
        "fetch_tasks"
    }

    fn description(&self) -> &str {
        "現在アクティブなタスク一覧を取得する。タスクの状況把握、優先度確認に使用。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _params: Value) -> Result<SkillResult> {
        // モックデータ
        let tasks = serde_json::json!([
            {
                "id": "101",
                "title": "提案書作成",
                "status": "todo",
                "priority": "urgent",
                "deadline": "2026-02-05"
            },
            {
                "id": "102",
                "title": "週次レポート",
                "status": "in_progress",
                "priority": "this_week",
                "deadline": "2026-02-07"
            },
            {
                "id": "103",
                "title": "ドキュメント整理",
                "status": "todo",
                "priority": "someday",
                "deadline": null
            }
        ]);

        Ok(SkillResult::ok_with_data(
            "3件のアクティブなタスクを取得しました",
            tasks,
        ))
    }
}

/// モック: タスクステータス更新
pub struct MockUpdateTaskStatus;

#[derive(Debug, Deserialize)]
struct UpdateTaskStatusParams {
    task_id: String,
    status: String,
}

#[async_trait]
impl Skill for MockUpdateTaskStatus {
    fn name(&self) -> &str {
        "update_task_status"
    }

    fn description(&self) -> &str {
        "タスクのステータスを更新する。todo/in_progress/doneに変更可能。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "タスクID"
                },
                "status": {
                    "type": "string",
                    "enum": ["todo", "in_progress", "done"],
                    "description": "新しいステータス"
                }
            },
            "required": ["task_id", "status"]
        })
    }

    async fn execute(&self, params: Value) -> Result<SkillResult> {
        let p: UpdateTaskStatusParams = serde_json::from_value(params)?;
        println!("[MOCK] タスク {} のステータスを {} に更新", p.task_id, p.status);

        Ok(SkillResult::ok(format!(
            "タスク {} のステータスを {} に更新しました",
            p.task_id, p.status
        )))
    }
}

/// モック: タスク追加
pub struct MockAddTask;

#[derive(Debug, Deserialize)]
struct AddTaskParams {
    title: String,
    priority: Option<String>,
    description: Option<String>,
}

#[async_trait]
impl Skill for MockAddTask {
    fn name(&self) -> &str {
        "add_task"
    }

    fn description(&self) -> &str {
        "新しいタスクを作成する。タスク分解時やリクエスト処理時に使用。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "タスクのタイトル"
                },
                "priority": {
                    "type": "string",
                    "enum": ["urgent", "this_week", "someday"],
                    "description": "優先度（デフォルト: someday）"
                },
                "description": {
                    "type": "string",
                    "description": "タスクの詳細説明"
                }
            },
            "required": ["title"]
        })
    }

    async fn execute(&self, params: Value) -> Result<SkillResult> {
        let p: AddTaskParams = serde_json::from_value(params)?;
        let priority = p.priority.unwrap_or_else(|| "someday".to_string());
        let mock_id = "999"; // モックID

        println!("[MOCK] タスク作成: {} (優先度: {})", p.title, priority);

        Ok(SkillResult::ok_with_data(
            format!("タスクを作成しました: {}", p.title),
            serde_json::json!({
                "id": mock_id,
                "title": p.title,
                "priority": priority
            }),
        ))
    }
}

/// モック: カレンダー取得
pub struct MockGetCalendar;

#[async_trait]
impl Skill for MockGetCalendar {
    fn name(&self) -> &str {
        "get_calendar"
    }

    fn description(&self) -> &str {
        "今日のカレンダー予定を取得する。スケジューリング時の空き時間確認に使用。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _params: Value) -> Result<SkillResult> {
        // モックデータ
        let events = serde_json::json!([
            {
                "title": "朝会",
                "start": "09:00",
                "end": "09:30"
            },
            {
                "title": "1on1",
                "start": "14:00",
                "end": "15:00"
            }
        ]);

        Ok(SkillResult::ok_with_data(
            "今日の予定: 2件",
            events,
        ))
    }
}

/// モック: 空き時間検索
pub struct MockFindFreeSlots;

#[async_trait]
impl Skill for MockFindFreeSlots {
    fn name(&self) -> &str {
        "find_free_slots"
    }

    fn description(&self) -> &str {
        "指定した時間内の空き時間枠を検索する。タスクのスケジューリングに使用。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "duration_minutes": {
                    "type": "integer",
                    "description": "必要な時間（分）"
                }
            },
            "required": ["duration_minutes"]
        })
    }

    async fn execute(&self, _params: Value) -> Result<SkillResult> {
        // モックデータ
        let slots = serde_json::json!([
            { "start": "09:30", "end": "14:00" },
            { "start": "15:00", "end": "18:00" }
        ]);

        Ok(SkillResult::ok_with_data(
            "2つの空き時間枠があります",
            slots,
        ))
    }
}
