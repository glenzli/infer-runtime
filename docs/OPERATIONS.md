# M3/M4 运维与账本

## 本地 Operator Console

`infer console` 是本机 operator 的浏览器管理面板，不是新的 daemon 或第二套控制平面。
它只投影现有 `/health`、contract、Job、metrics、provider、resource 与 budget API，
并可选择拥有一个 `inferd` 子进程。开发环境先构建相邻二进制：

```bash
cargo build -p infer -p inferd
target/debug/infer console --spawn
```

Web Console 默认只监听 `127.0.0.1:8790` 并自动打开默认浏览器；`--no-open` 只打印
地址，`--bind 127.0.0.1:<port>` 可选择其他 loopback 端口，非 loopback bind 会被拒绝。
也可以不带 `--spawn` 连接已经运行的 daemon；`--server`/`INFER_URL` 和
`--api-key`/`INFER_API_KEY` 可显式覆盖认证。默认情况下，daemon 与 CLI 共同使用
`[auth].managed_credentials_directory` 中 runtime 自动生成的 `local-operator` 凭证，
因此本机启动不要求用户传入 key。Console 用于调用 daemon 的 `local-operator` bearer
credential 只保留在后端，不会发送给浏览器；浏览器写操作使用随机的本次会话证明，并受
same-origin/CSP 保护。显式创建/轮换的 Consumer token 是唯一例外，只在 `no-store` 响应中
展示一次，关闭对话框后从页面清空。
控制台分为七页：

- **Overview**：连接状态、consumer contract、Job 总量、provider 队列与账本摘要；
- **Statistics**：本次 Console 会话内有界采样的吞吐、失败、队列、活跃 Attempt
  和 free-memory 趋势，以及 daemon 累计成功率、队列等待与 SQLite usage ledger 汇总；
- **Jobs**：最近 50 条当前 App 可见的无 payload Job 元数据；可搜索和筛选，打开
  routing/Attempt/error explain，并显式取消非终态 Job；
- **Resources**：Responses 与 MLX `audio_worker` provider、容量/熔断、本地 inventory、模型
  lifecycle 和系统压力；提供显式 load/unload、Responses provider compatibility probe
  和原生 inventory 刷新。动作继续受 `resource_admin`、reservation
  和 lifecycle 状态机保护；
- **Apps & Access**：登记 Consumer 身份、编辑允许提交的 Intent 以及可申请的
  policy/placement/quality/fallback 边界，创建或轮换 runtime-managed token，并撤销 App。
  既有 token 永不回显；新 token 只展示一次，所有变更均明确等待重启；
- **Logs**：本次控制台启动的 `inferd` stdout/stderr，默认保留最近 500 行；支持来源/
  warn/error 过滤、关键字搜索和暂停跟随，并显示会话内级别计数；
- **Config**：在浏览器内查看和编辑严格 TOML；只有完整反序列化与语义校验通过后才原子
  保存，不显示或保存凭据值。

Web Console 使用系统 `prefers-color-scheme` 自动选择浅色或深色主题；系统外观变化会由
浏览器即时应用，不需要刷新、重启 daemon 或写入额外配置。

无效配置会阻止启动和重启；编辑后不会隐式 hot reload，必须显式重启。credential
source/directory 改动还需要退出并重新打开 console，使控制客户端与新 daemon 使用同一
token。网络状态轮询在后台执行，不会因 endpoint 超时冻结页面交互。日志 transport 与
最终 ring buffer 都有界；
Console 忙碌期间若日志超过 transport 容量，会在容量恢复后写入明确的 dropped-lines
记录，而不是无界占用内存或悄悄伪装成完整日志。

旧终端 TUI 作为低依赖故障排查入口保留在 `infer terminal-console`，不再是默认管理面。
它使用原有键盘操作和会话统计，但不会成为后续产品交互的主入口。

`inferd` 默认记录非轮询 HTTP 请求的方法、路径、状态和耗时；4xx/5xx 分别进入 warn/error。
Console 的高频只读轮询只记 debug，避免淹没真正的接入事件。日志不记录 Authorization、
prompt、音频、response body 或 query string。Statistics 是有界的会话可视化，不是持久
时序数据库；刷新或关闭页面不会修改 daemon/SQLite 中的权威计数和账本。

