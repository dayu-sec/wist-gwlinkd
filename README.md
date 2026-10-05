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
wist-gwlinkd run              # 常驻（默认子命令）
wist-gwlinkd diagnose         # 本地诊断；有 FAIL 则退出码非 0
wist-gwlinkd version
```

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

## 交付与升级

不单独发版通道：多平台**静态**制品（`{x86_64,aarch64}-unknown-linux-musl` +
`{x86_64,aarch64}-apple-darwin`）**打包进网关镜像**当载体，栈的安装 / 升级阶段按宿主
`uname -s`/`uname -m` 抽出对应件、装 systemd。**版本随栈**。

## License

[Apache-2.0](LICENSE)
