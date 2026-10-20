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
//!
//! 写日志**不经缓冲**（`File` 直写），进程被杀也留得下已发生的阶段 —— 这正是排障要的。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde_json::json;

/// 保留最近多少个任务目录。
const KEEP_TASKS: usize = 20;
/// 单个 `output.log` 上限。
const OUTPUT_LIMIT: u64 = 4 * 1024 * 1024;
/// 目录名里 `kind` / `task_id` 的最大字符数（留够余量，别撞文件名长度上限）。
const MAX_KIND_CHARS: usize = 24;
const MAX_ID_CHARS: usize = 48;

/// 一次任务运行的记录器（可在异步块里用 `Arc` 共享）。
pub struct TaskLog {
    dir: PathBuf,
    started: Instant,
    finished: AtomicBool,
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
        let root = state_dir.join("tasks");
        if let Err(err) = std::fs::create_dir_all(&root) {
            log::warn!(
                "event=TaskLogUnavailable dir={} error={err}",
                root.display()
            );
            return None;
        }
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let stem = format!(
            "{}-{}-{ts}",
            sanitize(kind, MAX_KIND_CHARS),
            sanitize(task_id, MAX_ID_CHARS)
        );
        // 同一秒内同一计划的**重试**不能用同一个目录（否则两次运行的日志会混在一起）。
        // 用 `create_dir` 抢名字而不是 `exists()` 再建，避免并发任务的 TOCTOU。
        let dir = create_unique_dir(&root, &stem)?;
        // 日志可能含基础设施细节：目录 0700、文件 0600。
        restrict_permissions(&dir, true);

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
            // 别留半成品目录：否则这次失败会占掉一个保留位。
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        };
        restrict_permissions(&dir.join("events.jsonl"), false);
        restrict_permissions(&dir.join("output.log"), false);

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
        restrict_permissions(&dir.join("task.json"), false);

        prune_old_tasks(&root, &dir);
        Some(std::sync::Arc::new(Self {
            dir,
            started: Instant::now(),
            finished: AtomicBool::new(false),
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
        let mut guard = self.output.lock().unwrap_or_else(|e| e.into_inner());
        let (file, written) = &mut *guard;
        // 先看额度再脱敏：文件已满时不必白扫一遍（几 MB 的 body 也要跑几十个 marker）。
        if *written >= OUTPUT_LIMIT {
            return;
        }
        let payload = redact(text);
        let bytes = payload.as_bytes();
        let remaining = (OUTPUT_LIMIT - *written) as usize;
        if bytes.len() > remaining {
            // 按**字符边界**截断：切在多字节字符中间会往日志里写半个 UTF-8 字。
            let mut cut = remaining;
            while cut > 0 && !payload.is_char_boundary(cut) {
                cut -= 1;
            }
            let _ = file.write_all(&bytes[..cut]);
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
    ///
    /// 幂等：重复调用只写一次 `done`（防以后重构出双收尾）。
    pub fn finish(&self, status: &str, detail: &str) -> String {
        if !self.finished.swap(true, Ordering::SeqCst) {
            self.event("done", Some(&format!("status={status} {detail}")));
        }
        self.dir.display().to_string()
    }
}

/// 目录名片段消毒：只留 `[A-Za-z0-9_-]`，并限制长度（防超长 id 撞文件名上限）。
fn sanitize(raw: &str, max_chars: usize) -> String {
    let mut out: String = raw
        .chars()
        .take(max_chars)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('-');
    }
    out
}

/// 抢占一个唯一目录名：先试 `stem`，被占用依次加 `-1`、`-2`。
fn create_unique_dir(root: &Path, stem: &str) -> Option<PathBuf> {
    for attempt in 0..100 {
        let dir = if attempt == 0 {
            root.join(stem)
        } else {
            root.join(format!("{stem}-{attempt}"))
        };
        match std::fs::create_dir(&dir) {
            Ok(()) => return Some(dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                log::warn!("event=TaskLogUnavailable dir={} error={err}", dir.display());
                return None;
            }
        }
    }
    log::warn!(
        "event=TaskLogUnavailable dir={} error=too many attempts",
        root.join(stem).display()
    );
    None
}

/// 收紧任务日志的权限（Unix）：目录 0700、文件 0600。
fn restrict_permissions(path: &Path, is_dir: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if is_dir { 0o700 } else { 0o600 };
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(mode);
            if let Err(err) = std::fs::set_permissions(path, perms) {
                log::warn!(
                    "event=TaskLogChmodFailed path={} error={err}",
                    path.display()
                );
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (path, is_dir);
}

/// 把密钥从文本里抹掉：环境里的密钥值 + `Bearer xxx` / `token=xxx` 等模式。
fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for secret in secret_values() {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "***");
        }
    }
    // 先处理 `Authorization: <scheme> <值>`：否则只会抹掉认证方式，口令本身反而露出来。
    out = redact_authorization(&out);
    // 再处理裸的 `Bearer <值>`。
    out = redact_after(&out, "bearer ");
    for marker in redact_markers() {
        out = redact_after(&out, marker);
    }
    out
}

