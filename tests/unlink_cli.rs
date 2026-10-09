//! `wist-gwlinkd unlink` 的 **bin 层（独立进程）**用例。
//!
//! 起临时 `WIST_GWLINKD_CONFIG`（含 `state/` 与中心信任锚），直接 spawn 编译出的 `wist-gwlinkd`
//! 二进制，断言**退出码**与**文件系统产物** —— 覆盖 lib 单测够不到的 CLI 接线：参数解析、stdout/stderr、
//! 退出码、以及「用 `WIST_GWLINKD_CONFIG` 取配置」这条路径。
//!
//! 与 `src/unlink.rs` 的单测互补：那边测**逻辑**（不落盘的边界、顺序、容忍 NotFound…），
//! 这边测**接线**（真的把二进制跑起来、真的按 env 找到配置、真的按开关删/留）。

use std::path::PathBuf;
use std::process::{Command, Output};

use wist_gwlinkd::unlink::LINK_STATE_FILES;

fn temp_dir(tag: &str) -> PathBuf {
    // 同一集成测试二进制里的各测试跑在同一进程：靠 tag 区分目录；先清掉上次残留。
    let dir = std::env::temp_dir().join(format!("gwlinkd-unlink-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// 一个最小可用的 gwlinkd home：配置（可选 `link_token`）+ `state/`（空）+ 中心信任锚。
struct Fixture {
    dir: PathBuf,
    config: PathBuf,
    state_dir: PathBuf,
    anchor: PathBuf,
}

fn setup(tag: &str, link_token: Option<&str>) -> Fixture {
    let dir = temp_dir(tag);
    let state_dir = dir.join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    let anchor = dir.join("control-center.pem");
    std::fs::write(&anchor, "-----BEGIN CERTIFICATE-----\n").expect("ca");
    let mut text = format!(
        "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"{}\"\nstate_dir = \"{}\"\n",
        anchor.display(),
        state_dir.display()
    );
    if let Some(token) = link_token {
        text.push_str(&format!("link_token = \"{token}\"\n"));
    }
    let config = dir.join("gwlinkd.toml");
    std::fs::write(&config, text).expect("config");
    Fixture {
        dir,
        config,
        state_dir,
        anchor,
    }
}

impl Fixture {
    /// 在 `state/` 里造齐全部注册态文件（清单与实现共享）。
    fn touch_state(&self) {
        for name in LINK_STATE_FILES {
            std::fs::write(self.state_dir.join(name), b"x").expect("state file");
        }
    }

    fn state_file(&self, name: &str) -> PathBuf {
        self.state_dir.join(name)
    }

    /// 以独立进程跑 `<bin> unlink …`，配置走 `WIST_GWLINKD_CONFIG`。
    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wist-gwlinkd"));
        command.args(args).env("WIST_GWLINKD_CONFIG", &self.config);
        command.output().expect("spawn wist-gwlinkd")
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn out_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn unlink_removes_registration_state_and_link_token_but_keeps_the_anchor() {
    let fx = setup("full", Some("link_abc"));
    fx.touch_state();

    let out = fx.run(&["unlink"]);
    assert!(
        out.status.success(),
        "应成功；stderr={}",
        out_text(&out.stderr)
    );
    let stdout = out_text(&out.stdout);
    assert!(stdout.contains("已断开"), "{stdout}");

    for name in LINK_STATE_FILES {
        assert!(!fx.state_file(name).exists(), "{name} 应被删");
    }
    assert!(
        !std::fs::read_to_string(&fx.config)
            .expect("read config")
            .contains("link_token"),
        "link_token 应从配置去掉"
    );
    assert!(fx.anchor.exists(), "信任锚默认保留");
    fx.cleanup();
}

#[test]
fn unlink_dry_run_changes_nothing() {
    let fx = setup("dry", Some("link_abc"));
    fx.touch_state();
    let before = std::fs::read_to_string(&fx.config).expect("read config");

    let out = fx.run(&["unlink", "--dry-run"]);
    assert!(out.status.success(), "stderr={}", out_text(&out.stderr));
    assert!(
        out_text(&out.stdout).contains("DRY_RUN"),
        "{}",
        out_text(&out.stdout)
    );

    for name in LINK_STATE_FILES {
        assert!(fx.state_file(name).exists(), "{name} 不应被动");
    }
    assert_eq!(
        std::fs::read_to_string(&fx.config).expect("read config"),
        before,
        "干跑不应改配置"
    );
    fx.cleanup();
}

#[test]
fn unlink_forget_center_removes_the_anchor() {
    let fx = setup("forget", None);
    let out = fx.run(&["unlink", "--forget-center"]);
    assert!(out.status.success(), "stderr={}", out_text(&out.stderr));
    assert!(!fx.anchor.exists(), "--forget-center 应删掉中心信任锚");
    fx.cleanup();
}

#[test]
fn unlink_refuses_while_another_holder_has_the_lock() {
    let fx = setup("running", Some("link_abc"));
    fx.touch_state();
    // 本测试进程持有单实例锁 → 子进程里的 unlink 应拒绝（跨进程 flock）。
    let _guard =
        wist_gwlinkd::state::acquire_single_instance_lock(&fx.state_dir).expect("acquire lock");

    let out = fx.run(&["unlink"]);
    assert!(!out.status.success(), "运行中应拒绝");
    assert!(
        out_text(&out.stderr).contains("正在运行"),
        "{}",
        out_text(&out.stderr)
    );
    for name in LINK_STATE_FILES {
        assert!(
            fx.state_file(name).exists(),
            "{name} 不应被动（拒绝时零副作用）"
        );
    }
    fx.cleanup();
}

#[test]
fn unlink_unknown_arg_exits_2() {
    let fx = setup("badarg", None);
    let out = fx.run(&["unlink", "--bogus"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr={}",
        out_text(&out.stderr)
    );
    assert!(
        out_text(&out.stderr).contains("未知参数"),
        "{}",
        out_text(&out.stderr)
    );
    fx.cleanup();
}

#[test]
fn unlink_help_exits_0() {
    let fx = setup("help", None);
    let out = fx.run(&["unlink", "--help"]);
    assert!(out.status.success());
    assert!(
        out_text(&out.stdout).contains("用法"),
        "{}",
        out_text(&out.stdout)
    );
    fx.cleanup();
}

#[test]
fn unlink_without_config_fails() {
    let dir = temp_dir("noconfig");
    let mut command = Command::new(env!("CARGO_BIN_EXE_wist-gwlinkd"));
    command
        .arg("unlink")
        .env("WIST_GWLINKD_CONFIG", dir.join("does-not-exist.toml"));
    let out = command.output().expect("spawn");
    assert!(!out.status.success());
    assert!(
        out_text(&out.stderr).contains("配置不可读"),
        "{}",
        out_text(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
