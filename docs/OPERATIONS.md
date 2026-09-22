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
- **Jobs**：最近 50 条所有 App 的无 payload Job 元数据（Console 的
  `resource_admin` 身份专用）；可搜索和筛选，打开
  routing/Attempt/error explain，并显式取消非终态 Job；
- **Resources**：Responses 与 MLX `audio_worker` provider、容量/熔断、本地 inventory、模型
  lifecycle 和系统压力；提供显式 load/unload、Responses provider compatibility probe
  和原生 inventory 刷新。动作继续受 `resource_admin`、reservation
  和 lifecycle 状态机保护；
- **Apps & Access**：登记 Consumer 身份、编辑允许提交的 Intent 以及可申请的
  policy/placement/capability/reasoning effort/fallback 边界，创建或轮换 runtime-managed token，并撤销 App。
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

## 服务发现恢复

`/health` 正常只证明 HTTP 服务可达，不证明 Consumer 能通过 Discovery 找到它。若客户端报告
`Infer Runtime discovery failed`，应同时检查 `infra-protocol/registrations/infer-runtime--local.json`
和实际服务端点。macOS 的默认注册目录位于用户临时目录，不能假定声明在长时间运行中始终存在。

Daemon 每 60 秒只读检查其注册声明；文件单独丢失时，在确认仍持有原发布锁后补发相同内容。
正常检查不更新文件时间，不引入 lease 或协议版本变更。补发成功和错误状态变化会写入 Console
日志；重复错误不反复刷日志。目录或发布锁丢失/替换、声明内容冲突时不会接管，应通过拥有该
进程的 Console 执行 Restart，再检查注册与一次真实 Consumer 请求。不要手写注册文件或加入
固定端口 fallback。该恢复不重建被删除的 Unix socket；socket 丢失也需要受管重启。

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
`allowed_intents` 应按最小权限显式填写；省略和显式空数组都保留身份但禁止推理。仅受保护的
resource-admin operator 可显式设置 `allow_all_intents = true`，普通 Consumer 不能使用该 grant。
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

Codex 订阅模型组另有只读观测：Console 的 Provider 卡片和“查看模型组”显示已配置
Deployment 对当前完整 `model/list` 的可用、首次缺席、持续缺席状态以及最近一次观测
失败。首次成功清单缺席就暂停该 Deployment 的新请求路由；两次间隔至少五分钟的缺席
才升级提示。失败观测保留上次完整清单的路由结论，不把网络、认证或协议错误解释为
模型退役。需要迁移时，先验证继任模型的能力、effort、费用和 App 授权，再为相关
Intent 配置 `successor_deployments` 并允许请求级 fallback。清理旧 Deployment 和
App 授权是单独的配置变更；清单恢复不会自动撤销这一变更。

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
preferred_execution_providers = ["cpu"]
allow_cpu_fallback = false
```

下载先进入 `artifacts/staging`，不得直接放入可执行 blob 路径。固定来源 revision、license、
SHA-256、size、opset、tensor 和 preprocessing manifest 后，用本机 operator CLI 发布：

所有 Build 都应声明 `provenance.source_kind` 与 `license.status`。Provider cache 中的模型通常
使用 `provider_managed`；operator 自行安装的 MLX/ONNX 制品使用 `user_managed`。许可未完成
审核时写 `unreviewed`，不得猜测为开放许可。只有 Runtime 自己承担下载或捆绑责任时，才可
使用 `runtime_downloadable` / `runtime_bundled`，且配置必须同时提供固定 upstream、revision、
artifact SHA-256、verified license expression/URL/license-text SHA-256。

```bash
infer import-onnx yunet_2026may_onnx_cpu_v1 --file '/verified/staging/yunet.onnx'
infer import-onnx sface_2021dec_onnx_cpu_v1 --file '/verified/staging/sface.onnx'
infer import-onnx bisenet_resnet18_face_parsing_onnx_cpu_v1 \
  --file '/verified/staging/bisenet-resnet18.onnx'
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

