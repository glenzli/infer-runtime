# infer-runtime 系统设计

| 属性 | 值 |
| --- | --- |
| 状态 | Baseline Accepted / M4 已收口 / v0.1 发布门槛收口中 |
| 目标读者 | 核心开发者、Client SDK 与 Provider 实现者、上层应用开发者 |
| 设计范围 | 单用户、local-first 的推理控制平面；从本机走向可信远程节点 |
| 非目标 | Agent、领域工作流、统一所有模态的数据协议、模型训练与分发平台 |

## 1. 摘要

`infer-runtime` 接收应用提交的 `InferenceJob`。Job 描述“需要什么能力”和“必须满足什么约束”，而不是指定某个物理模型。runtime 根据资源状态、策略、预算、优先级和候选执行器健康状况完成准入、路由、排队、资源预留、执行、流式回传、计量和故障切换。

系统采用统一控制平面、分模态数据平面的设计：调度、配额、生命周期、指标和策略共享一套模型；文本生成、语音识别、语音合成、视觉等能力保留适合自身的数据协议。这样既能统一资源治理，又不会用虚假的通用接口抹平真实能力差异。

文本推理对应用和 provider 采用 OpenAI Responses API 兼容子集。音频已经作为第二个真实协议族落地：文件转写、强制对齐、语音合成、声音设计和声音克隆使用各自适合的 schema，共享同一控制平面和能力路由。目前还没有外部文本 consumer 完成接入；本机 Qwen3 ASR/ForcedAligner/TTS 是首批音频 deployments。

## 2. 目标与边界

### 2.1 目标

1. 为多个本地应用提供一个稳定的推理资源入口。
2. 用 capability 与 constraints 解耦应用意图和物理模型。
3. 在本地、云端和未来的远程 AI Host 之间进行可解释的选择。
4. 对交互任务和后台任务实施统一排队、抢占边界与背压。
5. 统一管理云端成本、provider 限流和应用预算。
6. 管理本地模型装载、空闲释放和资源预留，避免不同应用彼此争抢硬件。
7. 为每次选择、回退、失败与花费提供可观测、可审计的依据。

### 2.2 非目标

`infer-runtime` 不负责：

- Agent planning、tool loop、memory、workspace、sandbox 或 approval；
- Shadow、Video、Moment 等外部应用的领域逻辑；
- prompt 编排、RAG、会话记忆或业务工作流；
- 模型训练、微调、权重仓库和通用模型分发；
- 保证所有 provider 行为或输出完全一致；
- 将所有模态压成一个请求/响应 schema；
- 替代强依赖特定厂商模型的原生 API。若应用契约本身就是“必须使用某厂商/模型”，它应直接调用原生 API。

### 2.3 设计原则

- **意图优先**：普通应用请求 Intent Profile、能力下限和 constraints，不请求物理模型。
- **控制统一，数据分型**：资源治理统一；不同模态保留真实协议。
- **local-first 不等于推理本地优先**：控制平面、配置和数据所有权以本地为中心；具体请求走本地还是云端，由配置 profile、App policy、请求约束和实时资源共同决定。
- **策略与机制分离**：机制负责安全地执行决定；策略负责产生可解释决定。
- **所有消耗先预留后结算**：并发条件下也不能绕过预算和容量限制。
- **降级必须显式允许**：fallback 不能偷偷突破 placement/data policy、budget、deadline 等硬约束。
- **可取消、可限流、可恢复**：每个长操作都必须有终止和背压语义。
- **先打通垂直切片，再扩展能力**：不为未来模态预建大量空抽象。

### 2.4 异构本地执行与视觉 P0 的架构映射

Shadow 的 ONNX/视觉需求不改变当前核心方向，也不进入冻结的 v0.1 Consumer contract。
D-106 至 D-110 已关闭一组收窄的实验切片：共享 ONNX artifact store、Session Registry、通用
native lifecycle controller，以及同步、local-only 的 face、SigLIP 跨模态向量和 QwenVL
typed understanding 数据平面。
它与现有设计的关系为：

| 提案内容 | 当前覆盖 | 后续新增/决策门 |
| --- | --- | --- |
| 跨 ONNX/Ollama/MLX 统一 Job、priority、取消、审计 | Job/Attempt、Scheduler、Quota、Resource Manager 已覆盖 | provider contract 与混合负载验收 |
| 控制统一、视觉/音频/文本数据分型 | 设计原则与 ADR-0009 已覆盖 | face、SigLIP 与 QwenVL typed wire 已实现；不提供 tensor-map 或通用 VLM chat API |
| Model Profile → Build → Deployment → Provider → Node | ADR-0008 已覆盖 | ONNX Build manifest 已绑定 artifact/export、auxiliary、tensor、preprocess/tokenizer、space、EP 和 license identity |
| provider 原生 lifecycle | Ollama controller、MLX worker、M4 lifecycle owner 已覆盖 | ONNX Session Registry 已接入同一 native controller；模型语义留在 typed adapter |
| 同一 Node 跨 provider 资源竞争 | 全机压力、deployment reservation、per-provider capacity 覆盖当前同步 slices；SigLIP 实测合计约 1.9 GiB | QwenVL + SigLIP/MLX 混合负载、视觉 durable 或跨节点容量出现时复审显式 Resource Pool |
| 图片、人脸 crop、embedding 隐私 | 默认 payload-free metadata、local-only 和 ADR-0010 文本 spool 提供先例 | D-108 独立决定视觉引用、SensitiveBiometric、retention、durability 和 stale-result 仲裁 |
| Shadow consumer 边界 | App 身份、Intent、priority 和 provenance 已覆盖 | Shadow 保留 Catalog/Recipe/user facts；协议稳定后才做真实接入 |

具体边界、未关闭子门与验收门槛见 Accepted 的
[ADR-0011](docs/adr/0011-heterogeneous-local-runtimes-and-typed-vision.md)。该接受只授权列明的
实验切片；通用视觉 background、QwenVL structured understanding、通用 Resource Pool 与
stable contract promotion 仍需独立证据，不能从 ONNX provider 已存在推导为已支持。

### 2.5 订阅式推理桥接

Codex App Server、未来的 Claude/Antigravity CLI bridge 等属于新的 Provider 执行族：调用入口
可以是本机进程或 socket，但真正推理发生在云端并消耗用户订阅，因此 `placement=cloud`，且
Provider 使用独立的 `subscription` access class。App 的默认授权只有 `standard`；只有显式
加入 subscription class 的 App 才能把这类 Deployment 纳入 Candidate Plan。

一个已登录的 Codex App Server 实例是一个 Provider 与一个共享 quota/concurrency pool，
`model/list` 返回的是该 Provider 下的动态模型组。每个被准入的上游模型仍分别形成 Model
Profile → Build → Deployment；reasoning effort 是 Deployment 参数，不拆成 Deployment。
“发现”不等于“准入”：新增上游模型只出现在 operator inventory，必须经过显式 Build、能力
评级和 Deployment 配置才进入路由；已准入模型从 inventory 消失时执行 fail closed，不暗换模型。

Codex slice 只翻译 text/image 输入和文本 unary/SSE 输出的无工具 Responses 子集。adapter 使用空的 ephemeral
workspace、read-only sandbox、`approvalPolicy=never` 和禁用的 shell/web/plugin/app/multi-agent
功能；若事件流出现 command、file change、MCP、web search、delegation 等非推理 item，Attempt
以 `protocol` 失败。它不暴露 Codex thread、tool loop、memory、workspace mutation 或完整 agent
能力。图片必须通过独立 cloud modality ACL，且只接受有界 JPEG/PNG data URL 或 HTTPS URL；
持久会话、精确订阅 quota reconciliation 和第二种 CLI bridge 仍须独立扩展合同，不能从 App
Server 已接入推导为支持。Antigravity 采用与 Codex subscription 相同的“受信任本机会话”模型：
`agy` 直接复用当前用户真实 HOME/Keychain 中由 CLI 自己管理的登录态，Runtime 不读取、复制或轮换
认证材料。Consumer payload 与显式日志仅写入一次性 owner-only workspace，prompt 不进入 argv；
首版只准入 Gemini 3.6 Flash 的 low/medium/high 物理 slug，并确定映射到 Runtime
`reasoning.effort`。该边界不声称隔离 CLI 自身的账号级状态或内部 history，因此只作为显式授权的
experimental cloud/subscription unary text Provider，现有 Consumer 不会自动获得访问权。

## 3. 术语与核心对象

