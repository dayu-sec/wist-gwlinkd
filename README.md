# wist-gwlinkd

网关栈在 **host 侧** 的常驻，代表本机网关栈与 `WistCenter` 维持一条**独立于网关容器**的控制链路：

- **注册 / 心跳上报 / 客户端证书轮换**（本机是网关客户端证书+私钥的唯一持有者，对中心做 **mTLS**）；
- **升级取指令 + 回执**：从中心拉目标版本，驱动瞬态执行器（`gops prj upgrade`），把结果回报中心；
- **本地诊断**：`diagnose` 分项 OK/WARN/FAIL + 证据 + 下一步。

## 为什么不在网关容器里

远程升级正是**重建网关容器**。链路若在容器内，升级那一下恰好是盲区 —— 没人能上报「升级成没成」。
与 `wist-agentd` 同构：常驻 + 诊断 + 判死 + 驱动升级；升级执行器是瞬态进程，不被它托管。

**判死与诊断共用同一判据**（`crate::state::{heartbeat_is_fresh, UPGRADER_DEAD_AFTER}`）—— 这是
「判定」与「诊断」永不打架的关键。

背景与决策见 `wist-design/doc/design/foundation/cross-repo-issues.md` **CR-003**。

## 用法

```bash
wist-gwlinkd init-config [路径]  # 生成一份带注释的 gwlinkd.toml（路径缺省 = 当前配置路径）
wist-gwlinkd run              # 常驻（默认子命令）
wist-gwlinkd diagnose         # 本地诊断；有 FAIL 则退出码非 0
wist-gwlinkd service install --system   # 装成 OS 服务长期托管（开机自启/崩溃拉起）
wist-gwlinkd service status  --system   # 看服务定义/二进制/配置/落点/在跑否
wist-gwlinkd service print   --system   # 只渲染服务定义（systemd unit / launchd plist），不落盘
wist-gwlinkd service uninstall --system # 停用并删定义
wist-gwlinkd version
```

### 长期后台运行（正式运行必装）

`nohup &` 不是部署方式 —— 正式运行交给 OS 服务管理器，**开机自启 + 崩溃拉起 + 退出重启**：

```bash
# Linux（systemd unit: /etc/systemd/system/wist-gwlinkd.service，Restart=always）
sudo install -m 0755 wist-gwlinkd /usr/local/bin/
sudo wist-gwlinkd service install --system --bin /usr/local/bin/wist-gwlinkd \
     --config /etc/wist-gwlinkd/gwlinkd.toml

# macOS（LaunchDaemon: /Library/LaunchDaemons/com.dayu-sec.wist-gwlinkd.plist，KeepAlive）
sudo wist-gwlinkd service install --system ...
```

- `--system`（默认）= 系统级（开机即起；Linux `multi-user.target` / macOS `LaunchDaemons`）；
  `--user` = 登录后起（`~/.config/systemd/user` / `~/Library/LaunchAgents`）。
- 配置走 **绝对路径**（`--config`，落进 `WIST_GWLINKD_CONFIG`；默认 system `/etc/wist-gwlinkd/gwlinkd.toml`、
  user `~/.wist-gwlinkd/gwlinkd.toml`）—— 服务启动时工作目录不确定，不能用相对的 `gwlinkd.toml`。
- 日志：Linux → journald（`journalctl -u wist-gwlinkd -f`）；macOS → `/var/log/wist-gwlinkd/gwlinkd.err`
  （user：`~/Library/Logs/wist-gwlinkd/`）。
- 重复实例由 state 目录 `flock` 兜底（前任未退出时新实例快速失败，再由 `Restart`/`KeepAlive` 重试）。
- `service install` 会写定义 + `enable` + `restart`；只写定义不启用加 `--no-activate`；覆盖已有定义加 `--force`。

> 栈的**安装 / 升级阶段**（网关镜像里带着 gwlinkd 制品）就调这条命令把宿主侧常驻装成 systemd 服务 ——
> 与本文件「交付与升级」一节同一口径。

配置（`WIST_GWLINKD_CONFIG` 指定路径，缺省 `gwlinkd.toml`）：

```toml
control_center_endpoint = "https://dayu-01.example"
gateway_id = "gw-001"
trust_bundle = "/etc/wist-gwlinkd/ca/control-center.pem"
state_dir = "/var/lib/wist-gwlinkd/state"

# 可选：
gateway_self_endpoint = "https://127.0.0.1:3000"   # 网关容器自述面（环回）
renew_lead_seconds = 3600                          # 凭据续期提前量
upgrader_program = "gops"                          # 升级执行器
upgrade_on_failure = "rollback-all"                # gops --on-failure（rollback-all | halt）
upgrade_health_cmd = "curl -fsS http://127.0.0.1:3000/health"  # 栈外健康检查（给 gops --health-cmd）
upgrade_health_timeout_seconds = 120               # 健康检查超时（给 gops --health-timeout）
upgrade_project_dir = "/opt/wist/gateway-prj"      # gops 工程根（含 ops-prj.yml；gops 从 cwd 解析）
upgrade_project_name = "wist-gateway"              # 只升该系统（缺省 = 全部已导入系统）
upgrade_retry_on_dead = true                       # 判死后是否自动重驱同一计划（false = 只交管理面重派）
upgrade_tool_require_arch = true                   # tool-copy：要求制品架构可校验且与本机一致（缺省 true）

# 本机组件目录：计划里的组件名 → 本机安装机制（缺省 gops-project）。
# `tool-copy` = **无状态工具**：解包制品后把二进制复制覆盖到它在 PATH 上的原位置（不经 gops 工程）。
[[upgrade.component]]
name = "galaxy-ops"
install = "tool-copy"
binary = "gops"
[[upgrade.component]]
name = "galaxy-flow"
install = "tool-copy"
binary = "gx"
```

