# 架构决策清单

截至 2026-08-10，启动实现所需的 Blocking 决策和 M4 durable background payload ownership
已经关闭。异构本地执行与视觉决策在 v0.1 之外依次接受 ONNX artifact/session foundation、
同步 local-only 人脸能力、SigLIP 图文语义向量，以及 QwenVL typed understanding slices；它们
不改变冻结的 v0.1 发布门槛，也不授权视觉 durable、万能 tensor API 或通用 Resource Pool。

状态：`Proposed`、`Accepted`、`Rejected`、`Superseded`。

## 已接受决策

### D-001：文本数据面采用 OpenAI Responses API 兼容合同

- **状态**：Accepted
- **决定**：`inferd` 的文本数据面公开 `POST /v1/responses`，非流式响应与 SSE 流事件遵守 OpenAI Responses API 兼容合同。Job 查询、队列、预算、解释和管理功能使用独立的 `/infer/v1/*` 控制 API，避免污染 Responses wire schema。
- **Intent 语义**：标准 `model` 字段填写稳定 Intent Profile，如 `text.summarize`、`assistant.general`，而非应用别名或物理模型。Intent 定义模态、features、默认质量下限和 policy。管理员调试接口才允许物理 deployment override。
- **请求动态约束**：使用 Responses 的 `metadata` 中保留的 `infer.*` keys 表达 priority、placement、placement preference、provider access class 收窄、quality floor、latency、max cost、fallback、deadline 和可选 policy profile；标准 `reasoning.effort` 独立表达选中模型后的计算投入。
- **MVP 子集**：无状态 `input`/`instructions`、streaming、function tools、基础生成参数与 usage。`previous_response_id`、`conversation` 和服务端 conversation state 不在 MVP；不支持的字段返回明确错误，不能静默忽略。
- **合同制品**：首个外部反馈基线为 `0.1.0-candidate.1`，由 `contracts/v0.1/openapi.json`、fixtures 和 daemon 的 `/infer/v1/contract` 固定；operator resource/provider 管理路由不在该 consumer 承诺内。
- **ADR**：[ADR-0001](adr/0001-responses-api-data-plane.md)

### D-002：MVP 持久化 metadata 和账本，普通请求不恢复执行

- **状态**：Accepted
- **决定**：M1 使用内存 Job；M3 引入 SQLite，持久化配置快照引用、Job/Attempt metadata、decision events、usage ledger 和 quota reservations。interactive stream 不跨 daemon 重启续接，MVP 也不自动恢复未完成执行。
- **恢复语义**：普通交互请求重启时将未完成 Attempt 标记为 `unknown/interrupted`，所属 Job 进入确定的失败终态；不得自动重放可能重复计费或产生副作用的调用。M4 对显式 durable local background Job 的扩展由 D-105/ADR-0010 单独约束。
- **ADR**：[ADR-0002](adr/0002-mvp-persistence-and-recovery.md)

### D-003：TOML 配置 + 类型化 policy profiles + CLI 管理

- **状态**：Accepted
- **决定**：Intent、Model Profile、Model Build、Deployment、Provider、App policy、quota 和默认 profile 使用严格 schema 的 TOML，可进入版本控制。CLI 提供 validate、reload、show-effective-config 和 explain；SQLite 保存运行状态与历史，不作为配置真源。
- **严格性**：根配置及所有嵌套结构都拒绝未知键，避免拼写错误静默退回默认值；新增键必须先进入类型和迁移/兼容性评审。
- **动态性**：配置变更先完整解析和验证，再原子切换到新版本；已 admission 的 Job 固定使用提交时记录的 policy/config version。MVP 不引入通用 DSL、Web UI 或任意脚本策略。
- **ADR**：[ADR-0003](adr/0003-configuration-and-policy-profiles.md)

### D-004：公开文本合同固定为 Responses；上游协议由窄 adapter 翻译

