# infer-runtime 路线图

| 属性 | 值 |
| --- | --- |
| 状态 | M1、M2 已完成，M3 核心闭环完成，M4 退出门槛完成；现行 Consumer 接入面已收敛为日期化 `infer-runtime.consumer-core@20260813.1`、独立 Capability Catalog 与官方 SDK，历史 candidate 合同仅保留为迁移档案；下一退出门槛是四个真实 Consumer 完成 hard cut、统一切换与 soak。M6 的 ONNX foundation、同步人脸、SigLIP 图文向量与 QwenVL typed understanding 已作为发布外 experimental slices 落地，不扩大 v0.1 |
| 规划方式 | 以可演示的纵向能力和退出门槛推进，不以日期代替完成定义 |
| 首个发布目标 | 单机文本 + 本地文件音频推理控制平面 MVP |

## 1. 路线图原则

- 每个阶段都形成可运行、可测试的闭环；
- 先证明 Job 生命周期和调度正确，再增加 provider 数量；
- 先用 fake provider 覆盖失败语义，再连接真实付费服务；
- 任何阶段不得绕过 placement/data policy、capability floor、budget 和 cancellation 不变量；
- 远程节点和剩余多模态在基础 MVP 稳定后进入；已有本地权重驱动文件音频协议族提前形成纵向切片；
- 新 consumer/provider 提案可以提前完成架构评审，但不得借规划扩大或推迟当前 v0.1；
- milestone 只有在退出门槛全部满足时结束。

## 2. 阶段总览

```text
M0 工程基线
          |
M1 单 Provider 垂直切片
          |
M2 多 Provider 路由与可靠性
          |
M3 配额、持久化与可观测 MVP
          |
M4 本地资源与后台任务（已提前完成，不扩大 v0.1）
          |
   v0.1 发布门槛（当前）
          |
          +-----------------------------+
          |                             |
M5 可信远程节点与资源声明       M6 异构本地执行与类型化多模态
          |                             |
          +------ 共享 D-107 资源池设计门 ------+

M7 订阅式推理桥接（experimental，可独立于 M5/M6 演进）
```

M1-M3 合起来对应原始讨论中的 Phase 1；拆开是为了让每一步都有可验证的系统闭环。
M5 与 M6 的 stable 交付都在 v0.1 之后，可以按 consumer 优先级独立推进；ONNX 本地 P0 不依赖
远程 node-agent。D-107 已为 YuNet/SFace light slice 关闭为“不预建重型 Resource Pool”；
DINO/SigLIP、视觉 background 或 M5 capacity schema 任一出现时，两条线必须共同重开该门，
避免形成两套 Node 资源模型。

M7 不属于 v0.1 发布门槛。Codex App Server slice 已按真实 consumer 基础设施需求提前落地，
提供 text/image 输入、文本 unary/SSE 与独立授权的 hosted Web Search 子集，但保持无宿主机工具、
无会话；它不能延后当前稳定性收口，也不
把 Runtime 扩张成 Agent 平台。

## 3. M0：工程基线

### 目标

按已接受的设计与 ADR 建立仓库和可执行合同。工程基线已开始，尚未完成。

### 当前进度

- 已完成：Git 仓库、Cargo workspace、领域合同、Responses compatibility profile、fake provider、配置和本地 daemon/CLI 基线；
- 已完成：严格 lint、workspace 测试、本机 loopback API 冒烟，以及 CLI fixture → Ollama 的一次真实 Responses 调用；
- 已完成：历史 `candidate.1/2/3/4` 演进、日期化 Consumer Core `20260813.1`、独立 Capability Catalog、官方 Rust SDK、golden fixtures、运行时 contract manifest、严格 JSON/multipart/query/config 失败纪律与 route-level contract tests；
- 待完成：CI 和自动化端到端 acceptance scenario。

### 工作包

- 以 [已接受决策](docs/DECISIONS.md) 和 ADR 作为实现输入；
- 初始化 Git、Cargo workspace、license、格式化/lint/test 基线；
- 建立 crate/module ownership 与依赖方向检查；
- 定义 Responses compatibility profile、内部 Job envelope、控制事件与错误码的首版 schema；
- 建立 fake provider 与确定性测试时钟；
- 建立基础 CI，不依赖云凭证、GPU 或本机 Ollama；
- 写出通用 consumer 使用 OpenAI SDK 调用 `text.summarize`，并经轻量本地模型执行的端到端 acceptance scenario。

### 退出门槛

- `D-001` 至 `D-006` 的合同均已映射到 schema/tests；
- `/v1/responses` compatibility profile 与 `/infer/v1/*` 控制 API 有版本；
- fake provider 可模拟成功、慢流、限流、断流、取消和未知 usage；
- 空 daemon 与 CLI 可构建、启动、健康检查和干净退出；
- CI 在无外部服务条件下稳定通过。

## 4. M1：单 Provider 垂直切片

### 目标

用本机 Ollama 打通“Intent → workload rating → Build/Deployment → runtime 排队 → `/v1/responses` 执行 → 终态”的最小闭环。

### 当前进度

- 已完成：Intent 解析、按 workload 的 Model Profile/Build/Deployment registry、无状态 Responses 请求验证、Ollama adapter、有界优先队列、deadline、Job 状态、取消、控制面查询/取消/解释/metrics；
- 已完成：Intent 不泄漏为物理模型名、能力下限与 placement 正交过滤、应用别名拒绝、`infer.*` metadata 不转发上游、SSE 身份归一化测试；
- 已完成：独立音频核心契约、25 MiB 有界 multipart、常驻且单模型驻留的 MLX worker、音频 CLI，以及 ASR/强制对齐/三种 TTS build 的受控端到端验证；
- 待完成：取消对真实慢流的自动化测试，以及 M0 的 CI 和完整 acceptance scenario。