/// 需要脱敏的键名。
const SECRET_KEYS: [&str; 11] = [
    "token",
    "secret",
    "client_secret",
    "password",
    "passwd",
    "pwd",
    "api_key",
    "apikey",
    "api-key",
    "access_key",
    "accesskey",
];

/// 键名 + 分隔符的 marker 表：`token=`、`token: `、`"token": `、`"token":"`。
///
/// 四种写法都得盖：shell / k=v、日志文本、JSON（带空格）、JSON（不带空格）。
fn redact_markers() -> &'static [String] {
    static CACHE: OnceLock<Vec<String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut markers = Vec::with_capacity(SECRET_KEYS.len() * 4);
        for key in SECRET_KEYS {
            for sep in ["=", ": ", "\": ", "\":\""] {
                markers.push(format!("{key}{sep}"));
            }
        }
        markers
    })
}

/// 抹掉 `Authorization: <scheme> <值>` 里的**两个词**（scheme 与口令）。
///
/// 只按 `authorization: ` 这一个 marker 处理会把 `Basic xxx` 里的 `xxx` 留下来。
/// 只在紧跟已知 scheme 时才动第二个词，避免把普通句子里的 `basic` 误当认证方式。
fn redact_authorization(input: &str) -> String {
    const MARKER: &str = "authorization:";
    const SCHEMES: [&str; 4] = ["bearer", "basic", "digest", "token"];

    // 不手写逐字节滑窗：`to_ascii_lowercase` + `str::find`（Two-Way）快一个数量级。
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(pos) = lower[cursor..].find(MARKER) {
        let start = cursor + pos;
        let after = start + MARKER.len();
        out.push_str(&input[cursor..after]);

        let rest = &input[after..];
        let ws = rest.len() - rest.trim_start().len();
        out.push_str(&rest[..ws]);

        let word = &rest[ws..];
        let word_len = word.find(char::is_whitespace).unwrap_or(word.len());
        let scheme = word[..word_len].to_ascii_lowercase();
        if !SCHEMES.contains(&scheme.as_str()) {
            // 空白已写进 out，游标要跨过它，否则重拼时会把这段空白再输出一次。
            cursor = after + ws;
            continue;
        }
        out.push_str("***");

        let value = &word[word_len..];
        let vws = value.len() - value.trim_start().len();
        out.push_str(&value[..vws]);
        let quoted = &value[vws..];
        let quote_len = leading_quote_len(quoted);
        out.push_str(&quoted[..quote_len]);
        let tail = &quoted[quote_len..];
        let vlen = tail
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == '}')
            .unwrap_or(tail.len());
        cursor = after + ws + word_len + vws + quote_len;
        if vlen > 0 {
            out.push_str("***");
            cursor += vlen;
        }
    }
    out.push_str(&input[cursor..]);
    out
}