进程 ownership 是硬边界：控制台只停止它自己创建的 `inferd`，attach 到外部 daemon
时 `x/r` 不会影响外部进程；退出控制台会优雅停止所拥有的 Unix 子进程，超时才强制
回收。当前版本不是常驻服务管理器，不安装 launchd/systemd/Windows Service；日志和趋势
也只保存在当前 Console 进程/浏览器会话的有界内存中，attach 外部 daemon 时无法倒取其 stdout/stderr。
MLX provider 和队列已可见，但 worker 内部单模型 cache residency 尚未由 daemon 导出；
在该原生合同存在前，Console 不制造一个推测状态。

## App credential 与本机 bootstrap

loopback API 仍需要认证，因为同一用户会话中的其他本地进程也能访问它。默认配置不预置
任何外部产品身份，只注册本机 operator：

```toml
[auth]
managed_credentials_directory = ".infer-runtime/credentials"

[apps.local-operator]
credential = { source = "managed" }
resource_admin = true
```

首次运行 `inferd` 或本机 CLI 时，auth owner 自动创建
`.infer-runtime/credentials/local-operator.token`。token 是 256-bit 随机值，不进入 TOML、
SQLite snapshot、日志或 console；Unix 上目录和文件权限分别强制为 `0700`/`0600`。
CLI credential 解析顺序为：显式 `--api-key`/`INFER_API_KEY`，否则读取 `--config` 中
`--app-id`（默认 `local-operator`）的 credential source。

外部 consumer 按真实接入单独注册，默认无管理权限。推荐从 Web Console 的
「Apps 与访问」创建：面板只写入 `credential = { source = "managed" }` 引用，真实 256-bit
token 进入 owner-only credential file，并在创建/轮换成功时只向当前 loopback session
显示一次。

已有独立 secret provisioning 的 consumer 继续使用 environment source：

```toml
[apps.sample-consumer]
credential = { source = "environment", variable = "SAMPLE_CONSUMER_INFER_TOKEN" }
resource_admin = false
allowed_intents = ["text.summarize"]
max_pending_jobs = 16
default_policy = "balanced"
allowed_policies = ["balanced", "local-first"]
```

daemon 启动环境负责提供 `SAMPLE_CONSUMER_INFER_TOKEN`；consumer 自己可把相同值放在其
私有 secret store 中并通过 OpenAI SDK 的 `api_key` 使用，不要求采用 runtime 的变量名。
`allowed_intents` 应按最小权限显式填写；省略只用于兼容旧配置并表示允许所有 Intent，显式
空数组则保留身份但禁止推理。
Apps & Access 可以轮换 managed token 或撤销非 operator App；它不管理 environment secret，
只可移除该 App 的 runtime 登记。创建、权限修改、轮换和撤销都不会热改运行中认证表，必须
随后重启 daemon；页面会一直显示待应用状态。`local-operator` 是 protected identity，不能
从页面修改、轮换或删除。上述动作会写入当前 Console 会话的 payload-free access audit
日志，永不包含 token。macOS/Windows credential manager、持久 credential audit 与无重启
认证热更新仍是后续增强，不通过关闭认证来替代。

## 本地模型观测（M4 起步）

`[providers.<id>.local_inventory]` 是对本地 provider 原生控制接口的显式
配置，不从 Responses URL 猜测协议。当前实现支持 Ollama 的 `ollama_tags` 与 ONNX 的
`onnx_sessions`：

```toml
[providers.ollama-local.local_inventory]
kind = "ollama_tags"
endpoint = "http://127.0.0.1:11434"

[providers.onnx-local.local_inventory]
kind = "onnx_sessions"
```

认证后，`GET /infer/v1/resources` 或 `infer resources` 读取最后一次观测；
`inferd` 启动时会先完成一次只读主机压力采样，此后按低频周期独立刷新压力；这条
生命周期不会查询 Ollama/ONNX 等原生模型 inventory。`POST /infer/v1/resources` 或
`infer refresh-resources` 仍会显式刷新压力和全部已配置 inventory。
Ollama 刷新读取 `/api/tags` 与 `/api/ps`；ONNX 刷新读取进程内 Session Registry。两者都
绝不因 refresh 自动加载、卸载或修改模型。

