//! 宿主侧写盘的**挂载策略**（与 `gops sys perms` 同口径）。
//!
//! 落盘的模式不是"随便挑一个安全值"：`gops run start` 的 `perms_preflight` 见到待修项就
//! **拒绝启动**，所以凡落在 bind 挂载里的文件 / 目录，模式必须与契约一致 ——
//! 只读挂载 `2750/0640`、读写挂载 `2770/0660`。
//!
//! 这条约束被违反过两次，代价都是「整栈停在停止态」：
//! - 0.18.1：交付包（`<PACKAGE_DIR>/agents`）写死 `0644`；
//! - 0.18.2：业务身份 `.env` 写死 `0644`。
//!
//! 所以策略只留这一份实现：**调用点不要再写常量**。

use std::path::Path;

/// 挂载策略 → `(目录模式, 文件模式)`。
///
/// 判据是**最近的已存在目录**（典型是挂载点本身）是否已按读写对齐：
/// `setgid + 组可写` = 读写（`2770`），其余一律按只读（`2750`）。
///
/// 为什么不看目标目录自己的模式：它可能是我们刚用 `create_dir_all` 建的
/// （umask 002 下是 `0775`），那会把**只读**挂载误判成读写，`gops run start` 照样拒绝启动。
#[cfg(unix)]
pub fn mount_policy(dir: &Path) -> std::io::Result<(u32, u32)> {
    use std::os::unix::fs::PermissionsExt;
    let mut candidate = Some(dir);
    while let Some(path) = candidate {
        match std::fs::metadata(path) {
            Ok(meta) => {
                let mode = meta.permissions().mode();
                let read_write = mode & 0o2000 != 0 && mode & 0o020 != 0;
                return Ok(if read_write {
                    (0o2770, 0o660)
                } else {
                    (0o2750, 0o640)
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                candidate = path.parent();
            }
            Err(err) => return Err(err),
        }
    }
    Ok((0o2750, 0o640))
}

#[cfg(not(unix))]
pub fn mount_policy(_dir: &Path) -> std::io::Result<(u32, u32)> {
    Ok((0, 0))
}

/// 设置模式（非 unix 平台为空操作：这些环境本来也没有这套对齐）。
#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gwlinkd-host-mode-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn read_write_mount_yields_2770_and_0660() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("rw");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o2770)).unwrap();
        assert_eq!(mount_policy(&dir).unwrap(), (0o2770, 0o660));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没有 setgid 的目录（umask 造出来的 0775 / 0755）一律按只读：宁可收紧，也不能
    /// 把只读挂载判成读写 —— 否则写出来的 0660 仍然过不了 `gops sys perms`。
    #[cfg(unix)]
    #[test]
    fn plain_directory_falls_back_to_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("ro");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(mount_policy(&dir).unwrap(), (0o2750, 0o640));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目标还不存在时，用**最近的已存在祖先**判定（投放目录首次交付就是这种情形）。
    #[cfg(unix)]
    #[test]
    fn missing_target_walks_up_to_an_existing_parent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("walk");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o2770)).unwrap();
        let nested = dir.join("packages/agents");
        assert_eq!(mount_policy(&nested).unwrap(), (0o2770, 0o660));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