## SAM 2.1 Core ML 与 BiSeNet face parsing Build

SAM 2.1 Small 使用三个 Apple Core ML `.mlpackage`，但权重仍为 `user_managed`，不会进入 Git
或随 infer-runtime 分发。Operator 必须把每个 package 的 `Manifest.json`、`model.mlmodel` 与
`weights/weight.bin` 共九个文件逐一登记到 `LocalWorkerBuildManifest`；adapter 固定为
`sam21_coreml`。artifact-set SHA-256 仍按排序后的
`relative-name NUL file-sha256 LF` 记录计算。Provider 从 ArtifactStore 解析只读 Build root，
不会接受 Consumer 路径或联网下载。

SAM worker 使用 owner-only Python 3.12 venv 和
`tools/requirements-coreml-sam-runtime.txt`。启动 readiness 会先核对三个模型的精确输入输出；
请求只通过短 JSON frame 传递私有临时文件路径，stderr 被丢弃，取消会 kill/wait worker。
输入图、mask 和 Core ML 诊断不会写入普通 daemon 日志。`.mlpackage` 不能直接作为服务时
加载真源；先在安装/升级窗口生成宿主专用、可重建的 `.mlmodelc` 缓存：

```sh
<coreml-sam-python> tools/coreml_sam_worker.py \
  --compiled-cache-root <runtimes.coreml_sam.compiled_cache_root> \
  --prepare-model <resolved-sam-artifact-runtime-root> \
  --artifact-sha256 <sam-artifact-set-sha256>
```

缓存 identity 包含 artifact、macOS/kernel、架构、Python 与 Core ML Tools 版本；任一变化都会
fail closed 为 `sam_model_not_prepared`。Console/readiness 会显示“需要准备”，不能把分钟级编译
或 specialization 隐藏到首个 Consumer 请求。缓存是派生物，不得写回不可变 ArtifactStore。

当前 macOS 实测默认固定 `runtimes.coreml_sam.compute_units = "cpu_and_gpu"`。同一已编译
SAM 2.1 Small 使用 `coreml_all` 时触发 405–539 秒的异常 ANE 首请求；`cpu_and_gpu` 的正式
HTTP cold 为 1.26 秒，同图第二次点击因 image embedding cache 为 48.5 ms。改变 compute
units 必须同步 Build provenance 并重新做真实 cold/warm smoke，不能把 `ALL` 当作自动更快。

`infer.vision.subject-segmentation-soft-mask@20260814.1` 复用同一个 session、compiled cache 与
单项 image-embedding cache，但不复用旧二值输出：adapter 将选中 mask 的原生 256×256 logits 做稳定
sigmoid Gray8 量化，并声明线性 pixel-centre 映射。当前隔离真实 HTTP smoke 的 cold 为 991 ms、同图
warm 为 38.6 ms；它是本机单次验收，不是跨机性能承诺。请求断开、deadline 或显式 Job cancel 都由
`CancellationToken` 终止 worker 并 `kill/wait`，不会发布迟到 mask。

BiSeNet ResNet18 使用普通 `infer import-onnx`，但 typed adapter 会同时锁定 opset 20、
动态 batch `input=[N,3,512,512]`、三个已命名的动态输出（typed adapter 只消费主 `output`）、
ImageNet RGB normalization、1.8 倍 face context crop 与 19-class argmax/restore policy。任何
tensor、preprocess 或 ontology 变化都必须
建立新 Build，不能复用现有 identity。仓库代码为 MIT，但该预训练权重明确使用
CelebAMask-HQ；其数据协议仅允许非商业研究并禁止再分发数据/derived data，因此 Build 记录为
`restricted`。当前启用边界是用户自行下载、单机内部、非商业使用；infer-runtime 不分发权重。

## Grounding DINO 与 LaMa Build