daemon 刚启动时状态为 `unknown`，保持原有静态配置路由，避免把“尚未观测”
误判成模型不存在。一次成功的刷新会排除配置中但不在 Ollama inventory 的
deployment；刷新失败时该 inventory 的 deployment 会保守地暂时不可选，直到
下一次刷新成功。每个 Candidate Plan 都用 `deployment_unavailable` 记录该
决定，和 provider 熔断的 `provider_circuit_open` 区分开。

同一响应还包含 `system_pressure` 与每个 deployment 的 `model_lifecycle`。
当前 macOS sampler 读取系统的空闲内存百分比；Ollama 的 `/api/ps` 提供已驻留
模型的 RAM/VRAM 字节数。采样值与压力分类分开保存，分类阈值可按主机 headroom
配置，默认仍为 15% elevated、5% critical：

```toml
[resources.pressure]
refresh_interval_ms = 30000
elevated_at_or_below_free_memory_percent = 15
critical_at_or_below_free_memory_percent = 5
```

`refresh_interval_ms` 允许 `5000..=3600000`，默认 30 秒。首次采样完成前 observer 使用
`starting`/`unknown`，不会把尚未采样误报成 degraded；一次真实采样失败仍保留
`unknown` 与诊断错误，并由 observer 报告 `infer.resource.pressure_unknown`。

critical 阈值不得高于 elevated，两个值都不得为 100；对应 eviction target 必须高于
触发阈值，否则配置无法恢复任何 headroom。修改阈值只改变真实采样值的分类，不改变
或伪造 `free_memory_percent`。

从 M4 开始，每个受本地 Resource Manager 管理的 provider Attempt 会建立
`active_reservations`。guard 覆盖排队、执行和 stream 生命周期，取消、超时、
retry/fallback 与正常完成都会自动释放；未来的 unload owner 必须把非零
`active_reservations` 视为不可绕过的硬约束。当前 eviction planner 已有最小驻留
时间、等待 Job 保护和缺少/过期 reload benchmark 时的保守拒绝。后台动作只在显式
启用 monitor 且短维护租约有效时发生。

`resources.eviction` 可配置为 `disabled`（默认）或 `recommend`。后者只在
`elevated`/`critical` 压力下根据目标空闲内存百分比计算 deterministic dry-run plan，
并将结果放在 resources 响应的 `eviction_recommendation`。它不调用原生 API；没有
新鲜的 `[resources.reload_benchmarks.<deployment>]`（reload cost、观测时间、证据）
时，模型会以 `missing_reload_estimate` 或 `stale_reload_estimate` 被排除。配置作为
runtime 的版本化配置快照进入 SQLite，因此 benchmark 证据可追溯。

逐模型安全值按 `resources.eviction` 全局默认、`classes.<resource_class>`、
`deployments.<deployment>` 三层解析，后者优先。`automatic_eligible = false` 是默认值；
稀疏 override 的未填写字段会继承上一层，不会因为只调整最小驻留时间而意外放行。
示例策略允许 light/standard 参与推荐，heavy 默认关闭；只有具备本机 reload profile
且经过逐项审核的重型 Deployment 才应使用 override 放行，其余继续以
`automatic_eviction_disabled` 排除。即使 resolved eligibility 为 true，`recommend`
仍然不会在 refresh、snapshot 或普通请求路径自动执行 unload。

## 批准并应用一项目标

认证控制客户端可显式批准当前 recommendation 的第一项目标：

```text
POST /infer/v1/resources/eviction/apply
{
  "expected_deployment": "ollama_qwen3_5_2b",
  "reason": "maintenance-42"
}

infer apply-eviction ollama_qwen3_5_2b --reason maintenance-42
```