| 术语 | 含义 |
| --- | --- |
| App | 通过 Client SDK 或协议调用 runtime 的应用身份 |
| Intent Profile | Responses `model` 字段引用的稳定任务意图，如 `text.summarize`、`assistant.general`；不含应用名、位置或厂商 |
| Quality Grade | 某 Model Profile 在某个 Intent 上经评估得到的 `basic/general/advanced/frontier` 档位，不是全局模型等级 |
| Model Profile | 语义模型身份及其按 Intent 记录的能力评级，例如 Qwen 3.6 35B |
| Model Build | 一个可运行制品/量化变体，例如 `qwen3.6:35b-mlx` |
| Deployment | 某 Model Build 在一个 Provider 上的可执行实例；placement 从 Provider 获得 |
| Job | 一次可追踪、可取消、可计量的推理工作 |
| Candidate | 能满足某 Job 的模型、provider、node 和执行参数组合 |
| Provider | 将规范化调用翻译为具体后端协议的适配器 |
| Provider access class | Provider 的消费授权边界；`standard` 默认可用，`subscription` 必须由 App 显式授权 |
| Node | 可承载 provider/executor 的计算节点；本机也是一个 Node |
| Model/Deployment | 某节点或云 provider 上可执行的具体模型部署 |
| Policy | 将约束、偏好和运行状态转为候选过滤与排序结果的规则 |
| Reservation | 对预算、并发槽、内存或模型生命周期资源的临时占用 |
| Attempt | Job 的一次具体执行；fallback 会产生新的 Attempt |

必须区分 `Job` 和 `Attempt`。Job 表达用户意图并拥有最终状态；Attempt 记录一次具体落点及其独立的用量、输出和错误。重试或 fallback 不得覆盖历史 Attempt。

## 4. 系统上下文与总体架构

```text
 Applications (Shadow / Video / Moment / others)
                              |
                         Client SDK
                              |
                    +---------v---------+
                    |    inferd API      |
                    +---------+---------+
                              |
        +---------------------+----------------------+
        |                     |                      |
  Admission & Policy     Router & Scheduler    Job Coordinator
        |                     |                      |
        +---------------------+----------------------+
                              |
       +-----------+----------+----------+-----------+
       |           |                     |           |
    Registry   Quota Manager       Resource Manager  Store
       |           |                     |           |
       +-----------+----------+----------+-----------+
                              |
                      Provider Runtime
             +----------------+----------------+
             |                                 |
        Local Providers                  Cloud Providers
  (Ollama / MLX / ONNX)       (Responses APIs / subscription bridges)

 Future: trusted Node Agent -> remote providers / accelerators
```

### 4.1 控制平面

控制平面拥有：

- App 身份与准入；
- capability 与 deployment 注册；
- policy 计算；
- 候选过滤与排序；
- 优先级队列与并发控制；
- quota、rate limit 和资源预留；
- Job/Attempt 生命周期；
- 健康状态、指标和审计事件；
- 本地模型及未来远程节点生命周期。

### 4.2 数据平面

数据平面负责实际 payload 与流式输出，不强行共享一个 schema：

| 协议族 | 典型输入 | 典型输出 | 首次交付 |
| --- | --- | --- | --- |
| Text | Responses `input`/`instructions` | Responses object / SSE events | MVP |
| Embedding | text/batch | vectors | 后续 |
| File transcription | multipart audio + language/prompt | text / transcript segments | 已实现 |
| Duplex transcription | WebSocket configure + PCM chunks/commit | revisioned partial/final transcript | 实验，已实现 commit-redecode slice |
| Forced alignment | multipart audio + transcript | token timestamp spans | 已实现 |
| Speech | text、voice/instructions + execution mode | bounded audio file / append-only PCM chunks | 已实现 unary；streaming 实验 |
| Voice clone | reference audio/text + target text | bounded audio file | 已实现 |
| Face detection | bounded JPEG/PNG + source revision | boxes、landmarks、confidence、provenance | 实验，已实现同步 local-only slice |
| Face embedding | bounded JPEG/PNG + named five-point landmarks | normalized 128d vector、eligibility evidence、space/provenance | 实验，已实现同步 local-only slice |
| Image embedding | bounded image/reference | vector、embedding space、source/preprocess provenance | 后续，Proposed |
| Image classification/tag | bounded image/reference + controlled vocabulary | versioned vocabulary scores | 后续，Proposed |
| Vision caption/understanding | image/reference、prompt | typed understanding result | 后续 |

每个协议族可以有独立版本与能力协商，但必须接入共同的 Job envelope、取消、计量、错误和观测契约。
`vision.detect_faces` 与 `vision.embed_face` 保持 experimental，不属于冻结 v0.1 API；其余视觉
名称仍是 ADR-0011 的 taxonomy 候选。不能用 provider 已存在推断未列出的能力也已支持。

## 5. 功能需求

### 5.1 MVP 必须支持

- 通过 OpenAI Responses API 兼容子集提交和流式消费文本 Job；
- 通过独立控制 API 查询、解释和取消底层 Job；
- `interactive`、`normal`、`background` 三档优先级；
- 基于 capability 和约束生成候选；
- 一个通用 Responses-compatible adapter，以及 Ollama 的 capability profile；
- 有界队列、每 provider 并发限制和调用超时；
- fallback 与 retry 的明确边界；
- global/provider/app 三层预算与 RPM/TPM/concurrency 限制；
- Job、Attempt、usage、decision reason 和基础运行指标；
- CLI 查看状态、模型/部署、队列、预算和 Job；
- TOML 配置原子加载/重载；M3 用 SQLite 恢复 Job/Attempt metadata 与账本，但不恢复未完成执行。

### 5.2 后续能力

- 本机 RAM/VRAM 压力感知和模型装卸；
- 后台 Job 的持久化、恢复和批处理；
- 远程 Node 注册、心跳、租约和调度；
- 原生低延迟 ASR、压缩音频 streaming codec、更多 vision 协议族；
- benchmark 结果参与路由，但不能成为不可解释的黑盒分数；
- 更细粒度的公平调度、截止期调度与受控抢占。

## 6. 领域模型

以下是内部语义模型。文本 wire contract 采用 [ADR-0001](docs/adr/0001-responses-api-data-plane.md) 确定的 Responses 兼容子集；内部 Job 字段不要求逐一暴露为自定义顶层 JSON 字段。

### 6.1 InferenceJob

```text
InferenceJob
  id                 globally unique
  app_id             authenticated caller
  intent             stable workload profile
  priority           interactive | normal | background
  constraints        hard requirements and soft preferences
  payload_ref        protocol-family request or bounded inline payload
  idempotency_key?   app-scoped deduplication key
  created_at
  deadline_at?
  state
  attempts[]
```

不变量：

- `app_id` 来自认证上下文，不能只信任 payload 自报；
- intent 在 admission 时解析到具体版本，并派生模态、feature 和默认能力下限；
- Job 一旦进入执行，其硬约束不可被内部 fallback 放宽；
- payload 中的敏感原文默认不进入日志；
- 终态不可逆，只允许追加 usage、audit 等结算信息。

### 6.2 Intent、模型能力与部署

Intent 命名采用 `<domain>.<action>`，不包含 App、provider、位置或模型规模，例如：

```text
text.summarize
text.classify
text.proofread
assistant.general
reasoning.deep
audio.transcribe
audio.align
speech.analyze
speech.synthesize
speech.voice_design
speech.voice_clone
vision.caption
vision.detect_faces       experimental; synchronous local-only P0
vision.embed_face         experimental; synchronous local-only SensitiveBiometric
vision.embed_image        experimental; synchronous local-only SigLIP image encoder
vision.embed_text         experimental; synchronous local-only SigLIP text encoder
vision.describe_image     experimental; typed QwenVL bounded description/keywords
vision.review_classification experimental; Consumer-supplied closed set
```

Intent Profile 定义输入/输出模态、required features、默认能力下限和默认 policy；Responses Intent 还可定义调用方缺省时使用的 `default_max_output_tokens` 与 `default_reasoning_effort`。这些是任务级生成默认值，不是模型能力评级；调用方显式标准字段优先。Model Profile 则对每个 Intent 分别评级；同一模型可在总结上为 `general`、在深度推理上仅为 `basic`、在视觉上为 unsupported。参数量、provider 和 placement 均不能自动推出档位。

模型运行结构固定为：

```text
Intent Profile
      |
      | workload-specific rating
      v
Model Profile -> Model Build -> Deployment -> Provider -> Node
```

评级为 `provisional` 或 `benchmarked`。`benchmarked` 必须引用 eval profile；未经任务级评估的模型不得伪装成精确分数或高档能力。

Deployment 另有 `resource_class = light | standard | heavy | extreme`，只表示运行负担，不表示能力。美元价格同为 0 的本地候选仍可据此让总结优先使用 2B，而不是浪费 35B。

