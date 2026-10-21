//! 单实例闸门：**一台机器（一个操作系统）上只允许一个 `wist-gwlinkd` 在跑**。
//!
//! 与 [`crate::state::acquire_single_instance_lock`] 的分工：
//!
//! - **本模块 = 机器级**：锁文件落点**与配置无关**（默认 `/tmp/wist-gwlinkd.lock`）。两份配置、
//!   两个 cwd、system 作用域 + user 作用域各装一份 —— 只要还在同一台机器上，第二个都起不来。
//!   这是「只能有一个」这条硬要求的地方。
//! - `state_dir/gwlinkd.lock` = **部署级**：回答的是「**这份部署**在跑吗」，`unlink` 据它拒绝
//!   边跑边删、`service status` 据它判活。两把锁**都**持有，语义不同，别合并。
//!
//! 为什么不用 pidfile / 端口：pidfile 会把崩溃留成需要人清掉的脏状态（"看起来还在跑"正是最想
//! 消灭的错觉）；`flock` 挂在打开的文件描述上，进程一死（含 `kill -9`）内核自动释放。端口更不行
//! —— 两个实例完全可以配成不同端点，那时冲突只是表象。
//!
//! 与 `wist-gateway` 的同名闸门（`src/infra/single_instance.rs`）**同形同口径**：锁文件都在
//! `/tmp`、都用 `WIST_*_LOCK_FILE` 作逃生口。
//!
//! 边界（诚实说明，不假装它管得比实际宽）：
//!
//! - 锁文件在 `/tmp`，管的是**同一台机器上的裸进程**。容器有各自的 `/tmp`（gwlinkd 本来就是
//!   宿主侧常驻，不进容器），容器之间那一层不是这里能解决的；`/tmp` 必须是**本机文件系统**
//!   （NFS 等网络文件系统上的 `flock` 不保证互斥）；
//! - **不能用 `env::temp_dir()` 取默认路径**：macOS 的 `TMPDIR` 是**按用户**的，用它等于把
//!   「一个 OS 一个实例」悄悄降级成「一个用户一个实例」；
//! - 非 unix 目标（不在发布矩阵里：只发 linux-musl ×2 与 apple-darwin）不提供锁 —— 那是**已知
//!   缺口**，不是"刚好也行"；
//! - 锁文件在宿主的 `/tmp`：服务定义**不要**加 systemd 的 `PrivateTmp=yes`（那会把 `/tmp` 变成服务私有
//!   命名空间，闸门就退化成「按服务实例」）；`WIST_GWLINKD_LOCK_FILE` 若给的是**相对路径**，它就按进程
//!   cwd 解析 —— 那等于自己把闸门拆了，只为确实要并行两套时用；
//! - `/tmp` 的按龄清理（systemd-tmpfiles / macOS periodic）会把锁文件当垃圾删掉，删掉之后
//!   「新建 + `flock`」就是**另一个 inode**，闸门会漏。所以常驻要定期 [`InstanceLock::refresh`]：
//!   给锁文件续 mtime；发现被删 / 被换就重开重锁，拿不回来即判闸门失效。
//!
//! `WIST_GWLINKD_LOCK_FILE` 可覆盖锁文件路径：那是**故意的逃生口**（同一台机器上确实要并行两套
//! 时用，测试也走它）。默认值不提供任何"自动让路" —— 让路意味着两个实例真的同时跑起来了。

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 默认锁文件：`/tmp` 才是**按机器**共享的那个目录（见模块头）。
pub const DEFAULT_LOCK_FILE: &str = "/tmp/wist-gwlinkd.lock";
/// 覆盖锁文件路径的环境变量（逃生口，见模块头）。
pub const ENV_LOCK_FILE: &str = "WIST_GWLINKD_LOCK_FILE";

/// 拿不到闸门的**两类**原因：分清它们是给调用方落不同错误 reason 用的 ——
/// 「已经有一个在跑」是**正常结局**（退出即可，服务管理器拉起旧的就够），
/// 「锁文件根本打不开」是**环境 / 配置问题**（权限、路径、符号链接），把它当成「已有实例」
/// 会让排障的人一直去找进程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    /// 闸门已被同机另一个 gwlinkd 持有（或已被别的进程接管）。
    Held(String),
    /// 闸门本身不可用：打不开 / 加不了锁 / 被换成符号链接……
    Unavailable(String),
}

