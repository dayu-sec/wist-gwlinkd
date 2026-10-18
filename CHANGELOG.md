# 更新日志

本文件记录 `wist-gwlinkd` 的所有重要变更。格式遵循 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

## [0.13.1-alpha] - 2026-10-18

### 修复（对 0.13.0 的 3 轮 review）

- **不支持的动作用于不再每轮重试**：回报 `failed` + 落游标（0.13.0 只打日志，中心看不到，
  同一份计划每 30s 重来一次）。
- **未配置 `gateway_self_endpoint` 时**同样回报失败 + 落游标（agent 包与知识库包两条通道）。
- **空目标版本不再卡死包下发**：只有 `upgrade` 需要 `to_version`；知识库 / agent 包计划即使
  版本为空也会走一次并回执，而不是被外层条件挡掉后无限重试。
- **旧 `.sig` 会被清掉**：这次取不到签名时删除同名旧签名 —— 否则网关会拿上一版的签名验这一版，
  报错指向「签名不符」而不是「这次没取到签名」。
- **`agent_package_container_dir` 必须是容器内绝对路径**：配置错在 gwlinkd 侧就报清楚，
  不再等网关回 400。
- 投放目录清理（prune）在交付失败时也执行 —— 中转区不会因反复失败而无界增长。

### 变更

- 计划动作分派抽成纯函数 `plan_intent`，单测钉住「未知动作绝不回落成 `upgrade`」。
- 两份交付（agent 包 / 知识库包）共用 `finish_plan` 与 `write_drop_file`，避免回执 / 落盘口径漂移。

### 测试

- 新增：动作分派映射（未知 → Unsupported）、摘要不符拒绝交付且不落盘、
  签名缺失时不阻断交付并清掉旧签名。

## [0.13.0-alpha] - 2026-10-18

### 新增

- **知识库包下发（`action = push-knowledge-package`）**：中心派下的计划由 gwlinkd 取包并环回交付
  网关知识库管理 —— 与 Agent 包下发同一条通道（gwlinkd 持中心信任，网关只托管）。
  - 取包时校验中心给的 `artifact_sha256`；**签名旁文件 `.sig` 一并落地**（配了验签公钥的
    网关会按 `<本地包>.sig` 验签），取不到只告警不阻断。
  - 投放目录复用 `agent_package_drop_dir` / `agent_package_container_dir`；
    交付载荷 `{source, sha256, origin, activate}` 推进 `POST /api/v1/gateway/knowledge-package`。
  - `activate` 来自计划（缺省 false）：知识库**录入 ≠ 生效**。

### 变更

- **未知计划动作不再回落成 `upgrade`**：只认 `upgrade` / `push-agent-package` /
  `push-knowledge-package`；其余留痕（`event=UnknownPlanActionSkipped`）并跳过 ——
  避免拿知识库 / 新类型包去跑 gops 把网关搞坏。
- 依赖对齐 `wist-control 0.15`。

### 升级顺序

- **本版必须先于中心升级**：旧 gwlinkd 会把 `push-knowledge-package` 当 `upgrade` 执行。
  完整顺序：网关 → **gwlinkd** → 中心 → 中心前端。

## [0.12.1-alpha] - 2026-10-17

### 修复

- **作为系统服务运行时，升级执行器 `gops` 找不到**：launchd / systemd 的默认 PATH 只有系统目录，
  而安装脚本把 `gops` / `gx` 装在运行用户的 `~/bin`。现在 `service install` 渲染的服务定义会把
  运行身份的 `~/bin` 与 `~/.local/bin` 追加进 PATH（systemd 显式 `Environment=PATH=`），
  升级计划不再以 `启动升级执行器 gops 失败: No such file or directory (os error 2)` 落失败。

### 升级注意

- **已有服务定义不会自动更新**：要拿到新 PATH，请用本版重新执行一次
  `wist-gwlinkd service install --system --run-as <部署用户> ... --force`。
  不重装也可以：把 `upgrader_program` 配成 `gops` 的**绝对路径**（如 `$HOME/bin/gops`）。

## [0.12.0-alpha] - 2026-10-17

### 新增

- **支持中心升级计划的「自更新」**：计划里的 `component = "wist-gwlinkd"` 不再落到 gops —— 进程内取
  中心制品（同一套摘要 / 架构护栏）→ 原子覆盖服务定义指向的二进制 → 写 `restarting` 退出；服务管理器
  拉起后用**编译版本**核对计划的 `to_version`，再回执中心。前置不满足（未装服务 / 目标不是当前进程 /
  目录不可写）时**发请求前**就落可读失败。