### 工作包

- `InferenceJob`、`Attempt` 和状态机；
- Responses-compatible submit/stream 与控制 API get/cancel；
- 有界 FIFO 队列和单 provider concurrency；
- 文本 Responses Intents，以及 `audio.transcribe`、`audio.align`、`speech.synthesize`、`speech.design_voice`、`speech.clone_voice` 文件音频 Intents；
- 静态 registry、共享 Responses adapter 与 Ollama capability profile；
- Job Coordinator 和 provider error normalization；
- CLI：`status`、`jobs`、`models/deployments`；
- 结构化日志和最小 latency metrics。

### 退出门槛

- 通用 SDK fixture 指向 `inferd`，可由本地 Ollama 完整流式总结；
- Client 取消能终止或隔离后端输出，Job 终态唯一；
- 队列满时给出稳定、可识别的拒绝；
- 慢客户端不会导致无界内存增长；
- fake provider 覆盖所有状态转换与断流竞争；
- `infer explain <job>` 能显示基本执行链。

### 明确不做

多 provider fallback、付费预算、远程节点、通用模型自动装卸、类型化实时音频和其他多模态
（这些能力后来分别进入 M2/M7 或发布外 experimental slices，不回写 M1 退出范围）。

## 5. M2：多 Provider 路由与可靠性

### 目标

加入第二个 cloud provider deployment，以公开的 Responses 合同接入，使配置 profile 与 request constraints 真正驱动选择，并建立可控 retry/fallback。

### 工作包

- 已完成（第一段）：DeepSeek V4 Flash cloud Responses provider、云端 Provider/Build/Deployment 注册，以及本地/云端的能力分层和 placement 约束；V4 Pro 等其官方 Responses 支持后再激活；
- 已完成（第二段）：policy-ordered Candidate Plan、稳定 hard-constraint reason codes、Job 固化的 routing explain，以及 auth/429/timeout/unavailable/invalid-request/protocol 的 provider 错误归类；
- 已完成（第三段）：独立的 in-memory provider health owner；连续三次可恢复上游失败后打开 30 秒 circuit，路由计划以 `provider_circuit_open` 明确排除该 provider，成功调用清除该状态；
- 已完成（第四段）：Job Attempt 链、非流式最多 3 次 Attempt、同候选最多重试 1 次、受 admission Candidate Plan 约束的 `equivalent`/`allow_lower_capability` fallback，以及流式可见输出后禁止切换 provider；
- 已完成（第五段）：Provider instance 的 versioned capability profile；请求字段按实际所需 endpoint/model capability 参与 Candidate Plan；显式 `POST /infer/v1/providers/{provider_id}/probe` 逐项验证配置声明，并返回结构化报告；
- 已完成（第六段）：三档优先级、aging 和 `max_pending_jobs` per-App admission 上限；已验证持续 interactive 负载下 aged background 会先于较新的 interactive ticket 获得释放的 slot；
- 已完成（第七段）：Responses provider contract matrix 覆盖基础执行、instructions、SSE、tools、reasoning effort、sampling、truncation、metadata，以及失败后停止后续探测。

### 退出门槛

- 同一 Intent Job 可按 capability floor、placement、reasoning effort 和获准 request override 在本机和云候选间选择；可信节点的实际执行面属于 M5；
- `balanced`、`local-first`、`capability-first`、`latency-first`、`cost-first` 模板可配置，并能解释生效层级；
- `local_only` 失败场景证明没有任何云请求；
- 首个可见输出后失败不会静默拼接另一模型输出；
- background 在持续 interactive 负载下最终获得执行机会；
- provider 的 auth/429/timeout/5xx/malformed stream 被稳定归类；
- 每次 fallback 都可解释且受 deadline/attempt 上限约束。

## 6. M3：配额、持久化与可观测 MVP

### 目标

让多个真实应用能安全共享 runtime，形成 `v0.1` MVP。

### 工作包

- global/provider/app 预算层级；（已实现，缺省不设限）
- USD/token/RPM/TPM/concurrency reservation 与结算；（已实现；USD 仍以 deployment 整次估算计）
- 配置、policy、usage ledger 和约定 Job 元数据持久化；（已实现 SQLite config snapshot、Job/Attempt、reservation、ledger）
- daemon restart recovery；（已实现：未完成调用进入 interrupted/failed，绝不自动重放）
- App credential 的安全生命周期与日志脱敏；（已实现 runtime-managed `local-operator`、
  Apps & Access managed Consumer 的一次性创建/轮换/撤销、外部 App environment source、
  owner-only 文件和无 key 本机 bootstrap；provider key 仍只由 daemon secret environment 注入）
- 完整 metrics、traces/audit events；
- 已完成：Console 完成/失败趋势由 SQLite terminal Job time buckets 提供，避免 daemon/Console
  重启后历史清空；Provider 队列公开当前 slots、pending/active 与 bounded service/wait estimate；
  policy 的 `queue_time` / `deadline_fit` 已使用该瞬时估计。
- 已完成（默认不启用）：本机 node admission capacity 的 `cpu_slots`、`unified_memory_mib`、
  `accelerator_slots` 配置与 per-Deployment measured claim。只有实际容量和 claim 都由 operator
  测量填写时才参与跨 Provider admission，避免让未校准模型阻塞或绕过并行。
- CLI：`queue`、`budget`、`usage`、`providers`、`explain`；（均已有对应控制 API/CLI 命令）
- 官方 Rust `infer-runtime-client` 已建立；后续按真实 Consumer 需求补语言绑定，不复制 Discovery、
  鉴权、合同握手和错误解析；