动作 owner 会先串行化 apply、重新读取 inventory/pressure 并重算计划。若第一目标已
变化、压力已解除、模型有 reservation、benchmark 失效或 eligibility 改变，请求会
失败，不会换成另一个模型继续执行。每次批准最多卸载一个 deployment；成功后重新
观测 residency。原生调用出现不确定错误时 lifecycle 先 fail closed，再用 inventory
重新确认，而不是猜测卸载是否完成。

`expected_deployment` 是乐观并发保护，不是让调用方绕过 planner 指定任意目标。
`reason` 必填且最多 512 bytes。动作开始前会把 actor、目标、reason 和配置指纹写入
独立的 `resource_audit_events`；完成/失败也会追加记录。可通过
`GET /infer/v1/resources/events?limit=100` 或 `infer resource-events` 查看，查询上限
为 1000。completion audit 写入失败不会谎称 native unload 被回滚，返回值会将
`completion_audit_persisted` 标为 false。

上述 apply、显式 load/unload、reload benchmark 和资源审计读取还要求调用 App 在
配置中设置 `resource_admin = true`；普通 App 默认无此权限。当前开发配置只为
runtime-managed 的 `local-operator` 开启。这个布尔门是 MVP 的最小权限边界，不预先引入
角色/scope DSL。

## 后台 pressure monitor 与维护租约

后台 monitor 已实现，但开发配置默认关闭：

```toml
[resources.eviction.monitor]
enabled = false
poll_interval_ms = 30000
max_lease_ms = 3600000
```

只有同时满足以下条件才可能产生 native unload：配置为 `mode = "recommend"`、monitor
显式启用、存在未过期的进程内维护租约、fresh recommendation 有目标，并且 lifecycle
原子复核时仍无 reservation。每个 poll 最多应用一项；下一轮会重新观测并重新规划。
snapshot/refresh 仍保持只读。

启用配置并重启后，可管理短租约：

```text
infer eviction-window
infer grant-eviction-window --duration-seconds 900 --reason maintenance-42
infer revoke-eviction-window <lease_id>
```

对应 API 为 `GET/POST /infer/v1/resources/eviction/maintenance-lease` 与
`POST /infer/v1/resources/eviction/maintenance-lease/revoke`。租约最短 10 秒，不得超过
配置上限；同一时刻只允许一个。租约只存在于当前进程，daemon 重启立即失效；grant、
revoke、实际逐出和失败均进入资源审计。撤销或到期阻止后续 tick，但不会谎称可以回滚
已经开始的原生卸载。

开发配置在受控实机 soak 通过后仍保持 `enabled = false`：2026-08-09 在 32 GiB Apple
Silicon 主机上使用真实 `memory_pressure` 采样和临时提高的分类/恢复阈值，2B、4B
同时驻留时，10 秒租约按相邻 poll 逐项清退，两个 action audit 相隔约 1.15 秒；恢复
非零最小驻留后，同样压力与租约只产生 `minimum_residency` skip，未发生 unload。
最终 16 条 interactive 批次中，2B 有 10 个 reservations 时 planner 只选择空闲 4B；
首轮动作后 4B absent、2B 仍 ready 且保有 4 个 reservations，整轮 runtime 的 25 个
真实请求最终全部成功。默认不开启的剩余理由是尚未完成长时间日常负载验证，而不是
动作机制未打通。生产启用前仍应检查 recommendation，并在维护窗口观察 audit 与 Job
失败率。

## ONNX Runtime、公共制品目录与导入

ONNX 使用版本化 runtime library 与内容寻址的公共制品根；配置给出绝对路径，避免 daemon
工作目录变化后加载另一套二进制或模型：

```toml
[artifacts]
root = ".infer-runtime/artifacts"

[runtimes.onnx]
library = ".infer-runtime/runtimes/onnxruntime/lib/libonnxruntime.dylib"
version = "1.27.0"
preferred_execution_providers = ["coreml", "cpu"]
allow_cpu_fallback = true
```

下载先进入 `artifacts/staging`，不得直接放入可执行 blob 路径。固定来源 revision、license、
SHA-256、size、opset、tensor 和 preprocessing manifest 后，用本机 operator CLI 发布：