### 修复

- **升级回执读错身份目录**：`UpgradeReporter` 增加 `identity_dir`，回执现读凭据不再只找 `state_dir` ——
  网关栈的 `identity/gwlinkd` 与 `configs/gwlinkd/state` 分开时，旧实现会把回执静默跳过。

### 升级注意

- **首次启用自更新要先换本体**：旧版 gwlinkd 不认识 `component = "wist-gwlinkd"`，会把计划当 gops 升级
  而失败。先重跑中心「只装 gwlinkd」命令装上新版（同时完成可写布局迁移），之后才能用升级计划。

## [0.11.0-alpha] - 2026-10-16

### 新增

- **`diagnose` 新增「环回面可达」检查**（`self.reachable`）：真打一次本机网关的自述面，把
  「本机够不到本机网关」当场点出来。原先只查「配了没、文件在不在」，**不查连通** —— 而环回面在
  **容器部署**下会把宿主自己那一跳判成「外来」而全线 403（症状：心跳推不上中心、网关页面报
  「未检测到 gwlinkd 心跳」、接入不了），看着像中心配错，其实卡在本机这一跳。
  - 只有 2xx 算通；带环回护栏 code 的 403 判**红**，并给出可照做的步骤（网关需 ≥ v0.11.0、
    核对网关启动日志 `event=LocalHostPeers`、必要时 `[server] self_face_allow` 兜底）。
  - 400「缺 `gateway_id`」指向**本机配置**；其它非 2xx（如前置代理的 403）只判**黄**，
    不照抄「网关版本旧」的结论。
  - 自述面没配 → 跳过（那条已由 `self.endpoint` 单独提示）。

### 修复

- **`gateway_self_endpoint` 为空串时不再当「已配置」**：配置里写成 `gateway_self_endpoint = ""`
  （模板未做非空判断、或手改过渲染产物）时，旧行为报「已配置网关自述面」，新检查还会拿空串拼出
  相对 URL、报一条与真因无关的「TCP/TLS 失败」。现在空串等同未配置。

## [0.10.0-alpha] - 2026-10-15

### 新增

- **`diagnose` / `service status` 不必再指定配置**：`--conf` 缺省时自动读**已装的服务定义**里那份
  （bin / 主配置 / 现场态）—— host 侧常驻本就该认得自己那份，长路径不必敲两遍（敲错的名字会被
  静默丢掉、回落到 `./gwlinkd.toml`，症状看着像「配置读不到」）。回落时在 stderr 说明一句。
- **全局选项短名 `-c` / `-p`**（等价 `--conf` / `--pub`；前后位置与 `=` 内联写法同长名）。

### 变更

- **现场态变量名统一为 `GWLINKD_*`**（`init-config` 生成的声明 + 值骶架）：与网关栈 `.env` 同一套名，
  避免「值写在一个名下、声明引用另一个名」的死键。声明仍是变量名唯一的映射来源 —— 已有现场不动，
  但**两边要同名**。
- **`diagnose` 的「下一步」按现状给**：身份缺失那条不再让人去配**已经配好**的 `control_center_endpoint`。

### 修复

- **macOS：系统级 launchd + `--run-as <用户>` 装的常驻起不来**：日志目录由 `service install`（root）
  建出，launchd 以运行身份打不开 `StandardOutPath` / `StandardErrorPath`，`posix_spawn` 直接 EACCES ——
  而**应用日志永远是空的**，极难定位。现在装服务时把该目录（及已存在的 stdio 文件）交给运行身份。

## [0.9.3-alpha] - 2026-10-14

### 变更

- **COS 元数据键改名**：`x-cos-meta-sha256` → `x-cos-meta-artifact-sha256`（与契约里的
  `artifact_sha256` 字段同词；`x-cos-meta-` 是 COS 对自定义元数据的强制前缀，去不掉）。

## [0.9.2-alpha] - 2026-10-14

### 变更

- **制品推送到 COS 时自带 sha256**：上传的每个对象带自定义元数据 `x-cos-meta-sha256`（COS 对象只存
  MD5 ETag，GitHub 资产摘要过不来）。录安装包 / 校验制品时可用 `coscli stat`（HEAD）读回，摘要不再「丢」。

## [0.9.1-alpha] - 2026-10-13

### 变更