ONNX Build 不能只由一个文件名或模型家族标识。当前版本化 manifest 已绑定 exact
artifact/checkpoint/export digest、ONNX opset/export revision、输入输出 tensor contract、
orientation/resize/crop/color/layout/mean/std/dtype、对齐模板与输出归一化、
允许的 Execution Provider/precision/fallback，以及 code/weight/data license 与
redistribution facts；需要 tokenizer/vocabulary 的 Build 必须继续把其 digest 纳入 identity。
对 embedding 模型，这些字段共同决定
embedding-space identity；不同 space/revision 的向量不得进入同一比较或索引域。

### 6.3 Constraints

约束分为不可违反的 **hard constraints** 与用于排序的 **preferences**。

```text
Hard constraints
  placement     local_only | private | anywhere | cloud_only
  offline_required?
  quality_floor basic | general | advanced | frontier
  max_cost?     per-job upper bound
  deadline_at?
  required_features[]   e.g. tool_calls, json_schema, image_input
  fallback      none | equivalent | allow_lower_quality

Preferences
  latency       interactive | balanced | throughput
  placement     prefer local | trusted_node | cloud
  policy        balanced | local-first | quality-first | latency-first | cost-first
```

设计要求：

- hard constraint 先过滤，preference 后排序；
- placement 是允许集合，`local`、可信远程节点和第三方 cloud 是不同信任边界；fallback 不得扩大集合；
- 能力下限必须先过滤。较弱模型即使使用更高 reasoning effort，也不能被当成更高能力档位；
- `fallback=allow_lower_quality` 只可撤销调用方通过 `infer.quality_floor` 额外提高的门槛，绝不能低于 Intent 的 `default_quality_floor`；
- 真正禁止付费使用 `max_cost=0` 表达；`cost=economy` 只是 profile 内的排序偏好；
- deadline 包含排队和执行预算，而非仅 provider 超时；
- provider/model 特有特性通过 `required_features` 协商，普通 App 不使用物理 backend 名称；
- 配置定义 profiles、默认 profile 和 request override 权限。请求只能在授权范围内选择 profile、增加 hard constraints 或调整 preferences；
- 合并顺序固定为：系统安全不变量 → global/provider hard limits → App policy → Intent defaults → request overrides。后层不得放宽前层 hard constraints。

### 6.4 Intent 与 Responses 映射

Responses 请求的标准 `model` 字段引用 Intent Profile，例如：

```text
intent: text.summarize
input_modalities: [text]
output_modalities: [text]
default_quality_floor: basic
default_profile: cost-first
```

应用在自身领域内把“总结转写”等动作映射到 `text.summarize`；runtime 不保存 `app.summary` 之类应用别名。普通 App 不能把 `model` 写成 `ollama:qwen...` 来绕过 Intent。管理员 debug/benchmark surface 可以显式指定物理 deployment，但仍受安全、placement 和预算 hard limits。

请求级动态控制使用 Responses `metadata` 中保留的 `infer.*` keys。MVP 预留：

```text
infer.policy          named profile
infer.priority        interactive | normal | background
infer.placement       local_only | private | anywhere | cloud_only
infer.prefer          local | trusted_node | cloud
infer.offline_required true | false
infer.quality_floor   basic | general | advanced | frontier
infer.provider_access_class standard | subscription（只能缩窄 App ACL）
infer.latency         interactive | balanced | throughput
infer.max_cost_usd    decimal string
infer.fallback        none | equivalent | allow_lower_quality
infer.deadline_ms     positive integer string
```

这些 metadata 由 runtime 消费，默认不发送给下游 provider。未知 `infer.*` key、越权 override 或格式错误均在 admission 阶段明确拒绝，不能静默忽略。

标准 Responses `reasoning.effort` 独立表达选中模型后的计算投入。deployment 必须声明支持的 effort；提高 effort 不得绕过 `quality_floor`。

### 6.5 音频协议族

音频公共接口按任务合同归并，而不是按模型拆分：

| Intent | HTTP 数据面 | 当前 build |
| --- | --- | --- |
| `audio.transcribe` | `POST /v1/audio/transcriptions` multipart | Qwen3-ASR 1.7B 8-bit |
| `audio.align` | `POST /v1/audio/alignments` multipart | Qwen3-ForcedAligner 0.6B 8-bit |
| `speech.synthesize` | `POST /v1/audio/speech` JSON | Qwen3-TTS CustomVoice 1.7B 8-bit |
| `speech.voice_design` | `POST /v1/audio/speech` JSON | Qwen3-TTS VoiceDesign 1.7B 8-bit |
| `speech.voice_clone` | `POST /v1/audio/voice-clones` multipart | Qwen3-TTS Base 1.7B 8-bit |

文件上传上限为 25 MiB。payload 只在 API 请求和单次 executor 临时目录中存在，不进入 Job metadata；临时目录拥有输入、参考音频和输出的完整生命周期，执行完成即删除。MLX worker 是常驻进程，但默认仅缓存一个已加载模型，切换 build 时显式释放旧模型与 MLX cache，以控制统一内存压力。

本地 worker 使用 JSON-lines 作为进程内协议，request id 用于隔离 deadline/cancel 后的迟到结果。具体模型路径、Python 环境和运行位置属于 Deployment/Provider 配置，不泄露到公开音频 API。

执行模式与数据模态正交：`unary` 返回完整结果，`server_stream` 只由 provider 向 Consumer
增量输出，`duplex` 在一个受调度 session 内双向交换。`POST /v1/audio/speech` 在
`execution_mode=server_stream,response_format=pcm` 时返回带固定 sample-rate/channel headers 的
append-only `pcm_s16le` body；其他格式仍为 unary 文件。`GET /v1/audio/transcriptions/stream`
升级 WebSocket，首帧配置 PCM shape，二进制帧输入音频，commit/finish 产生单调 revision 的
partial/final transcript。当前 Qwen3-ASR adapter 会重新解码已提交前缀，因此披露
`transcription_mode=commit_redecode` 和 `revisable`；这能支持边听边修订，但不等于模型原生实时
ASR。alignment、embedding 继续保持 unary。

### 6.6 Model、Deployment 与 Node

```text
Deployment
  id
  provider_id
  model_build_ref
  supported_efforts[]
  supported_execution_modes[]  unary | server_stream | duplex
  resource_class
  pricing
  health
  lifecycle_state
  labels

Node
  id
  trust_class
  resources
  providers[]
  heartbeat / lease
```

注册信息是声明，健康探测和实际执行结果是证据。Router 不能把过期声明当作实时容量。
对未来 ONNX Attempt，provenance 还必须记录实际 Execution Provider、precision、fallback
reason、runtime version、Build/preprocess identity 和 input artifact/source revision；不能只记录
配置希望使用的 backend。平台间允许任务定义的数值 tolerance，但不能把一个 backend 的
`benchmarked` 结果无条件授予另一个 backend。

### 6.7 Job 状态机

```text
submitted -> admitted -> queued -> reserving -> running -> succeeded
     |          |          |          |            |
     |          |          |          |            +-> cancelling -> cancelled
     |          |          |          |            +-> retry_wait -> queued
     |          |          |          +-> queued (reservation unavailable)
     |          |          +-> expired
     |          +-> rejected
     +-> rejected

Any non-terminal state -> failed, only when no permitted attempt remains.
```

终态：`succeeded`、`failed`、`cancelled`、`expired`、`rejected`。

关键语义：

- `rejected` 表示从未被系统接受，例如 schema、身份、预算硬限制或无可满足能力；
- `failed` 表示接受后尝试执行但最终失败；
- 客户端断开不等于自动取消，行为由提交选项明确指定；
- 取消是 best-effort，但终态必须唯一；迟到的 provider 输出只能用于结算和诊断，不能重新成功；
- retry 复用同一 Candidate；fallback 选择新 Candidate。两者都创建新 Attempt。
- M2 首版单 Job 最多 3 个 Attempt，同一 Candidate 最多自动重试 1 次；`rate_limited`、`timeout`、`unavailable` 可重试，`invalid_request` 不得重试或 fallback。其他 provider 失败可在仍有 deadline 时切换到固化计划中的下一 Candidate。
- 非流式结果在完整成功前不可见，可以按上述规则 fallback；流式调用一旦进入事件流便固定 Candidate，任何后续断流只返回失败事件，绝不拼接另一模型输出。

## 7. 请求处理流程

### 7.1 提交与准入

1. API 验证身份、Responses schema、payload 大小和 Intent；Intent 解析出模态、features、默认能力下限与默认 profile。
2. Admission 按固定层级合并 global/provider limits、App policy、Intent defaults 与获准的 `infer.*` request overrides，规范化 constraints 并执行静态拒绝条件。
3. Quota Manager 做最小准入检查；真正的费用和容量预留延迟到 dispatch 前，以免排队时长期占用。
4. Job Coordinator 持久化或记录 Job，进入有界优先级队列。
5. 返回 Job handle；流式调用可继续等待事件。

