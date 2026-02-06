use anyhow::Result;

use crate::kintone::client::KintoneClient;
use crate::kintone::models::TaskRecord;
use crate::kintone::query;

/// Poll kintone for active tasks.
pub async fn fetch_active_tasks(client: &KintoneClient) -> Result<Vec<TaskRecord>> {
    client.fetch_records(&query::active_tasks_query()).await
}

/// Poll kintone for new unprocessed requests.
pub async fn fetch_new_requests(client: &KintoneClient) -> Result<Vec<TaskRecord>> {
    client.fetch_records(&query::new_requests_query()).await
}

/// Fetch children of a given parent task.
pub async fn fetch_children(client: &KintoneClient, parent_id: u64) -> Result<Vec<TaskRecord>> {
    client
        .fetch_records(&query::children_query(parent_id))
        .await
}