- 运维文档：安装、配置、备份/迁移、故障排查。

### 退出门槛

- 并发压力测试证明预算和 concurrency 不超卖；
- provider usage 缺失时明确标记 estimated；
- queued/running/settling 时崩溃均有确定恢复结果；
- 无凭证或敏感 prompt 出现在默认日志；
- 至少一个真实 consumer 完成集成并满足 ADR-0006 SLO；
- 24 小时混合负载 soak test 无 Job 泄漏、reservation 泄漏或无界增长；
- 发布 `v0.1.0`，MVP 范围和已知限制清晰。

### 当前反馈阶段

- 现行外部骨架已冻结为 `infer-runtime.consumer-core@20260813.1`，typed 数据平面由独立
  Capability Catalog/version 管理；历史 candidate 合同只保留为迁移档案，operator
  resource/provider 管理面继续留在 experimental；
- 已完成本地浏览器 `infer console` 接入期管理面：可 attach 或会话内启停 daemon，异步投影
  Jobs/provider（含 MLX audio）/resources，提供滚动 Statistics、可过滤/搜索日志、Job explain/
  cancel、显式 lifecycle load/unload/probe/refresh 和严格配置校验/原子保存；runtime credential
  保留在 loopback Console 后端，旧 TUI 只作为 `terminal-console` 故障排查入口。它复用现有
  控制 API，不冻结 experimental operator schema，也不把会话趋势误写成持久监控；
- 已加入 Apps & Access operator surface：按 App 管理 policy/request override、managed token
  指纹与一次性创建/轮换/撤销，保护 `local-operator`，并以明确的 restart-required 状态保持
  “配置已保存”与“运行中认证表已生效”的区别；现已加入 App-level Intent allowlist，统一在
  Job admission 前覆盖 Responses、音频、视觉和 durable submission，拒绝不会创建 Job 或
  触发 provider；
- Echo 已迁移到 runtime-managed Consumer credential，并一次性导入 Echo 自己的 owner-only
  secret store；`resource_admin=false`、`local-first/local_only/background` 和无 fallback 的
  最小权限保持不变。重启认证表后的真实 `audio.transcribe -> audio.align` 已通过，两个 Job
  均为 `app_id=echo`、local MLX deployment 和单次成功 Attempt，合同、provider、deployment 与
  model build 证据已写入 Echo Catalog。该证据开启 consumer 反馈期，但尚不替代连续 SLO/soak 门槛；
- 下一步由 Echo、Shadow、Shape、Symbiont-d 统一迁到官方 SDK，覆盖非流式、SSE、取消、错误和
  explain；所有租户就绪前不切换当前 daemon；
- Core breaking 才发布新的日期化 Core；单能力 breaking 只提升对应 Capability 日期版本，已发布
  身份不原地修改；
- 正式发布不抢跑，继续等待 24 小时混合 soak、SLO、trace 与连续使用证据；常驻系统服务、
  跨会话指标/日志持久化和 MLX worker cache residency 观测不是外部 consumer 接入前置条件，
  分别在出现真实运维需求与资源合同证据后推进。

## 7. M4：本地资源与后台任务

### 目标

从“请求路由器”升级为真正感知本机资源与模型生命周期的 runtime。

### 当前进度

- 已完成基础闭环：Ollama `/api/tags` + `/api/ps` inventory、macOS 系统压力观测、
  deployment lifecycle snapshot，以及失败时保守排除不可用 deployment；
- 已完成安全控制面：Attempt reservation 覆盖排队/执行/stream 生命周期，Ollama
  load/unload 只能由认证 operator 显式发起；卸载在 atomic draining 中拒绝现有和
  新增 reservation，失败或取消会回滚；
- 已完成纯内存确定性验证：eviction 选择、最小驻留、缺失/过期 reload benchmark 的
  保守拒绝、pressure 的 dry-run recommendation，以及 refresh 与 lifecycle action
  重叠时不重新开放路由；
- 已完成显式 reload benchmark command：只测量 absent deployment，输出带证据的
  version-controlled TOML 记录，不直接改配置；
- 已在受控开发环境采集多个资源档位的 reload profile；单次极重模型样本只作
  可行性证据，不进入可移植 eviction 配置；
- 已完成全局、resource class、deployment 三层 eviction safety：自动候选默认关闭，
  heavy 默认保护，只有经本机测量和逐项审核的 Deployment 才能覆盖放行；并以受控
  reload cost + 合成 resident-memory 快照通过 critical-pressure deterministic 模拟；
- 已完成 approval-gated action 内核：fresh plan exact-target 确认、串行且单目标执行、
  reservation 原子复核、错误后 fail-closed reconcile，以及独立于 Job 的持久资源审计；
- 已完成默认关闭的后台 pressure monitor 与短期进程内维护租约；每轮 fresh plan 最多
  执行一项，重启/到期/撤销 fail closed；512 轮 reservation/unload 线程竞态验证每轮
  恰好只有一方成功；
- 已完成首次受控实机 soak：2B/4B/VL 逐级加载、2B 四请求并发、短维护租约与取消
  均能收敛，排队请求全程持有 resource reservation，清理后无 Job/reservation/模型驻留
  泄漏；当前 Ollama 会自行调整同时驻留集合，整轮系统压力保持 normal，因此没有伪造
  pressure 来宣称自动清退已通过；
- 实测还发现 thinking-capable 2B 在简单总结上会产生数千 token 的无界思考，已据此为
  Responses Intent 增加可配置的默认输出预算与 reasoning effort，默认值不覆盖调用方显式
  标准字段；