- **发布时可选把制品推送到腾讯云 COS**：打 tag 时新增 `upload-cos` job，把各平台 tarball 上传到
  `cos://<桶>/wist-gwlinkd/<tag>/` 与 `.../latest/`（未配置 `COS_BUCKET` 时自动跳过，不影响发布）。

## [0.9.0-alpha] - 2026-10-13

### 新增

- **全局选项 `--conf` / `--pub`：命令行指定配置落点**（也接受 `--conf=<path>` / `--pub=<path>`，
  可放在子命令前或后）。之前主配置 / 现场态声明的可加载路径只能靠环境变量
  （`WIST_GWLINKD_CONFIG` / `WIST_GWLINKD_PUB_CONFIG`）给，手动跑 `diagnose` 等得先 `export`；
  现在 `wist-gwlinkd diagnose --conf <主配置> --pub <现场态声明>` 一步到位。相对路径按**当前工作目录**
  绝对化（不做 `canonicalize` —— 现场态声明可能尚不存在）；与 `service` 子命令自己的
  `--config` / `--pub-config` 不冲突（名不同、语义不同）。

### 变更

- **配置分层：主配置（交付物）+ 现场态 `gwlinkd-pub.toml`（现场选择）**（与 `wist-gateway` 的
  impl/pub 同口径）。主配置只放随部署环境渲染的项（`state_dir` / 回环自述面 / 投放目录 / 升级参数 / 日志），
  升级 / 重渲**应该**覆盖它；而「接的是哪个中心、本网关在中心侧叫什么、信谁」是**现场选择** ——
  现在单独一份 `gwlinkd-pub.toml`（路径由 `WIST_GWLINKD_PUB_CONFIG` 指定；未设则取主配置同目录），
  由接入与`init-config` / 安装脚本写入，**升级 / 重渲永不覆盖**。
  加载时读**两份**并合并：段（TOML table）可以重合、**叶子键不许重复**（重复即拒 —— 归属唯一，
  不靠加载顺序）。为此 `control_center_endpoint` / `gateway_id` / `trust_bundle` 改为可缺省（空 = 未接入）。
  **之前**这三个键同时由模板渲染与页面 `upsert` 写回，只是靠“两边恰好一致”才没出问题。
- **身份目录与运行态目录分开**：新增 `identity_dir`（缺省 = `state_dir`，**不配即与旧版行为一致**）。
  不可再生的身份材料（客户端证书 + 私钥 `credential.json`、中心信任锚）落这里，与 `state_dir`
  （单实例锁 / 升级簿记 / 临时注册券 —— 可删可重建）分开：前者要进 `sys-prj.yml` 的 `contract`
  （不进包 / 不重渲 / 必进备份），后者不该被当成现场事实。
  `service install` 新增 `--pub-config`（把现场态路径写进 unit / plist 的环境变量），
  `init-config` 同时生成主配置与现场态骶架（后者**只在缺失时补**，不覆盖现场值），
  `unlink` 跨两目录删（主配置与现场态配置里的 `link_token` **都**会去掉）。
- **`diagnose` 的「中心可达」从 TCP connect 升级为真打一次带客户端证书的接口**：旧实现只测 TCP 连通，
  `https://` 打到明文端口这类**协议/scheme 不匹配**照样报绿（中心其实一直在拒所有上报）。
  现在用 trust bundle + 客户端证书发一次 `GET /api/v1/gateway/upgrade-plan`，一次验完
  「TCP / TLS / 证书被中心认」；错误串带**源链**（能直接看到 `InvalidContentType` / `UnknownIssuer` /
  `certificate is not standards compliant` 这类真因）。本机**还没有**客户端证书（尚未注册）时，中心的
  401 判 **WARN**（可达、只是未注册），不再误报为红。
- **`upgrade.project` 检查口径调整（`upgrade_project_dir` = 执行器工作目录，不再等同 gops 工程根）**：
  `upgrade_project_dir` 的通用要求只是「配了就得是**已存在的目录**」（执行器以它为 cwd）；`ops-prj.yml`
  是 **gops 特有**要求。于是：目录不存在 → FAIL；目录在但（用 gops 执行器且）缺 `ops-prj.yml` 时
  只给 **WARN**（运行期 `GopsExecutor::preflight` 仍会拒发）。

### 修复

- **一次性接入券用完即删**：接入券是**一次性**的（中心侧 `consume_link_token` 成功即清 hash，另有短 TTL），
  之前页面路径把它写进配置文件后就**不再清理** —— 一张已作废的明文券会长期躺在盘上。
  现在 `onboard` 成功即删，并在 `diagnose` 新增一条检查（**已接入却仍残留 `link_token`** → WARN）。
  `remove_link_token` 也对“文件不存在”幂等（返回 `false` 而非报错）。

