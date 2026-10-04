# wist-gwlinkd

网关栈在 **host 侧** 的常驻，代表本机网关栈与 `WistCenter` 维持一条**独立于网关容器**的控制链路：

- **注册 / 心跳上报 / 凭据续期**（运行期凭据 `rt_` 的唯一持有者）；
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
```

环境变量：`WIST_GWLINKD_CONFIG`（配置文件路径）、`WIST_GWLINKD_BOOTSTRAP_TOKEN`（首跑置备用的一次性引导 Token）、
`WIST_GWLINKD_UPGRADE_TO`（手动触发一次升级，占位 CR-002 C2 的「拉 desired」）。

## 交付与升级

不单独发版通道：多平台**静态**制品（`{x86_64,aarch64}-unknown-linux-musl` +
`{x86_64,aarch64}-apple-darwin`）**打包进网关镜像**当载体，栈的安装 / 升级阶段按宿主
`uname -s`/`uname -m` 抽出对应件、装 systemd。**版本随栈**。

## License

[Apache-2.0](LICENSE)