### 7.2 候选生成与路由

Router 负责“在哪里执行”，不负责“何时轮到”：

1. Registry 根据 Intent 找出具有对应评级的 Model Profile，再展开到 Build/Deployment；
2. 过滤模态/features、quality floor、placement、reasoning effort、deadline、cost 和健康状态；
3. Policy Engine 对剩余候选计算可解释排序；
4. 输出有序 Candidate Plan，以及每个候选被接受/拒绝的 reason codes；
5. Plan 带有短期有效期，资源状态显著变化时必须重算。

排序由当前 policy profile 的类型化有序规则决定，而不是 runtime 内置一个普遍顺序或不可解释总分。profile 可依次比较 deadline 可行性、placement preference、已加载状态、预计排队、成本、资源重量、质量等属性；每一级都必须产生稳定 reason code。`quality_fit` 选择与有效 floor 距离最近的最低充分 grade，`quality` 选择最强合格 grade。系统提供 `balanced`、`local-first`、`quality-first`、`latency-first`、`cost-first` 模板，但全局/App 默认值和模板内容均可配置。

### 7.3 调度与资源预留

Scheduler 负责“何时执行”：

- 三个优先级队列均有容量上限；
- `interactive` 可优先获得新释放的槽位，但 MVP 不强杀正在生成的请求；
- 必须采用 aging 或配额轮转避免 background 永久饥饿；
- 同一 App 的 pending Job 数由 `max_pending_jobs` 限制；Job 从 admission 到终态都占一个 slot，避免单一 App 淹没 provider queue；细分的 App concurrency/rate quota 属于 M3；
- dispatch 前按固定顺序预留：budget/rate -> provider concurrency -> node capacity -> model lifecycle；
- 任何一步失败都要释放先前预留，且使用有序、可超时的 reservation protocol，避免死锁；
- 实际开始执行前再次检查 deadline 和 Candidate 有效性。

### 7.4 执行、流式返回与结算

1. Provider adapter 将规范化请求翻译为后端调用。
2. 每个 append-only stream event 附带单调递增序号；可修订 transcript 使用单调 revision，final
   事件才是终态。文本、音频和 transcript 不共享一个 payload schema。
3. Client 慢消费触发有界 buffer 和背压；若后端不可背压，则超过上限后按协议终止，不能无限增长内存。
4. 完成或失败后，Quota Manager 用实际 usage 结算并释放预留。
5. Job Coordinator 追加 Attempt 结果、decision reasons、latency breakdown 与错误分类。
6. 只有在约束允许、错误可回退且仍有 deadline/budget 时，才选择下一 Candidate。

## 8. 组件职责与边界

每个组件是语义 owner：拥有自己的状态、不变量和失败策略。实现时可以合并在较少 crate 中，但不应合并职责。

### 8.1 API Service

拥有协议版本、认证上下文、请求大小限制、事件编码与连接生命周期。它不做路由或业务策略，只把合法请求交给 Job Coordinator。

### 8.2 Job Coordinator

拥有 Job/Attempt 状态机、幂等、取消、终态竞争和 workflow orchestration。它调用其他 owner，不复制它们的策略。正常变更应能只在状态机及相邻合同中完成。

### 8.3 Admission & Policy Engine

拥有 App policy、constraint 规范化、硬约束判定与排序规则版本。每次决策必须记录 policy version 和 reason codes。不得调用 provider 发起推理。

### 8.4 Registry

拥有 Node、provider、deployment、capability 和 feature 的目录视图，以及声明信息的新鲜度。它不拥有实时容量 reservation，也不自行调度。

### 8.5 Router

拥有 Candidate Plan 的生成和失效规则。输入是 Job intent、registry snapshot、resource/quota snapshot 和 policy；输出是可解释计划。它不拥有队列。

### 8.6 Scheduler

拥有队列、公平性、aging、dispatch eligibility 和并发时序。它不解释 provider payload，不直接改预算账本。

### 8.7 Quota Manager

拥有 global/provider/app 的预算、RPM、TPM、并发限制、reservation 和 reconciliation。预算检查与扣减必须原子化；未知最终 token 的请求按估算预留并在结束时调整。

### 8.8 Resource Manager

拥有 Node 资源 snapshot、容量 reservation、本地模型生命周期和资源压力动作。Registry 回答“有什么”，Resource Manager 回答“现在能不能承载”。当前已实现 Ollama inventory、Attempt reservation 与认证 operator 显式 load/unload；压力驱动的自动装卸仍由后续 policy owner 决定。

### 8.9 Provider Runtime

拥有 adapter 生命周期、健康探测、协议翻译、错误归类、usage 提取和 provider 取消。每种 adapter 保持自身协议细节；共享的是 Provider SPI，不是一个充满条件分支的万能 client。

M2 的首版健康状态在 control plane 中按 provider 维护：连续三次 `unavailable`、`timeout` 或 `rate_limited` 失败后 circuit 打开 30 秒。Router 读取其快照并用 `provider_circuit_open` 排除候选；成功调用清除状态。它是内存态，重启即清空；持久化健康历史属于 M3。

### 8.10 Store

拥有 schema、事务、迁移和恢复边界，不拥有领域策略。凭证不存入普通数据库；Store 只保存 secret reference。M1 使用内存状态；M3 的 SQLite 保存 Job/Attempt metadata、decision events、usage ledger 和 reservations，但不恢复执行。

### 8.11 Observability

拥有结构化日志、metrics、traces 和 audit event schema。它必须能回答：谁提交、为何选择该 Candidate、排队多久、花费多少、为何 fallback、哪个约束导致拒绝。

## 9. Provider SPI

Provider 接口最少包含：

```text
discover() / configured_deployments()
health()
capabilities(deployment)
estimate(job, deployment)
execute(attempt, cancellation) -> event stream
normalize_usage(raw_result)
classify_error(raw_error)
```

边界要求：

- provider-specific request options 只能通过受控、命名空间化扩展进入，并受 App policy 限制；
- `requires_api_key = true` 的 provider 在 credential 缺失时不进入 Candidate Plan；这不是运行后才暴露的 401，也不妨碍本地-only daemon 启动；
- adapter 必须报告能力缺失，不能静默忽略参数；
- 错误至少归类为 authentication、rate_limited、capacity、timeout、unavailable、invalid_request、content_policy、cancelled、protocol、internal；
- 只有明确标记为 retryable/fallback-eligible 的错误才能触发自动重试；
- 健康检查成功不等价于某模型必然可执行；实际调用结果会反哺短期熔断状态。

MVP adapter：

- **Responses-compatible execution adapter**：统一处理无状态 `/v1/responses` 请求、Responses object、SSE events、错误和 usage；
- **DeepSeek Flash profile**：复用 Responses adapter；当前 V4 Pro 没有 `/responses` deployment，待该能力正式可用后再激活。Chat Completions SPI 只有在第二个独立 provider 需求证实时才建立；
- **Provider capability profile**：每个 provider instance 声明并探测支持字段与行为，不能从“OpenAI-compatible”标签推断完整兼容；
- **Ollama control extension**：只负责 Ollama 专有的模型发现、健康和未来生命周期操作，文本执行复用 Responses adapter。官方兼容基线是加入 `/v1/responses` 的 Ollama v0.13.3，但 runtime 仍必须运行 capability probe，不能只信版本号。
- **Codex App Server bridge（experimental）**：独立进程/JSON-RPC adapter，把一个订阅账号建模为
  一个 Provider，把显式准入的 Sol/Terra/Luna 建模为多个 Deployment；`model/list` 只刷新动态
  inventory。bridge 支持有界 text/image input 和 append-only text SSE；模型每次必须实际声明
  image modality。它不进入 HTTP Responses adapter 的条件分支，也不开放 tools/agent 能力。

MVP Responses profile 支持 `model`、`input`、`instructions`、`stream`、function tools、`temperature`、`top_p`、`max_output_tokens`、`truncation` 和 usage。function tools 只描述模型可以返回的 tool calls；runtime 不执行工具或接管 tool loop，调用方负责后续交互。MVP 是无状态调用：`previous_response_id`、`conversation` 及服务端 conversation state 明确返回 unsupported。字段集合以后只能通过 profile 和合同测试扩展。

当前 capability profile 为 provider instance 的 version 1 配置，分别声明 `responses`、`instructions`、`streaming`、`function_tools`、`reasoning_effort`、sampling、`max_output_tokens`、`truncation` 和普通 metadata 支持。每个请求由其实际字段推导出 endpoint 与模型 feature requirements，Candidate Plan 在 dispatch 前以 `provider_capability_missing` 或 `required_feature_missing` 排除不满足者。`POST /infer/v1/providers/{provider_id}/probe` 是显式、可能计费的 operator action：它用已注册的物理 build 验证所声明的每项能力，首项失败后停止，避免对故障 provider 继续施压。`GET /infer/v1/providers/{provider_id}/models` 返回 provider-native 动态模型组及 `admitted` 标记；该观察接口不能写 Registry，也不能绕过 App access class。

