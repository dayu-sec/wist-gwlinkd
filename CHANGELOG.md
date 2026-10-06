# 更新日志

本文件记录 `wist-gwlinkd` 的所有重要变更。格式遵循 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

## [0.5.2-alpha] - 2026-10-06

### Added
- **注册与周期状态上报带上网关对外域名**：读网关自述面的 `public_base_url`，随 `register` 与
  `report_status` 转带（老网关不带该键 → `None`，不阻断）。注册时若自述面取不到（网关还没起来 /
  未配 `gateway_self_endpoint`）则不带，随后周期上报自然补上。
- **执行器取件目标可带中心派生地址**：升级驱动新增「取件目标」入参；有计划派生的
  `artifact_url` 就用它作 `gops --to <url>`，否则回落 `to_version`。**台账 / 回执仍记版本**。

### Changed
- **依赖**：`wist-control` `0.8` → `0.9`（新增可选字段 `public_base_url`）。

## [0.5.1-alpha] - 2026-10-06

### Changed
- **网关面 wire 类型统一到 `wist-control`**：link-upstream / register / status / credentials:renew /
  upgrade-plan / upgrade-result 全部用模型生成类型（原 `wist-contracts::gateway_control` 手写副本已删）。
  **线上 JSON 不变**；`requested_at` / `issued_at` / `not_before` / `not_after` 改为 `wist_control::DateTime`。
- **依赖**：`wist-control` `0.6` → `0.8`；**移除 `wist-contracts` 依赖**。

## [0.5.0-alpha] - 2026-10-05

### Added
- **网关状态上报富化**：主循环上报中心时填充**进程 / 机队 / 存储 / 数据面 / 主机资源**字段
  （`uptime_seconds` / `cpu_percent` / `memory_bytes` / 机队 / `store_bytes` / ingest 计数 / 主机
  `memory_total_bytes` / `load_*` / `disk_*`），与网关自述面同源（对齐 `wist-control` 0.6.1）。
- **gwlinkd 状态心跳**：主循环每拍（含首跑等待期）把自身状态
  （`state` / `version` / `center_endpoint` / 客户端证书到期 / 最近上报中心时刻 / 最近错误）
  环回 `POST /api/v1/gateway/linkd-status` 推给网关（纯出站，无入站面）——网关 Web 据此展示
  「宿主侧常驻在不在跑」。设计 `wist-design/doc/design/edge/gateway-linkd-status.md`。
- **OS 服务托管（长期后台运行）**：新增 `service` 子命令（`print` / `install` / `uninstall` / `status`）——
  渲染并安装 **systemd unit**（Linux，`Restart=always`，日志进 journald）或 **launchd plist**
  （macOS，`KeepAlive`，日志进 `/var/log/wist-gwlinkd`），开机自启 + 崩溃拉起 + 退出重启。
  配置走**绝对路径**（`--config` → `WIST_GWLINKD_CONFIG`）；`--system`（默认）/ `--user` 两作用域。
  与 `wist-agentd service` 同形，供栈的安装/升级阶段调用
  （`wist-gwlinkd service install --system --bin <抽出件> --config <配置>`）。
- `state::is_running`（`flock` 探活）供 `service status` 判「在跑否」。

## [0.4.0-alpha] - 2026-10-05

### Added
- **页面发起接入**：新增环回 `link_request` 客户端，首跑优先拉取网关侧接入请求
  （中心地址 / 接入券 / CA），CA 落盘到 `<state_dir>/control-center.pem`；无待办且无 env 券时
  等待页面提交；接入成功/失败回报网关（`Connected` / `Failed`）。
- **环回信任锚 `gateway_self_ca`**：网关 loopback 面（self-state / link-request）以**自签证书**
  提供 HTTPS 时，用它作信任根（PEM）；缺省 = 系统根。没有它，真部署的 gwlinkd 够不到网关环回面
  （纯 HTTP 桩会把这个缺口掩盖掉）。`diagnose` 同源新增 `self.ca` 检查：未配=OK，配了但缺失/非法=FAIL。