- 已完成可控实机 action soak：压力分类阈值成为版本化主机策略，默认保持 15%/5%；
  临时提高阈值后仍使用真实系统采样，验证 2B/4B 按相邻 poll 逐项清退、内存恢复、
  最小驻留阻止抖动，以及 16 条 interactive 批次持有 2B reservations 时只清退空闲
  4B；同一 runtime 共 25 个真实 Job 全部成功；
- 已完成 App-scoped Job metadata keyset paging：SQLite 迁移增加 priority 投影和排序索引，
  API/CLI 支持按 state/priority 筛选；100,005 条记录以固定 257 条页面完整遍历，无重复、
  漏读或完整 Job snapshot 反序列化；
- 已完成 D-105 payload ownership：新增独立 AES-256-GCM spool，SQLite 只持有带密钥
  HMAC identity 的引用；输入/结果、App binding、size/path、tamper、权限、原子发布、
  retention 和 orphan reconciliation 均有确定性测试；
- 已完成 Responses local background 纵向链路：`background: true` create、retrieve、cancel，
  App isolation，同一 Response ID 的 bounded restart recovery，以及
  `initial/interrupted → recovery/succeeded` Attempt/audit；provider 实际收到的 request 会移除
  background ownership；
- 已用真实 Ollama `qwen3.5:2b-mlx` 验证正常 background 和 running 中断后重启恢复；
  两条 Job 均成功，恢复计数为 1，spool/SQLite/WAL 未检出输入或结果明文，测试结束后
  模型、daemon 和临时数据均已清理；
- 已完成持久化故障注入：结果 blob 已写入但 SQLite 发布失败时删除未发布结果、保留
  running Job 与加密输入；输入引用清除事务失败时也不删除仍被引用的 blob。故障移除并
  重启后，同一 Job 以 Recovery Attempt 成功完成；
- 已完成 512 个 running background Job 的三轮确定性 restart soak：前两轮分别收敛到
  replay 1/2，第三轮全部按上限进入失败终态；512 个 reservation 只结算一次，最终无
  active reservation、pending Job 或遗留 metadata reference；
- monitor 继续默认关闭，作为 production-safe 默认值；未来日常负载数据只决定是否启用
  自动动作，不再阻塞 M4；
- **M4 已收口**：全部退出门槛满足。background batch、音频/大 payload external reference
  和 cloud/可信节点幂等恢复需要新的 payload/幂等合同，分别留给后续阶段，不扩张本里程碑。

### 工作包

- RAM/VRAM/系统压力采集的可插拔实现；
- 模型 lifecycle state machine 与 load/unload owner；
- reservation-aware eviction、最小驻留时间和 anti-thrashing；
- durable local text background Job、故障安全发布与有界重启恢复；（已完成）
- 更完善的 fairness 与吞吐调度；
- benchmark 命令及可审计的性能档案；
- 资源模拟器与 deterministic scheduler tests。

### 退出门槛

- 活跃模型绝不在 reservation 存在时被卸载；
- 在受控内存压力场景中可释放空闲模型并运行高优先级 Job；
- 重复交替负载不会造成无界 load/unload 抖动；
- 10 万级 background Job 元数据不要求一次性驻留内存；
- benchmark 数据过期或缺失时有保守退化行为；
- local background 在 daemon restart 后保留 Response ID，重放有界且不泄漏 payload 明文。

## 8. M5：可信远程节点

实验进展（2026-09-23）：已增加显式配对的 unary text Node 切片，复用执行机器的 Runtime，
提供独立 mTLS 入口、导入契约校验、动态可用性过滤、远程准入租约、取消及结果未知时禁止重放。
同机 A/B/C 独立进程测试使用确定性后端验证协议；不代表真实局域网、模型负载或完整 M5 已通过。
配置与验收见 [TRUSTED_NODES.md](docs/TRUSTED_NODES.md) 和 [ADR-0019](docs/adr/0019-trusted-text-nodes.md)。

### 目标

让 DGX、Linux GPU、Windows GPU 等成为受 runtime 管理的 `Infer Node`，而不只是另一个 HTTP endpoint。

### 工作包

- 关闭 D-107：审查本机/远程 Node 的 capacity/resource declaration 是否需要显式
  Resource Pool；若需要，先定义 system/unified/GPU memory、CPU slots、accelerator queue、
  resident-model budget 的最小跨 provider schema 与原子 reservation 语义；
- 独立 `node-agent` 与版本协商；
- 配对、节点身份、mTLS/等价安全通道与吊销；
- capability/resource 声明、心跳租约和 stale-node 处理；
- 远程 reservation、dispatch、取消和断连语义；
- 数据 locality/size 参与路由；
- 节点管理 CLI 与审计；
- 协议兼容和滚动升级策略。

### 退出门槛

- Node capacity schema 与本机 Resource Manager 使用同一概念模型，未知/过期/估算不确定时
  fail closed；不能让 remote 与 future ONNX 各自发明容量单位；
- 未批准或已吊销节点不能获得 Job/payload；
- 节点失联后 reservation 有界回收，Job 结果不出现双重成功；
- 旧 agent 与新 daemon 的兼容范围明确并经过测试；
- `trusted_nodes` 与 `local_only`/`cloud_allowed` 边界可验证；
- 大 payload 不因控制平面中转产生无界复制。

## 9. M6：异构本地执行与类型化多模态协议族

> 当前状态：foundation、同步人脸、SigLIP image/text embedding 与 QwenVL typed understanding
> slices 已实现并进入 experimental feedback；视觉 durable 与 stable promotion 尚未开放。

### M6.1：RawNIND 同机制品租约与实验执行纵切

