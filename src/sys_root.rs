//! gops **系统根**探测：`gops run diagnose|stop|start` 的 cwd、`gops prj upgrade NAME` 的 NAME 都靠它。
//!
//! 与「gops **工程根**」（含 `ops-prj.yml`，`gops prj upgrade` 的 cwd）要分开：
//! 工程根通常挂着若干**系统子目录**，两者混用会得到「系统未定义 diagnose 流程」「未知系统或无效版本 spec」
//! 这类看着像配置问题、其实是跑错目录 / 传错名字的结论。

use std::path::{Path, PathBuf};

use crate::config::Config;

/// 系统根标记（与 gops `SysOperator::load_kind` 一致）：`<系统根>/sys/sys_model.yml`。
///
/// 只有这一种布局 —— `gops run` / `gops prj` 都只认它。
pub(crate) const SYS_MODEL_PATH: &str = "sys/sys_model.yml";

/// 目录是不是 gops 的**系统根**。
pub(crate) fn is_system_root(dir: &Path) -> bool {
    dir.join(SYS_MODEL_PATH).is_file()
}

/// `root` 下“看起来是系统”的子目录（按名字排序）。
pub(crate) fn system_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && is_system_root(path))
        .collect();
    found.sort();
    found
}

/// 目录名（报错里用）。
pub(crate) fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string())
}

/// 系统根 `sys/sys_model.yml` 的顶层 `name:`（gops 就是拿它当 `prj upgrade NAME` 的）。
///
/// 不引 YAML 依赖：只认顶格那一行 `name: <x>`，缩进行（嵌套键）一律不算。
pub(crate) fn model_name(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(SYS_MODEL_PATH)).ok()?;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("name:") else {
            continue;
        };
        let name = rest.trim().trim_matches(['\'', '"']).trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

/// 这个系统目录是否“叫” `name`：目录名 **或** `sys_model.yml` 的 `name:` 任一命中。
///
/// 两边都算，是因为两种口径都存在：`gops prj upgrade NAME` 认**模型名**，
/// 而人的直觉（和栈目录）通常是**目录名**。
pub(crate) fn system_matches(dir: &Path, name: &str) -> bool {
    dir_name(dir) == name || model_name(dir).as_deref() == Some(name)
}

/// 工程里的候选系统：`root` 下的系统子目录；`root` **本身就是系统根**时也算一个（兼容旧布局）。
pub(crate) fn systems_under(root: &Path) -> Vec<PathBuf> {
    let mut found = system_dirs(root);
    if is_system_root(root) {
        found.insert(0, root.to_path_buf());
    }
    found
}

/// 按名字在工程里定位系统（规则见 [`system_matches`]）。
pub(crate) fn find_system(root: &Path, name: &str) -> Option<PathBuf> {
    let by_dir = root.join(name);
    if is_system_root(&by_dir) {
        return Some(by_dir);
    }
    systems_under(root)
        .into_iter()
        .find(|dir| system_matches(dir, name))
}

/// 候选系统转成一行（回执里要能直接看出该配哪个）。
/// 模型名与目录名不同时一并写出 —— `gops prj upgrade NAME` 认的是模型名。
pub(crate) fn list_systems(dirs: &[PathBuf]) -> String {
    if dirs.is_empty() {
        return "（无）".to_string();
    }
    dirs.iter()
        .map(|dir| {
            let name = dir_name(dir);
            match model_name(dir) {
                Some(model) if model != name => format!("{name}（name: {model}）"),
                _ => name,
            }
        })
        .collect::<Vec<_>>()
        .join("、")
}