- **状态**：Accepted
- **决定**：`inferd` 对应用始终公开 OpenAI Responses 兼容子集，原生支持 `/responses` 的服务复用同一个 Responses adapter。当前本机 Ollama 和 DeepSeek V4 Flash 均走这条路径；应用、Intent、策略与 Deployment registry 不感知供应商差异。
- **DeepSeek 边界**：官方 Responses API 当前只支持 `deepseek-v4-flash`，所以 Flash 是实际 cloud deployment；V4 Pro 的 Profile/Build 可以预先登记，但在其 Responses 支持正式可用前不得创建 deployment 或候选。Chat Completions 不在当前 runtime 的执行面，避免为了单一 provider 形成过早抽象。
- **Capability contract**：每个 provider instance 必须配置 versioned capability profile；请求实际使用的 fields 在 Candidate Plan 中检查，缺失时以稳定 reason code 拒绝。`POST /infer/v1/providers/{provider_id}/probe` 是显式、可能计费的合同验证，逐项检查声明并在首项失败后停止；它不在启动时自动运行。
- **Ollama 边界**：最低兼容基线为支持 `/v1/responses` 的 Ollama 版本；当前官方文档标记该端点自 v0.13.3 加入，且只支持无状态 flavor。probe 不能只依据版本号。Ollama 专有模型发现和生命周期控制作为窄扩展存在，不改变推理数据面。
- **ADR**：[ADR-0004](adr/0004-responses-provider-profile.md)

### D-005：配置决定策略，请求在授权范围内动态收窄或选 profile

- **状态**：Accepted
- **决定**：系统不硬编码“本地优先”“质量优先”或“最低成本”作为普遍路由顺序。全局配置定义可用 policy profiles 和默认 profile；App policy 指定默认值及允许请求覆盖的字段/范围；request 在该范围内选择 profile、增加 hard constraints 或调整 preferences。
- **合并顺序**：系统安全不变量 → global/provider hard limits → App policy → Intent defaults → request overrides。后层只能在授权范围内收窄前层 hard constraints，不能放宽 placement、预算或 provider allow/deny 边界。
- **内置模板**：提供 `balanced`、`local-first`、`quality-first`、`latency-first`、`cost-first` 作为可编辑模板，不把任何模板写死为不可修改的系统真理。全局和每个 App 的默认 profile 都可配置。
- **路由算法**：profile 使用类型化、可解释的过滤条件和有序比较规则；MVP 不使用不可解释的单一加权黑盒分数。
- **App admission**：每个 App 配置 `max_pending_jobs`；slot 从 Job admission 持有至终态，provider queue 未被单一 App 消耗。更细的 App concurrency/RPM/预算由 M3 统一实现。
- **ADR**：[ADR-0005](adr/0005-configurable-routing-policy.md)

### D-006：首个真实 consumer 验收不预注册产品身份

- **状态**：Accepted
- **决定**：早期讨论中的候选应用从未实际接入，不作为 runtime 内置身份或发布事实。首个真实 consumer 在自身领域内把动作映射到公共 Intent；runtime 不注册 `app.summary` 等应用级别名。接入前使用通用 fixture，接入后以实际 App 配置独立 credential、policy 和 quota。
- **参考环境**：第一开发与验收环境为 macOS Apple Silicon + Ollama；CI 仍必须在没有 Ollama、GPU 和云凭证时通过 fake provider。
- **v0.1 SLO**：在 provider 有空闲容量时，runtime 自身从收到合法请求到 dispatch 的 p95 ≤ 25 ms；首个 provider stream event 的额外转发延迟 p95 ≤ 20 ms；取消在 100 ms 内完成控制平面终态仲裁，并在 provider 支持取消时 2 s 内释放执行 reservation。隐私越界、预算超卖、重复终态和 reservation 泄漏容忍度均为零。
- **ADR**：[ADR-0006](adr/0006-first-consumer-and-slo.md)