该工作包先关闭 D-102 的同机实验切片，再以独立发布门交付 RawNIND 模型纵切：

1. **Phase 0 / 合同冻结**：Shadow 保留 decoder、RawFrame staging、Recipe/source revision、
   `.shadowrawf` verifier/cache/recovery/publish 与 UI；Infer 后续只接管 immutable Build adapter、
   ONNX Session、admission、cancel/progress 和 Attempt provenance。RGGB normalization、tensor、
   tiling/overlap/blending 与两遍 global gain 随 Build adapter，不进入本轮实现。
2. **Phase 1 / Unix FD lease**：认证控制面预发 App/Job/generation-bound ticket；owner-only Unix
   socket 以 `SCM_RIGHTS` 接受 read-only input + empty exclusive writable output，形成 TTL/one-shot
   lease。拒绝路径、symlink/device、hard link、same-file、错误 open flags、过期 generation 与
   scope mismatch；cancel/revoke/drop 关闭句柄。
3. **Acceptance fixture**：确定性大文件只通过 64 KiB stripe buffer copy/hash；receipt 明确报告
   bytes/digest/maximum explicit buffer 与零 algorithmic full-payload buffer。真实 benchmark 继续测 copy volume、
   private dirty memory 与 warm overhead，不能只凭代码结构宣称零拷贝。
4. **Windows gate**：冻结 Named Pipe + peer identity + duplicated HANDLE 共同语义；实现、reparse/
   file-id/access-mask 测试和真实 Windows smoke 完成前保持 blocked。

**Phase 1 stop/go（已通过）**：Unix peer/FD/TTL/one-shot/revoke/expiry/cleanup 与有界 fixture 全部
通过后才进入 Phase 2；这条顺序约束保留为后续大制品 Provider 的先例。

**Phase 2 当前 checkpoint（实现与发布门已通过）**：typed descriptor + single sample FD、两遍
32-tile CPU adapter、lease-owned `.shadowrawf` writer、production composition、candidate.3 route、
exact Build/Deployment 与 Shadow 最小 ACL 已接线。ORT 1.27.0 使用独立 experimental Build/cache
identity；旧 1.24.4 cache 不复用。真实 HTTP/UDS/ORT E2E 已验证成功执行、精确 graph/revision
provenance、pending cancel、in-flight cancel 与终态 `cancelled`。synthetic comparator 达到 exact payload/stripe/pixel parity，
max-abs=RMSE=0；流式 writer 的显式整图输出 buffer 为 0、滚动累积器最多 824/2048 rows；实测
wall 与 RSS 在 legacy 同量级，single-tile in-flight cancel 约 1.7ms。Shadow strict client 已绑定
专用 Provider 与 exact graph/revision identity。剩余 promotion 门是扩大真实性能样本、24 小时 soak、
真实照片域质量评估和 Windows handle 实现；这些不阻塞本机 candidate.3 experimental feedback。

### 目标

在已验证的文件音频和 M4 Resource Manager 上，逐个加入独立数据协议族与执行族，继续验证
“调度统一、能力分型、原生生命周期分型”。Shadow 的 ONNX/视觉需求属于本阶段的候选纵向
切片，不属于 v0.1，也不要求等全部 M5 功能完成；它只依赖 D-107 的共享资源合同先关闭。

### 实现前设计门

1. **D-106 / ONNX 与视觉边界**：确认 ONNX Provider Runtime、Session Registry 与 typed
   capability adapters 的职责；普通 App 不暴露 tensor-map、物理 backend 或模型路径；
2. **D-107 / Resource Pool**：以 Shadow background 索引、interactive QwenVL 和 MLX audio
   混合场景评估当前 per-provider capacity + 全机 pressure 是否足够；证据需要时才增加显式
   Node Resource Pool 与多资源原子 reservation；
3. **D-108 / 视觉隐私**：关闭有界 RGB artifact/reference、source/Recipe revision、
   `SensitiveBiometric`、retention、删除和迟到结果发布合同；视觉 background 另行复审，
   不默认复用 ADR-0010 文本 spool；
4. **Build manifest schema**：版本化 artifact/export digest、ONNX opset、tensor contract、完整
   preprocessing、embedding space、tokenizer/vocabulary、Execution Provider/precision/fallback、
   平台验证和 code/weight/data license facts。

P0 已按 D-106/D-107/D-108 的收窄决定关闭并实现；同步 SFace face embedding 是第二个
experimental slice，SigLIP 2 image/text 共享 space 是第三个。SAM 2.1 prompted subject mask 与
BiSeNet 19-class face parsing 已作为另外两条 local-only typed slice 接入。重型 Resource Pool 已按真实
约 1.9 GiB residency 复审但没有扩张核心；视觉 durable、稳定协议 promotion 和 Windows
tolerance 仍不得借用本次关闭结果提前进入生产承诺。

### 推荐纵向顺序

1. **ONNX provider foundation**：Session Registry、Build verification、create/warm/drop、实际
   Execution Provider route/provenance、资源估算、取消和错误归类；先由 fake model/session
   覆盖合同，不以真实权重作为 CI 前置；
2. **首个 Shadow 视觉 slice**：YuNet `vision.detect_faces` 已完成；
3. **后续本地视觉 slices**：SFace `vision.embed_face`、SigLIP
   `semantic.embed_image` + `semantic.embed_text`、SAM 2.1 `vision.segment_subject` 与 BiSeNet
   `vision.parse_face` 已分别按 Build/space/privacy 门槛形成 typed vertical slice；
   DINO 或受控 vocabulary classification/tagging 仍须重新验证；