### 9.1 ONNX Provider/视觉 adapter 边界（P0 已实现，协议仍 experimental）

ONNX Runtime 是新的本地 Provider/执行族；Session 或 tensor 逻辑不进入 Router/Scheduler。
公共 ONNX owner 负责 Session Registry、Build verification、
Session create/warm/drop、Execution Provider 选择与实际 route 记录、资源估算/reload
benchmark、取消和原生错误归类。YuNet/SFace/DINOv2/SigLIP 等专属 adapter 分别拥有预处理、
tensor 编码、输出解释和模型语义。

先开放 YuNet `vision.detect_faces`，随后按独立隐私门槛开放 SFace `vision.embed_face`；
两者都是同步、local-only 的实验协议。SFace 接收原图和 detector 产出的命名五点，adapter
独占官方对齐、tensor 与归一化语义；128 维向量只出现在同步响应。普通应用不能指定 tensor、
文件路径或物理 EP。

第三个 slice 以两个 typed endpoint 暴露 SigLIP `vision.embed_image` / `vision.embed_text`：
image adapter 独占 FixRes 224/RGB/NCHW normalization，text adapter 独占 lowercase、固定 64-token
tokenizer；两个 immutable Builds 绑定相同 768d L2/cosine space。当前真实 Core ML 不能完整接管
且失败探测代价过高，因此当前 Builds 明确 CPU-only，未来 EP 组合形成新 Build。QwenVL 另以
`vision.describe_image` / `vision.review_classification` 两条 typed 数据面封装 Ollama：前者返回
有界描述和关键词 proposal，后者只在 Consumer 提供的闭集内返回
`matched|none|uncertain`。4B 是 basic/standard bulk 候选，8B 是 general/heavy 明确复核候选；
Consumer 只请求 Intent 与 quality floor，不绑定 Ollama tag。MLX audio 继续保持独立数据面。

## 10. 调度策略

### 10.1 优先级与公平性

默认语义：

| Priority | 目标 | 典型任务 |
| --- | --- | --- |
| interactive | 缩短排队和首 token 延迟 | 用户正在等待的分析/生成 |
| normal | 平衡延迟与吞吐 | 普通应用请求 |
| background | 最大化剩余容量利用 | 批量照片、离线索引 |

MVP 采用非抢占执行：已运行 Attempt 不因高优先级 Job 被强制中断。优先级作用于队列和新资源分配。模型卸载只作用于无活跃 reservation 的模型。

### 10.2 背压

背压必须同时存在于：

- API 入站并发；
- 每 App pending Job 数；
- 每优先级队列长度；
- 每 provider/node 并发；
- stream buffer；
- provider rate limit。

达到限制时返回明确的 retry hint 或进入有界队列；不能无限接收后依赖内存。

### 10.3 资源压力与模型生命周期（M4 已收口）

模型状态：`absent -> loading -> ready -> draining -> unloading -> absent`，另有 `failed`。Resource Manager 是唯一能触发装卸的 owner。

卸载候选至少考虑：

- 活跃 reservation 必须为零；
- 最近使用与重新加载成本；
- 等待队列是否需要该模型；
- 释放后能否满足更高优先级任务；
- 最小驻留时间，避免 thrashing。

MVP 不承诺跨 provider 的通用 unload API；Ollama 能力应按实际协议单独实现。

当前 Resource Manager 将系统压力、已驻留模型和版本化的
`[resources.reload_benchmarks.<deployment>]` 合成为 deterministic eviction
recommendation。配置的默认模式为 `disabled`；`recommend` 模式只生成 dry-run
计划，只有独立的显式 approval action 或启用且持有短维护租约的 monitor 才能发起原生
调用。主机采样值与 `[resources.pressure]` 分类阈值分离，允许不同统一内存/GPU 主机
保留不同 headroom，同时不改写观测值。每个模型的安全值依次合并全局默认、`resource_class` 和
deployment override；`automatic_eligible` 默认 false，最小驻留时间也可逐层加严或
针对已验证 build 调整。缺少、过期或没有证据的 reload benchmark，以及 resolved
eligibility 为 false，都会使模型不可逐出。

执行侧采用独立的 approval-gated eviction action owner。调用方必须确认 fresh plan 的
第一项 deployment，并提供有界 reason；owner 串行化动作、每次只处理一个目标，目标
漂移时拒绝。Runtime 在 native mutation 前把批准事件写入与 Job audit 分离的
`resource_audit_events`，Resource Manager 再复用同一个 lifecycle reservation gate
执行 unload。refresh/snapshot 保持只读。

后台化不改变这一动作契约：独立 monitor 按配置间隔轮询，只在短期、进程内、单实例
maintenance lease 有效时调用 current-first-target action，每轮最多一项。每轮重新获取
pressure/inventory/benchmark/lifecycle 输入；lease 重启丢失、到期 fail closed，撤销不
取消已经进入 native lifecycle 的动作。受控实机 soak 已验证逐项清退、最小驻留和活跃
reservation 保护；开发配置继续默认关闭，等待长时间日常负载验证。

所有 native resource mutation、reload benchmark 与资源审计读取还需 App 显式声明
`resource_admin = true`，默认 false。MVP 使用这一窄布尔门，不把尚不存在的多租户
角色系统或通用 scope DSL 提前引入核心配置。

reload benchmark 由独立的 operator workflow 采集：只允许从已确认的 `absent`
deployment 开始，测量 load 后立即 unload，并在整个过程中占用 `benchmarking`
lifecycle 状态。结果只生成带时间和证据的 TOML 记录；daemon 不写配置真源。操作
失败或取消后，系统必须通过原生 inventory 重新确认 residency，不能根据控制调用的
局部结果假定模型已经卸载。

M4 当前组合的是全机压力、deployment lifecycle reservation 与 per-provider
queue/capacity。SigLIP image+text release 实测合计约 1.9 GiB RSS，已登记为 heavy 并纳入
上述 admission/residency 门；这一同步 slice 仍未证明必须把资源拆成原子多池。当前结构也
尚未证明能精确表达 ONNX Session、Ollama residency 和 MLX cache 在同一
Node 上对 unified/GPU memory、CPU slots 或 accelerator queue 的联合竞争。D-107 保持两个
方案开放：继续由 provider 拥有局部容量、只增强统一 admission estimate；或增加显式 Node
Resource Pool 与多资源原子 reservation。该设计评审是 M5 resource declaration 与 M6 ONNX
admission 的共享前置门，但不属于 v0.1，也不能反向扩张已经收口的 M4。

## 11. 配额、计费与限流

层级：

```text
Global
  +-- Provider
  |     +-- model/deployment (optional)
  +-- App
        +-- capability/priority class (optional later)
```

每次 Attempt 建立 reservation：

- 货币：根据 input 上限、max output 和价格估算；
- token：已知 input + max/estimated output；
- RPM/TPM：滑动窗口或 token bucket；
- concurrency：Attempt 生命周期内占用。

完成后使用 provider usage 结算；provider 不返回 usage 时标记为 estimated，不得伪装成精确值。账本需要支持失败、超时、取消和迟到响应。global/provider/app 多层更新必须具有一致的原子边界。

订阅 Provider 没有可信的逐请求 USD 账单，不能把“月费已付”解释为无限或免费。首个 slice
以 Provider 单并发队列、App subscription entitlement、上游 usage/rate-limit 错误和 runtime
Attempt 计数治理；后续只有在上游提供稳定 quota snapshot 后才增加窗口 reconciliation。

## 12. 失败、重试与 fallback

### 12.1 基本规则

- schema/auth/content policy 错误默认不重试；
- 网络瞬断、明确 rate limit、临时 unavailable 可按 provider policy 重试；
- 退避必须计入 deadline；
- streaming 已向客户端发出可见内容后，默认不跨模型自动 fallback，以免拼接两个模型的输出；若未来支持，必须成为显式协议语义；
- fallback 不能放宽 placement/data policy、quality floor、required_features 或 max_cost；
- 每个 Job 限制最大 Attempts、总执行时间和累计估算费用。

### 12.2 熔断与健康

熔断以 provider/deployment 为合理粒度，区分认证失败、全局 endpoint 故障与单模型容量问题。半开探测不应占用普通 App 的全部并发配额。

## 13. API 与 CLI 契约

### 13.1 文本数据面

```text
POST /v1/responses
  model     -> stable Intent Profile, e.g. text.summarize
  input / instructions / tools / generation params
  metadata  -> optional authorized infer.* constraints
  stream    -> false: Responses object; true: Responses SSE events
```