/// 控制类命令（体检 / 重启网关）真正该用的 **cwd**。
///
/// 为什么不能直接用 `upgrade_project_dir`：它是 **gops 工程根**（`gops prj upgrade` 从 cwd 读
/// `ops-prj.yml`），而 `gops run diagnose|stop|start` 要的是**系统根**（含 `sys/sys_model.yml`）。
///
/// 解析顺序（先精确、后推断，**多个候选不猜**）：
/// 1. 配了 `upgrade_project_name` → 按目录名 / `sys_model.yml` 的 `name:` 对号，
///    对不上就**失败**（显式配了名字却静默换一个系统，比报错更糟）；
/// 2. `upgrade_project_dir` 本身就是系统根 → 用它；
/// 3. 工程根下**唯一**的系统子目录 → 用它；
/// 4. 没有 / 多个 → **失败**，并把候选列进回执（不替人做选择）。
pub(crate) fn control_workdir(config: &Config) -> Result<PathBuf, String> {
    let Some(root) = config.upgrade_project_dir.as_deref() else {
        return Err("未配置工程根：upgrade_project_dir 未设置（gops 从 cwd 解析系统）".to_string());
    };
    if !root.is_dir() {
        return Err(format!("工程根不存在：{} 不是已存在的目录", root.display()));
    }
    // 显式名字优先：**它会拿去对号**，对不上就报（不能因为工程根恰好本身是系统根
    // 就静默用它 —— 那样 diagnose/restart 与 `prj upgrade NAME` 会各选一个系统）。
    if let Some(name) = config
        .upgrade_project_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if let Some(found) = find_system(root, name) {
            return Ok(found);
        }
        return Err(format!(
            "upgrade_project_name = `{name}` 找不到对应系统：{} 下没有同名目录，也没有 name 相同的系统（已找到：{}）",
            root.display(),
            list_systems(&systems_under(root))
        ));
    }
    if is_system_root(root) {
        return Ok(root.to_path_buf());
    }
    let found = system_dirs(root);
    match found.len() {
        1 => Ok(found.into_iter().next().expect("len == 1")),
        0 => Err(format!(
            "找不到系统根：{} 既不是系统根（缺 sys/sys_model.yml），也没有系统子目录",
            root.display()
        )),
        _ => Err(format!(
            "{} 下有多个系统，无法确定是哪一台：{}；请把 upgrade_project_name 配成其中一个的名字",
            root.display(),
            list_systems(&found)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wist-gwlinkd-sysroot-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个系统根（模型名 = 目录名）。
    fn make_system(dir: &Path) {
        std::fs::create_dir_all(dir.join("sys")).unwrap();
        std::fs::write(
            dir.join(SYS_MODEL_PATH),
            format!("name: {}\nkind: docker-compose\n", dir_name(dir)),
        )
        .unwrap();
    }

    fn config_at(state: &Path, project: &Path, name: Option<&str>) -> Config {
        let path = state.join("gwlinkd.toml");
        let name = name
            .map(|name| format!("upgrade_project_name = \"{name}\"\n"))
            .unwrap_or_default();
        std::fs::write(
            &path,
            format!(
                "state_dir = \"{}\"\nupgrade_project_dir = \"{}\"\n{name}",
                state.display(),
                project.display()
            ),
        )
        .unwrap();
        Config::load(&path).expect("config")
    }

    #[test]
    fn a_system_root_is_recognised_by_sys_model_only() {
        let dir = temp_dir("marker");
        assert!(!is_system_root(&dir));
        // 根目录上的 `sys_model.yml` 不是 gops 认的标记（`load_kind` 只看 `sys/`）。
        std::fs::write(dir.join("sys_model.yml"), "name: x\n").unwrap();
        assert!(!is_system_root(&dir), "根目录文件不算");
        make_system(&dir);
        assert!(is_system_root(&dir));
        assert_eq!(model_name(&dir).as_deref(), Some(&*dir_name(&dir)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn model_name_ignores_nested_and_quoted_forms() {
        let dir = temp_dir("model-name");
        std::fs::create_dir_all(dir.join("sys")).unwrap();
        std::fs::write(
            dir.join(SYS_MODEL_PATH),
            "  name: nested\nvender: x\nname: 'quoted-name'\n",
        )
        .unwrap();
        assert_eq!(model_name(&dir).as_deref(), Some("quoted-name"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn control_workdir_uses_a_system_root_directly() {
        let state = temp_dir("cwd-root");
        make_system(&state);
        let config = config_at(&state, &state, None);
        assert_eq!(control_workdir(&config).unwrap(), state);
    }

    #[test]
    fn control_workdir_descends_into_the_only_system() {
        // 现场形态：`upgrade_project_dir` 是 gops 工程根，系统在子目录里。
        let state = temp_dir("cwd-project");
        let project = state.join("prj");
        let system = project.join("wist-gateway-stack");
        make_system(&system);
        std::fs::write(project.join("ops-prj.yml"), "kind: ops\n").unwrap();
        let config = config_at(&state, &project, None);
        assert_eq!(control_workdir(&config).unwrap(), system);
    }

    #[test]
    fn control_workdir_honors_upgrade_project_name() {
        let state = temp_dir("cwd-name");
        let project = state.join("prj");
        let wanted = project.join("stack-b");
        make_system(&project.join("stack-a"));
        make_system(&wanted);
        let config = config_at(&state, &project, Some("stack-b"));
        assert_eq!(control_workdir(&config).unwrap(), wanted);
    }

    #[test]
    fn control_workdir_matches_the_model_name_too() {
        // `upgrade_project_name` 认的是 `sys_model.yml` 的 `name:`（gops 的口径）。
        let state = temp_dir("cwd-model-name");
        let project = state.join("prj");
        let system = project.join("dir-a");
        std::fs::create_dir_all(system.join("sys")).unwrap();
        std::fs::write(
            system.join(SYS_MODEL_PATH),
            "name: wist-gateway-stack\nkind: docker-compose\n",
        )
        .unwrap();
        let config = config_at(&state, &project, Some("wist-gateway-stack"));
        assert_eq!(control_workdir(&config).unwrap(), system);
    }

    #[test]
    fn control_workdir_refuses_to_guess_between_systems() {
        // 多个系统且没配名字：不替人挑（挑错就是把别人的栈停了）。
        let state = temp_dir("cwd-multi");
        let project = state.join("prj");
        make_system(&project.join("stack-a"));
        make_system(&project.join("stack-b"));
        let config = config_at(&state, &project, None);
        let err = control_workdir(&config).unwrap_err();
        assert!(err.contains("多个系统"), "{err}");
        assert!(err.contains("stack-a"), "要列出候选：{err}");
        assert!(err.contains("stack-b"), "要列出候选：{err}");
        assert!(
            err.contains("upgrade_project_name"),
            "要告诉人怎么配：{err}"
        );
    }

    #[test]
    fn control_workdir_fails_when_the_configured_name_is_not_a_system() {
        // 显式配了名字却不是系统 → 报错，不静默换成唯一候选。
        let state = temp_dir("cwd-bad-name");
        let project = state.join("prj");
        make_system(&project.join("stack-a"));
        let config = config_at(&state, &project, Some("typo"));
        let err = control_workdir(&config).unwrap_err();
        assert!(err.contains("typo"), "{err}");
        assert!(err.contains("stack-a"), "要列出实际存在的系统：{err}");
    }

    #[test]
    fn control_workdir_fails_when_there_is_no_system_at_all() {
        let state = temp_dir("cwd-empty");
        let project = state.join("prj");
        std::fs::create_dir_all(&project).unwrap();
        let config = config_at(&state, &project, None);
        let err = control_workdir(&config).unwrap_err();
        assert!(err.contains("sys/sys_model.yml"), "{err}");
    }

    #[test]
    fn control_workdir_fails_without_a_project_dir() {
        let state = temp_dir("cwd-noproject");
        let path = state.join("gwlinkd.toml");
        std::fs::write(&path, format!("state_dir = \"{}\"\n", state.display())).unwrap();
        let config = Config::load(&path).unwrap();
        let err = control_workdir(&config).unwrap_err();
        assert!(err.contains("upgrade_project_dir"), "{err}");
        let _ = std::fs::remove_dir_all(state);
    }

    #[test]
    fn control_workdir_reports_a_missing_project_dir() {
        // 目录不存在与“目录在但没有系统”是两回事：前者的处置是路径写错。
        let state = temp_dir("cwd-missing-dir");
        let config = config_at(&state, &state.join("nope"), None);
        let err = control_workdir(&config).unwrap_err();
        assert!(err.contains("工程根不存在"), "{err}");
        let _ = std::fs::remove_dir_all(state);
    }

    #[test]
    fn find_system_accepts_the_dir_name_and_the_model_name() {
        let state = temp_dir("find");
        let project = state.join("prj");
        let system = project.join("dir-a");
        std::fs::create_dir_all(system.join("sys")).unwrap();
        std::fs::write(system.join(SYS_MODEL_PATH), "name: model-a\n").unwrap();
        assert_eq!(find_system(&project, "dir-a"), Some(system.clone()));
        assert_eq!(find_system(&project, "model-a"), Some(system.clone()));
        assert_eq!(find_system(&project, "nope"), None);
        // 工程根本身是系统根：也能按名字定位。
        assert_eq!(find_system(&system, "model-a"), Some(system.clone()));
        let _ = std::fs::remove_dir_all(state);
    }

    #[test]
    fn a_configured_name_must_match_even_when_the_project_dir_is_a_system_root() {
        // 工程根本身是系统根，但 `upgrade_project_name` 写了别的：**不能**静默用目录本身
        // （否则 diagnose/restart 用它、`prj upgrade NAME` 又用那个名字 —— 两个系统）。
        let state = temp_dir("cwd-root-name-mismatch");
        make_system(&state);
        let config = config_at(&state, &state, Some("some-other-system"));
        let err = control_workdir(&config).unwrap_err();
        assert!(err.contains("some-other-system"), "{err}");
        assert!(err.contains(&dir_name(&state)), "要列出实际系统：{err}");
        let _ = std::fs::remove_dir_all(state);
    }

    #[test]
    fn list_systems_marks_a_model_name_that_differs_from_the_dir() {
        let state = temp_dir("list");
        let dir = state.join("dir-a");
        std::fs::create_dir_all(dir.join("sys")).unwrap();
        std::fs::write(dir.join(SYS_MODEL_PATH), "name: model-a\n").unwrap();
        assert_eq!(
            list_systems(std::slice::from_ref(&dir)),
            "dir-a（name: model-a）"
        );
        assert_eq!(list_systems(&[]), "（无）");
        let _ = std::fs::remove_dir_all(state);
    }
}
