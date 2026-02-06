use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct KintoneRecordsResponse {
    pub records: Vec<KintoneRawRecord>,
}

#[derive(Debug, Deserialize)]
pub struct KintoneRawRecord {
    #[serde(rename = "$id")]
    pub id: KintoneFieldValue,
    #[serde(rename = "タイトル")]
    pub title: KintoneFieldValue,
    pub status: KintoneFieldValue,
    #[serde(rename = "優先度")]
    pub priority: KintoneFieldValue,
    #[serde(rename = "期限")]
    pub deadline: KintoneFieldValue,
    #[serde(rename = "作成日時")]
    pub created_at: KintoneFieldValue,
    #[serde(default, rename = "completed_at")]
    pub completed_at: KintoneFieldValue,
    #[serde(default)]
    pub channel_id: KintoneFieldValue,
    #[serde(default)]
    pub thread_ts: KintoneFieldValue,
    #[serde(default)]
    pub parent_id: KintoneFieldValue,
    #[serde(default)]
    pub task_type: KintoneFieldValue,
    #[serde(default)]
    pub description: KintoneFieldValue,
    #[serde(default)]
    pub calendar_event_id: KintoneFieldValue,
}

#[derive(Debug, Default, Deserialize)]
pub struct KintoneFieldValue {
    #[serde(default)]
    pub value: Option<String>,
}

/// Normalized task record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub title: String,
    pub status: String,       // todo, in_progress, done
    pub priority: String,     // urgent, this_week, someday
    pub deadline: String,
    pub created_at: String,
    pub completed_at: String,
    pub channel_id: String,
    pub thread_ts: String,
    pub parent_id: Option<u64>,
    pub task_type: String,    // request, todo
    pub description: String,
    pub calendar_event_id: String,
}

impl TaskRecord {
    pub fn from_raw(raw: &KintoneRawRecord) -> Self {
        Self {
            id: raw.id.value.clone().unwrap_or_default(),
            title: raw.title.value.clone().unwrap_or_default(),
            status: raw.status.value.clone().unwrap_or_default(),
            priority: raw.priority.value.clone().unwrap_or_default(),
            deadline: raw.deadline.value.clone().unwrap_or_default(),
            created_at: raw.created_at.value.clone().unwrap_or_default(),
            completed_at: raw.completed_at.value.clone().unwrap_or_default(),
            channel_id: raw.channel_id.value.clone().unwrap_or_default(),
            thread_ts: raw.thread_ts.value.clone().unwrap_or_default(),
            parent_id: raw
                .parent_id
                .value
                .as_ref()
                .and_then(|v| v.parse().ok()),
            task_type: raw.task_type.value.clone().unwrap_or_default(),
            description: raw.description.value.clone().unwrap_or_default(),
            calendar_event_id: raw.calendar_event_id.value.clone().unwrap_or_default(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct KintoneAddPayload {
    pub app: String,
    pub record: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct KintoneAddResponse {
    pub id: String,
}