### D-007：35B 本机模型归入通用推理档位，不作短总结默认模型

- **状态**：Superseded by D-008
- **决定**：该决策正确识别了“模型规模不等于能力”，但仍把 placement 写进 route 名、把模型当作单一全局档位；这些结构已由 D-008 替代。
- **理由**：约 21 GB 的 35B 模型用于高频短总结成本不成比例；同时本机没有已验证的高档推理候选，不能以 route 名称或 35B 参数规模制造该能力已存在的假象。
- **影响**：历史背景保留在 ADR-0007，不再作为当前配置合同。

### D-008：Intent、能力评级、推理投入与 Placement 正交建模

- **状态**：Accepted
- **决定**：公共 `model` 是 Intent Profile。模型结构分为 `Model Profile → Model Build → Deployment → Provider/Node`；Model Profile 对每个 Intent 分别记录 `basic/general/advanced/frontier` 评级及 `provisional/benchmarked` 证据状态。
- **正交约束**：`quality_floor` 选择满足能力门槛的模型；标准 `reasoning.effort` 控制选中模型的计算投入；placement 通过 `local/trusted_node/cloud` 允许集合与偏好表达。任何一个维度都不能冒充另一个。
- **最低充分与最强优先分离**：`quality_fit` 按与有效 quality floor 的距离选择最低充分 grade，避免普通请求无意升级；`quality` 保留最强合格模型优先语义，供 `quality-first` 使用。
- **fallback**：只在 Intent、required features、质量下限、placement、deadline 和预算包络内替换。降低质量或扩大数据边界必须显式授权。
- **降级下限**：即使请求显式使用 `fallback=allow_lower_quality`，也不得低于 Intent 的 `default_quality_floor`；它只可撤销该请求额外提高的质量门槛。
- **Attempt 边界**：首版最多 3 个 Attempt、同 Candidate 最多重试 1 次。所有 retry/fallback 共用 Job 原 deadline；`invalid_request` 不重试或回退。流式开始后 Candidate 固定，断流不得用另一模型续接。
- **ADR**：[ADR-0008](adr/0008-intent-capability-deployment-model.md)

### D-009：音频按任务协议族归并，本地模型走常驻 executor

- **状态**：Accepted
- **决定**：音频不进入 Responses，也不按物理模型拆接口。公共任务为 `audio.transcribe`、`audio.align`、`speech.synthesize`、`speech.voice_design`、`speech.voice_clone`；分别映射到转写、对齐、speech 和 voice-clone 数据面，共享控制平面。
- **执行**：本机 Qwen3 音频 builds 由常驻 MLX worker 懒加载，默认只保留一个模型；文件 payload 上限 25 MiB，只存在于单次请求临时目录，不写入 Job metadata。模型使用精确本地 snapshot 和 offline 模式。
- **边界**：当前只承诺文件调用；实时 session/streaming 另行设计。SenseVoice 缺少可运行 FunASR 环境，因此只有 Profile/Build，没有 Deployment。
- **ADR**：[ADR-0009](adr/0009-task-oriented-audio-protocols.md)

### D-010：本机 operator credential 由 runtime 管理，外部 App 身份不预置

- **状态**：Accepted
- **决定**：loopback API 继续要求 bearer authentication，但默认 `local-operator` 的 256-bit token 由 runtime 首次启动时生成，CLI/console 从同一 managed credential store 自动读取；本机启动不要求用户传入 API key。
- **权限边界**：只有 `local-operator` 默认拥有 `resource_admin`。外部 consumer 必须以独立 App 注册，使用独立 environment credential，默认没有资源管理权限；runtime 不预置任何产品名称。
- **存储边界**：managed credential directory 和 token file 在 Unix 上分别强制 `0700`/`0600`；token 不进入 TOML、SQLite config snapshot、日志、错误或 console UI。`--api-key`/`INFER_API_KEY` 只作为外部实例或显式覆盖入口。
- **理由**：loopback 并不是信任边界，其他本地进程仍不能无授权地消耗云额度、读取审计或装卸模型；同时 secret bootstrap 不应成为日常启动负担。
- **ADR**：[ADR-0012](adr/0012-runtime-managed-local-operator-credential.md)