```bash
infer import-onnx yunet_2026may_onnx --file '/verified/staging/yunet.onnx'
infer import-onnx sface_2021dec_onnx --file '/verified/staging/sface.onnx'
infer import-onnx siglip2_base_patch16_224_text_onnx_cpu_v1 \
  --file '/verified/staging/siglip2-text.onnx'
infer import-onnx-auxiliary siglip2_base_patch16_224_text_onnx_cpu_v1 tokenizer \
  --file '/verified/staging/tokenizer.json'
```

两种导入命令都不连接 daemon、不需要 bearer token，并拒绝 digest、size 或 Build identity
漂移。主 ONNX graph 先建立 immutable Build manifest；auxiliary 导入只能发布该 manifest
已经声明的 tokenizer/vocabulary 等依赖。`resolve_onnx` 会在每次建 Session 前重新校验主图与
全部 auxiliary，缺失或被修改都会 fail closed。

SigLIP 的可复现导出脚本和固定依赖位于 `tools/export_siglip2.py` 与
`tools/requirements-siglip2-export.txt`。它们只面向 operator 的离线 staging 工作流：输入必须
是已下载并校验 digest 的 exact Hugging Face snapshot，输出 image/text 两张 opset 17 graph
与 manifest；脚本不会把约 1.5 GB 权重写入 Git。任何 checkpoint、export toolchain、opset、
preprocess/tokenizer 或 precision 变化都必须形成新 Build/space，不能覆盖已发布 manifest。

导入后重启 daemon 读取新配置；`infer refresh-resources` 只刷新 Session residency，不扫描
未知文件。`load-model onnx-local <deployment>` 会创建并验证 Session，`unload-model` 会在无
活跃 reservation 时释放 Session 与可重建缓存。它们不删除内容寻址 artifact。

每次 Attempt provenance 必须以 runtime 实际启用的 EP 为准。当前 macOS/ORT 1.27.0 的
YuNet/SFace 严格 Core ML 测试不能让整个图脱离 CPU；若 Build 与 runtime 同时允许 fallback，
provider 会重建纯 CPU Session 并披露稳定 fallback reason。禁止 CPU fallback 时必须失败，
不能记录一个虚假的 Core ML route。升级 ORT、修改 EP、precision 或模型导出都要作为新的
验证组合，不得沿用旧证据。

## 本地模型显式装载与卸载

Ollama 与 ONNX 的原生生命周期控制都是独立的 operator action，不属于普通路由、retry
或 eviction planner。认证后可调用：

```text
POST /infer/v1/resources/<provider_id>/deployments/<deployment_id>/load
POST /infer/v1/resources/<provider_id>/deployments/<deployment_id>/unload

infer load-model ollama-local ollama_qwen3_5_2b
infer unload-model ollama-local ollama_qwen3_5_2b
infer load-model onnx-local onnx_yunet_2026may
infer unload-model onnx-local onnx_yunet_2026may
```

`load` 使用 Ollama 原生 `/api/generate` 的负 `keep_alive` 预载并保持模型；
`unload` 使用 `keep_alive: 0` 释放驻留内存，但不删除已安装的模型文件。每项
操作都会先刷新 `/api/ps`，再进入 lifecycle state machine。卸载会先原子地进入
`draining`：有活跃 Attempt reservation 时拒绝操作，转换期间的新请求同样被拒绝。
原生请求失败或被取消会自动还原先前状态；成功后再刷新观测结果。当前控制面不会
依系统压力或空闲时间自动发起这两项操作。

ONNX 的对应动作创建/释放 Session；公共 Resource Manager 使用同一 `draining`、reservation
和 audit 纪律，但 native controller 不假设 Ollama HTTP 语义。

## Reload benchmark 采集

当需要让模型成为 dry-run eviction 的候选时，使用显式 benchmark 控制面：

```text
POST /infer/v1/resources/<provider_id>/deployments/<deployment_id>/benchmark-reload
{ "samples": 3, "evidence": "Apple Silicon 本机非驻留 reload，daemon 空闲" }

infer benchmark-model ollama-local ollama_qwen3_5_2b \
  --samples 3 --evidence 'Apple Silicon 本机非驻留 reload，daemon 空闲'
```