4. **QwenVL 结构化理解**：`vision.describe_image` 返回有界短描述与关键词 proposal，
   `vision.classify_closed_set` 只在 Consumer 提供的闭集内返回 `matched|none|uncertain`；
   两者均不泄漏 Ollama chat schema。4B 是 foundational/standard bulk 候选，8B 是 capable/heavy
   明确复核候选；Consumer 以 capability floor 选择能力下限，不绑定物理 tag。首版保持同步，
   可以使用 background priority，但不开放视觉 durable；
5. **实时 ASR/TTS**：PCM TTS server-stream 与 commit-redecode ASR duplex experimental slice
   已完成；下一门槛是真实 Consumer 的慢读/断开 soak、长会话内存与延迟曲线，以及原生增量 ASR
   provider。它与视觉 slice 可按 consumer 优先级独立提升稳定级别。

每个协议族必须单独完成 schema、capability、provider 合同套件、resource estimate、取消、usage、
SLO 和至少一个真实应用集成。不能只新增 Intent 枚举或共享 Session owner 就声称支持。

### ONNX/视觉合同测试门槛

- artifact/checkpoint/export、tensor、preprocess、tokenizer/vocabulary、embedding-space 与
  license identity 可验证，identity 漂移必须形成新 Build/space；
- Session load/unload 与 lifecycle/capacity reservation 竞态下，活跃 Attempt 不被卸载、容量
  不超卖、失败后 residency 可 reconcile；
- queued/running cancel、provider 不可中断和迟到结果均保持唯一终态，revision 失配结果不发布；
- Execution Provider fallback 明确记录实际 backend、precision、fallback reason 和 runtime
  version，不静默借用配置 backend 的 provenance；
- macOS 与 Windows 使用任务级 numerical tolerance 和 decision-stability 门槛；不要求浮点逐位
  一致，但检测阈值/近邻排序/top-k 决策不得越过已定义稳定性界限；
- `local_only` 和 `SensitiveBiometric` 对 cloud fake endpoint 零触达，默认 metadata/log/audit
  不含像素、face crop 或 embedding；
- Shadow 只在类型化协议、Build/preprocess identity 和 D-108 payload 合同稳定后接入，并验证
  source/Recipe revision、stale-result 丢弃、viewport/background priority 映射。

### P0 当前完成证据（更新于 2026-08-13）

- 内容寻址 artifact store、原子 publish、manifest drift/digest/size 校验已完成；
- ONNX Runtime 1.27.0 动态加载、Session Registry、统一 native load/unload/inventory 已完成；
- YuNet 2026May CPU inference、官方示例人像 boxes/五点、完整 HTTP/鉴权/Job/Attempt 链路已通过；
- SFace CPU Session load/inventory/unload、官方五点相似对齐、128 维 L2 归一化与完整
  HTTP/鉴权/ACL/Job/Attempt 链路已通过；向量只出现在同步响应，不进入 Job metadata；
- Apple Core ML SAM 2.1 Small 九文件 artifact-set 已逐文件验证并发布；合成无敏感图的真实
  Core ML cold smoke 返回同尺寸二值 PNG mask。point/box prompt、取消和 worker frame 均有界；
- BiSeNet ResNet18 opset 20 Build 固定 512 RGB/ImageNet preprocessing、19-class ontology、
  1.8 倍 face context 与 full-image indexed PNG restore；typed adapter 与边界单测已通过；
- SigLIP 2 exact checkpoint/export/image graph/text graph/tokenizer 已形成内容寻址 Builds；
  `semantic.embed_image`/`semantic.embed_text` 返回同一 768d L2 space，中文文本与 Shadow managed
  credential 的真实 HTTP/ACL/Job/Attempt E2E 已通过；图片、查询与向量不进入 Job metadata；
- release CPU 实测 image/text warm 约 100/33 ms，两 Session 合计约 1.9 GiB RSS，均标记
  `heavy`；当前 graph 的 Core ML 失败探测过慢，因此 immutable Builds 固定 CPU-only；
- QwenVL 两条 typed route 已完成 strict multipart、闭集输出仲裁、4B/8B capability routing、
  local-only/offline/no-fallback 强制约束和 payload-free Job metadata；provider adapter 使用
  Ollama native vision transport，但 Consumer 不接触其 chat schema；真实 Shadow credential
  HTTP E2E 已通过：4B foundational 描述约 32.5 秒（load 2.8 秒）、8B capable 描述约 51.0 秒
  （load 6.2 秒）、warm 8B 闭集复核约 9.4 秒；本机 8B 实际驻留约 7.81 GB。数值只作为
  当前 Build/机器的 admission 与 SLO 起点，不提升 provisional 能力评级；
- Core ML 使用严格“不得暗中借 CPU”建 Session；两份当前 Build 均明确拒绝严格 Core ML，配置
  允许时重新建立纯 CPU Session并披露 requested/actual EP 与稳定 fallback reason；
- experimental schema 尚未提升为冻结 Consumer contract；真实 Consumer feedback 与跨平台
  tolerance 仍是 promotion 门槛。

### Shape `image.edit` 外部依赖（Blocked，不得提前开放）

Shape 已冻结生成式多输入图片编辑需求，但当前 Runtime 没有任何已验证的 raster-output
Provider/Build/Deployment，因此本工作包只登记依赖，不创建 Intent、route、ACL 或 mock executor。
实现时必须按 D-111 additive 发布独立的 `infer.image.edit@<date-revision>` Capability Schema，
使用独立 `POST /v1/images/edits` strict multipart 数据面；不需要升级 Consumer Core，也不能
复用只返回文本的 `/v1/responses` 或 QwenVL understanding route。