## [0.8.0-alpha] - 2026-10-11

### 变更

- **运行日志改用共享 crate `wist-log`**：删掉本仓 `logging.rs`，`[log]` 段统一由 `wist_log::init`
  装配（级别 / 格式 / 落点 / 轮转），与 gateway / center / agentd 同口径；键名规范为 `max_seconds`
  （旧名 `max_age_seconds` 以 alias 继续接受，缺少配置不改）。

## [0.7.1-alpha] - 2026-10-10

### 变更

- **适配中心 `{ "error": { code, message } }` 错误信封**：gwlinkd 的 center 客户端从错误体里拆出
  `code` / `message`（`code` 一并折进 detail，按子原因的判断如 `certificate_required` 仍成立），
  不再把整段 JSON 抄进错误文案；非信封（旧式纯文本）保持原样。

## [0.7.0-alpha] - 2026-10-10

### 新增

- **`unlink` 子命令：断开本机网关与 Center 的连接**。与 link/`onboard` 同属 gwlinkd（同一份注册态、
  同一个 owner），由它集中管理：删注册态（客户端证书 / 链接配置 / 待消费注册券 / 身份 / 实例 / 升级游标），
  并从 `gwlinkd.toml` 去掉一次性接入券 `link_token`（留着它会绕过页面自动重连），网关即回到未接入。
  中心信任锚（`trust_bundle` 的 PEM）**默认保留**（是中心的公开证书，不是连接；页面接入把 CA 写进
  `state/` 也无需搬动 —— 只删注册态那几个文件，不整目录清空），`--forget-center` 才一并删，
  且删**已知的每一份**中心 CA（`trust_bundle` 指向的 + 页面写进 `state/control-center.pem` 的副本，去重）；
  `--dry-run` 只算不落盘。运行中（持有单实例锁）**拒绝执行**，要求先停。删完**尽最大努力**把
  「已断开接入」环回告知网关（`linkd-status`，state=`WaitingLinkRequest`、不带凭据），让「链接上级」页
  的接入状态卡**立刻**显示「未接入」（否则要等心跳失联 ~90s，且会被上一回的终态待办误报成「已接入」）。
  开发态 `dev/unlink-center.sh` 退化为「停进程 + 调本命令」。
- **`service install --run-as <USER>`：系统级服务以非 root 运行**。`--system` 作用域新增可选运行身份
  （`--run-as` / `--run-as-group`）：systemd 写 `User=`/`Group=`、launchd 写 `UserName`/`GroupName`。
  从而既有**开机即起**的系统服务语义，进程属主又是**部署用户** —— gwlinkd 回写 `gwlinkd.toml`（接入券）、
  落身份/凭据、跑 `gops prj upgrade`（`upgrade_project_dir`）都不再以 root 落盘。只对 `--system` 有效
  （`--user` 本就以本人运行；给了报错）；用它的前提是 `--config` 与 `state_dir` 该用户**可读写**。

### 变更

- **运行日志支持配置 `[log]` 段**：级别 / 格式 / 落点从 config 读 —— `level`（过滤器指令，如
  `wist_gwlinkd=debug,hyper=warn`）、`format`（`text` | `json`）、`file`（给了就写文件、自动建父目录；
  否则 stderr）。优先级 **`RUST_LOG` > `[log] level` > 缺省 `info`**（保留环境变量临时加详情的习惯）。
  文件**写满自轮转**：`max_bytes`（单文件上限，缺省 64 MiB）、`keep_files`（保留分卷数，缺省 4）、
  `max_age_seconds`（分卷保留时长，缺省 7 天）—— 日志文件不再无界增长。日志在**读到配置之后**才
  初始化；配置本身读不了时退写 stderr。`init-config` 模板同步。
- **`[log] file` 相对路径按配置文件目录解析**（与 `wist-gateway` 同一口径）：原先按进程 cwd，
  systemd / launchd 以任意 cwd 拉起时会落到意外位置。`try_init` 失败不再静默（打印告警），
  并注明日志由进程自轮转、**不要**再挂 logrotate。