### Changed（不兼容）
- **接入券环境变量改名**：`WIST_GWLINKD_BOOTSTRAP_TOKEN` → `WIST_GWLINKD_LINK_TOKEN`
  （旧名仍可读，会打印弃用告警；下一版移除）。名字改准 —— 它只在**首跑接入**（`link-upstream`）
  时用一次，gateway 容器部署/启动本身不需要它。

## [0.3.0-alpha] - 2026-10-05

### Added
- **升级执行器适配层**（`executor` 模块）：驱动不再写死 gops，只认「构造调用 + 归一化结局」；
  `GopsExecutor` 是其一。换执行器（别的交付器 / 站点脚本）只需另给一个 `UpgradeExecutor` impl。（CR-003 R4）
- **升级成功佐证**：执行器报成后，再用网关**自述面**（`gateway_self_endpoint`）独立确认「网关确实回来了且健康」；
  观测不到就记 `unverified` 而非 `done` —— 只信执行器一面之词会让「成功」可能是假的。
- 配置项 `upgrade_health_cmd` / `upgrade_health_timeout_seconds`：给 gops `--health-cmd` / `--health-timeout`，
  **给了才让 gops 据栈外健康判定回滚**。

### Fixed
- **未知 gops `status` 不再静默归 `failed`**：保留原值并加前缀（`unknown:<原值>`），契约漂移看得见。
- **佐证单次探测按剩余窗口约束**：一次卡住的 fetch 不再把窗口拖到 `HTTP_TIMEOUT`（30s）。
- **心跳覆盖整个升级事务**（含佐证窗口）：此前佐证期（最长 `verify_timeout`）心跳停跳，会被误判「已死」、
  甚至重启后重驱同一计划；现用 RAII 守卫托管心跳任务（正常退出 / panic 都收走，不留孤儿）。
- **并发排空执行器 stdout**：子进程写满管道会阻塞退出，原「先 `wait()` 再读」会死锁。
- **执行器起不来 = 终态**：`spawn` 失败写 `status=failed`（`step=spawn`），不再把台账停在 `running`。
- **`interpret()` 的 `ok` 与 `status` 对齐**：空 `status` + 退出码 0 此前会得到 `status=done` 但 `ok=false` 的不一致；
  `ok` 现由 `status` 推导（调用方据 `ok` gate「成功佐证」）。
- **诊断不再把终态升级误报为「进行中」**：`upgrade.local` 现在区分进行中与已结束 —— 终态 `done` 报绿，
  非 `done` 终态（`failed` / `rolled_back` / `unverified`）报 **WARN**（提示但不判死）。

## [0.2.0-alpha] - 2026-10-04

### Changed（不兼容）
- **网关长期身份改为客户端证书（mTLS）**：注册/轮换时本机生成密钥对、只交 CSR，中心用 CA-G 签出
  客户端证书后回执；`state/credential.json` 改存「证书 + 私钥」（0600），**私钥永不出本机**。
  取代旧的对称 bearer `rt_`。
- 已注册后所有网关面调用（status / renew / upgrade-plan / upgrade-result）改为 **mTLS**（reqwest 挂
  客户端证书/私钥）；`link-upstream` 首跑置备仍用一次性 bootstrap bearer。
- 轮换 = **证书轮换**：到期前再生成一套密钥对，以当前证书证明身份 + 新 CSR 换新证书，旧证书作废。
- wire 契约切到 `wist-contracts::gateway_control`（`GatewayCredentialBundle` 只带 `certificate` 等）。

### Added
- **首跑注册可重试**：`link-upstream` 成功即把 RegistToken 落盘（0600）；register 失败后下次
  **免 bootstrap** 重试（bootstrap 已被消费）。
- 诊断：`credential.pending_regist`（有未消费 RegistToken 时提示）；credential.local 改看客户端证书到期。
- 依赖：`wist-contracts`（0.2）/ `rcgen` / `x509-parser`。

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