该操作只接受刷新后处于 `absent` 的 deployment，避免为了测量而驱逐正在驻留的
模型。每个样本调用原生 load、记录耗时、再调用 unload；期间 lifecycle 为
`benchmarking`，不允许新的 Attempt reservation。样本数必须在 1–5；返回值包含
排序后的样本、上中位数 reload cost、证据和可直接粘贴到
`[resources.reload_benchmarks.<deployment>]` 的 TOML 片段。

daemon 不会改写 TOML，因为配置文件仍是版本化真源。操作者审查片段后自行提交配置
并重启或通过未来的原子 reload 流程生效。传输失败或客户端取消时 native residency
不能被猜测：lifecycle 会保守标为 `failed`，应先刷新 resources；若模型仍驻留，再
使用显式 unload。不要为 35B 这类压力过大的模型执行 benchmark，除非已确认有足够
资源和维护窗口。

## 数据位置与内容

`[persistence].path` 指向 SQLite 数据库，默认是 daemon 当前目录下的
`infer-runtime.sqlite3`。数据库是运行历史和账本，不是配置真源；TOML
仍是 provider、App、policy 与 quota 的唯一配置来源。

`[artifacts].root` 是 ONNX 可执行制品与 manifest 的真源；`[runtimes.onnx].library` 是本机
ORT 动态库的精确位置。两者都不属于 SQLite backup，也不应放进项目 Git。灾难恢复需在
daemon 停止或一致快照下单独备份，并在恢复后重新做 digest/manifest verification。

数据库保存配置快照指纹与内容、Job/Attempt metadata、quota reservation、
usage ledger 和 lifecycle/routing audit event。它不保存 API key 的值、
Responses input、文本 prompt、上传音频或 provider 的原始错误 body。配置
只保存环境变量名称等 secret reference。

## Durable Responses background

该功能会改变 payload retention，默认关闭。首版只接受非流式、`local_only` 的文本
Responses Job；cloud、音频、batch 和调用方 URL/path reference 不在当前合同内。先生成
独立的 32-byte key，并通过环境变量注入；不要把值写入 TOML、日志或数据库备份说明：

```sh
openssl rand -hex 32
export INFER_BACKGROUND_KEY='<paste-generated-64-hex-characters>'
```

然后配置：

```toml
[background]
enabled = true
payload_directory = "infer-runtime-payloads"
key_env = "INFER_BACKGROUND_KEY"
max_payload_bytes = 4194304
result_retention_ms = 86400000
max_recovery_replays = 2
```

`payload_directory` 相对 daemon 工作目录解析。Unix 上 runtime 将目录设为 `0700`、
blob 设为 `0600`；SQLite 只保存引用、HMAC identity、大小和有效期。输入在结果与成功
终态原子发布后删除，失败/取消只在终态已持久化后删除，结果在 retention 到期后由
启动或读取路径清理。闪存上的文件删除不等于可证明的物理覆写；高敏感部署应使用
加密卷和受控备份。

调用方式：

```sh
infer run --route text.summarize --input '需要总结的内容' --background
infer response resp_0123456789abcdef
infer cancel-response resp_0123456789abcdef
```

也可直接使用 `POST /v1/responses` 的 `background: true`，再以
`GET /v1/responses/{id}` 轮询或 `POST /v1/responses/{id}/cancel` 取消。调用方无需同时
传 `infer.priority`/`infer.placement`；runtime 会固定为 `background`/`local_only`，若请求
显式给出冲突值则拒绝，而不是悄悄扩大数据边界。

daemon 必须以同一数据库、payload directory、config fingerprint 和 key 恢复 pending
Job。启动会先只读认证 pending 输入，所以丢失/错误 key、缺失 blob 或 tamper 会在任何
Attempt/replay metadata 变化前 fail closed 并保留引用；数据库仍有 pending durable Job 时
把 `enabled` 改为 false 也会阻止启动。请把 SQLite 一致快照与 payload directory 当作同一个备份/恢复单元，
不要只复制其中一个。需要轮换 key 时，当前版本必须先等所有 pending Job 终止并取走或
过期现有结果，再切换到新的空 spool。