/// 把 `marker`（**大小写不敏感**）之后连续的非空白、非引号字符替换成 `***`。
///
/// 值被引号包着（`"password": "xxx"`）时保留引号本身，只抹引号里的内容。
fn redact_after(input: &str, marker: &str) -> String {
    // 同上：小写副本只多一次拷贝，换掉 O(n·m) 的自写搜。
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(pos) = lower[cursor..].find(marker) {
        let start = cursor + pos;
        let value_start = start + marker.len();
        out.push_str(&input[cursor..value_start]);

        let tail = &input[value_start..];
        let quote_len = leading_quote_len(tail);
        out.push_str(&tail[..quote_len]);
        let value = &tail[quote_len..];
        let len = value
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == '}')
            .unwrap_or(value.len());
        if len > 0 {
            out.push_str("***");
            cursor = value_start + quote_len + len;
        } else {
            cursor = value_start + quote_len;
        }
    }
    out.push_str(&input[cursor..]);
    out
}

/// 值前面的开头引号长度（`0` 或 `1`）。
fn leading_quote_len(text: &str) -> usize {
    if text.starts_with('"') || text.starts_with('\'') {
        1
    } else {
        0
    }
}

/// 环境里像密钥的变量值（长于 8 字节才算，避免把 `true` 这类短值当密钥替换）。
///
/// 缓存一次：gwlinkd 是长驻进程，环境变量启动后不再变化，而 `output()` 每行都会调这里。
fn secret_values() -> &'static [String] {
    static CACHE: OnceLock<Vec<String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut values: Vec<String> = std::env::vars_os()
            // 用 `vars_os`：环境里只要有一个非 UTF-8 值，`vars()` 会 panic。
            .filter_map(|(key, value)| {
                Some((key.to_str()?.to_string(), value.to_str()?.to_string()))
            })
            .filter(|(key, _)| {
                let k = key.to_ascii_lowercase();
                k.contains("token")
                    || k.contains("secret")
                    || k.contains("password")
                    || k.contains("api_key")
                    || k.contains("apikey")
                    || k.contains("access_key")
                    || k.contains("credential")
                    || k.contains("private")
            })
            .map(|(_, value)| value)
            .filter(|value| value.len() >= 8)
            .collect();
        // 长的先替换：短密钥若恰是长密钥的子串，先换长的才不会留下残片。
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        values
    })
}

