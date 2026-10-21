//! `wist-gwlinkd run` 的 **bin 层（独立进程）** 单实例用例。
//!
//! 两把锁、两层语义（见 `src/main.rs` 的 `InstanceLocks`）：
//!
//! - **机器级**（`single_instance`，默认 `/tmp/wist-gwlinkd.lock`）：一个操作系统上只允许一个
//!   gwlinkd —— 与配置 / state 目录无关。这里覆盖「另一份配置已经占着闸门」。
//! - **部署级**（`state::acquire_single_instance_lock`，`state_dir/gwlinkd.lock`）：这份部署
//!   在跑吗。`src/state.rs` 的单测证明**同进程**拿不到第二把，这里补**跨进程**那一层。
//!
//! 用例一律用 `WIST_GWLINKD_LOCK_FILE` 把机器级锁指到临时目录：默认路径 `/tmp/wist-gwlinkd.lock`
//! 是**全机共享**的，测试若用它，既会与开发机上真实常驻的 gwlinkd 打架，也会让并行用例互相干扰。
//!
//! 反向也测一条：没人持锁时不能误报「已有实例」—— 否则服务管理器永远拉不起来。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use wist_gwlinkd::single_instance::InstanceLock;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gwlinkd-run-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// 最小可跑 `run` 的配置：只有 `state_dir`（部署级锁的落点），其余键缺省。
///
/// 这样「没人持锁」时 `run` 会走到首跑、因**没有接入券**而快速失败 —— 与本用例无关，
/// 但它证明进程确实**过了**单实例锁这一关。
struct Fixture {
    dir: PathBuf,
    config: PathBuf,
    state_dir: PathBuf,
    /// 机器级锁文件（经 `WIST_GWLINKD_LOCK_FILE` 指过来，别动全机默认路径）。
    lock_file: PathBuf,
}

fn setup(tag: &str) -> Fixture {
    let dir = temp_dir(tag);
    let state_dir = dir.join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    let lock_file = dir.join("machine.lock");
    let config = dir.join("gwlinkd.toml");
    std::fs::write(
        &config,
        format!("state_dir = \"{}\"\n", state_dir.display()),
    )
    .expect("config");
    Fixture {
        dir,
        config,
        state_dir,
        lock_file,
    }
}

impl Fixture {
    /// 以独立进程跑 `<bin> run`，配置走 `WIST_GWLINKD_CONFIG`、机器级闸门走 `WIST_GWLINKD_LOCK_FILE`。
    fn run(&self) -> Output {
        self.run_with_lock(&self.lock_file)
    }

