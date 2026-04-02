//! Context Rot 検知モジュール
//!
//! スキルファイルの mtime と ops 成功率から陳腐化を検出し、
//! Slack に通知する。

#![allow(dead_code)]

use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};

use crate::db::Db;
use crate::repo_config::ReposConfig;

const STALE_DAYS: u64 = 30;
const ERROR_RATE_THRESHOLD: f64 = 0.30; // 30%超 = 成功率70%未満
const RECENT_OPS_COUNT: usize = 10;
const RENOTIFY_DAYS: i64 = 7;

/// 陳腐化検知結果
pub struct ContextRotAlert {
    pub repo_key: String,
    pub skill_path: String,
    pub days_since_update: u64,
    pub success_rate_pct: u64,
    pub ops_channel: String,
}

/// 全リポジトリをスキャンして陳腐化アラートを返す
pub fn scan_all_repos(config: &ReposConfig, db: &Db) -> Vec<ContextRotAlert> {
    let mut alerts = Vec::new();

    for (_idx, repo) in config.get_all_ops_entries() {
        let ops_skills = match &repo.ops_skills {
            Some(skills) if !skills.is_empty() => skills,
            _ => continue,
        };

        let ops_channel = match &repo.ops_channel {
            Some(ch) => ch.clone(),
            None => continue, // 通知先がなければスキップ
        };

        // 再通知防止チェック（repo_key 単位）
        if let Ok(Some(ts)) = db.get_last_context_rot_notification(&repo.key) {
            if let Ok(last) = ts.parse::<DateTime<Utc>>() {
                let days_since = (Utc::now() - last).num_days();
                if days_since < RENOTIFY_DAYS {
                    continue;
                }
            }
        }

        // 成功率チェック（repo_key 単位で共通）
        let outcomes = db
            .get_recent_ops_outcomes_by_repo(&repo.key, RECENT_OPS_COUNT)
            .unwrap_or_default();
        if outcomes.is_empty() {
            continue; // 実行履歴なし → 判定不能
        }

        // completed と error/failed のみを対象に成功率を計算（no_action は除外）
        let relevant: Vec<&String> = outcomes
            .iter()
            .filter(|o| *o == "completed" || *o == "error" || *o == "failed")
            .collect();
        if relevant.is_empty() {
            continue;
        }
        let error_count = relevant.iter().filter(|o| **o == "error" || **o == "failed").count();
        let error_rate = error_count as f64 / relevant.len() as f64;
        if error_rate <= ERROR_RATE_THRESHOLD {
            continue; // 成功率十分
        }

        let success_rate_pct = ((1.0 - error_rate) * 100.0) as u64;

        // 各スキルファイルの mtime チェック
        let repo_path = config.repo_local_path(repo);
        for skill_path in ops_skills {
            let full_path = repo_path.join(skill_path);
            let mtime = match std::fs::metadata(&full_path).and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => {
                    tracing::warn!(
                        "context_rot: skill file not found: {}",
                        full_path.display()
                    );
                    continue;
                }
            };

            let days = SystemTime::now()
                .duration_since(mtime)
                .unwrap_or(Duration::ZERO)
                .as_secs()
                / 86400;

            if days < STALE_DAYS {
                continue; // このスキルファイルは新しい
            }

            alerts.push(ContextRotAlert {
                repo_key: repo.key.clone(),
                skill_path: skill_path.clone(),
                days_since_update: days,
                success_rate_pct,
                ops_channel: ops_channel.clone(),
            });
        }
    }

    alerts
}

/// Slack 通知メッセージをフォーマット
pub fn format_rot_alert(alert: &ContextRotAlert) -> String {
    format!(
        ":warning: *スキルファイルの陳腐化を検知*\n\
         リポジトリ: {}\n\
         スキルファイル: {}\n\
         最終更新: {}日前\n\
         直近の成功率: {}%（閾値: 70%）\n\n\
         スキルファイルの内容が実態と乖離している可能性があります。\n\
         内容を見直してください。",
        alert.repo_key,
        alert.skill_path,
        alert.days_since_update,
        alert.success_rate_pct,
    )
}