开放顺序固定为：真实执行面与 artifact/Build identity → 有界 source/mask/reference parser →
单 raster unary 输出及 SHA-256/geometry headers → payload-free Job/Attempt provenance → cancel/late
result、ACL 负面与真实 HTTP raster E2E。全部门槛通过后才给 `apps.shape` 增加 `image.edit`；
cloud image input、Voice/identity 模仿、Runtime 持久化像素和 Shape Scene/Candidate ownership 均不
随该工作包隐式开放。

### 当前模型候选（仅评测输入）

| 能力 | 候选 | 进入 Deployment 前仍需确认 |
| --- | --- | --- |
| 人脸检测 | YuNet 2026May ONNX（已固定 Build） | Shadow 域质量、Windows tolerance、stable contract promotion |
| 人脸向量 | SFace ONNX（实验同步协议已实现）；合法授权的 ArcFace/InsightFace build | Shadow 域质量、Windows tolerance、stable contract promotion |
| 图片相似向量 | DINOv2 ViT-S/14 ONNX export | exact export/opset、许可、内存/延迟、Shadow 照片域检索稳定性 |
| 场景语义/图文检索 | SigLIP 2 Base 224 ONNX（experimental image/text slice 已实现） | Shadow 照片域质量、Windows tolerance、stable promotion |
| Caption/复杂理解 | QwenVL-4B/8B via Ollama（experimental typed slices 已实现） | Shadow 照片域质量、混合压力/SLO、stable promotion |
| 音频 | 现有 MLX audio worker | 保持独立音频协议与 native cache owner |

候选清单不是采购、授权、下载、转换、分发或支持承诺；本机发现某文件也不自动创建 Build 或
Deployment。

### 退出门槛

- 各协议族没有污染其他协议的 payload 类型，核心 scheduler 不解释 tensor/preprocessing；
- 共享 Job envelope、quota、scheduler、Resource Manager 和 audit 契约无需分叉；
- 每个 ONNX/视觉 slice 通过上述全部合同门槛和 Shadow 真实 consumer acceptance；SigLIP 当前
  达到可接入反馈的 experimental 门槛，stable promotion 仍等待真实图库域与 Windows；
- 大对象采用有界传输或有授权/租约的引用机制，视觉与 biometric 隐私边界可验证；
- 未实现的视觉能力没有因共享 ONNX provider 或模型候选而出现在支持矩阵。

## 10. M7：订阅式推理桥接

> 当前状态：Codex App Server execution slice 已实现，并增加标准 Responses hosted Web Search 的
> 有界子集与独立 App ACL。
> Web Search 当前以 candidate.3 的有界标准 Responses tool 形状发布；Provider 执行仍是
> experimental，Intent、subscription access 与 hosted-tool ACL 三道授权保持独立。

### 已完成的纵切

- `codex-subscription` 是一个 cloud/subscription Provider，共享一套 scheduler/concurrency pool；
- `model/list` 动态发现模型组，但只有 Sol、Terra、Luna 的静态 Build/Deployment 可路由；
- Luna 作为 `language.respond`/`multimodal.respond`/`reasoning.solve` advanced 候选，Terra 作为
  language/reasoning expert 候选，Sol 作为 `reasoning.solve` exceptional、multimodal expert 候选；
  评级均为 provisional；
- public surface 支持 text/image、non-streaming 或 Responses SSE、instructions、
  `reasoning.effort`，以及标准 `web_search + tool_choice` 的有界子集；仍不支持 function tool
  执行、sampling、`max_output_tokens`、metadata passthrough、conversation 或 durable background；
- image data URL 会有界校验并写入 ephemeral workspace；HTTPS image 保持 URL input，本地路径
  不进入公共合同；调用前还会复核动态 catalog 的 `inputModalities`；
- App 默认只有 `standard` access class；`subscription` 必须显式授权；图片进入 cloud 还要求
  独立 `allowed_cloud_input_modalities=image`，两类拒绝分别使用稳定 reason code；
- hosted Web Search 还要求独立 `allowed_builtin_tools=web_search`；Intent、subscription access
  class 与 Provider capability 都不能隐式授予。非 Web Attempt 强制 disabled，获准请求才按
  `external_web_access` 选择 cached/live，`required` 还必须观察到完成的 `webSearch` item；
- 受控订阅环境的 `model/list` 与一次低投入调用已通过；fake App Server 覆盖 catalog、usage
  normalization、Web Search action normalization、required postcondition 和越权 item fail-closed。

### 后续工作包

1. **进程生命周期与取消**：从每 Attempt fail-closed 子进程评估为受监管的持久 App Server；
   只有证明 turn interrupt、进程崩溃恢复、并发隔离和 credential/session 生命周期可靠后才复用；
2. **订阅 quota**：把稳定的 rate-limit snapshot 映射为 operator telemetry 与 admission 窗口；
   不把月费误记为逐请求 USD=0，也不向普通 Consumer 暴露账号级详情；
3. **Streaming soak**：已实现 Responses SSE normalization；继续验证真实慢消费者、首 delta 后
   cancel/late-result、进程 crash 与长输出背压；
4. **图片输入 soak**：已实现有界输入和独立 cloud modality ACL；继续验证真实 Consumer、URL
   取图失败分类、图片域限制与订阅额度行为；
5. **模型准入自动审计**：catalog drift 只产生告警，新增/升级模型必须显式评测和配置变更；
6. **Web Search soak**：用真实低风险请求验证 cached/live、SSE、取消、上游 quota/error 与
   `web_search_call` 输出；App Server 提供稳定 citation DTO 前不伪造 URL annotation；
7. **第二种 bridge**：只有 Claude 等第二协议出现且重复模式成立后才提取更多共性；仍不抽象通用
   CLI DSL。

### 退出门槛