impl LockError {
    /// 给人看的那句话（两种都自带处置建议）。
    pub fn detail(&self) -> &str {
        match self {
            Self::Held(detail) | Self::Unavailable(detail) => detail,
        }
    }
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

impl std::error::Error for LockError {}

/// 机器级闸门的持有句柄：**活到进程结束即持锁**，无需显式释放。
#[derive(Debug)]
pub struct InstanceLock {
    /// 持锁的那个 fd。字段本身不读，它的存在就是锁 —— 所以带下划线前缀防 unused 告警。
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// 按默认（或 `WIST_GWLINKD_LOCK_FILE` 覆盖）的路径占位。
    ///
    /// 失败回的是**给人看的**一句话（区分「已被持有」与「闸门不可用」，见 [`LockError`]）。
    pub fn acquire() -> Result<Self, LockError> {
        Self::acquire_at(&resolve_lock_path(env::var_os(ENV_LOCK_FILE)))
    }

    /// 在指定路径占位（测试与逃生口都走它）。
    pub fn acquire_at(path: &Path) -> Result<Self, LockError> {
        let (mut file, writable) = open_lock_file(path)?;

        if !try_lock_exclusive(&file).map_err(|err| {
            LockError::Unavailable(format!("对 {} 加锁失败：{err}", path.display()))
        })? {
            return Err(LockError::Held(held_message(path, &mut file)));
        }
        record_pid(&mut file, writable);

        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// 实际用的锁文件路径（启动日志里打出来，省得人猜）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 定期续命（常驻主循环每拍调一次）：`/tmp` 的按龄清理会把锁文件删掉，删了之后
    /// 「新建 + `flock`」是另一个 inode —— 闸门就漏了。所以：
    ///
    /// - 锁文件还是**我们持有的那个 inode** → 把 mtime 顶到当下（清理按 atime/mtime/ctime
    ///   最新者判龄）；
    /// - 被删 / 被换（inode 变了）→ 重开重锁；**重锁拿不到 = 闸门已被别的进程接管**，回错让
    ///   调用方退出 —— 宁可让服务管理器重启，也不假装自己还是唯一实例。
    pub fn refresh(&mut self) -> Result<(), LockError> {
        if let Ok(meta) = std::fs::metadata(&self.path)
            && same_inode(&meta, &self.file)
        {
            // 没写权限（锁文件归别人）也没关系：闸门靠 flock，mtime 只是给清理用的提示。
            let _ = self
                .file
                .set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()));
            return Ok(());
        }

        let (mut file, writable) = open_lock_file(&self.path)?;
        if !try_lock_exclusive(&file).map_err(|err| {
            LockError::Unavailable(format!("对 {} 加锁失败：{err}", self.path.display()))
        })? {
            return Err(LockError::Held(format!(
                "单实例闸门失效：锁文件 {} 被外部删除/替换，且新的锁已被另一个进程持有（{}）。\
                 本进程不再是唯一实例，退出以避免双实例并存",
                self.path.display(),
                read_holder_pid(&mut file)
                    .map(|pid| format!("pid {pid}"))
                    .unwrap_or_else(|| "持有者未知".to_string())
            )));
        }
        record_pid(&mut file, writable);
        self.file = file;
        Ok(())
    }
}

/// 锁文件路径：显式给了（且非空）就用它，否则回落默认。
///
/// 单独的纯函数是为了能测 —— 直接读 env 的测试在并行测试里既不稳定也会互相干扰。
fn resolve_lock_path(explicit: Option<OsString>) -> PathBuf {
    explicit
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_LOCK_FILE))
}