### D-011：App 使用显式 Intent allowlist，所有数据面在统一 admission 前授权

- **状态**：Accepted
- **决定**：`AppConfig.allowed_intents` 是普通 Consumer 的稳定 workload ACL。非空清单精确授权，显式空清单禁止推理；字段省略仅为旧配置兼容并表示允许全部。Apps & Access 创建或编辑 Consumer 时写入显式清单。
- **执行点**：授权只在公共 Job preparation owner 中执行，并先于 Candidate Plan、App/provider admission、持久化和 provider 调用；Responses、音频、视觉与 durable background 不建立各自 ACL 分支。
- **失败合同**：未知 Intent 仍是 `400 invalid_request_error`；存在但未授权的 Intent 是 `403 intent_forbidden`，不产生 Job、Attempt、reservation 或 provider side effect。配置中的未知或重复 allowlist 项 fail closed。
- **边界**：Intent ACL、`resource_admin` 与 request override 上限正交。不引入 role/scope DSL，不允许应用直接授权 Deployment、ONNX tensor 或 provider 原生操作。

### D-012：订阅式 bridge 是 cloud Provider，动态模型组必须显式准入

- **状态**：Accepted for experimental Codex and Antigravity execution
- **决定**：一个已登录的订阅账号实例建模为一个 Provider 和一个共享 quota/concurrency pool；
  Codex `model/list` 或 Antigravity `agy models` 返回的每个可路由模型分别映射为 Model Profile →
  Build → Deployment。reasoning effort 是 Deployment 参数，不拆成独立 Deployment。
- **发现/准入**：动态发现只进入 operator inventory。当前只准入 Sol/Terra/Luna；新增、隐藏、
  upgrade 或消失不会自动改路由。配置模型缺失时 Attempt fail closed，不暗中选择默认模型。
- **placement/授权**：即使 JSON-RPC transport 在本机，推理仍发生在云端，必须标记
  `placement=cloud` 和 `access_class=subscription`。App 默认只有 `standard`；只有显式加入
  subscription class 才能消费，缺失授权记录 `provider_access_not_allowed`。
- **单次请求收窄**：双授权 App 可使用 `infer.provider_access_class` 只选 `standard` 或
  `subscription`。该字段不能扩大 ACL；非目标 class 记录 `provider_access_class_mismatch`。
- **能力边界**：初始版本只开放 text/non-streaming；D-013 随后以独立 cloud modality ACL 扩展为
  text/image 输入与文本 unary/SSE 输出。仍只支持 instructions/reasoning-effort Responses 子集。
  App Server 的 thread、tools、shell、workspace、web、MCP、delegation、memory 和 agent loop 不
  构成 infer-runtime 能力；出现非推理 item 即以 protocol 失败。
- **生命周期**：首版每个 Attempt 使用空 ephemeral、read-only、no-approval 子进程，优先保证
  cancel/drop 时可终止和不复用污染状态；是否升级为持久进程取决于 interrupt/crash/concurrency
  合同证据。订阅窗口与账号级 rate-limit 仅作为后续 operator telemetry，不伪装成逐请求 USD。
- **第二协议复审**：Antigravity 复用 Provider SPI、模型组 DTO 和 subscription ACL，但保留独立
  CLI owner。它按受信任本机会话复用真实 HOME/Keychain，Runtime 不读取、复制或轮换认证材料；
  prompt 经一次性 workspace 文档传递而不进入 argv。首版只准入 Gemini 3.6 Flash
  effort-suffixed slugs，并把 Low、None/Medium、High 精确映射到对应物理 variant。CLI 自身可能
  维护账号级状态或 history，因此该 Provider 仍是 experimental、非敏感、显式 subscription ACL
  能力，不宣称 credential/history 隔离。
