# 更新日志

本文件记录 `wist-gwlinkd` 的所有重要变更。格式遵循 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

## [0.1.1] - 2026-10-04

### Fixed
- **升级回执**：body 改用中心契约 `ReportGatewayUpgradeResult`（含必填 `gateway_id`/`reported_at`）——
  原直接发本地 `UpgradeRecord` 会被中心 422 拒掉，回执永远进不去。
- **gops 调用**：补 `--on-failure`（gops 2.2.x 必填，缺省 `rollback-all`）与 `--json`；支持 gops 工程根
  （`upgrade_project_dir`，gops 从 cwd 解析工程）与目标系统名（`upgrade_project_name` / 计划 `component`）。
- **常驻不再被升级拖死**：驱动升级 / 落游标 / 续期落盘失败一律记事件并继续，不再 `?` 传播导致进程退出。
- **判死后自愈**：检测到被判死的升级时清游标，使同一计划可被重新驱动（执行器**真失败**不重试）；
  启动时只做一次（避免逐 tick 刷日志）；可用 `upgrade_retry_on_dead = false` 关掉、改走管理面重派。
- **升级记录原子写**：`upgrade.json` 改临时文件 + rename，避免半截被误判为「无升级」。
- **执行器日志落盘**：stdout/stderr 写 `state_dir/wist-upgrader.log`，不再丢现场。
- **回执不含凭据快照**：回执时现读最新凭据，跨越 renew（旧 `rt_` 立即失效）不再 401。
- **凭据损坏与缺失区分**：损坏不再被当缺失静默重置备。
- **身份漂移**：身份文件内容畸形时报错，不再静默换身份。
- **ticker** 用 `MissedTickBehavior::Skip`，慢轮后不突发补 tick。

### Added
- **诊断扩充**：信任锚可解析、凭据有效期、自述面是否配置、升级执行器是否可解析、中心 TCP 可达、endpoint scheme；
  「判定与诊断同源」不变。
- **执行器回收**：常驻退出时收走升级执行器（`kill_on_drop` + Linux `PR_SET_PDEATHSIG=SIGTERM`），
  不留孤儿执行器继续动现场（macOS 无 PDEATHSIG，靠正常退出时的 `kill_on_drop` 与 systemd cgroup 兼容）。
- 配置项：`upgrade_on_failure` / `upgrade_project_dir` / `upgrade_project_name` / `upgrade_retry_on_dead`。

### Changed
- `instance_id` 前缀 `boot-` → `inst-`（名实相符；不影响中心侧语义）。

## [0.1.0] - 2026-10-04

首个骨架：host 侧常驻的 CLI（`run` / `diagnose`）与模块边界（config / state / center / upgrade / doctor），
外加**唯一来源的判死判据**（`state::{heartbeat_is_fresh, UPGRADER_DEAD_AFTER}`）。
对接 `WistCenter`（link-upstream / register / status / credentials:renew）、网关**自述面消费**（环回）、
凭据**续期**、升级**驱动 + 回执**（`upgrade-result`），身份/实例/凭据落盘。见 CR-003。