/// 打开锁文件，回「句柄 + 能不能写」。
///
/// 拿不到**写**权限时退一步用只读打开：`flock` 不在乎打开模式，闸门照旧成立 —— 只是记不了
/// 自己的 pid。这一条很实际：有人用 root 起过一次（dev 机器上很常见），锁文件就归了 root，
/// 之后以部署用户再起如果非要写权限，会以一句莫名其妙的 “Permission denied” 把人挡在门外
/// —— 那是**锁文件的权限**问题，不是「已有实例」。
fn open_lock_file(path: &Path) -> Result<(File, bool), LockError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // 不跟随符号链接：锁文件路径可预测，`/tmp` 又人人可写 —— 不挡住的话，别人把
        // `/tmp/wist-gwlinkd.lock` 指向 `/etc/xxx`，以 root 运行的本进程就会去写那个文件。
        options.custom_flags(libc::O_NOFOLLOW);
        // 显式 0644：锁文件里只有一个 pid，但它要能被**其他用户**读到 —— 读不到就只能报
        // 「闸门不可用」（fail-closed，仍安全但文案会把人往错方向引）；由 umask=077 的服务跑过一次
        // 就会变成 0600。写权限仍归创建者，其它用户走只读打开 + flock。
        options.mode(0o644);
    }
    match options.open(path) {
        Ok(file) => Ok((file, true)),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            let mut readonly = OpenOptions::new();
            readonly.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                readonly.custom_flags(libc::O_NOFOLLOW);
            }
            readonly
                .open(path)
                .map(|file| (file, false))
                .map_err(|err| LockError::Unavailable(unopenable(path, &err)))
        }
        Err(err) => Err(LockError::Unavailable(unopenable(path, &err))),
    }
}

/// 「锁文件打不开」的运维话术（与「已有实例」分开说，否则人一直去找进程）。
fn unopenable(path: &Path, err: &io::Error) -> String {
    format!(
        "打不开单实例锁文件 {}：{err}（先看目录/文件权限与是否被换成了符号链接；锁文件在 /tmp，\
         若它归别的用户所有，可用 {ENV_LOCK_FILE} 指到你自己可写的位置）",
        path.display()
    )
}

/// 「已被别人持有」时给运维看的话：谁持着、怎么处置。
fn held_message(path: &Path, file: &mut File) -> String {
    let holder = read_holder_pid(file)
        .map(|pid| format!("pid {pid}"))
        .unwrap_or_else(|| "另一个进程".to_string());
    format!(
        "这台机器上已经有另一个 wist-gwlinkd 在跑（{holder} 持有锁 {}）。\
         一个操作系统上只允许一个实例：先停掉那个（它可能用的是**另一份配置**，\
         光看 state 目录 / 端口会漏），或者——只在你确实要并行两套时——用 {ENV_LOCK_FILE} \
         指到另一个锁文件。",
        path.display()
    )
}

/// 记下自己的 pid 只为**诊断**：出问题时"谁占着锁"要一眼看得到。
/// 只读打开的（锁文件是别人建的）就跳过 —— 拿不到写权限不影响闸门本身。
fn record_pid(file: &mut File, writable: bool) {
    if !writable {
        return;
    }
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = writeln!(file, "{}", std::process::id());
    let _ = file.flush();
}

/// 非阻塞独占锁：拿到回 `Ok(true)`，被别人持有回 `Ok(false)`，其它才是错误。
#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    // SAFETY: fd 取自仍然活着的 `file`；`flock` 只借用这个描述符，不转移所有权、不释放它。
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // EWOULDBLOCK / EAGAIN 是同一个数，但两个名字都写出来，别让读的人以为漏了一种。
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Ok(false),
        _ => Err(err),
    }
}

/// 非 unix：**不提供锁**（发布矩阵里没有非 unix 目标）。返回「拿到了」是**已知缺口**，
/// 不是「这里也管得住」—— 真要支持得换 `LockFileEx` 那一套。
#[cfg(not(unix))]
fn try_lock_exclusive(_file: &File) -> io::Result<bool> {
    Ok(true)
}