普通应用可使用官方 OpenAI SDK，修改 base URL 并把 `model` 设为已配置 Intent Profile。runtime 的 Job ID 与公开 response ID 建立稳定映射。MVP 不承诺 OpenAI 平台的存储、conversation、hosted tools 或所有字段；兼容范围由版本化 profile 声明，不支持即报错。

典型总结请求：

```json
{
  "model": "text.summarize",
  "instructions": "Summarize the transcript while preserving decisions and action items.",
  "input": "<transcript text>",
  "stream": true,
  "metadata": {
    "infer.priority": "interactive",
    "infer.policy": "cost-first",
    "infer.placement": "private",
    "infer.prefer": "local",
    "infer.quality_floor": "basic",
    "infer.fallback": "equivalent",
    "infer.deadline_ms": "30000"
  }
}
```

这里的 `cost-first` 是排序 profile，`private` 是只允许本机和可信节点的 hard constraint，`local` 只是该允许集合内的偏好。若 App policy 不允许某项 override，admission 直接拒绝。

### 13.2 控制面

```text
/infer/v1/jobs       App-scoped keyset paging over lightweight Job metadata
/infer/v1/jobs/{id}  query one underlying Job/Attempt snapshot
/infer/v1/jobs/{id}/cancel  cancel one non-terminal Job owned by the App
/infer/v1/queue      inspect queue and pressure
/infer/v1/intents    inspect Intent Profiles and defaults
/infer/v1/models     inspect workload-specific ratings and evidence
/infer/v1/deployments inspect runnable builds and placements
/infer/v1/providers  health and capability profiles
/infer/v1/resources  native inventory, pressure and model lifecycle snapshots
/infer/v1/resources/{provider}/deployments/{deployment}/load|unload  explicit native operator actions
/infer/v1/resources/eviction/apply  approve and apply exactly one fresh eviction target
/infer/v1/resources/events  inspect durable resource action audit events
/infer/v1/resources/eviction/maintenance-lease  inspect or grant the monitor lease
/infer/v1/resources/eviction/maintenance-lease/revoke  revoke the monitor lease
/infer/v1/providers/{provider_id}/probe  run an explicit, billable Responses compatibility probe
/infer/v1/budget     quota and usage
/infer/v1/explain    decision chain
/infer/v1/status     daemon status
```

CLI 对应提供 `status`、`jobs`、`queue`、`intents`、`models`、`deployments`、`providers`、`nodes`、`budget/usage`、`policy explain`，以及 post-MVP 的 `benchmark`。CLI 只调用控制 API，不直接读写数据库。

Responses SSE 对普通 Client 只暴露兼容事件；queued、attempt_started、fallback reason 等控制平面细节保存在 Job audit stream 和 explain 输出中，避免发明非标准 Responses event。

音频 streaming 不复用 Responses SSE：TTS 使用 chunked PCM body，duplex ASR 使用 WebSocket。
三者仍共享 admission、Job/Attempt、deadline、cancel、reservation 和唯一终态。首个可见输出后
不得 fallback；duplex session 从创建到 final/cancel/disconnect 持有同一调度与模型 reservation。

物理 backend override 只允许 admin/debug surface，并明确标记绕过哪些 routing preference；它仍不能绕过安全、隐私和预算硬限制。

## 14. 配置、身份与安全

### 14.1 默认威胁边界

MVP 是单用户设备服务，只监听 loopback，不开放局域网或公网入站。远程 Client 访问与远程 Node 是后续独立安全边界，不能仅通过修改 listen address 偷渡进首版。

### 14.2 App 身份

App policy、quota 和审计都依赖可信 `app_id`。不能让任意客户端在 request body 或 `metadata` 中自选身份。MVP 为每个 App 配置独立 bearer credential，认证层将 credential 映射为 `app_id`。本机 `local-operator` 的 256-bit credential 由 runtime 自动生成并保存在 owner-only 文件中；CLI 自动读取，用户无需在每次启动时传入。外部 consumer 可以由本机 Apps & Access operator surface 创建独立 managed credential，或使用环境注入 credential；两者默认都没有 `resource_admin`。每个普通 App 用 `allowed_intents` 声明最小 workload 权限，并用 `allowed_provider_access_classes` 决定是否能消费订阅式 Provider；默认值只有 `standard`。cloud payload egress 另由 `allowed_cloud_input_modalities` 控制，默认仅 `text`；获得 subscription 并不自动允许外发图片。统一 Job preparation 在任何 planning、admission、持久化或 provider 调用前执行检查，所有数据面和 durable submission 复用这一 owner。远程节点和远程客户端必须使用独立凭证与双向身份验证。

### 14.3 配置与 policy reload

TOML 是 Intent、Model Profile、Model Build、Deployment、Provider、App policy、quota 和 policy profiles 的配置真源。CLI 提供 `config validate`、`config reload`、`config effective` 和 `policy explain`。reload 必须完整验证后原子切换；已 admission 的 Job 固定记录并使用原 config/policy version，不因热重载改变执行语义。MVP 不执行任意脚本或通用 policy DSL。

下面是当前 M3 schema 的语义示例。未设置的 quota 字段没有隐式限制；每次
Attempt 在同一个 SQLite transaction 内同时检查 global、provider 与 App
三个适用 scope：

```toml
[defaults]
policy = "balanced"

[persistence]
path = "infer-runtime.sqlite3"

[auth]
managed_credentials_directory = ".infer-runtime/credentials"

[quota.global]
requests_per_minute = 120

[quota.providers.openai-cloud]
max_usd = 20.0
tokens_per_minute = 200000

[quota.apps.sample-consumer]
max_usd = 2.0
max_concurrent_attempts = 1

[profiles.local-first]
order = ["placement", "deadline_fit", "queue_time", "cost", "quality_fit"]

[profiles.quality-first]
order = ["quality", "deadline_fit", "cost", "placement"]

[providers.ollama-local]
kind = "responses"
base_url = "http://127.0.0.1:11434/v1"
placement = "local"

[providers.ollama-local.capability_profile]
version = 1
protocol = "responses"
capabilities = ["responses", "instructions", "streaming", "temperature", "top_p", "max_output_tokens", "truncation", "metadata"]

[providers.openai-cloud]
kind = "responses"
base_url = "https://api.openai.com/v1"
placement = "cloud"

[providers.codex-subscription]
kind = "codex_app_server"
access_class = "subscription"
command = "codex"
placement = "cloud"

# Dynamic inventory may be registered before any model is admitted. Stable
# upstream model slugs become routable only after explicit Build/Deployment
# configuration and evaluation.
[providers.antigravity-subscription]
kind = "antigravity_cli"
access_class = "subscription"
command = "/absolute/path/to/agy"
placement = "cloud"

[providers.antigravity-subscription.capability_profile]
version = 1
protocol = "antigravity_cli"
capabilities = ["responses", "instructions", "reasoning_effort"]

[intents."text.summarize"]
input_modalities = ["text"]
output_modalities = ["text"]
default_quality_floor = "basic"
default_policy = "cost-first"

[model_profiles.qwen_2b]
family = "qwen"
[model_profiles.qwen_2b.ratings."text.summarize"]
grade = "basic"
status = "benchmarked"
eval_profile = "summary-zh-v1"
score = 0.82

[model_builds.qwen_2b_mlx]
profile = "qwen_2b"
model_id = "qwen:2b-mlx"
input_modalities = ["text"]
output_modalities = ["text"]

[deployments.local_qwen_2b]
provider = "ollama-local"
build = "qwen_2b_mlx"

[apps.local-operator]
credential = { source = "managed" }
resource_admin = true
allowed_provider_access_classes = ["standard", "subscription"]
allowed_intents = ["text.summarize", "assistant.general"]
max_pending_jobs = 16
default_policy = "balanced"
allowed_policies = ["local-first", "balanced", "quality-first"]

[apps.local-operator.request_overrides]
priority = ["interactive", "normal"]
placement = ["local_only", "private", "anywhere"]
prefer = ["local", "trusted_node", "cloud"]
quality_floor = ["basic", "general", "advanced", "frontier"]
max_cost_usd = { min = 0.0, max = 0.10 }
```

profiles 中的规则名称来自受版本控制的类型化集合；用户可以改变顺序和参数，但不能注入任意代码。配置不定义某次请求的实时资源结果，Router 仍会结合健康、queue 和 reservation snapshot 动态决策。

`allowed_intents` 的三态语义是：字段省略表示兼容旧配置、允许全部当前 Intent；显式空数组
表示保留 App 身份但禁止推理；非空数组表示精确 allowlist。配置验证拒绝未知或重复 Intent。
不在清单中的请求返回稳定的 `403 intent_forbidden`，不会产生 Job/Attempt、配额预留或
provider side effect。该清单与 placement/policy/quality 等 request override 上限正交，
不演化成角色 DSL 或 endpoint 专属权限表。