/// 只留最近 `KEEP_TASKS` 个任务目录，`keep`（当前正在写的）永不删。
fn prune_old_tasks(root: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    // 按**修改时间**排序，不按目录名：名字里虽有时间戳，但不同 `kind` 前缀会让字典序 ≠ 时间序。
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path != keep)
        .map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (modified, path)
        })
        .collect();
    dirs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let total = dirs.len() + 1; // +1：当前目录也算一个
    let excess = total.saturating_sub(KEEP_TASKS);
    for (_, dir) in dirs.into_iter().take(excess) {
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
    fn redact_is_idempotent() {
        // 已经抹过的输出再抹一次不能变形（回执里会把 detail 再写一遍到 output.log）。
        for input in [
            "Authorization: Bearer abcdefghijkl",
            r#"{"token":"abcdefghijkl"}"#,
            "token=abcdefghijkl done",
        ] {
            let once = redact(input);
            assert_eq!(redact(&once), once, "{input}");
        }
    }

    #[test]
    fn redacts_common_credential_shapes() {
        let cases = [
            ("curl -H 'x-api-key: abcd1234efgh'", "abcd1234efgh"),
            ("POST api_key=abcd1234efgh", "abcd1234efgh"),
            ("apikey=abcd1234efgh", "abcd1234efgh"),
            ("Authorization: Basic dXNlcjpwYXNz", "dXNlcjpwYXNz"),
            ("BEARER AbCdEfGhIjKl", "AbCdEfGhIjKl"),
            ("client_secret=abcd1234efgh", "abcd1234efgh"),
            ("access_key=abcd1234efgh", "abcd1234efgh"),
            (r#"{"token":"abcd1234efgh"}"#, "abcd1234efgh"),
            (r#"{"password": "abcd1234efgh"}"#, "abcd1234efgh"),
            ("Authorization: Digest abcd1234efgh", "abcd1234efgh"),
        ];
        for (input, secret) in cases {
            let out = redact(input);
            assert!(!out.contains(secret), "{input} → {out}");
            assert!(out.contains("***"), "{input} → {out}");
        }
    }

    #[test]
    fn keeps_only_the_latest_tasks() {
        let tmp = tmp("prune");
        let root = tmp.join("tasks");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..(KEEP_TASKS + 3) {
            std::fs::create_dir_all(root.join(format!("upgrade-x-2026010100{i:04}"))).unwrap();
        }
        let keep = root.join("upgrade-x-running");
        std::fs::create_dir_all(&keep).unwrap();
        prune_old_tasks(&root, &keep);
        let count = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .count();
        assert_eq!(count, KEEP_TASKS);
        assert!(keep.exists(), "正在写的目录不能被剪掉");
    }

    #[test]
    fn same_second_creates_distinct_dirs() {
        let tmp = tmp("collide");
        let a = TaskLog::create(&tmp, "upgrade", "plan-9", json!({})).expect("a");
        let b = TaskLog::create(&tmp, "upgrade", "plan-9", json!({})).expect("b");
        assert_ne!(a.dir(), b.dir());
        assert!(a.dir().exists() && b.dir().exists());
    }

    #[test]
    fn task_dir_names_are_sanitized() {
        let tmp = tmp("sanitize");
        let task = TaskLog::create(&tmp, "up grade", "../../etc", json!({})).expect("task");
        let name = task
            .dir()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        assert!(name.starts_with("up_grade-"), "{name}");
        assert!(name.contains("_"), "{name}");
        assert!(!name.contains(".."), "{name}");
        assert_eq!(task.dir().parent().unwrap(), tmp.join("tasks"));
    }

    #[test]
    fn finish_is_idempotent() {
        let tmp = tmp("finish");
        let task = TaskLog::create(&tmp, "upgrade", "plan-2", json!({})).expect("task");
        task.finish("done", "ok");
        task.finish("failed", "again");
        let events = std::fs::read_to_string(task.dir().join("events.jsonl")).unwrap();
        assert_eq!(events.matches("\"phase\":\"done\"").count(), 1, "{events}");
    }

    #[test]
    fn concurrent_output_keeps_lines_intact() {
        let tmp = tmp("concurrent");
        let task = TaskLog::create(&tmp, "upgrade", "plan-3", json!({})).expect("task");
        let mut handles = Vec::new();
        for t in 0..4 {
            let task = task.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..50 {
                    task.output(&format!("thread={t} line={i}\n"));
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let output = std::fs::read_to_string(task.dir().join("output.log")).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 200, "行数不对，可能发生写交叉");
        for line in lines {
            assert!(line.starts_with("thread="), "整行被写坏：{line}");
            assert!(line.contains(" line="), "整行被写坏：{line}");
        }
    }

    #[test]
    fn truncates_on_char_boundary() {
        let tmp = tmp("truncate");
        let task = TaskLog::create(&tmp, "upgrade", "plan-4", json!({})).expect("task");
        // `字` 占 3 字节，且 4 MiB 不是 3 的整数倍 —— 正好落在字符中间。
        let payload = "字".repeat(OUTPUT_LIMIT as usize);
        task.output(&payload);
        let raw = std::fs::read(task.dir().join("output.log")).unwrap();
        assert!(raw.len() as u64 <= OUTPUT_LIMIT + 64, "len={}", raw.len());
        let text = String::from_utf8(raw).expect("截断后仍是合法 UTF-8");
        assert!(text.contains("truncated"), "{text}");
    }

    #[test]
    fn unavailable_state_dir_returns_none_without_panicking() {
        // `tasks` 被一个普通文件占着：建目录必失败，但**不能影响主流程**。
        let tmp = tmp("unavailable");
        std::fs::write(tmp.join("tasks"), b"not a directory").unwrap();
        assert!(TaskLog::create(&tmp, "upgrade", "plan-6", json!({})).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn restricts_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tmp("perms");
        let task = TaskLog::create(&tmp, "upgrade", "plan-5", json!({})).expect("task");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(task.dir()), 0o700);
        assert_eq!(mode(&task.dir().join("events.jsonl")), 0o600);
        assert_eq!(mode(&task.dir().join("output.log")), 0o600);
        assert_eq!(mode(&task.dir().join("task.json")), 0o600);
    }
}