/// 磁盘上的锁文件是不是**我们持有的那个**（被删/被换时要能看出来）。
#[cfg(unix)]
fn same_inode(meta: &std::fs::Metadata, file: &File) -> bool {
    use std::os::unix::fs::MetadataExt;
    match file.metadata() {
        Ok(held) => meta.dev() == held.dev() && meta.ino() == held.ino(),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn same_inode(_meta: &std::fs::Metadata, _file: &File) -> bool {
    true
}

/// 读出锁文件里记的持有者 pid（可能读不到：持有者刚拿到锁还没来得及写）。
fn read_holder_pid(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gwlinkd-instance-lock-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn the_second_instance_is_refused_and_told_who_holds_the_lock() {
        let dir = temp_dir("second");
        let path = dir.join("gwlinkd.lock");

        let first = InstanceLock::acquire_at(&path).expect("第一个实例应当拿到锁");
        assert_eq!(first.path(), path.as_path());

        match InstanceLock::acquire_at(&path).expect_err("第二个实例必须被拒") {
            LockError::Held(detail) => {
                assert!(detail.contains("已经有另一个 wist-gwlinkd"), "{detail}");
                assert!(
                    detail.contains(&std::process::id().to_string()),
                    "错误里要报出持有者 pid，人才知道去停谁：{detail}"
                );
                assert!(
                    detail.contains(ENV_LOCK_FILE),
                    "错误里要给出逃生口（有意并行两套时用）：{detail}"
                );
            }
            other => panic!("被别人持有应当报 Held，不是 {other:?}"),
        }

        // 前一个一放，后一个就能拿到 —— 锁跟着**进程/句柄**走，不跟崩溃残留走。
        drop(first);
        InstanceLock::acquire_at(&path).expect("前一个释放后应当能再拿到");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_lock_path_falls_back_to_the_default_when_unset_or_empty() {
        assert_eq!(resolve_lock_path(None), PathBuf::from(DEFAULT_LOCK_FILE));
        assert_eq!(
            resolve_lock_path(Some(OsString::new())),
            PathBuf::from(DEFAULT_LOCK_FILE)
        );
        assert_eq!(
            resolve_lock_path(Some(OsString::from("/tmp/other.lock"))),
            PathBuf::from("/tmp/other.lock")
        );
    }

    /// 默认锁文件必须是**按机器**共享的那个目录：`temp_dir()` 在 macOS 上按用户，
    /// 用它就等于把「一个 OS 一个实例」悄悄降级成「一个用户一个实例」。
    #[test]
    fn the_default_lock_file_is_host_wide_not_per_user() {
        assert!(
            DEFAULT_LOCK_FILE.starts_with("/tmp/"),
            "{DEFAULT_LOCK_FILE}"
        );
        let per_user = env::temp_dir();
        if per_user != Path::new("/tmp") {
            assert!(
                !DEFAULT_LOCK_FILE.starts_with(per_user.display().to_string().as_str()),
                "默认锁文件落进了按用户的临时目录（macOS 的 TMPDIR）：{DEFAULT_LOCK_FILE}"
            );
        }
    }

    #[test]
    fn the_holder_pid_is_recorded_for_diagnosis() {
        let dir = temp_dir("pid");
        let path = dir.join("gwlinkd.lock");
        let _lock = InstanceLock::acquire_at(&path).expect("拿锁");
        let recorded = fs::read_to_string(&path).expect("读锁文件");
        assert_eq!(recorded.trim(), std::process::id().to_string());
        let _ = fs::remove_dir_all(dir);
    }

    /// 锁文件归了别人（典型：有人用 root 起过一次）也不该把人挡在门外：
    /// 只读打开也能 flock —— 闸门照旧，只是不自报 pid。
    #[cfg(unix)]
    #[test]
    fn a_lock_file_we_cannot_write_still_works() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("readonly");
        let path = dir.join("gwlinkd.lock");
        fs::write(&path, "12345\n").expect("预置锁文件");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("只读化");

        let lock = InstanceLock::acquire_at(&path).expect("只读的锁文件也应当能占位");
        assert_eq!(fs::read_to_string(&path).expect("读锁文件").trim(), "12345");

        // 但**互斥**一点没少：第二个照样被拒。
        match InstanceLock::acquire_at(&path).expect_err("第二个必须被拒") {
            LockError::Held(detail) => {
                assert!(detail.contains("已经有另一个 wist-gwlinkd"), "{detail}")
            }
            other => panic!("只读锁文件上的竞争也应当是 Held，不是 {other:?}"),
        }

        drop(lock);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("还原权限");
        let _ = fs::remove_dir_all(dir);
    }

    /// 闸门**不可用**（目录不存在 / 路径不可写 / 被换成符号链接）与「已有实例」必须分开报：
    /// 都归到「另一个实例在跑」的话，排障的人会一直去找一个根本不存在的进程。
    #[test]
    fn an_unopenable_lock_path_is_unavailable_not_held() {
        let dir = temp_dir("unavailable");
        let path = dir.join("missing-dir").join("gwlinkd.lock");

        match InstanceLock::acquire_at(&path).expect_err("父目录不存在应当失败") {
            LockError::Unavailable(detail) => {
                assert!(detail.contains("打不开单实例锁文件"), "{detail}");
                assert!(!detail.contains("已经有另一个"), "{detail}");
            }
            other => panic!("环境问题应当报 Unavailable，不是 {other:?}"),
        }
        let _ = fs::remove_dir_all(dir);
    }

    /// `/tmp/wist-gwlinkd.lock` 路径可预测，而 `/tmp` 人人可写：锁文件被换成**符号链接**时
    /// 不能跟着走（否则以 root 运行时会把 pid 写进链接目标），而是直接拒（Unavailable）。
    #[cfg(unix)]
    #[test]
    fn a_symlinked_lock_file_is_refused() {
        let dir = temp_dir("symlink");
        let target = dir.join("target.txt");
        fs::write(&target, "do not touch\n").expect("目标文件");
        let path = dir.join("gwlinkd.lock");
        std::os::unix::fs::symlink(&target, &path).expect("造符号链接");

        match InstanceLock::acquire_at(&path).expect_err("符号链接必须被拒") {
            LockError::Unavailable(detail) => {
                assert!(detail.contains("打不开单实例锁文件"), "{detail}")
            }
            other => panic!("符号链接应当报 Unavailable，不是 {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(&target).expect("读目标文件").trim(),
            "do not touch",
            "不该往链接目标里写东西"
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// 逃生口的语义：**不同锁文件就是两道闸门** —— 改动不能让 `WIST_GWLINKD_LOCK_FILE`
    /// 只能指向同一把锁（有意并行两套时靠它）。
    #[test]
    fn different_lock_paths_do_not_exclude_each_other() {
        let dir = temp_dir("two-paths");
        let a = dir.join("a.lock");
        let b = dir.join("b.lock");

        let first = InstanceLock::acquire_at(&a).expect("a 应当拿到");
        let second = InstanceLock::acquire_at(&b).expect("b 是另一道闸门，应当也能拿到");
        drop((first, second));
        let _ = fs::remove_dir_all(dir);
    }

    /// `/tmp` 的按龄清理把锁文件删掉之后：`refresh` 必须把它建回来并重新占住 ——
    /// 否则「删文件 → 新实例新建自己的锁文件」就等于闸门自动失效。
    #[test]
    fn refresh_recreates_a_lock_file_removed_by_cleanup() {
        let dir = temp_dir("refresh");
        let path = dir.join("gwlinkd.lock");
        let mut lock = InstanceLock::acquire_at(&path).expect("拿锁");

        fs::remove_file(&path).expect("模拟 /tmp 清理");
        lock.refresh().expect("锁文件被删后应当能重建并重新占住");

        assert!(path.exists(), "refresh 要把锁文件建回来");
        match InstanceLock::acquire_at(&path).expect_err("重建后仍须独占") {
            LockError::Held(detail) => {
                assert!(detail.contains("已经有另一个 wist-gwlinkd"), "{detail}")
            }
            other => panic!("重建后应当是 Held，不是 {other:?}"),
        }

        // 新一轮 refresh 不能把锁自己放掉（幂等：文件还在、inode 未变）。
        lock.refresh().expect("没变化时 refresh 应当幂等");
        assert!(
            matches!(
                InstanceLock::acquire_at(&path).expect_err("幂等 refresh 后仍须独占"),
                LockError::Held(_)
            ),
            "幂等 refresh 不能把闸门放掉"
        );

        drop(lock);
        let _ = fs::remove_dir_all(dir);
    }

    /// 锁文件被删且**被别人先占了**：本进程已经不再是唯一实例，必须报错（由调用方退出），
    /// 不能装作没事继续跑。
    #[test]
    fn refresh_reports_a_gate_taken_over_by_another_process() {
        let dir = temp_dir("taken");
        let path = dir.join("gwlinkd.lock");
        let mut lock = InstanceLock::acquire_at(&path).expect("拿锁");

        fs::remove_file(&path).expect("模拟 /tmp 清理");
        let _intruder = InstanceLock::acquire_at(&path).expect("新文件由别人占住");

        match lock.refresh().expect_err("闸门被接管必须报错") {
            LockError::Held(detail) => assert!(detail.contains("单实例闸门失效"), "{detail}"),
            other => panic!("被接管应当报 Held（不是环境问题）：{other:?}"),
        }

        drop(lock);
        let _ = fs::remove_dir_all(dir);
    }
}