`allowed_provider_access_classes` 是第二个正交硬边界：旧配置和新建 Consumer 默认只有
`standard`，因此新增 `subscription` Provider 不会自动扩大任何 Consumer 的付费/订阅权限。
缺少授权的候选记录 `provider_access_not_allowed`，不会进入 provider queue 或启动本机 bridge。

### 14.4 Secret

- 本机 operator 与显式创建的 managed Consumer token 进入 runtime-managed owner-only
  credential file；外部 App 也可继续由环境或后续 OS credential provider 注入。Provider key
  始终只在 daemon 端注入。配置和数据库只保存 source/reference；
- Apps & Access 对已有 token 只显示 SHA-256 短指纹；创建/轮换明文只展示一次且响应为
  `no-store`。轮换/撤销在 daemon 重启前不宣称生效；`local-operator` 不允许从该页面变更；
- 日志、trace、错误正文默认脱敏 Authorization、prompt、音频/图片原文；
- admin 导出必须明确区分 metadata 与 sensitive payload；
- provider endpoint 的 TLS 校验默认不能关闭。

### 14.5 远程 Node（后续）

Node 需要稳定身份、配对/批准、心跳租约、能力声明签名或受信通道、任务授权和吊销。远程节点不是普通 Responses-compatible endpoint 的同义词：前者参与资源控制与生命周期，后者只是 provider。

### 14.6 视觉与生物特征 payload（后续，Proposed）

视觉能力默认 `local_only`，不得因 provider 失败隐式扩展到 cloud；face embedding 属于
`SensitiveBiometric`。同步 Job 的通用 metadata、日志和普通 audit details 不保存图片像素、
face crop 或 embedding。应用优先提供有界、版本化的 RGB artifact/reference，而不是 RAW；
结果必须绑定 exact input artifact/source revision 和 preprocess/Build identity，迟到或 revision
失配的结果不得发布。

Shadow 拥有照片选择、Recipe/source revision、stale 判断、Catalog 派生证据、人物命名与用户
确认事实；runtime 不把这些领域对象复制进核心 Job 模型。视觉引用授权、retention、删除责任
和结果发布合同由 D-108 关闭。ADR-0010 只允许 local Responses 文本 background；视觉 durable
background 必须重新定义 payload owner、加密/引用租约、恢复幂等和删除语义，不能默认复用。

## 15. 持久化与恢复

至少需要保存：

- 当前与历史 config/policy version 的内容摘要及引用；
- Job/Attempt metadata、decision events 与 response ID 映射；
- usage ledger、quota reservations 与审计元数据；
- provider/deployment 健康历史中恢复决策所需的最小部分。

显式 durable background 文本 Job 另有受限 payload owner：`infer-payload` 把 Responses
input/result 写入独立的 AES-256-GCM spool，SQLite 只保存随机 blob ID、类型、带密钥
HMAC identity、大小和 retention metadata。密钥值只从配置引用的环境变量读取；App ID、
payload 类型、blob identity 与长度都进入认证上下文。该目录不是任意文件仓库，也不接受
调用方路径或 URL。Unix 权限默认为目录 `0700`、文件 `0600`，发布使用原子 rename；
启动只清理由该 owner 命名且未被数据库引用的 orphan。

集合查询不得加载完整 `snapshot_json`。SQLite 为 Job 单独投影 `app_id`、`state`、
`priority`、路由落点和时间戳；`GET /infer/v1/jobs` 使用 `(created_at_ms, id)` 稳定
keyset cursor，每页最多 1,000 条，并始终由认证 App 作为首个过滤条件。单 Job 查询、
解释和取消同样先校验 App ownership，不能把不可猜测 ID 当作授权边界。

恢复原则：

- daemon 重启后，无法证明仍在运行的 Attempt 不得直接标记成功；
- 普通/interactive Job 的未完成 Attempt 标为 `unknown/interrupted`，所属 Job 进入确定的失败终态，不自动重放 provider 请求；
- interactive stream 不跨重启续流；只有调用方显式提交 `background: true`、非流式且 `local_only` 的 Responses Job 进入 durable 恢复合同；
- durable local background 保留原 Response ID 和 admission-time Candidate Plan；running Attempt 先记为 `interrupted`，新 Attempt 标记 `recovery`，同时受全局 Attempt budget、配置化 recovery replay 上限和原始 deadline 约束；
- config fingerprint 漂移、密钥不可用、payload 认证失败和 replay exhaustion 均 fail closed；普通请求、cloud 调用、音频与带外副作用不借此自动重放；
- 结果密文与 succeeded metadata 原子发布后才清除输入引用；失败/取消仅在终态持久化后回收输入，结果按 retention 过期；
- migration 必须前向执行、备份元数据并可检测不兼容版本。

Responses background 的公开生命周期为 `POST /v1/responses` + `background: true`、
`GET /v1/responses/{id}` 和 `POST /v1/responses/{id}/cancel`。runtime 在转发本地 provider
前移除 `background`，防止同时创建两个互不一致的 durable owner。

## 16. 可观测性

### 16.1 Metrics

- Job 数与终态，按 app/intent/priority/provider 分类；
- queue depth、queue wait、time-to-first-event、total latency；
- provider 请求、错误、重试、fallback 和熔断；
- token、估算/实际 USD、quota rejection；
- active reservations、stream buffer pressure；
- 后续的 RAM/VRAM、模型 load/unload latency 和 node heartbeat age。

高基数 ID 不进入 metric label；Job/Attempt ID 放在 logs/traces。

### 16.2 Decision explainability

每个 Attempt 至少记录：

- policy version；
- 候选入选原因；
- 关键落选候选及 reason code；
- 调度等待原因；
- fallback/retry trigger；
- 使用的是实际还是估算 usage。

`infer explain <job-id>` 应能把这些信息组织为人能读懂的决策链。

## 17. 推荐代码拓扑

采用 Rust Cargo workspace；crate 是部署与依赖边界，不追求一组件一 crate。建议初始保持少量粗粒度 owner：

```text
infer-runtime/
  crates/
    infer-auth/          managed credentials, secret file policy, App authentication
    infer-core/          domain types, state machine contracts, errors
    infer-control/       admission, policy, router, scheduler, quota orchestration
    infer-provider/      Provider SPI and shared protocol primitives
    infer-provider-responses/  shared Responses-compatible execution
    infer-provider-ollama/     narrow discovery/lifecycle extension
    infer-store/         persistence, ledger, migrations
    infer-payload/       authenticated encrypted background payload spool
    infer-api/           transport-facing API and event protocol
  apps/
    inferd/              composition root and daemon lifecycle
    infer/               client/admin CLI and local operator console
  sdk/                   only after wire contract stabilizes
  docs/
    adr/
```

边界说明：

- `inferd` 只做配置装配、生命周期和依赖注入，不成为业务逻辑中心；
- `infer console` 只是现有 operator API 的异步状态投影与可选子进程 owner；它不复制
  scheduler/resource 状态；credential 只由 `infer-auth` 解析并留在本机 Console 后端，
  browser renderer 不展示凭证，console 也不停止 attach 的外部 daemon。Operator API client、
  child supervisor、loopback Web presentation 与保留的 terminal fallback 分属独立 owner；
  Statistics 只保留会话滚动窗口，不能冒充持久监控或改变 daemon 权威计数，避免继续扩大
  CLI command router；
- `infer-auth` 独立拥有 managed credential 的生成、owner-only 文件、环境 credential 解析、
  secret 比较与 App identity 映射；配置只声明 source，`infer-control` 只持有解析后的认证表；
- `infer-control` 初期可作为一个 crate，但内部按 semantic owner 分模块。只有依赖、发布或测试生命周期真正独立时才继续拆 crate；
- `infer-payload` 独立，是因为加密格式、密钥、文件权限、retention 与 orphan reconciliation 拥有独立的安全/生命周期边界；后台恢复编排仍由 `infer-control::background_jobs` 负责，durable metadata 事务由 `infer-store::background` 负责；
- Provider adapter 独立，是因为协议、依赖、发布节奏和兼容性测试不同；
- ONNX artifact identity 因独立的内容寻址、权限、校验和原子发布生命周期放在
  `infer-artifact`；Session Registry 与 capability adapter 留在 `infer-provider::onnx`，
  `infer-core::vision` 只拥有类型化请求/结果，不让 `infer-control` 或 scheduler 解释 tensor；
- `node-agent` 在远程节点阶段再增加，不能用空 crate 预占未来；
- 避免 `common`、`utils`、`manager` 大杂烩；共享类型先确认唯一语义 owner。