    /// 同上，但把机器级锁指到指定路径（测「闸门不可用」与「闸门释放」用）。
    fn run_with_lock(&self, lock_file: &Path) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wist-gwlinkd"));
        command
            .arg("run")
            .env("WIST_GWLINKD_CONFIG", &self.config)
            .env("WIST_GWLINKD_LOCK_FILE", lock_file)
            // 别把测试机上的现场环境带进来（会让「无券」这条路径变成真接入）。
            .env_remove("WIST_GWLINKD_PUB_CONFIG")
            .env_remove("WIST_GWLINKD_ENV_FILE")
            .env_remove("WIST_GWLINKD_LINK_TOKEN");
        command.output().expect("spawn wist-gwlinkd run")
    }

    /// 状态目录里除「部署级锁文件」之外的条目（用来断言「被拒时零副作用」）。
    fn state_entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.state_dir)
            .expect("read state dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name != wist_gwlinkd::state::LOCK_FILE)
            .collect();
        names.sort();
        names
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn err_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn run_refuses_a_second_instance_of_the_same_deployment() {
    let fx = setup("second");
    // 本测试进程持有**部署级**锁 → 子进程里的 `run` 必须被拒（跨进程 flock）。
    let _guard =
        wist_gwlinkd::state::acquire_single_instance_lock(&fx.state_dir).expect("acquire lock");

    let out = fx.run();
    assert!(!out.status.success(), "第二个常驻必须拒绝启动");
    assert!(
        err_text(&out.stderr).contains("已有网关常驻在跑"),
        "{}",
        err_text(&out.stderr)
    );
    // 拒绝发生在**任何落盘之前**：除了锁文件，状态目录里不该多出身份 / 实例标识。
    assert!(
        fx.state_entries().is_empty(),
        "被拒的实例不应产生副作用：{:?}",
        fx.state_entries()
    );
    fx.cleanup();
}

/// **只能有一个**：闸门是机器级的 —— 另一份配置（另一个 `state_dir`）在跑，也拦住。
#[test]
fn run_refuses_when_another_config_already_holds_the_machine_gate() {
    let fx = setup("machine-gate");
    // 别的「部署」占着机器级闸门（模拟另一份配置 / 另一个 cwd 起来的 gwlinkd）。
    let _other = InstanceLock::acquire_at(&fx.lock_file).expect("别的实例占着机器级闸门");

    let out = fx.run();
    assert!(
        !out.status.success(),
        "同机已有 gwlinkd 时，第二份配置也必须被拒"
    );
    let stderr = err_text(&out.stderr);
    assert!(stderr.contains("已经有另一个 wist-gwlinkd"), "{stderr}");
    assert!(
        stderr.contains("WIST_GWLINKD_LOCK_FILE"),
        "错误里要给出逃生口：{stderr}"
    );
    // 机器级闸门都没过 → 部署级锁根本不该被创建，更不该写身份目录。
    assert!(
        fx.state_entries().is_empty(),
        "被机器级闸门拒掉时不应动 state 目录：{:?}",
        fx.state_entries()
    );
    assert!(
        !Path::new(&fx.state_dir)
            .join(wist_gwlinkd::state::LOCK_FILE)
            .exists(),
        "机器级闸门在前：不该留下部署级锁文件"
    );
    fx.cleanup();
}

#[test]
fn run_is_not_blocked_when_no_other_instance_holds_the_lock() {
    let fx = setup("free");
    let out = fx.run();
    let stderr = err_text(&out.stderr);
    assert!(
        !stderr.contains("已经有另一个 wist-gwlinkd") && !stderr.contains("已有网关常驻在跑"),
        "没人持锁时不能误判成重复实例：{stderr}"
    );
    // 没有接入券 → 首跑失败；那是**过了**单实例锁之后的事，与本用例无关。
    assert!(
        !out.status.success(),
        "无券首跑应失败；stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    fx.cleanup();
}

/// 闸门不能变成「粘住的脏状态」：前一个释放/退出后，下一个能正常过闸门。
#[test]
fn run_starts_after_the_previous_instance_released_the_gate() {
    let fx = setup("release");
    {
        let held = InstanceLock::acquire_at(&fx.lock_file).expect("先占着");
        let refused = fx.run();
        assert!(!refused.status.success(), "占着时必须被拒");
        drop(held);
    }

    let out = fx.run();
    let stderr = err_text(&out.stderr);
    assert!(
        !stderr.contains("已经有另一个 wist-gwlinkd") && !stderr.contains("已有网关常驻在跑"),
        "释放后不能再报重复实例：{stderr}"
    );
    fx.cleanup();
}

/// 闸门**不可用**（锁文件打不开）不是「已有实例」：错误文案与 reason 都得能区分，
/// 否则排障的人会去找一个不存在的 gwlinkd 进程。
#[test]
fn run_reports_an_unusable_gate_as_a_state_problem_not_another_instance() {
    let fx = setup("unavailable");
    let unusable = fx.dir.join("missing-dir").join("machine.lock");

    let out = fx.run_with_lock(&unusable);
    assert!(!out.status.success(), "锁文件打不开时必须拒启");
    let stderr = err_text(&out.stderr);
    assert!(stderr.contains("打不开单实例锁文件"), "{stderr}");
    assert!(
        !stderr.contains("已经有另一个 wist-gwlinkd"),
        "不能把环境问题说成「已有实例」：{stderr}"
    );
    assert!(
        stderr.contains("state"),
        "reason 应归到本地状态（state）而不是 already running：{stderr}"
    );
    fx.cleanup();
}

/// 部署级锁同样要区分「已有实例」与「状态目录不可用」：`state_dir` 指向一个普通文件
/// （建不了目录）时，报的必须是环境问题，而不是「已有网关常驻在跑」。
#[test]
fn run_reports_an_unusable_state_dir_as_a_state_problem_not_another_instance() {
    let fx = setup("bad-state-dir");
    let not_a_dir = fx.dir.join("state-file");
    std::fs::write(&not_a_dir, b"not a dir").expect("造一个普通文件冒充 state_dir");
    std::fs::write(
        &fx.config,
        format!("state_dir = \"{}\"\n", not_a_dir.display()),
    )
    .expect("config");

    let out = fx.run();
    assert!(!out.status.success(), "状态目录不可用时必须拒启");
    let stderr = err_text(&out.stderr);
    assert!(
        !stderr.contains("已有网关常驻在跑"),
        "不能把状态目录问题说成「已有实例」：{stderr}"
    );
    assert!(
        stderr.contains("state"),
        "reason 应归到本地状态（state）：{stderr}"
    );
    fx.cleanup();
}

/// 真·双进程：第一个 `run` **真的在跑**（停在「等待页面接入」循环里），第二个用**另一份配置**
/// （自己的 `state_dir`）也必须被拒 —— 这才是「一个操作系统只能有一个」，而不是「一个配置只能有一个」。
///
/// 最后 `kill -9` 第一个：锁由内核释放，不需要人工清锁（也没有陈旧锁可清）。
#[test]
fn two_real_processes_cannot_coexist_and_the_gate_is_released_by_kill() {
    let first = setup("live-first");
    let second = setup("live-second");

    // 让第一个**不自己退出**：配了环回自述面又没有券 → `first_run` 进 wait_for_gateway_request 循环。
    // 端口不通没关系（每轮只记 warn），进程会一直留着 —— 正是我们要的「真常驻」。
    std::fs::write(
        &first.config,
        format!(
            "state_dir = \"{}\"\ngateway_self_endpoint = \"http://127.0.0.1:9\"\ngateway_id = \"gw-live\"\n",
            first.state_dir.display()
        ),
    )
    .expect("config");

    let mut daemon = LiveDaemon::spawn(&first);
    wait_until_lock_holder_is(&daemon, &first, Duration::from_secs(20));

    let refused = second.run_with_lock(&first.lock_file);
    assert!(!refused.status.success(), "同机第二个 gwlinkd 必须被拒");
    let stderr = err_text(&refused.stderr);
    assert!(
        stderr.contains("已经有另一个 wist-gwlinkd"),
        "拒绝应来自**机器级**闸门：{stderr}"
    );
    assert!(
        !stderr.contains("已有网关常驻在跑"),
        "第二个用的是自己的 state 目录，不该是部署级锁拒的：{stderr}"
    );
    assert!(
        second.state_entries().is_empty(),
        "被机器级闸门拒掉时不应动 state 目录：{:?}",
        second.state_entries()
    );
    assert!(
        daemon.is_running(),
        "拒绝必须发生在第一个**仍在运行**时（不然闸门没被真正验证）"
    );

    daemon.kill();
    let reacquired = wait_for_free_gate(&first.lock_file, Duration::from_secs(10));
    drop(reacquired);

    first.cleanup();
    second.cleanup();
}

/// 真常驻句柄：用例失败 / panic 时也要把子进程收掉，不留后台 gwlinkd。
struct LiveDaemon {
    child: Child,
    log: PathBuf,
}

impl LiveDaemon {
    fn spawn(fx: &Fixture) -> Self {
        let log = fx.dir.join("daemon.err");
        let sink = std::fs::File::create(&log).expect("daemon 日志");
        let child = Command::new(env!("CARGO_BIN_EXE_wist-gwlinkd"))
            .arg("run")
            .env("WIST_GWLINKD_CONFIG", &fx.config)
            .env("WIST_GWLINKD_LOCK_FILE", &fx.lock_file)
            .env_remove("WIST_GWLINKD_PUB_CONFIG")
            .env_remove("WIST_GWLINKD_ENV_FILE")
            .env_remove("WIST_GWLINKD_LINK_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::from(sink))
            .spawn()
            .expect("spawn wist-gwlinkd run");
        Self { child, log }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    /// 仍在跑吗（`try_wait` 会顺手收尸，但不阻塞）。
    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LiveDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 等常驻把**自己的 pid** 写进机器级锁文件（= 闸门已拿到）。
///
/// 刻意不用 flock 探测：探测本身要抢锁，会在“常驻正要拿锁”的窗口里把它挤掉（直接造成用例抖动）。
fn wait_until_lock_holder_is(daemon: &LiveDaemon, fx: &Fixture, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(&fx.lock_file)
            && text.trim() == daemon.id().to_string()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "常驻没在预期时间内拿到机器级闸门；daemon 日志：\n{}",
            daemon.log_text()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// 等闸门重新空闲（`kill -9` 后由内核释放）：拿得到就返回持有句柄。
fn wait_for_free_gate(path: &Path, timeout: Duration) -> InstanceLock {
    let deadline = Instant::now() + timeout;
    loop {
        match InstanceLock::acquire_at(path) {
            Ok(lock) => return lock,
            Err(err) => {
                assert!(
                    Instant::now() < deadline,
                    "kill 之后闸门仍未释放（陈旧锁？）：{err}"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}