- **错误日志补全根因（P0–P3 复核）**：事件日志（`event=…`）与跨域 lift 原先用 `Display`
  （`{err}` / `.to_string()`），会丢掉 `orion-error` 的 `Caused by` 链（原始 io / HTTP 原因）——
  出错时日志里看不到根因。现统一走 `error::chain_one_line()`（单行化 `display_chain()`）与
  `op_display_chain()`；`upgrade.rs` / `tool_install.rs` 里残余的 `eprintln!("event=…")` 收归 `log`
  facade（`[log] file` 不再漏收）；`unlink` / `service` 也按 `[log]` 段初始化；`center` 错误 detail 里的
  响应体截断到 512 字符；`upgrade` 里一处在常驻路径上的 `expect`（会 panic）改为返回结构化错误。
  新增守护测试：lift 不丢 source chain、`chain_one_line` 单行且含根因、分卷 age-prune。
- **对齐生态版本**：`wist-control` `0.13 → 0.14`（连带 `wist-shared` `0.1 → 0.2`）。纯版本 pin，无行为变更。
- **升级 `orion-error` 0.8 → 0.9（Auto-Log Guard）**：0.9 把「操作上下文数据」与「Drop 日志 guard」
  拆成两个类型 —— `OperationContext` 变为纯数据（可 `Clone`、无 `Drop`），自动日志改由非 `Clone` 的
  `AutoLogGuard` 在 Drop 时**恰好一次**写出（修
  [orion-error#64](https://github.com/galaxio-labs/orion-error/issues/64)：guard 被 clone / 附到错误
  导致失败日志重复或延迟）。`error::logged_op` 随之改用官方写法：`with_auto_log()` 武装 guard，
  成功 `mark_success()` 记 `suc!`、失败 Drop 记 `fail!`（连带 `cause=` 完整因果链，一行看全），
  并以**借用**方式（`&guard`）把纯数据附到错误供边界 `display_chain()` —— 不再把带副作用的对象
  clone / move 进错误，失败日志既不重复也不延迟。
- **再接入失败的报错改成可处置的话**：当 `link-upstream` 因该 `gateway_id` 在中心**已初始化**而返回
  401 `certificate_required` 时（典型：刚 unlink、本地无客户端证书），报错明确「同一 `gateway_id`
  **不能**再接入（本仓无「重置实例」，且这是**有意**的安全边界），请在中心**新建实例**换用新
  `gateway_id`」—— 不再把裸 code 抛给运维。
- **发布 ②「Agent 包下发」改为「gwlinkd 取包 + 交付网关」**：收到 `action=push-agent-package` 计划时，
  gwlinkd 不再只把**地址**环回给网关，而是先用**自己的中心客户端**（CA-S / 客户端证书，`artifact_http_client`）
  拉 `artifact_url`、校验 `artifact_sha256`，把字节落到**宿主投放目录**，再把**本机路径**交付网关托管 ——
  网关不出网、不需要中心信任（修此前的 502：网关不信任中心私有 CA）。新增配置 `agent_package_drop_dir`
  （宿主侧目录，与网关容器挂载 `PACKAGE_DIR:/packages:ro` 一致）与 `agent_package_container_dir`
  （该目录在容器里的路径，如 `/packages`）；缺配置则 ② fail-closed。交付载荷带 `origin`（中心地址，留痕）；
  交付后按 `agent_package_drop_keep`（缺省 12，`0` = 不清理）保留投放目录里最新若干份 —— **无论成败都清**
  （失败也清，免得反复失败把目录撑爆），且**保留数不低于本批份数**（`keep` 偏小也不会误清刚落的文件）。
  见设计 `edge/center-content-delivery.md`（分层：中心内容 gwlinkd 取、网关只托管）与
  `edge/agent-package-push-to-gateways.md`（特性）。
- **发布 ② 支持多平台**：计划带的 `artifacts`（该版本**全平台**）优先 —— gwlinkd 逐平台取包 / 校验 / 落盘，
  再**一次** POST 交付全平台（网关侧整批一次提交，任一不合格整体拒绝落库）；`artifacts` 为空才回落单值 `artifact_url` + 本机平台
  （旧中心 / ① 兼容）。契约 `GatewayUpgradePlan.artifacts`（`wist-control 0.13.0`）。
- **错误处理统一到 `orion-error`**：配置装载、本地状态、中心调用、升级执行、服务托管、包交付与接入流程
  从字符串错误收敛为**结构化错误**（按域 reason + 稳定 identity），失败时的诊断 / 回执保留**完整因果链**
  （`display_chain`），便于排障与稳定归类。对外行为不变。
- **运行日志接入 `log` / `env_logger`**：运行期 `event=…` 诊断从 `println!` / `eprintln!` 改走 `log`
  （`info` / `warn` / `error`），级别由 `RUST_LOG` 控制（缺省 `info`），落 **stderr**（由 systemd / journald
  收集）—— `env_logger` 与 `orion-error` 走同一条 `log` facade。
- **失败上下文诊断日志（`orion-error` 模式）**：关键低频操作（中心 link-upstream / register / renew、
  升级驱动、工具安装）包在 `OperationContext` 里 —— 成功记 `suc!`、失败记 `fail!`（带 action + 结构化字段
  + **完整因果链**），并把上下文附到错误上（边界 `display_chain()` 一并可见）。出错时**一条 `error!`** 即可
  看到「在做什么 + 关键字段 + 根因链」。每 30s 的 `status` / `upgrade-plan` 不包（避免刷屏）。

## [0.6.1-alpha] - 2026-10-08

### 变更

- **拉升级目标时自述本机平台**：`GET /api/v1/gateway/upgrade-plan` 带上 `platform=<target-triple>`
  （`HostTarget::target_triple`，如 `aarch64-apple-darwin`），中心据此挑**平台匹配**的制品下发地址。
  多平台组件（`galaxy-ops` / `galaxy-flow`）此前会拿到错平台制品 —— 在架构护栏处拒装（
  `架构校验失败，未覆盖 gops：制品操作系统 linux 与本机 macos 不符`）。认不出平台的 OS 不声明（退化为旧行为）。

## [0.6.0-alpha] - 2026-10-07

### 变更

- **无状态工具安装前的摘要校验**（`tool-copy`）：覆盖原二进制**之前**先核制品 sha256 —— 期望值取自中心计划带的
  `artifact_sha256`（有就用，`sha256:` 前缀/大小写容忍；形态不对即拒）＞制品名的内容寻址前缀
  （`pkg-<sha256 前 16 位>`）。实得不符 → **拒装**，绝不用来路不明的字节覆盖本机工具。
  `artifact_sha256` 不在 `wist-control` 0.9 的 `GatewayUpgradePlan` 里 —— 用 `#[serde(flatten)]` **宽容读取**：
  中心带上就校验，不带（或契约未升）就自动回落内容寻址前缀，**不等契约升级也能用**。
- **`diagnose` 新增 `upgrade.tool` 体检**：列出 `install=tool-copy` 的组件及其 `binary` / `PATH` 命中 / `require_arch`
  策略；缺 `binary` → FAIL，`binary` 不在 `PATH` → WARN（直接对着「表面修了其实没生效、静默回退 gops」的坑）。
- **无状态工具安装前的架构护栏**（`tool-copy`）：覆盖 `PATH` 上的原二进制**之前**先核制品 target-triple ——
  架构/操作系统与本机**不符必拒**，**读不出**架构缺省也拒（配置 `upgrade_tool_require_arch = false` 才放宽「读不出」
  这一种；已识别出的不符仍拒）。此前不校验，x86_64 制品会「装成功」却把 Darwin arm64 的 Mach-O 覆盖成
  Linux ELF，工具**静默报废**。判定**整段精确比**（不用 `contains`，32 位 x86 宿主不会误放行 `x86_64`）、与词表
  顺序无关，也不把组件名里的架构词（`wist-arm-tool-…`）误当三元组。识别口径与发布域同源（新模块 `src/target.rs`）。
- **取件来源形状前置校验**（`tool-copy`）：来源必须是 `https://…` 或 `/abs/path`。中心没派 `artifact_url`、只剩裸
  版本串时**报可读失败**，不再把它当 URL 去误取。
- **解包定位放宽**：`tool-copy` 找包内二进制的最大深度 4 → 16 层（报错里列出的包内文件也相应增多），覆盖更深一层
  的布局。
- **解包对 `..` 条目的行为写明**：`tar` 的路径穿越条目会被**静默跳过**（不报错、也不写到目标之外）—— 补测固定该行为。
- **升级执行前的前置校验**：gops 缺工程根（`upgrade_project_dir` 未设 / 无 `ops-prj.yml`）时**不再把执行器发出去**，
  直接落可读失败并回执（此前只会在退出码 255 上失败、还读不出原因）。
- **按中心派生的制品地址取件**：计划带的 `artifact_url` 交给执行器作 `--to <url>`；台账与回执仍记**版本**。
- **失败时把本次运行的关键 stderr 折进回执**（有界），少让人翻日志。

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
