//! 任务执行记录（P0）：每个中心派下来的 plan / 命令一个目录。
//!
//! 为什么需要：升级失败时只有 `event=SelfUpdateFailed ... detail=artifact` 这种一行事件，
//! 没有 HTTP 状态码、没有执行器输出、没有耗时 —— 有事件，没有证据。
//!
//! 落点（`state_dir` 是 `configs/gwlinkd/state`，随 `configs/` 进备份）：
//!
//! ```text
//! <state_dir>/tasks/<kind>-<task_id>-<时间戳>/
//!   task.json      # kind / work_id / 组件 / 版本 / 开始时间
//!   events.jsonl   # 阶段 start/end（含耗时、错误摘要）
//!   output.log     # 执行器 stdout + stderr（截断 + 脱敏）
//! ```
//!
//! 三条约束与 gops 的 `run_log` 一致：**有界**（单文件 4 MiB、保留 20 次）、
//! **写入时脱敏**、**不影响主流程**（任何 IO 失败只告警）。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use serde_json::json;

/// 保留最近多少个任务目录。
const KEEP_TASKS: usize = 20;
/// 单个 `output.log` 上限。
const OUTPUT_LIMIT: u64 = 4 * 1024 * 1024;

/// 一次任务运行的记录器（可在异步块里用 `Arc` 共享）。
pub struct TaskLog {
    dir: PathBuf,
    started: Instant,
    events: Mutex<File>,
    output: Mutex<(File, u64)>,
}

impl TaskLog {
    /// 建目录并写 `task.json`；**失败返回 `None`**（调用方只告警，任务照跑）。
    pub fn create(
        state_dir: &Path,
        kind: &str,
        task_id: &str,
        meta: serde_json::Value,
    ) -> Option<std::sync::Arc<Self>> {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let safe_id: String = task_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let dir = state_dir
            .join("tasks")
            .join(format!("{kind}-{safe_id}-{ts}"));
        if let Err(err) = std::fs::create_dir_all(&dir) {
            log::warn!("event=TaskLogUnavailable dir={} error={err}", dir.display());
            return None;
        }
        let events = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"));
        let output = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("output.log"));
        let (Ok(events), Ok(output)) = (events, output) else {
            log::warn!("event=TaskLogUnavailable dir={}", dir.display());
            return None;
        };

        let mut task_json = meta;
        task_json["kind"] = json!(kind);
        task_json["task_id"] = json!(task_id);
        task_json["started_at"] = json!(chrono::Utc::now().to_rfc3339());
        task_json["log_dir"] = json!(dir.display().to_string());
        if let Err(err) = std::fs::write(
            dir.join("task.json"),
            serde_json::to_string_pretty(&task_json).unwrap_or_default(),
        ) {
            log::warn!(
                "event=TaskJsonWriteFailed dir={} error={err}",
                dir.display()
            );
        }

        prune_old_tasks(state_dir);
        Some(std::sync::Arc::new(Self {
            dir,
            started: Instant::now(),
            events: Mutex::new(events),
            output: Mutex::new((output, 0)),
        }))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 记一个事件（`detail` 会脱敏）。
    pub fn event(&self, phase: &str, detail: Option<&str>) {
        let mut value = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "phase": phase,
            "elapsed_ms": self.started.elapsed().as_millis() as u64,
        });
        if let Some(detail) = detail {
            value["detail"] = json!(redact(detail));
        }
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(err) = writeln!(events, "{value}") {
            log::warn!("event=TaskEventWriteFailed error={err}");
        }
    }

    /// 追加命令输出（`output.log`；超过上限只记一次截断标记）。
    pub fn output(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let payload = redact(text);
        let mut guard = self.output.lock().unwrap_or_else(|e| e.into_inner());
        let (file, written) = &mut *guard;
        if *written >= OUTPUT_LIMIT {
            return;
        }
        let bytes = payload.as_bytes();
        let remaining = (OUTPUT_LIMIT - *written) as usize;
        if bytes.len() > remaining {
            let _ = file.write_all(&bytes[..remaining]);
            let _ = writeln!(
                file,
                "\n… [truncated: 超过 {} MiB]",
                OUTPUT_LIMIT / 1024 / 1024
            );
            *written = OUTPUT_LIMIT;
            return;
        }
        if let Err(err) = file.write_all(bytes) {
            log::warn!("event=TaskOutputWriteFailed error={err}");
            return;
        }
        *written += bytes.len() as u64;
    }

    /// 收尾：写终态事件并返回日志目录（调用方拼进回执的 `detail`）。
    pub fn finish(&self, status: &str, detail: &str) -> String {
        self.event("done", Some(&format!("status={status} {detail}")));
        self.dir.display().to_string()
    }
}