环境变量：`WIST_GWLINKD_CONFIG`（配置文件路径）、`WIST_GWLINKD_LINK_TOKEN`（首跑置备用的一次性接入 token）。

升级由常驻周期从中心 `GET /api/v1/gateway/upgrade-plan` 拉 desired 驱动（按 `plan_id` 幂等，游标落盘跨重启）。
执行器经 [**适配层**](src/executor.rs) 调用（`gops` 只是其中一个 impl）：驱动只认「构造调用 + 归一化结局」。
gops 调用契约（2.2.x）：`gops prj upgrade --to <版本|URL|路径> --on-failure <rollback-all|halt>
[--health-cmd <cmd> [--health-timeout <s>]] --json [NAME]`；
`--on-failure` 现阶段**必填**（缺省全回滚），`--json` 出机读结局（成功/失败/已回滚），执行器日志落
`state_dir/wist-upgrader.log`。**成功要佐证**：执行器报成后，若配了 `gateway_self_endpoint`，还会独立观测网关
自述面是否恢复且健康，观测不到则记 `unverified`（不认 `done`）；没配自述面则回执细节里标注「未佐证」。
**常驻退出时会收走执行器**（`kill_on_drop` + Linux `PR_SET_PDEATHSIG`），
不留孤儿执行器继续动现场。其余：同机**单实例**（`flock` 锁）、HTTP 带**超时**、`trust_bundle` 作为**自定义信任锚**、
凭据**原子落盘**。

**无状态工具**（`galaxy-ops` / `galaxy-flow`）不走 gops 工程：`[[upgrade.component]]` 里标 `install = "tool-copy"`
后，驱动改为**进程内**取制品（带信任锚的客户端）→ gzip+tar 解包 → 把 `binary` **就地覆盖**到它在 `PATH` 上的**原位置**
（旧版先备份到 `state_dir/tool-backups/`，失败可回滚）。这条路径**不起子进程**、也**不做成功佐证**（工具不影响网
关容器/自述面）；`binary` 在 `PATH` 上找不到时**前置报可读失败**、绝不静默回落 gops。

**摘要校验**（`tool-copy` 覆盖前必过）：期望 sha256 取自 —— 中心计划带的 `artifact_sha256`（有就用，形态不对即拒）
＞制品名的内容寻址前缀（`pkg-<sha256 前 16 位>`，如 `pkg-955e0dc75215c3a6`）。实得摘要不符 → **拒装**（不覆盖）。

**架构护栏**（`tool-copy` 覆盖前必过）：制品是**平台专用**的（`<name>-<version>-<target-triple>.tar.gz`），把
x86_64 的 ELF 覆盖到 Darwin arm64 的 Mach-O 上**不会报错、只会让工具静默报废** —— 所以覆盖前先核工件三元组：

- 架构 / 操作系统**与本机不符** → **拒装**（旧二进制原封不动）；
- **读不出** target-triple（内容寻址名 `pkg-<hash>` 等）→ 缺省**拒装**，确需放行时设 `upgrade_tool_require_arch = false`
  （只放宽「读不出」这一种；已识别出的架构不符**仍拒**）。

判定**整段精确比**（不用 `contains`，故 `x86_64` 不会在 32 位 x86 宿主上被误放行）、与词表顺序无关，也不把组件名里
本就有的架构词（`wist-arm-tool-…`）误当三元组；详见 [`src/target.rs`](src/target.rs)。

**取件来源**必须是可取形态（`https://…` 或 `/abs/path`）：中心没派 `artifact_url`、只剩裸版本串时**可读失败**，
不再当成 URL 去误取。`wist-gwlinkd diagnose` 的 `upgrade.tool` 一条会体检无状态工具组件（目录、`binary` 配置、
`PATH` 命中与架构策略），联调时一眼看出「配置里有没有目录、会不会回退 gops」。

## 交付与升级

不单独发版通道：多平台**静态**制品（`{x86_64,aarch64}-unknown-linux-musl` +
`{x86_64,aarch64}-apple-darwin`）**打包进网关镜像**当载体，栈的安装 / 升级阶段按宿主
`uname -s`/`uname -m` 抽出对应件、用 `wist-gwlinkd service install --system --bin <抽出件> --config <配置>`
装成 **systemd / launchd** 服务长期托管。**版本随栈**。

## License

[Apache-2.0](LICENSE)