该拓扑应用了 source-cohesion growth review：控制平面仍是 admission/execution 的粗粒度 owner；durable payload crypto 因独立安全生命周期提取为 crate，恢复与存储事务分别留在已有语义 owner。音频作为另一种 payload 生命周期和协议族，已分别提取到 `infer-core::audio`、`infer-provider::audio_worker` 与 API multipart owner。DeepSeek Flash 复用已经稳定的 Responses adapter；Codex App Server 因独立的进程生命周期、JSON-RPC、动态模型组和 Agent 能力收窄要求，落在 `infer-provider::codex_app_server` 独立 semantic owner，而不是继续扩大 HTTP adapter。Antigravity 复用 Provider SPI、模型组 DTO 与 subscription ACL，但其 CLI 用户会话、一次性请求 workspace、进程/NDJSON wire 和错误策略仍由 `infer-provider::antigravity_cli` 独立拥有；它明确披露真实 HOME 会话边界，不在 Runtime 内复制 credential，也不把 CLI history 冒充已隔离。第二个协议没有证明 JSON-RPC 与 CLI wire 应被抹平，因此不建立万能 Agent/CLI adapter。通用 Chat Completions owner 尚无第二个独立需求验证，因此不预占模块。ONNX 的制品发布、Session 生命周期、类型化视觉合同和模型专属语义分别落在 artifact/provider/core+API/adapter owner；新增边界对应真实权限、依赖和生命周期，未把 tensor 或图片 payload 引入核心 Job。远程 node 仍推迟到真实需求出现时提取。

## 18. 测试与验证策略

### 18.1 单元/属性测试

- Job 状态转换和终态竞争；
- hard constraint 永不被 fallback 放宽；
- quota reservation/settlement 守恒；
- priority aging 与队列容量；
- candidate ordering 的稳定 reason codes；
- error classification 和 retry eligibility。

### 18.2 合同测试

- 每个 Provider adapter 运行同一 Provider SPI 合同套件；
- 使用可脚本化 fake provider 验证 streaming、慢消费者、取消、超时、迟到响应和 malformed response；
- Responses adapter 使用版本化 capability profile 测试，不以单一供应商通过推断全部兼容；
- Codex bridge 使用 fake JSON-RPC 覆盖动态 model group、显式 admission、usage、malformed
  event、非推理 item fail-closed 和子进程 drop；本机可选 acceptance 才使用真实订阅；
- 官方 OpenAI SDK 对 `inferd` 运行 client-side compatibility tests，覆盖非流式、SSE 和明确 unsupported errors。
- 未来 ONNX provider 必须校验 artifact/export、tensor、preprocess、tokenizer/vocabulary、
  embedding-space 与 license identity，并覆盖 Session load/unload/reservation race、取消和迟到结果；
- Execution Provider fallback 必须披露 actual route/precision/reason，macOS/Windows 以任务级数值
  tolerance 和 decision stability 验收，不要求浮点逐位一致。

### 18.3 集成与故障测试

- Ollama 可选本机 smoke test；默认 CI 不要求 GPU；
- MLX 音频 worker 的协议测试不加载真实权重；本机 acceptance 覆盖 TTS → ASR → forced alignment 以及 VoiceDesign/voice clone；
- 并发预算下不超卖；
- daemon 在 queued/running/settling 各阶段崩溃后的恢复；
- durable local background 在 running 中断后保持同一 Response ID，Attempt 链明确记录 `interrupted` 与 `recovery`，spool/SQLite/WAL 不出现输入或结果明文；
- 对 durable result publication 和 input-reference retirement 注入 SQLite 事务失败，证明 metadata 引用未提交时 blob 绝不删除，故障修复后可恢复；
- 以至少 512 个 running background Job 连续模拟超过 replay 上限的 daemon restart，证明 reservation 单次结算、discard 引用完备且最终状态有界收敛；
- provider rate limit、断流、首 token 后失败；
- 订阅 Provider 覆盖未授权 App、model disappearance/upgrade drift、quota exhaustion、deadline/
  cancel、process crash，以及 local-only/offline 零触达；
- 隐私 Job 在本地失败时绝不触达云端 fake endpoint。
- 首个视觉 slice 必须用 fake provider 验证 `local_only`/`SensitiveBiometric` 零云触达、
  metadata/log 无像素/crop/embedding，以及 source/Recipe revision 失配时丢弃迟到结果；
- Shadow 只在类型化 wire schema、Build/preprocess identity 与 D-108 payload 合同稳定后进行真实
  consumer acceptance；共享 ONNX provider 不代表未验收的其他视觉 Intent 可用。SigLIP
  还需验证 image/text shared space、tokenizer identity、只允许同 space 比较，以及 Job/log
  中不包含图片、查询和向量。

### 18.4 性能门槛

在首个真实 consumer 的参考环境建立基线，分别衡量 runtime 与 provider：provider 有空闲容量时 admission-to-dispatch p95 ≤ 25 ms；首个 provider stream event 的额外转发延迟 p95 ≤ 20 ms；取消在 100 ms 内完成控制平面终态仲裁，并在 provider 支持取消时 2 s 内释放 reservation。隐私越界、预算超卖、重复终态和 reservation 泄漏均为零容忍正确性指标。

## 19. 版本与兼容性

- `/v1/responses` 兼容 profile、控制 API 和错误码显式版本化；
- consumer contract 以 `contracts/v0.1/openapi.json`、golden fixtures 和 daemon 的
  `/infer/v1/contract` 为共同身份；`0.1.0-candidate.1` 已冻结，当前
  `0.1.0-candidate.2` 也交给外部 consumer 后不可原地修改；
- Responses、文件音频、app-scoped Job 控制属于 candidate consumer surface；metrics、provider
  probe 和 resource lifecycle/eviction 属于 experimental operator surface，不共享稳定性承诺；
- capability schema 可新增可选字段，破坏性变更使用新版本；
- Provider SPI 在进程内初期不承诺第三方 ABI 稳定；
- 数据库 migration 与 daemon 版本绑定并记录 schema version；
- CLI 是 admin client，不直接读取或修改内部数据库。

## 20. MVP 范围切线

MVP **包含**：单机 `inferd`、无状态 provider execution 的 OpenAI Responses API 兼容文本协议、Ollama、本地 MLX 文件音频协议族、Intent/Model/Build/Deployment registry、流式文本、排队、三档优先级、policy 路由、受约束 fallback、基础 quota、取消、指标和 CLI 管理面。M4 在其上增加 runtime-managed、local-only 的 Responses background 生命周期，不把 durability 下推给 provider。

冻结的 v0.1 stable Consumer contract **不包含**：远程 Node、GPU 级抢占、通用模型下载、自动 benchmark 路由、低延迟原生增量 ASR、Vision/Embedding 稳定数据面、ONNX stable contract、Agent、第三方动态插件 ABI、HA、多用户/组织权限。candidate.2 新增 PCM TTS server-stream，并把 commit-redecode ASR duplex 明确列为 experimental route；这不等于承诺原生实时转写。ONNX P0 与 Web Console 同样作为 experimental surface 独立演进，不回填或扩大 v0.1 stable 范围。

## 21. 决策状态

启动实现所需的 `D-001` 至 `D-013` 以及 M4 payload ownership `D-105` 已接受，详见 [docs/DECISIONS.md](docs/DECISIONS.md) 及对应 ADR。D-012/D-013 只接受 experimental Codex text/image + SSE 和类型化音频 streaming 的收窄 slice，不接受 Agent surface、万能流协议或其他 CLI 的推定兼容。`D-101` 至 `D-104` 属于远程节点、跨节点大 payload、第三方扩展与 benchmark 后续阶段。`D-106` 至 `D-110` 已接受收窄的 ONNX foundation、同步 local-only face/SigLIP 与 QwenVL typed slices；视觉 durable、Windows tolerance、照片域质量与通用 Resource Pool 仍保留独立门槛。

本设计基线已进入实现：Git/Cargo workspace、Responses 垂直链路、本地文件音频垂直链路、受 Candidate Plan 约束的有界 retry/fallback、短期 provider 熔断、versioned capability profile/显式 probe、per-App admission、SQLite 持久化/预算，以及 M4 的 Ollama/ONNX inventory、Attempt reservation、显式生命周期控制与加密 local background restart recovery 均可运行。ONNX 另有内容寻址 artifact store、YuNet/SFace/SigLIP Builds、Session Registry 和四条可路由的 experimental typed data planes；Ollama QwenVL 另有有界描述/关键词与闭集分类复核两条 typed data planes；Codex App Server 另有动态模型组、subscription + cloud modality ACL、图像输入与文本 SSE；音频另有 PCM TTS server-stream 与 commit-redecode ASR duplex experimental slices。CI、自动资源长期启用、视觉/音频 stable promotion、Codex quota reconciliation、照片域质量、background batch/音频/cloud 幂等与远程节点仍待完成。