- **ADR**：[ADR-0013](adr/0013-subscription-backed-inference-bridges.md)

### D-013：执行模式与数据模态正交；流式协议按数据平面分型

- **状态**：Accepted for experimental Codex/audio streaming slices
- **决定**：控制面使用 `unary/server_stream/duplex`，不以一个全局 `stream: bool` 抹平全部
  传输。Responses `stream=true` 只映射为 server-stream；TTS 以 chunked PCM 输出；ASR 以
  WebSocket 同时接收 PCM 和返回 revisioned partial/final transcript。
- **稳定性**：文本 delta 是 append-only；transcript partial 是 revisioned replacement。当前
  MLX ASR 明确披露 `commit_redecode`，不冒充模型原生低延迟流。alignment/embedding 保持 unary。
- **多模态云边界**：subscription/provider access 只授权经济与账号边界；App 还必须通过独立
  `allowed_cloud_input_modalities` 才能外发图片。默认只有 text，拒绝记录
  `cloud_input_modality_not_allowed`。
- **Codex**：bridge 可翻译有界 text/image input 与 agentMessage delta，但继续封闭 thread、工具、
  Shell、MCP、workspace path、memory 和 agent loop。完成结果以权威 completed event 为准。
- **ADR**：[ADR-0014](adr/0014-execution-modes-and-typed-streaming.md)

### D-014：Consumer API 通过 Infra Discovery 定位，鉴权仍由 App 身份承担

- **状态**：Accepted
- **决定**：`infer-runtime` 的同一 registration 除只读状态外，发布独立的
  `infer-runtime.consumer` offer；protocol version 精确等于冻结 Consumer contract，当前为
  `0.1.0-candidate.2`。Infer 自有 `infer-runtime.http-loopback` binding 只接受 canonical numeric
  loopback URL。
- **安全边界**：Discovery 不包含 App id、token、credential id、ACL 或 Provider secret。Consumer
  发现地址后仍必须使用自己的 bearer 身份，并校验 owner、lease、generation、协议、binding 和
  endpoint；HTTP client 禁止代理和 redirect。
- **迁移**：显式 endpoint override 优先，固定 `127.0.0.1:8787` 只作迁移 fallback。Echo、Shadow、
  Symbiont-d 保留各自现有 token/ACL，不共享 `local-operator`。
- **ADR**：[ADR-0015](adr/0015-consumer-infra-discovery.md)

## Non-blocking / 后续阶段再定

### D-101：远程 Node 的信任与配对模型

- **状态**：Proposed，M5 前关闭
- **推荐方向**：用户显式批准的设备身份 + 双向安全通道 + 可撤销证书/密钥；Node trust class 参与 placement/data policy hard constraint。

### D-102：大 payload 的传输与所有权

- **状态**：Proposed，M6 前关闭
- **当前进展**：本地音频已采用 25 MiB 有界 multipart + executor 临时目录，payload 不进入 Job metadata。跨节点/云端仍推荐根据 locality 选择直传或预签名对象，并在 M5/M6 前关闭剩余决策。

### D-103：第三方 Provider 扩展方式

- **状态**：Proposed，出现外部 provider 作者前关闭
- **推荐方向**：先保持 workspace 内静态 adapter；Responses、audio worker、ONNX 与 Codex
  App Server 已证明执行协议需要分型，但尚未证明第三方插件 ABI。待至少第二种独立订阅/CLI
  bridge 和真实外部 provider 作者出现后，再选择进程外插件协议；首版不承诺 Rust ABI。

### D-104：模型 benchmark 如何参与路由

- **状态**：Proposed，在 benchmark 参与能力/质量路由前关闭
- **当前边界**：M4 的 reload benchmark 只作为资源 eviction safety 证据，不评价模型能力，也不进入 quality routing。
- **推荐方向**：未来按 node/deployment/workload profile 保存带时间戳的数据，作为可解释排序输入；不把合成 benchmark 当作质量真值。

