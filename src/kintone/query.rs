/// URL-encode a string for kintone query parameters.
pub fn urlencode(s: &str) -> String {
    let mut result = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", b));
            }
        }
    }
    result
}

/// Build a kintone query string for active tasks.
pub fn active_tasks_query() -> String {
    r#"status in ("todo", "in_progress") order by 作成日時 desc limit 100"#.to_string()
}

/// Build a query for new request records (unprocessed).
pub fn new_requests_query() -> String {
    r#"task_type in ("request") and status in ("todo") order by 作成日時 asc"#.to_string()
}

/// Build a query for children of a given parent_id.
pub fn children_query(parent_id: u64) -> String {
    format!(
        r#"parent_id = "{}" order by 作成日時 asc"#,
        parent_id
    )
}