- 一个 Provider 多 Deployment 的 discovery/admission/absence/upgrade 行为有合同测试；
- 未授权 tool/file/network side effect 对标准推理 surface 为零，获准 Web Search 只产生标准
  `web_search_call`，任何其他非推理 item 都 fail closed；
- subscription entitlement 在 App、fallback、retry 和 explain 中不可绕过；
- cancellation、deadline、quota exhaustion、进程 crash 和 malformed JSON-RPC 有唯一终态；
- 至少一个真实 Consumer 经过持续反馈后，才把所需字段从 experimental 提升为 stable。

## 11. 首批可直接转成 Issue 的工作包

以下是当前剩余的首批工作包；仓库和核心垂直链路已经开工：

1. 建立 CI 和依赖方向检查；
2. 补齐 Attempt 状态、终态竞争和属性测试；
3. 扩展 deterministic fake provider/executor 的失败、取消和迟到结果场景；
4. 自动化通用 SDK fixture → `text.summarize` → Ollama acceptance；
5. 自动化 TTS → ASR → alignment 本机可选 acceptance；
6. 增加完整 explain reason trace；
7. 定义 SenseVoice/FunASR executor 是否进入支持矩阵。

建议每个 issue 指明唯一 primary owner、输入/输出合同、失败语义和验收测试，避免“实现 scheduler”这类不可关闭的大任务。

## 12. 发布门槛

### v0.1（单机文本 + 文件音频 MVP）

- M0-M3 完成；
- 至少一个真实 consumer 连续使用；
- wire/schema migration policy 文档化；（candidate 已完成，正式发布前按 consumer 反馈复核）
- 默认安全绑定、secret 管理和脱敏通过审查；
- 无 P0/P1 correctness 问题，尤其是隐私越界、预算超卖、重复终态和 reservation 泄漏。
- ONNX provider、视觉协议、Shadow 集成、D-106 至 D-110 均不是发布门槛，不得因此延期。

### v0.2（本地资源 runtime）

- M4 完成；
- 至少一个平台有可靠资源感知；
- durable background workloads 经过规模测试。

### v0.3（多节点）

- M5 完成；
- 节点身份、断连和升级经过安全与故障测试。

多模态可以按独立 minor release 逐项发布，不等待全部完成。

## 13. 风险登记

| 风险 | 早期信号 | 缓解措施 |
| --- | --- | --- |
| Responses-compatible 差异远大于预期 | 参数被忽略、stream/usage 不一致 | capability profile + provider 合同测试，不宣称完整 OpenAI 平台兼容 |
| Scheduler/Router 职责混合 | 选择逻辑依赖队列内部状态且难测试 | Candidate Plan 明确边界，snapshot 输入与 reason output |
| 并发下预算超卖 | 多 App 同时通过检查后超过上限 | reservation + 原子多层账本 + 压力/属性测试 |
| streaming 取消竞态 | cancelled Job 后又变 succeeded | Coordinator 独占终态转换，迟到事件隔离 |
| 本地模型反复装卸 | 交替任务导致延迟和硬件抖动 | 最小驻留、load cost、hysteresis、仿真测试 |
| 多个本地 runtime 形成多个资源 owner | Shadow/ONNX、Ollama、MLX 同时认为容量可用 | 统一 admission/lifecycle reservation；D-107 用混合负载决定是否引入 Resource Pool |
| embedding identity 漂移后仍混用向量 | 模型/预处理升级后近邻结果异常 | Build 绑定 artifact/preprocess/space identity，不同 space 禁止直接比较 |
| 人脸/图片 payload 泄漏 | embedding 或 crop 出现在 Job metadata/log/云请求 | D-108、SensitiveBiometric、local-only、payload-free metadata 与 fake cloud 零触达测试 |
| Execution Provider fallback 不透明 | 配置显示 CoreML/WinML，实际落到其他 backend | Attempt 记录 actual backend/precision/reason，按平台 tolerance 与 decision stability 验收 |
| 指标高基数或泄露 prompt | 观测成本增长、敏感数据进入日志 | 固定 labels，ID 进 trace，默认 metadata-only |
| 过早抽象多模态 | core 类型出现大量 optional 字段 | 协议族分离，只共享 envelope 与控制合同 |
| 远程节点扩大攻击面 | 未授权节点/客户端能提交或读取任务 | 明确推迟；配对、双向身份、最小授权、审计 |
| 路由“聪明”但不可解释 | 用户无法理解为何走云端/花钱 | 分层规则、policy version、reason codes、explain CLI |
| 范围膨胀为 Agent 平台 | 出现 workflow/memory/tool loop 需求 | 守住非目标，交给独立上层 runtime |
| 本机 bridge 被误判为本地推理 | 隐私请求或 offline 请求进入订阅云端 | placement 固定 cloud，offline/local-only 硬过滤，App subscription entitlement |
| 动态 catalog 自动扩大能力 | 上游新增模型未经评测就进入路由 | discovered != admitted；静态 Build/Deployment allowlist 与 drift 告警 |
| Agent 工具从推理桥泄漏 | 标准 inference request 触发文件、命令或 MCP side effect | 功能禁用、空只读 workspace、禁止审批、item fail-closed 与真实合同测试 |

## 14. 暂不排期

- Agent runtime、tool execution、memory；
- Web 管理台；
- 模型训练/下载市场；
- 多用户组织、计费产品化、HA control plane；
- Kubernetes 风格集群调度；
- 第三方进程内动态插件 ABI；
- 自动 prompt 优化或模型输出质量裁判；
- 无人工确认的自治成本扩张。

只有出现明确 consumer、稳定使用场景和可验证验收条件时，才把这些项目移入路线图。