语义定位使用 `grounding_dino_tiny_onnx_int8_v1`。Build 固定 800×800 RGB/ImageNet
预处理、BERT uncased tokenizer、最多 256 tokens、900 个候选与归一化 box/NMS 后处理。
模型 graph 和 tokenizer 必须分别通过 `infer import-onnx` 与
`infer import-onnx-auxiliary` 导入；两项 SHA-256 或大小任一不符都会拒绝发布。Runtime 只返回
有界候选框，最终蒙版仍由 Consumer 细化和组合。

图像补全使用 `lama_inpainting_onnx_v1`。Build 固定 512×512 RGB 图和二值 mask 输入；adapter
在返回 PNG 前逐像素合成，仅允许 mask 选中位置采用模型输出。请求图、mask 和完成栅格不进入
Job metadata 或日志。该 route 不是文本引导生成，也不接受 Consumer 指定模型路径。

## CLAP 音频-文本 embedding Build（实验性，未激活）

`infer.audio.embedding@20260815.2` 的首个 Build 使用本机下载、验证并发布到 ArtifactStore 的
`laion/clap-htsat-unfused` exact revision
`8fa0f1c6d0433df6e97c127f64b2a1d6c0dcda8a`。它是 Runtime-owned local artifact，不是
Consumer 上传、不是 API 可下载资产，也不会被提交进 Git。发布前需要逐文件 digest 与 artifact-set
digest 均匹配 `config/infer.example.toml` 中 Build 的 identity；随后由
`clap_audio_embedding_worker.py --verify-model <admitted-root>` 在完全离线、MPS-only 模式下做
无 payload readiness probe。

Provider 的 Python command 必须是 owner-installed isolated runtime 的绝对入口；worker 通过
`providers.<id>.runtime_dependencies.ffmpeg` 解析并固定 decoder 的 canonical target。它把 Consumer
上传的有界音频临时文件解码为固定 48 kHz mono FP32，拒绝超过 10 decoded seconds 的输入。模型目录
仅来自 ArtifactStore 的 Build resolution；worker 不接受 Consumer-provided model/path，也不在 serving
期间联网下载。MPS 不可用或一个 kernel 不支持时，`PYTORCH_ENABLE_MPS_FALLBACK=0` 使 Provider
unavailable，而不是悄悄改为 CPU。

这个 baseline 目前只可作为 English-first 检索实验：`language` 字段只是 Consumer 提供的查询证据，
不表示模型已通过任何语言的召回评测。中文短查询由已冻结的本地
`ollama_qwen3_5_2b` / `qwen3_5_2b_mlx` 通过 `infer.audio.zh-en-short-query@20260815.1` 进行专属
预处理；它不是通用翻译接口，normalizer 不可用时该分支 fail closed。配置示例登记
Provider/Build/Deployment 是为了可复现的 operator assembly；它不会
自动创建 App ACL、不会启用现有 daemon，也不授权 cloud fallback。具体激活门槛及中文限制见
[ADR-0018](adr/0018-local-clap-audio-text-embedding.md)。

## YAMNet 声音事件 Build

`audio.detect_events` 使用 typed `audio_worker`，但模型文件仍必须先进入公共 ArtifactStore；
Provider 不接受配置或请求中的任意路径。Operator 从官方 TF Hub 获取精确
`google/yamnet/1` archive 后，先用 `tools/install_yamnet.py` 校验 archive 大小、SHA-256、成员
类型和四个逐文件 digest，解包到 owner-only staging。随后用
`infer-artifact` 的 `publish_local_worker_build` example 发布 manifest；adapter 固定为
`yamnet_audio_events`，artifact names 固定为：

- `saved_model.pb`
- `variables/variables.data-00000-of-00001`
- `variables/variables.index`
- `assets/yamnet_class_map.csv`

四个 `ArtifactIdentityConfig` 必须记录 exact size/digest/source revision/license declaration；
artifact-set SHA-256 是对排序后的 `relative-name NUL file-sha256 LF` 记录求摘要。Runtime 启动时
通过 manifest、adapter 和 artifact-set digest 重新解析并逐文件验证，之后才把只读 Build root
交给 YAMNet executor。serving 期间不联网下载。