### D-106：ONNX 本地执行族与类型化视觉数据面的边界

- **状态**：Accepted（foundation + experimental face/SigLIP/QwenVL typed slices）；不属于冻结的 v0.1 Consumer contract
- **已覆盖基线**：D-008 已固定 Intent/Model/Build/Deployment/Provider/Node 身份链，D-009 已固定“控制统一、数据分型”，M2-M4 已拥有 Job/Attempt、取消、reservation、audit 和 lifecycle owner。
- **推荐方向**：ONNX Runtime 作为新的本地 Provider/执行族接入；公共 owner 管理 Session Registry、artifact verification、原生 load/unload、Execution Provider route 和错误归类，模型专属 adapter 独占 preprocessing/tensor/postprocessing。普通 App 只使用类型化视觉协议，不接触万能 tensor-map、物理 backend 或模型路径。
- **跨平台边界**：Apple Vision 不作为共同模型语义来源；受 Build 合同约束的 Core ML、WinML 或其他 ONNX Runtime Execution Provider 只是可验证的候选执行后端，实际 route/precision/fallback 必须进入 Attempt provenance。
- **决定**：首个切片选择 `vision.detect_faces` / `vision.face_detection`。第二个独立切片接受原图、source revision 与 YuNet 命名五点，通过 SFace `vision.embed_face` / `vision.face_embedding` 返回归一化 128 维向量、eligibility evidence、精确 space 与完整 provenance。第三个切片使用独立 `vision.embed_image` / `vision.embed_text` 数据平面，把 SigLIP image/text encoder 绑定到同一个版本化 768d space。第四个切片以 `vision.describe_image` 与 `vision.review_classification` 两条独立 typed schema 封装 QwenVL，不泄漏 Ollama chat wire。
- **Build 门槛**：exact artifact/export digest、opset、tensor contract、完整预处理、embedding space、tokenizer/vocabulary、允许的 Execution Provider/precision/fallback，以及 code/weight/data license facts 必须共同进入版本化 Build identity。
- **ADR**：[ADR-0011](adr/0011-heterogeneous-local-runtimes-and-typed-vision.md)

### D-107：同一 Node 上跨 Provider Resource Pool 的容量合同

- **状态**：Accepted for current synchronous ONNX slices；QwenVL + SigLIP/MLX 混合负载、视觉 durable 或 M5 capacity schema 再触发复审
- **决定**：YuNet/SFace 不足以证明需要通用多资源池。SigLIP image+text 两 Session 的 release 实测驻留约 1.9 GiB，已登记为 heavy；当前同步 slice 继续复用全机 pressure、deployment lifecycle reservation 与 per-provider queue/capacity。真实数值证明需要保守 admission/residency，但仍没有证明必须在核心中预建 system/GPU/CPU 多资源原子池。
- **备选**：保持 provider 局部容量并增加统一 admission estimate；或显式建模 system memory、unified/GPU memory、CPU slots、accelerator queue、resident-model budget 等 Node Resource Pool。
- **复审门槛**：用 Shadow 背景 SigLIP 索引与交互 QwenVL/音频混合负载建立资源估算误差、原子多资源 reservation、回收和 fail-closed 模拟；同时确认与 M5 远程 Node 声明共享什么 schema。没有证据时不建设通用集群调度器。
- **ADR**：[ADR-0011](adr/0011-heterogeneous-local-runtimes-and-typed-vision.md)

### D-108：视觉 payload、SensitiveBiometric 与 durable ownership