如果结果加密已经完成，但 SQLite 结果发布事务失败，daemon 会记录不含 payload 的错误，
删除未发布的结果 blob，并保留 `running` Job、输入引用和输入密文；引用清除事务失败时
也不会删除 blob。此时 Response 会继续显示 `in_progress`，不要另行提交相同工作。先修复
磁盘空间、权限、SQLite/WAL 或文件系统问题，再使用相同配置和 key 重启 daemon；该 Job
会受 `max_recovery_replays` 约束，以原 Response ID 进入 Recovery Attempt。只有明确的
结果大小超限属于确定性失败，不会通过重启无限重试。

停止 daemon 后应同时备份 SQLite 主文件及同目录的 `-wal`/`-shm` 文件；
更稳妥的方式是用 SQLite online backup 在运行时生成一致快照。恢复时先
恢复这组文件，再启动 daemon。迁移由 runtime 启动时执行，不能通过手工
修改 schema_migrations 跳过。

## 配额

所有字段都是可选的；未设置代表该 scope 没有限额。可组合 global、单个
provider 和单个 App 的约束，Attempt 必须同时通过所有适用 scope：

```toml
[quota.global]
max_usd = 20.0
requests_per_minute = 120

[quota.providers.deepseek-cloud]
tokens_per_minute = 200000
max_concurrent_attempts = 3

[quota.apps.sample-consumer]
max_usd = 2.0
max_concurrent_attempts = 1
```

- `max_usd` 是当前 SQLite ledger 的累计上限；需要新结算周期时，使用
  新数据库或未来显式 accounting-period 功能，不能隐式清零。
- RPM、TPM 是 60 秒滑动窗口；TPM 先按请求输入和 `max_output_tokens`
  的保守估算预留。
- concurrency 在 Attempt 生命周期中占用；它可以比 provider scheduler
  的 `max_concurrency` 更严格。
- provider 不返回 usage，或 deployment 尚未配置 token pricing 时，ledger
  的 USD 条目会标为 `estimated: true`。当前 registry 的
  `estimated_cost_usd` 是整次 Attempt 估计，尚不是按 token 精确计费。

`GET /infer/v1/budget` 与 `infer budget`/`infer usage` 显示已结算 ledger
和仍在执行的 reservation。CLI 只调用控制 API，不直接读 SQLite。

`GET /infer/v1/explain/{response_id}` 会同时返回 Candidate Plan、Attempt
链和按发生顺序持久化的 audit events，因此 Job 终态和路由原因在 daemon
重启后仍可审计。

## Job metadata 分页

`GET /infer/v1/jobs` 和 `infer jobs` 只返回认证 App 自己的轻量 metadata，不读取
Responses input、音频文件或完整 Job snapshot。默认每页 100 条，最大 1,000 条：

```sh
infer jobs --limit 100 --priority background --state queued
infer jobs --limit 100 --cursor '<previous next_cursor>'
```

游标按 `created_at_ms` 和 Job ID 做稳定 keyset paging；调用方应把 `next_cursor` 视为
opaque token，不应自行计算。单 Job 查询、解释和取消也执行相同的 App ownership
检查。该分页路径已用 100,005 条 synthetic metadata、257 条固定页面完整遍历验证，
且查询计划命中新建的 App/state/priority 排序索引。

分页路径仍不读取 payload。durable local text Job 的输入/结果位于独立加密 spool；
数据库继续不保存 prompt、Responses input 或上传音频。

## 重启语义

启动时 runtime 在接收新请求前扫描 queued/running Job。普通请求会变为 `failed`，
其 running Attempt 变为 `interrupted`，未结算 reservation 按原估算记入 ledger；
runtime 不重放普通、streaming 或 cloud provider 调用，客户端应显式提交新请求。

只有符合上述合同的 durable local background Job 会保留原 Response ID 重新排队。原
running Attempt 记录为 `interrupted`，新 Attempt trigger 为 `recovery`；恢复沿用 admission
时的 Candidate Plan 和 config fingerprint，并受总 Attempt budget、原始 deadline 和
`max_recovery_replays` 限制。超过上限或配置漂移会进入失败终态，不会无限重试。