TensorFlow runtime 使用独立 owner-only venv；`tools/requirements-yamnet-runtime.txt` 只固定顶层
`tensorflow==2.20.0`。当前本机 deployment receipt 记录 Python 3.12.13、TensorFlow 2.20.0、
NumPy 2.5.2 和 ffmpeg 8.1.2；后续部署仍须重新记录实际 platform 与这些版本。模型/代码、
AudioSet training data 与 ontology 当前分别记录 Apache-2.0、CC-BY-4.0、
CC-BY-SA-4.0 的 operator declaration；没有归档 license text、摘要和审核日期前，Build 必须保持
`license.status=declared`，不能标记 verified。

Console 可能由 launchd 等守护进程启动，不能只依赖交互 shell 的 `PATH`。YAMNet Provider
通过 `providers.yamnet-local.runtime_dependencies.ffmpeg = "ffmpeg"` 声明 decoder；Runtime
在启动/配置保存时只搜索继承 `PATH` 中的绝对目录，以及 `/opt/homebrew/bin`、
`/usr/local/bin` 和系统 binary 目录。Decoder 固定为 canonical 绝对 target；Python worker 则
保留 venv 的绝对入口路径（否则会脱离虚拟环境），同时在 readiness 中记录其 canonical target
供审计。Operator 仍可配置一个绝对路径以固定部署。解析与 `ffmpeg -version`、
Python/TensorFlow、exact artifact Build 的
无音频自检只在 startup/readiness 阶段发生，不在每次请求重新发现。

缺少 command、ffmpeg、TensorFlow 或 admitted artifact 时，daemon 的其他 Provider 仍可启动，
但该 Provider 会从 Candidate Plan 中 fail closed 排除。Console 的配置页与 Provider 卡片显示
缺失项、已检查目录和修复提示；普通 Consumer 仍只收到稳定的 no-candidate/unavailable 语义，
不会看到本机路径或 worker 诊断。worker 仍会把实际 decoder 版本写入 provenance，不向请求
开放任意可执行路径。

worker stdin 上限 64 KiB，stdout 单帧上限 4 MiB，音频仍受 25 MiB upload 与 600 秒 decoded
上限。取消、deadline、协议失败或超限会终止并重建 worker；stderr 不进入 daemon 日志，固定错误
不会包含音频、临时路径、转写、请求 body 或 token。

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

Console 使用独立的 operator experimental read-only 路由
`GET /infer/v1/operator/jobs` 来显示近期所有 App 的同一轻量投影。该路由要求
`resource_admin=true`，支持相同的 `limit`、`cursor`、`priority` 与 `state` 过滤，且不会
改变普通 Consumer 的 App ownership 隔离；它也不返回 Job input、output、完整 snapshot 或
`explain` 内容。Console 对单条记录的诊断和取消分别使用
`GET /infer/v1/operator/jobs/{response_id}/explain` 和
`POST /infer/v1/operator/jobs/{response_id}/cancel`；二者同样仅限 resource admin，普通
`/infer/v1/explain/{response_id}` 和 `/infer/v1/jobs/{response_id}/cancel` 继续按 App 隔离。

## 重启语义

启动时 runtime 在接收新请求前扫描 queued/running Job。普通请求会变为 `failed`，
其 running Attempt 变为 `interrupted`，未结算 reservation 按原估算记入 ledger；
runtime 不重放普通、streaming 或 cloud provider 调用，客户端应显式提交新请求。

只有符合上述合同的 durable local background Job 会保留原 Response ID 重新排队。原
running Attempt 记录为 `interrupted`，新 Attempt trigger 为 `recovery`；恢复沿用 admission
时的 Candidate Plan 和 config fingerprint，并受总 Attempt budget、原始 deadline 和
`max_recovery_replays` 限制。超过上限或配置漂移会进入失败终态，不会无限重试。