- **状态**：Accepted for synchronous face and semantic embeddings；reference/durable background 仍 Proposed
- **安全下限**：视觉默认 `local_only`，不得隐式 cloud fallback；face embedding 标为 `SensitiveBiometric`；像素、crop 和 embedding 不进入通用 Job metadata、默认日志或普通 audit details。
- **决定**：同步视觉只接受 20 MiB 内 JPEG/PNG；服务器强制 local-only/offline/no-fallback，像素、查询文本和 embedding 不进入通用 metadata/log/audit/durable store；`source_revision`/`query_revision` 必填并回显，Consumer 负责 stale-result 发布仲裁。SFace 另要求 detector 坐标系中的命名五点，由 Runtime 对齐。SigLIP image encoder 只接受 Consumer 已做 orientation normalization 的 display-sized raster；不接受 RAW、任意路径或 URL。
- **durability 边界**：ADR-0010 只覆盖 local Responses 文本。视觉 background 在定义独立 payload 类型/owner、加密或引用租约、重启幂等、结果仲裁和删除语义前不得复用该能力。
- **ADR**：[ADR-0011](adr/0011-heterogeneous-local-runtimes-and-typed-vision.md)

### D-109：SigLIP 跨模态 space、导出与当前 EP 身份

- **状态**：Accepted for experimental synchronous slice
- **Build**：固定 `google/siglip2-base-patch16-224@75de2d55ec2d0b4efc50b3e9ad70dba96a7b2fa2`、Apache-2.0、FixRes 224、lowercase + 64-token tokenizer、opset 17 与 export toolchain。image/text graph 和 tokenizer 都是 Build identity 的内容寻址组成部分。
- **space**：两个 typed endpoint 必须返回完全相同的 `siglip2_base_patch16_224@75de2d55:fixres224:lowercase64:l2_768_fp32:v1`；只允许比较相同 space，identity 漂移形成新索引域。
- **EP**：当前 immutable Builds 为 CPU-only。实测 Core ML 无法完整接管，失败后回退代价约为 image 17.5 秒、text 223.5 秒；不能把 fallback 后成功标成 Core ML 能力。未来导出、ORT、Core ML/Windows EP 必须使用新 Build 通过 numerical tolerance 和检索 decision-stability。
- **资源**：release 实测 image warm 约 100 ms、text warm 约 33 ms，两 Session 合计约 1.9 GiB RSS；因此标为 heavy 并重开 D-107，但本 slice 不据此建设通用 Resource Pool。
- **ADR**：[ADR-0011](adr/0011-heterogeneous-local-runtimes-and-typed-vision.md)

### D-110：QwenVL typed understanding、闭集复核与质量路由

- **状态**：Accepted for experimental synchronous slice
- **合同**：`vision.describe_image` 返回一条有界短描述和去重关键词 proposal；`vision.review_classification` 只接受 Consumer 提供的最多 64 个类别，并只返回 `matched`、`none` 或 `uncertain`。`matched` 的 category id 必须来自请求闭集；不返回伪校准 confidence，也不把 proposal 解释为用户反馈。
- **路由**：Consumer 只请求 Intent 与 quality floor。当前 4B Build 在两条 Intent 上评级 `basic`、resource class 为 `standard`；8B Build 评级 `general`、resource class 为 `heavy`。描述默认 basic，闭集复核默认 general；显式 `general` 描述可选择 8B。物理 Ollama tag 不进入公共请求。
- **数据与 durable 边界**：只接受 Consumer 已完成 orientation normalization 的 display-sized JPEG/PNG、必填 `source_revision`，并强制 `local_only`、`offline_required=true`、`fallback=none`。图片、类别、描述与关键词不进入 Job metadata、默认日志、audit details 或文本 durable spool；background 只是一种队列优先级，不是可恢复照片队列。
- **Provider 边界**：Ollama adapter 可使用其 native image/chat transport，但公共 endpoint 不暴露该 schema。当前 provider 的结构化输出兼容性实测要求使用 revisioned prompt + 严格反序列化，而不依赖 native `format`；schema/prompt revision 与实际 Build/physical model 进入 provenance。
- **复审门槛**：Shadow 真实照片域准确性、长时间 bulk/background 与 interactive/MLX/SigLIP 混合压力、取消/断线 soak、8B residency 与 stable contract promotion。证据不足时不建设通用 Resource Pool，也不开放视觉 durable。