/// 把密钥从文本里抹掉：环境里的密钥值 + `Bearer xxx` / `token=xxx` 等模式。
fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for secret in secret_values() {
        if secret.len() >= 8 && out.contains(&secret) {
            out = out.replace(&secret, "***");
        }
    }
    for marker in ["Bearer ", "bearer "] {
        out = redact_after(&out, marker);
    }
    for marker in ["token=", "secret=", "password=", "passwd="] {
        out = redact_after_ci(&out, marker);
    }
    out
}

/// 把 `marker` 之后连续的非空白、非引号字符替换成 `***`。
fn redact_after(input: &str, marker: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find(marker) {
        out.push_str(&rest[..pos + marker.len()]);
        let tail = &rest[pos + marker.len()..];
        let len = tail
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == '}')
            .unwrap_or(tail.len());
        if len > 0 {
            out.push_str("***");
            rest = &tail[len..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn redact_after_ci(input: &str, marker: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(pos) = lower[cursor..].find(marker) {
        let start = cursor + pos;
        out.push_str(&input[cursor..start + marker.len()]);
        let tail = &input[start + marker.len()..];
        let len = tail
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == '}')
            .unwrap_or(tail.len());
        if len > 0 {
            out.push_str("***");
            cursor = start + marker.len() + len;
        } else {
            cursor = start + marker.len();
        }
    }
    out.push_str(&input[cursor..]);
    out
}

fn secret_values() -> Vec<String> {
    let mut values: Vec<String> = std::env::vars()
        .filter(|(key, _)| {
            let k = key.to_ascii_lowercase();
            k.contains("token") || k.contains("secret") || k.contains("password")
        })
        .map(|(_, value)| value)
        .filter(|value| value.len() >= 8)
        .collect();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.dedup();
    values
}

fn prune_old_tasks(state_dir: &Path) {
    let root = state_dir.join("tasks");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    let excess = dirs.len().saturating_sub(KEEP_TASKS);
    for dir in dirs.into_iter().take(excess) {
        if let Err(err) = std::fs::remove_dir_all(&dir) {
            log::warn!("event=TaskPruneFailed dir={} error={err}", dir.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wist-gwlinkd-tasklog-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_task_dir_events_and_redacted_output() {
        let tmp = tmp("events");
        let task = TaskLog::create(
            &tmp,
            "upgrade",
            "plan-1",
            json!({"component": "wist-gwlinkd", "to_version": "v0.15.0-alpha"}),
        )
        .expect("task log");

        task.event("run.start", Some("component=wist-gwlinkd"));
        task.output("Authorization: Bearer abcdefghijkl\n");
        task.output("download token=deadbeefcafe done\n");
        let dir = task.finish("done", "");

        let task_json = std::fs::read_to_string(Path::new(&dir).join("task.json")).unwrap();
        assert!(task_json.contains("\"kind\": \"upgrade\""), "{task_json}");
        assert!(task_json.contains("wist-gwlinkd"), "{task_json}");

        let events = std::fs::read_to_string(Path::new(&dir).join("events.jsonl")).unwrap();
        assert!(events.contains("run.start"), "{events}");
        assert!(events.contains("\"phase\":\"done\""), "{events}");

        let output = std::fs::read_to_string(Path::new(&dir).join("output.log")).unwrap();
        assert!(!output.contains("abcdefghijkl"), "Bearer 要脱敏：{output}");
        assert!(!output.contains("deadbeefcafe"), "token= 要脱敏：{output}");
        assert!(output.contains("***"));
    }

    #[test]
    fn keeps_only_the_latest_tasks() {
        let tmp = tmp("prune");
        let root = tmp.join("tasks");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..(KEEP_TASKS + 3) {
            std::fs::create_dir_all(root.join(format!("upgrade-x-2026010100{i:04}"))).unwrap();
        }
        prune_old_tasks(&tmp);
        let count = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .count();
        assert_eq!(count, KEEP_TASKS);
    }
}