### D-111：生成式 raster image edit 必须等待真实执行面

- **状态**：Proposed / Blocked；当前没有可输出 raster 的已验证 Provider、Build 或 Deployment
- **边界**：`image.edit` 是独立类型化数据平面，不进入只返回文本的 Responses/VLM 合同。只有真实执行面通过端到端验证后，才可 additive 发布 `0.1.0-candidate.3`、`infer.image.edit@20260811.1` 与 `POST /v1/images/edits`；空路由、mock、文本输出或仅登记模型均不构成能力。
- **拟定输入**：strict multipart；`model=image.edit`；必需 JPEG/PNG `source_image`（不超过 20 MiB/40 MP、orientation-normalized display pixels）与 `source_revision`；必需不超过 16 KiB UTF-8 `instruction`；可选同尺寸 PNG mask（白/1 可编辑、黑/0 保留）及 `mask_revision`；最多四个带 `role=style|identity` 和 revision 的图片引用。identity 授权事实由 Consumer 持有。
- **拟定输出**：unary 单张 raw raster；`Content-Type`、Job id、输出 SHA-256、宽高、orientation、colorspace 通过稳定 header 返回，完整 Build/Attempt/placement/policy/fallback/cost provenance 由 Job snapshot 提供。
- **隐私/所有权**：Runtime 不持久化 source、instruction、mask、references 或 output pixels，也不接管 Shape Scene、Candidate、Compare/Accept 或 immutable Revision。`apps.shape` 只有在真实能力就绪后才增加 Intent；local-first/local-only/offline/no-fallback/max-cost/provider class 边界保持不变，cloud image modality 必须另行授权。
- **开放门槛**：真实 Provider/Build/Deployment、取消与迟到结果测试、digest/header 校验、ACL 正负面测试和真实 raster HTTP E2E 全部通过。门槛关闭前 Shape 保持 semantic proposal only。
- **ADR**：[ADR-0011](adr/0011-heterogeneous-local-runtimes-and-typed-vision.md)

## 已关闭的阶段决策

### D-105：durable background payload ownership

- **状态**：Accepted
- **决定**：首版使用独立于 metadata SQLite 的 runtime-managed encrypted spool。配置只保存密钥环境变量名；spool 使用随机 nonce 的 AES-256-GCM，App/类型/blob ID/HMAC digest/明文长度进入认证上下文，SQLite 只保存不透明引用、HMAC identity、长度和 retention metadata。
- **恢复语义**：只有显式 `background: true` 且 `local_only` 的非流式文本 Job 可恢复；重启保留 Response ID 和 Candidate Plan，running Attempt 记为 `interrupted`，新 Attempt 标记 `recovery`，并受配置化重放上限约束。普通交互请求仍按 D-002 失败且不重放。
- **生命周期**：输入在结果与成功终态原子发布后删除；失败/取消只在终态已持久化后删除；结果按 retention 到期。启动会清理只属于 spool owner 的 orphan，不接受任意文件路径或 URL。启用时密钥不可用会 fail closed；存在 pending durable Job 时不能通过关闭功能绕过恢复。
- **边界**：首版不恢复 cloud 调用，避免缺少上游幂等键时重复计费或副作用。external immutable reference、音频 background 和 batch 留给后续独立合同。
- **ADR**：[ADR-0010](adr/0010-durable-background-payload-spool.md)

## 决策流程

1. 用真实 consumer 或失败场景描述问题；
2. 列出至少一个备选和不做决定的成本；
3. 确认决定、适用范围和复审触发条件；
4. 为跨模块/长期决定创建 ADR；
5. 更新 DESIGN、ROADMAP、schema 和测试门槛，避免文档互相矛盾。
