# infer-runtime 外部应用接入指南

本文面向调用 `infer-runtime` 的应用开发者。它说明如何登记一个 App、取得调用凭证、发送
第一条文本或音频请求，以及在反馈测试中保留哪些诊断信息。

当前外部骨架合同为
[`infer-runtime.consumer-core@20260813.1`](CORE-CONTRACT-20260813.1.md)，能力集合由
[`infer-runtime.capability-catalog@20260813.1`](CAPABILITY-CATALOG-20260813.1.md) 独立声明。
机器可读字段、枚举和响应以
[Core OpenAPI](../contracts/consumer-core/20260813.1/openapi.json) 与 Catalog 引用的
[能力专属 schemas](../contracts/capabilities/) 为准。旧 candidate 文档只用于历史
审计；新 Consumer 不实现 candidate 分支。

## 1. 先分清两个地址和两类密钥

| 用途 | 默认地址 | 面向对象 |
| --- | --- | --- |
| inference API | Infra Discovery；显式 endpoint 仅用于开发诊断 | 外部应用与 `infer` CLI |
| Web Console | `http://127.0.0.1:8790` | 本机 operator 的管理界面 |

外部应用只连接 Discovery 选中的 inference endpoint，不调用 Web Console 的内部接口。请求中的 bearer token 用来让
`infer-runtime` 识别调用 App；它不是 DeepSeek、OpenAI 或其他 provider 的 API key。
Provider key 只由 daemon 在服务端读取，不得交给 consumer。

正式本机接入应按 [Consumer Discovery](CONSUMER_DISCOVERY.md) 选择
`infer-runtime.consumer-core@20260813.1` offer。显式 endpoint 配置可覆盖 Discovery，供开发与
诊断使用；产品接入不保留固定端口 fallback。
选择后，`GET /infer/v1/contract` 与所有受保护 Consumer 数据/控制请求都必须恰好发送一个
`Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1`。Runtime 固定返回的
`core_contract` 与单元素 `supported_core_contracts` 必须与 Discovery offer 交叉一致。缺少、重复、
裸日期或旧 candidate 会返回 426，`error.code=consumer_core_unsupported`。
类型化业务请求还必须发送 Catalog 中的精确 `Infer-Capability-Contract`；SDK 会自动完成这两层
握手。缺少或错误能力身份返回 `426 capability_contract_unsupported`。

## 2. 为 consumer 登记独立 App

不要把 `local-operator` token 交给产品应用。它拥有资源管理权限。推荐在 Web Console 的
「Apps 与访问」页面创建 Consumer：选择权限预设或精确约束后，runtime 会生成一个 managed
token，并且只在创建成功时展示一次。把它立即保存到 Consumer 自己的 secret store，然后
按页面提示重启 `inferd`。

若 Consumer 已有自己的 secret provisioning，也可以继续在本机 `config/infer.toml` 中登记环境
变量型身份：

```toml
[apps.sample-consumer]
credential = { source = "environment", variable = "SAMPLE_CONSUMER_INFER_TOKEN" }
resource_admin = false
allowed_intents = ["text.summarize", "audio.transcribe"]
max_pending_jobs = 16
default_policy = "balanced"
allowed_policies = ["balanced", "local-first"]

[apps.sample-consumer.request_overrides]
priority = ["interactive", "normal", "background"]
placement = ["local_only", "private", "anywhere"]
prefer = ["local", "trusted_node", "cloud"]
offline_required = true
capability_floor = ["foundational", "capable", "advanced", "expert"]
latency = ["interactive", "balanced", "throughput"]
fallback = ["none", "equivalent"]
max_cost_usd = { min = 0.0, max = 1.0 }
```

为 `SAMPLE_CONSUMER_INFER_TOKEN` 生成高熵随机值，把同一个值分别放入 daemon 的启动环境和
consumer 自己的 secret store。面板创建的 managed token 则保存在 owner-only credential
file 中，不需要 daemon 环境变量。两种模式都不要把真实值写进 TOML、源码、日志或反馈报告。配置中的
`allowed_intents` 是 App 能提交的稳定 workload 清单；未知项会使配置校验失败，已知但未授权
的 Intent 会在创建 Job、占用 admission slot 或调用 provider 前以 `403 intent_forbidden`
拒绝。完全省略该字段与显式空数组相同，均禁止该 App 提交推理。受保护的 `local-operator`
只有在 `resource_admin=true` 且显式 `allow_all_intents=true` 时才跟随完整 Intent registry；不能把
它的 token 交给产品 Consumer。云端 text/image payload 也都必须经
`allowed_cloud_input_modalities` 显式授权。`request_overrides` 是该 App 可申请的约束上限；没有获准的 `infer.*` 值会
被拒绝，而不是静默降级。

凭证和配置在 daemon 启动时读取。修改后先在 Web Console 的配置页校验，再显式重启
daemon。若不用 Console，直接以前台方式启动；严格配置校验失败时进程会拒绝启动：

```bash
SAMPLE_CONSUMER_INFER_TOKEN='<从 secret store 注入>' \
  cp config/infer.example.toml config/infer.toml
  cargo run -p inferd -- --config config/infer.toml
```

面板不会列出现有 token 明文。managed token 只有创建或轮换响应显示一次；撤销会移除 App
登记和 managed credential。运行中的 daemon 在重启前仍持有旧认证表，因此上述三种变更
都必须完成一次显式重启。

## 3. 确认合同与服务身份

下面四个只读入口不需要凭证。`INFER_BASE_URL` 必须来自 SDK/Discovery 或显式开发 override：

```bash
curl "$INFER_BASE_URL/health"
curl -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' "$INFER_BASE_URL/infer/v1/contract"
curl -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' "$INFER_BASE_URL/infer/v1/capabilities"
curl "$INFER_BASE_URL/infer/v1/openapi.json"
```

Consumer 应在诊断信息中记录 `/infer/v1/contract` 返回的 `core_contract` 与 Job 的
`capability_contract`；operator 还应
另行提供部署所用的 daemon Git commit。不要只根据产品版本猜测 wire contract。

## 4. 发出第一条文本请求

文本使用 OpenAI Responses 风格的无状态子集。`model` 填稳定 Intent，而不是 Ollama tag
或云端物理模型名：

```bash
curl "$INFER_BASE_URL/v1/responses" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.responses@20260812.1' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "text.summarize",
    "input": "需要总结的内容",
    "metadata": {
      "infer.placement": "local_only",
      "infer.priority": "normal"
    }
  }'
```

常用 Intent 包括 `text.summarize`、`text.proofread`、`text.edit`、`text.deduplicate`、
`language.respond` 和 `reasoning.solve`。`text.deduplicate` 只适合 Consumer 提供的有界记录
同一性匹配；Consumer 必须验证返回标识，并在调用失败或输出不合法时 fail open。它不能替代
价值、兴趣、事实或偏好判断。可用的 Intent 及默认路由以运行配置和合同为准。`infer.placement`、
`infer.capability_floor`、`infer.fallback` 等是约束。Core 提供经 App routing ACL 授权的
`infer.deployment_ids` 或 `infer.model_profile_ids` 作为可选具名硬收窄；两者互斥，值为最多 16 个
有序、唯一的 Runtime ID。`model` 仍然只能是 Intent，不能传 provider-native physical model。

普通 HTTP client 必须显式处理两层合同；产品代码优先使用官方 SDK：

```javascript
const response = await fetch(`${inferBaseUrl}/v1/responses`, {
  method: "POST",
  headers: {
    Authorization: `Bearer ${inferToken}`,
    "Infer-Consumer-Contract": "infer-runtime.consumer-core@20260813.1",
    "Infer-Capability-Contract": "infer.responses@20260812.1",
    "Content-Type": "application/json",
  },
  body: JSON.stringify({
    model: "language.respond",
    input: "解释这张卡片里的错误",
    metadata: { "infer.placement": "local_only" },
  }),
});

const body = await response.json();
if (!response.ok) {
  throw new Error(`${response.status}: ${body.error?.code ?? "unknown_error"}`);
}
```

程序应根据 HTTP status 与 `error.code` 分支，不要解析 `error.message`。响应中未来可能增加
字段，consumer 必须忽略未知响应字段。未知请求字段则会严格返回 400。

### Streaming

设置 `"stream": true` 后响应为 `text/event-stream`：

```bash
curl -N "$INFER_BASE_URL/v1/responses" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.responses@20260812.1' \
  -H 'Content-Type: application/json' \
  -d '{"model":"language.respond","input":"你好","stream":true}'
```

客户端取消 HTTP/SSE 连接后仍应按正常取消路径处理。不要假设所有 provider 都产生完全相同
的扩展事件；只依赖所选 capability schema 明确保证的事件与字段。

### Local background

只有 operator 显式启用加密 background spool 后，consumer 才能提交非流式、
`local_only` 的文本 background Job：

```http
POST /v1/responses                     { "background": true, ... }
GET  /v1/responses/{response_id}
POST /v1/responses/{response_id}/cancel
```

收到 `background_unavailable` 时应视为功能未启用，不要自动改成 cloud 请求。

## 5. 接入本地音频能力

音频与文本共享鉴权、Job、排队、取消和审计控制面，但不强行套入 Responses JSON。它按任务
使用类型化 endpoint：

| Intent | Endpoint | 请求形态 | 结果 |
| --- | --- | --- | --- |
| `audio.transcribe` | `POST /v1/audio/transcriptions` | multipart | JSON 或 text |
| `audio.detect_events` | `POST /v1/audio/event-detections` | multipart | AudioSet events + coverage/evidence/provenance JSON |
| `audio.embed` | `POST /v1/audio/embeddings` | multipart | bounded audio's 512d local retrieval evidence |
| `audio.embed_text_query` | `POST /v1/audio/text-embeddings` | strict JSON | paired text-query 512d retrieval evidence |
| `audio.transcribe`（实验流） | `GET /v1/audio/transcriptions/stream` | WebSocket PCM + control JSON | revisioned partial/final JSON |
| `audio.align` | `POST /v1/audio/alignments` | multipart | JSON timestamps |
| `speech.synthesize` / `speech.design_voice` | `POST /v1/audio/speech` | JSON | 完整音频或 streamed PCM bytes |
| `speech.clone_voice` | `POST /v1/audio/voice-clones` | multipart | 音频 bytes |

`audio.embed` / `audio.embed_text_query` 是独立的 experimental CLAP retrieval space，不是 transcript、
AudioSet event 或 SigLIP text embedding 的替代品。两个 endpoint 均强制 local-only、offline 和
no-fallback；音频最多 25 MiB 且首个 Build 限为 10 decoded seconds。`source_revision` 或
`query_revision` 必填并原样回显。`language` 只是 text query 的 BCP-47-shaped evidence，不能被
Consumer 用作模型已支持该语言的质量声明。仅在 capability catalog 宣告
`infer.audio.embedding@20260815.2`、App 有两个相应 Intent ACL 且 exact Build 已 ready 时调用。
`language=en` 直接进入 CLAP text tower；`zh`/`zh-*` 只会走 Runtime-owned local short-query
normalizer，并在响应 `query_normalizer` 回显 `ollama_qwen3_5_2b`、`qwen3_5_2b_mlx`、
`infer.audio.zh-en-short-query@20260815.1` 和 `zh`→`en`。其它语言、normalizer 不可用或其输出不满足
English-only bounded contract 时返回稳定 provider-unavailable 类错误；Consumer 应保留既有搜索，不应发送
raw Chinese 给 CLAP。

转写示例：

```bash
curl "$INFER_BASE_URL/v1/audio/transcriptions" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.audio.transcription@20260814.1' \
  -F model=audio.transcribe \
  -F file=@sample.wav \
  -F language=Chinese \
  -F response_format=verbose_json
```

`infer.audio.transcription@20260814.1` keeps `language` as one unambiguous,
document-level language only. Mixed-language input returns `language: null`
together with typed `language_evidence`: `kind=input_set` is a provider-reported
whole-input language set and deliberately has no invented time boundaries;
`kind=segments` is only emitted when the provider supplies interval evidence.
Consumers must use `language_evidence` rather than treating a null scalar as
an absence of speech or of language evidence.

TTS 示例：

```bash
curl "$INFER_BASE_URL/v1/audio/speech" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.audio.speech@20260811.1' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "speech.synthesize",
    "input": "你好，这是 infer-runtime。",
    "voice": "speech.voice.zh.bright_female.v1",
    "language": "Chinese",
    "response_format": "wav"
  }' \
  --output speech.wav
```

上传单文件上限当前为 25 MiB。Multipart 字段拼写、重复字段和 MIME/格式错误都会严格失败。
语音生成响应的 `x-infer-job-id` 与 `x-infer-model` header 可用于诊断；不要把音频 payload
写入通用日志。

### 声音事件检测

Rust Consumer 使用官方 `infer-runtime-client` 的
`Client::detect_audio_events_file(path, content_type, metadata)`；该方法先通过 Infra Discovery
选择 Core，再读取 Catalog、校验能力 OpenAPI digest，并只接受
`infer.audio.event-detection@20260813.2`。不要从 Echo 自行调用 worker、传模型路径或复制
YAMNet 解析逻辑。

请求 metadata 必须保持 `infer.placement=local_only`、`infer.offline_required=true`、
`infer.fallback=none`，Echo 还固定 `background`、零成本与 local-first。Runtime 返回的
`events` 和 `speech_presence` 是两个独立证据字段：合法的空 `events=[]` 不自动表示无人声；
只有 `coverage.status=full` 且 speech-family score 不高于响应内版本化阈值时，SDK 才接受
`speech_presence.status=absent`。partial/none coverage 必须保留 `unknown`。

每个事件使用 AudioSet MID `class_id`、展示 `label`、秒级闭开分析区间和 raw sigmoid score；
Consumer 应同时保存 Job id、Capability identity、ontology/policy/provenance revision 作为证据，
不要只保存 label。未知 response 字段可向前兼容，但现有强类型字段缺失、阈值矛盾或事件超出
coverage 时必须拒绝。

`speech.synthesize` 的 `voice` 是 Runtime 逻辑别名，不是 provider 原生 speaker 字符串。目录
revision `infer.speech.voice-aliases@20260811.1` 当前发布
`speech.voice.zh.bright_female.v1`，合同用途是普通中文旁白/对话，并要求显式
`language=Chinese`。alias 的角色、保证语言或物理映射发生语义变化时必须发布新版本；现有
alias 不会被静默重定义。获得该 Intent 不会同时获得 VoiceDesign、VoiceClone 或录音引用能力。
对需要稳定产品合同的 App，配置
`allowed_speech_voice_aliases = ["speech.voice.zh.bright_female.v1"]`；该 App 传入任何 provider
原生 speaker 字符串都会在创建 Job 前被拒绝。省略 allowlist 只用于兼容尚未迁移到 Runtime 逻辑别名的
调用方。

### TTS server stream（experimental）

同一 speech endpoint 使用显式执行模式，不另造模型专属接口。首个 streaming codec 固定为
`pcm_s16le`，所以必须同时使用 `response_format=pcm`：

```bash
curl --no-buffer "$INFER_BASE_URL/v1/audio/speech" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.audio.speech@20260811.1' \
  -H 'Content-Type: application/json' \
  -d '{
    "model":"speech.synthesize",
    "input":"这段音频会边生成边返回。",
    "voice":"speech.voice.zh.bright_female.v1",
    "language":"Chinese",
    "response_format":"pcm",
    "execution_mode":"server_stream"
  }' --output speech.pcm
```

开始读取 body 前先保存 `x-infer-audio-format`、`x-infer-sample-rate-hz`、
`x-infer-channels`。body 是 append-only 原始 PCM，不含 WAV header；断流后不可把不同 Job 的
chunks 拼接。取消 HTTP 读取会走 Job cancel/Attempt 终态与 reservation 释放。

### Duplex ASR（experimental）

`GET /v1/audio/transcriptions/stream` 使用 bearer header 升级 WebSocket。首个文本帧必须是：

```json
{
  "type": "session.configure",
  "session": {
    "model": "audio.transcribe",
    "input_audio_format": "pcm_s16le",
    "sample_rate_hz": 16000,
    "channels": 1,
    "language": "Chinese",
    "metadata": {
      "infer.placement": "local_only",
      "infer.priority": "interactive",
      "infer.fallback": "none"
    }
  }
}
```

之后二进制帧只包含完整 PCM sample frames。文本控制帧为
`{"type":"input_audio.commit"}`、`{"type":"input_audio.finish"}` 或
`{"type":"session.cancel"}`。服务端依次发 `session.created`、零或多个
`transcript.partial`、一个 `transcript.final` 和 `session.completed`；错误为
`{"type":"error","error":{"code":"..."}}`。partial 的 `revision` 单调增加并完整替换上一
partial，只有 final 可按终态保存。

当前 MLX Qwen3-ASR 不是已验证的原生增量 decoder。Runtime 会在 commit 时重解码当前完整前缀，
并明确返回 `transcription_mode=commit_redecode`、`stream_semantics=revisable`。它可以用于边听边
修订验证，但长会话计算量会增长；Consumer 不应把 duplex transport 当成固定实时延迟保证。

## 6. 试用实验性本地人脸、主体蒙版与面部区域能力

ONNX P0 提供刻意收窄、独立版本化的 experimental capability；外部接入时
必须单独固定 daemon commit，并接受在 stable promotion 前可能调整 schema：

| 项目 | 当前 wire contract |
| --- | --- |
| Base URL | SDK / Infra Discovery 选择的 endpoint |
| 检测 | `POST /infer/v1/vision/face-detections` |
| 单脸向量 | `POST /infer/v1/vision/face-embeddings` |
| Error discriminator | HTTP status + JSON `error.code`；不要解析 `error.message` |
| 响应扩展 | Consumer 必须忽略未知响应字段；multipart 的未知/重复请求字段严格失败 |

`source_revision` 必填且最多 256 个 UTF-8 bytes。它应稳定标识本次提交的确切像素制品，重试
时保持不变，像素、尺寸或 orientation 归一化结果改变时必须改变。Shadow 推荐使用不含本机
路径的格式，例如 `shadow:<photo-uuid>/recipe:<revision>/artifact:<sha256>`；完整值需保持在
256 bytes 内。Runtime 只绑定并回显该值，stale-result 判断仍由 Consumer 负责。

```bash
curl "$INFER_BASE_URL/infer/v1/vision/face-detections" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.vision.face-detection@20260811.1' \
  -F model=vision.detect_faces \
  -F source_revision='catalog-item-42/revision-7' \
  -F image=@sample.jpg\;type=image/jpeg
```

请求只接受 JPEG/PNG，encoded image 上限 20 MiB，decoded pixel 上限 40M。Runtime 强制
`local_only`、`offline_required=true`、`fallback=none`；调用方若试图通过 metadata 放宽
任一项会得到明确错误。普通 App 不能传模型文件路径、tensor 名或 Execution Provider。

响应返回 `detections[].bounding_box`、五点 `landmarks`、`confidence`，以及 Job/Attempt、Build、
Deployment、实际 Execution Provider、fallback reason、preprocess identity 和原始
`source_revision`。当前 macOS 实测实际 route 为 CPU；请求的 Core ML 无法完整接管该图时，
只会在 Build 明确允许后回退并披露，不会伪装成 Core ML 成功。

`image.orientation` 当前精确值是 `input_pixels_no_exif_transform`：`image.width`、
`image.height`、bounding box 和五点都属于上传文件解码出的原始 raster，坐标原点在左上角，
`x` 向右、`y` 向下；Runtime 不应用 JPEG EXIF orientation。Consumer 不得把它解释为相机
orientation 已归一化后的坐标。若 Consumer 在缓存 JPEG/PNG 前主动旋转像素，则坐标自然属于
旋转后写入的新 raster，并应使用对应的新 `source_revision`。未来若收到未知 orientation 值，
Consumer 应拒绝持久化坐标，而不是猜测变换。

像素不会进入通用 Job metadata/log，也不会 cloud fallback。Consumer 仍负责把
`source_revision` 与当前 Catalog/Recipe 对照，只发布仍然匹配的结果；迟到结果必须丢弃。
检测结果的命名五点可直接作为 SFace 输入，坐标必须仍属于完全相同的 cached image bytes
所解码出的 raster；首版按人脸逐次调用，不提供 batch endpoint：

```bash
curl "$INFER_BASE_URL/infer/v1/vision/face-embeddings" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.vision.face-embedding@20260811.1' \
  -F model=vision.embed_face \
  -F source_revision='catalog-item-42/revision-7' \
  -F 'landmarks={"right_eye":{"x":120.1,"y":92.4},"left_eye":{"x":164.8,"y":91.9},"nose_tip":{"x":143.0,"y":116.2},"right_mouth_corner":{"x":126.7,"y":139.5},"left_mouth_corner":{"x":160.2,"y":139.1}}' \
  -F image=@sample.jpg\;type=image/jpeg
```

Runtime 负责固定的 112×112 SFace similarity alignment，不接受调用方自对齐 crop。响应的
`embedding` 包含 128 个 L2-normalized values、`distance_metric=cosine` 与绑定 Build digest/
postprocess identity 的 `space`；`eligibility` 给出五点是否在图内、眼距和 alignment RMSE。
不同 `space` 的向量不得直接比较。

该响应标记 `data_classification=sensitive_biometric`。像素和向量不会进入通用 Job metadata、
默认日志或 durable spool；Consumer 若要保存 embedding，必须自行执行本地生物特征政策、
retention 和删除语义。为对应 App 的 `allowed_intents` 显式加入 `vision.embed_face` 后才可调用。

官方 SDK 另外提供两个相同安全边界的 typed 方法：

- `segment_subject(...)` 对应 `vision.segment_subject` 与
  `infer.vision.subject-segmentation@20260813.1`。输入为 1–16 个归一化前景/背景点击；可选 box
  占两个 prompt slot。返回与输入同尺寸、仅含 0/255 的 PNG mask、摘要和完整 provenance。
- `segment_subject_soft_mask(...)` 对应同一 Intent 和新增的
  `infer.vision.subject-segmentation-soft-mask@20260814.1`。输入合同完全相同，但返回 SAM
  原生 256×256 Gray8 sigmoid probability PNG；`input_coordinate_extent` 与
  `raster_extent.coordinate_mapping=linear_full_extent_pixel_centers_v1` 规定它如何映射回提交的
  display raster。Consumer 自己决定 feather、opacity 和持久化，Runtime 不保存像素或 mask。
- `parse_face(...)` 对应 `vision.parse_face` 与 `infer.vision.face-parsing@20260813.1`。输入为
  orientation-normalized display raster 上的 YuNet face box；Runtime 固定扩张 1.8 倍上下文，
  返回全图尺寸的 19-class indexed PNG label map。结果标记为 `sensitive_biometric`。

这些 route 只表示 YuNet/SFace、SAM 2.1 与 BiSeNet typed slice 可试用；它们不开放视觉
background、cloud fallback、持久 artifact reference、任意模型路径或通用 tensor API。
SigLIP 语义向量使用下面独立的数据合同。

## 7. 试用 SigLIP 2 图文语义向量

SigLIP 2 纵向切片把图像和文本编码为同一个语义空间，供 Consumer 自己建立索引和做
文本检索。Runtime 不持有图库、索引、查询历史或 stale-result 事实：

| Intent | Endpoint | 请求 |
| --- | --- | --- |
| `semantic.embed_image` | `POST /infer/v1/vision/image-embeddings` | multipart JPEG/PNG |
| `semantic.embed_text` | `POST /infer/v1/vision/text-embeddings` | strict JSON |

图像必须由 Consumer 预先完成 EXIF/orientation 归一化并编码为 display-sized JPEG/PNG。当前
合同要求 `image_orientation=display_pixels_orientation_normalized`，Runtime 不再应用 EXIF
变换；响应的 `image.width`/`height` 指向提交的归一化 raster。`source_revision` 必填、最多
256 个 UTF-8 bytes，必须稳定标识这份确切像素制品：

```bash
curl "$INFER_BASE_URL/infer/v1/vision/image-embeddings" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.vision.image-embedding@20260811.1' \
  -F model=semantic.embed_image \
  -F source_revision='shadow:photo-42/recipe:7/artifact:abc123' \
  -F image_orientation=display_pixels_orientation_normalized \
  -F image=@display.jpg\;type=image/jpeg
```

文本请求的 `text` 为 1–4096 个 UTF-8 bytes，`query_revision` 必填且最多 256 bytes；
`language` 可省略，提供时必须是最长 35 bytes 的 BCP-47 形状标签。当前 Build 按模型合同
先 lower-case，再用固定 tokenizer 截断/填充为 64 tokens：

```bash
curl "$INFER_BASE_URL/infer/v1/vision/text-embeddings" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.vision.text-embedding@20260811.1' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "semantic.embed_text",
    "text": "海边日落",
    "query_revision": "shadow:semantic-query:v1",
    "language": "zh-CN"
  }'
```

两种响应的 `embedding` 均包含 768 个有限、L2-normalized `values`，以及
`dimensions=768`、`distance_metric=cosine` 和完全相同的 `space`。只有 `space` 完全相等的
图像和文本向量才能比较；Consumer 应持久化 space 并在 Build/space 变化时重建索引。
`provenance` 同时绑定 Build、主图 artifact、preprocess/postprocess、tokenizer、runtime、
requested/actual EP、precision 和 fallback reason。Consumer 必须忽略未知响应字段；请求
字段严格，稳定失败分支仍使用 HTTP status + `error.code`。

当前 exact Builds 为：

- image: `siglip2_base_patch16_224_image_onnx_cpu_v1`；
- text: `siglip2_base_patch16_224_text_onnx_cpu_v1`；
- shared space: `siglip2_base_patch16_224@75de2d55:fixres224:lowercase64:l2_768_fp32:v1`。

当前验证组合固定 `requested_execution_provider=cpu`、`actual_execution_provider=cpu`。
这不是声称 Core ML 永久不适用，而是当前 graph/ORT 组合不能完整接管，避免每次请求先做昂贵
失败探测再回退。Windows 与其他 EP 必须以新 Build/平台证据通过 numerical tolerance 与
检索 decision-stability 门槛，不能直接复用本机结论。

两条路由都强制 `local_only`、`offline_required=true`、`fallback=none`。图片、文本和向量不
进入普通日志、通用 Job metadata 或文本 durable spool。本轮没有视觉 durable background；
十万张图库的 checkpoint、恢复、限速、重试、source revision/stale-result 仲裁和 Catalog
发布全部由 Shadow 持有，Runtime 只负责单请求 Job/Attempt、背压、公平性、取消和 provenance。

## 8. 试用 Qwen 高级图片理解（experimental）

这组接口把本机 VLM 封装成两个职责分离的 typed capability；Consumer 不依赖 Ollama chat
schema，也不传物理模型名：

| Intent | Endpoint | 结果 |
| --- | --- | --- |
| `vision.describe_image` | `POST /infer/v1/vision/image-descriptions` | 短描述 + 自由关键词 proposal |
| `vision.classify_closed_set` | `POST /infer/v1/vision/classification-reviews` | 闭集中的 `matched`，或 `none` / `uncertain` |

两者都是严格 multipart 请求，必填 `model`、`source_revision`、
`image_orientation=display_pixels_orientation_normalized` 和名为 `image` 的 JPEG/PNG。图片最多
20 MiB，解码后最多 40,000,000 pixels；Runtime 不接受文件路径、URL、RAW 或未做 orientation
normalization 的像素。`source_revision` 最多 256 UTF-8 bytes，必须稳定标识这份确切 raster。

描述还要求最长 35 ASCII bytes 的 BCP-47-shaped `language`：

```bash
curl "$INFER_BASE_URL/infer/v1/vision/image-descriptions" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.vision.image-description@20260811.1' \
  -F model=vision.describe_image \
  -F source_revision='shadow:photo-42/recipe:7/artifact:abc123' \
  -F image_orientation=display_pixels_orientation_normalized \
  -F language=zh-CN \
  -F infer.priority=background \
  -F infer.capability_floor=foundational \
  -F infer.placement=local_only \
  -F infer.offline_required=true \
  -F infer.fallback=none \
  -F image=@display.jpg\;type=image/jpeg
```

`result.description` 最多 1,024 UTF-8 bytes；`result.keyword_suggestions` 最多 16 项，每项最多
128 bytes，并在大小写折叠后唯一。它们只是 assistant proposal，Runtime 不写用户关键字，
也不把模型生成内容当成用户接受记录。

分类复核另外要求 `taxonomy_revision` 与 JSON-string `categories`。类别数为 1–64；整个 JSON
最多 64 KiB；每项只允许 `id`、`name` 和可选 `description`，未知字段会拒绝：

```bash
curl "$INFER_BASE_URL/infer/v1/vision/classification-reviews" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.vision.classification-review@20260811.1' \
  -F model=vision.classify_closed_set \
  -F source_revision='shadow:photo-42/recipe:7/artifact:abc123' \
  -F image_orientation=display_pixels_orientation_normalized \
  -F taxonomy_revision='shadow:categories:9' \
  -F 'categories=[{"id":"travel","name":"旅行"},{"id":"food","name":"食物"}]' \
  -F infer.priority=interactive \
  -F infer.capability_floor=capable \
  -F infer.placement=local_only \
  -F infer.offline_required=true \
  -F infer.fallback=none \
  -F image=@display.jpg\;type=image/jpeg
```

`suggestion.disposition=matched` 时必须带请求闭集内的 `category_id`；`none` / `uncertain` 必须
省略它。响应不提供伪校准 confidence，Consumer 也不能把这个 proposal 冒充用户 feedback。
公共响应可增加字段，Consumer 必须忽略未知响应字段；请求始终严格。错误继续按 HTTP status +
`error.code` 处理，不解析诊断 message。

当前 4B Build 在两条 Intent 上评级 `foundational`、resource class 为 `standard`；8B Build 评级
`capable`、resource class 为 `heavy`。描述默认 `foundational`，闭集复核默认 `capable`；Consumer 可为
明确复核请求使用 `infer.capability_floor=capable`，但不得提交 Ollama tag。响应 `provenance` 会披露实际
provider、deployment、model profile/build、physical model、runtime、schema/prompt revision 与
可用的 native timing。

两条路由都强制 `local_only`、`offline_required=true`、`fallback=none`。`background` 只是调度
优先级，不表示请求可跨重启恢复；Shadow 等 Consumer 继续拥有扫描 checkpoint、resubmit、
source revision/stale-result 仲裁与用户接受链路。图片、闭集、描述和关键词不进入普通日志、
通用 Job metadata、audit details 或文本 durable spool。Ollama provider 当前共享有界并发与
模型 residency；运行中取消会中止 HTTP body 读取并释放 Attempt/reservation，但 provider
原生计算不承诺硬抢占，迟到结果不会发布为第二个终态。

App 必须只为实际需要显式加入 `vision.describe_image` 和/或
`vision.classify_closed_set`；不需要 `resource_admin`，也不因此获得其他视觉 Intent。

## 9. 试用 Codex 订阅模型组（experimental）

Codex App Server 在 Runtime 中是一个 `placement=cloud`、`access_class=subscription` 的
Provider；本机 stdio 只是 transport。一个登录会话当前发现 Sol、Terra、Luna 等多个上游模型，
但 Consumer 仍然请求 Intent，不得提交这些物理模型名。Runtime 按静态准入的 Deployment 路由，
不会因为 `model/list` 出现新模型就自动开放。

普通 App 默认只有 `standard` access class。需要使用订阅 Provider 的 App 必须由 operator 在
配置中显式授权，同时保留最小 Intent 与 cloud placement 上限：

```toml
[apps.sample-advanced-consumer]
credential = { source = "managed" }
resource_admin = false
allowed_intents = ["language.respond", "reasoning.solve", "image.generate"]
allowed_provider_access_classes = ["standard", "subscription"]
allowed_cloud_input_modalities = ["text"]
default_policy = "capability-first"
allowed_policies = ["capability-first"]

[apps.sample-advanced-consumer.request_overrides]
priority = ["interactive"]
placement = ["cloud_only"]
prefer = ["cloud"]
capability_floor = ["expert", "exceptional"]
fallback = ["none"]
```

bridge 支持非流式或 SSE 文本输出、字符串 `instructions` 和 `reasoning.effort`；除下述独立
`image.generate` 合同外，不支持 tools、sampling、`max_output_tokens`、普通 metadata
passthrough、conversation 或 durable background：

```bash
curl "$INFER_BASE_URL/v1/responses" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.responses@20260812.1' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "reasoning.solve",
    "input": "比较两个方案并给出简洁结论。",
    "reasoning": {"effort": "low"},
    "metadata": {
      "infer.policy": "capability-first",
      "infer.priority": "interactive",
      "infer.placement": "cloud_only",
      "infer.provider_access_class": "subscription",
      "infer.capability_floor": "expert",
      "infer.fallback": "none"
    }
  }'
```

图片请求使用独立 `multimodal.respond` Intent，且 App 必须把 `image` 加入
`allowed_cloud_input_modalities`。获得 subscription 访问权并不会自动允许图片外发。输入采用
Responses parts；只接受 HTTPS image URL 或有界 JPEG/PNG data URL，本地文件路径不会被接受：

```json
{
  "model": "multimodal.respond",
  "stream": true,
  "input": [{
    "role": "user",
    "content": [
      {"type": "input_text", "text": "简洁描述图片。"},
      {"type": "input_image", "image_url": "data:image/jpeg;base64,<base64>"}
    ]
  }],
  "reasoning": {"effort": "low"},
  "metadata": {"infer.placement": "cloud_only", "infer.fallback": "none"}
}
```

单图上限 20 MiB，单请求最多 8 图、合计最多 40 MiB；调用前 Runtime 还会确认动态
`model/list` 对实际物理模型声明了 image input。App 缺少 cloud image egress 时，候选解释记录
`cloud_input_modality_not_allowed`，不会启动 Codex Attempt。

`image.generate` 是另一条输出图片的实验合同，不是普通 Codex tool 代理。它仍使用
`POST /v1/responses`，首版必须且只能传一个无附加字段的
`{"type":"image_generation"}` tool；输入只允许文本，输出恰好一张 Base64 PNG：

```bash
curl "$INFER_BASE_URL/v1/responses" \
  -H "Authorization: Bearer $SAMPLE_CONSUMER_INFER_TOKEN" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.responses@20260812.1' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "image.generate",
    "input": "一枚简洁的蓝色圆形图标，白色背景，不含文字。",
    "tools": [{"type": "image_generation"}],
    "metadata": {
      "infer.provider_access_class": "subscription",
      "infer.max_cost_usd": "0",
      "infer.fallback": "none"
    }
  }'
```

成功响应的 `output[0].type` 是 `image_generation_call`，`result` 是原始 Base64 PNG；不要把
该字段写入日志或 Job metadata。Runtime 在返回前验证 Base64、PNG header、20 MiB decoded
上限、最大 4096 边长和 16,777,216 pixels；不会跟随 App Server 的 `savedPath`。该纵切只支持
单张、unary、text-to-image；stream、durable background、图片输入/编辑、tool 参数以及任何第二
种 tool 都会失败。调用前 adapter 还会读取 `modelProvider/capabilities/read`，当前登录/provider
未报告 `imageGeneration=true` 时不会启动生成 turn。

因为输入只有文本，`image.generate` 不需要 cloud image input egress 权限；但 App 仍必须显式
允许该 Intent 和 `subscription` access class。现有 `symbiont-d` 是首个获得该 Intent 的普通
Consumer，继续保持 `resource_admin=false` 与单次 `max_cost_usd=0` 上限。

如果 App 没有 subscription 授权，候选解释会记录 `provider_access_not_allowed`；请求不会启动
Codex 子进程或消费订阅。`local_only` 和 `offline_required=true` 也始终排除这组模型。

如果同一个 App 同时获准使用 standard API 与 subscription bridge，可在单次请求中使用
`infer.provider_access_class=standard|subscription` 做硬性收窄。它只减少候选，不会扩大 App 的
`allowed_provider_access_classes`；越权值会在创建 Job、进入队列或启动 Provider 前被拒绝。

模型能力与推理投入是两个维度：`infer.capability_floor` 决定候选模型至少达到哪个能力 level，
`reasoning.effort` 决定选中模型本次投入多少计算。默认模板使用 `capability_fit` 选择最低充分 level；
显式 `capability-first` 才使用 `capability` 追求最强合格模型。公共 effort 顺序为
`none|low|medium|high|xhigh|max|ultra`，但每个 Deployment 只接受自己明确声明的子集。

operator 可读取动态模型组；`admitted=false` 只表示发现，不能路由：

```http
GET /infer/v1/providers/codex-subscription/models
```

Codex Web Search 使用标准 Responses request 形状，不增加 `web.research` 一类模型别名：

```json
{
  "model": "language.respond",
  "input": "查询当前信息并给出简洁结论。",
  "tools": [{"type": "web_search", "external_web_access": true}],
  "tool_choice": "auto",
  "metadata": {
    "infer.provider_access_class": "subscription",
    "infer.placement": "cloud_only",
    "infer.fallback": "none"
  }
}
```

这个 Codex experimental extension 沿用标准 Responses 形状，当前接受
`tool_choice = auto|required|none`，以及单个 `web_search` tool 的 `external_web_access` 布尔值。
它是 `infer.responses@20260812.1` 中的有界 hosted-tool 子集；获得 Intent 或 subscription access
并不自动授权 Web Search，App 仍必须显式取得 `allowed_builtin_tools=web_search`。domain filter、
位置、图片结果和 object-form tool choice 留到独立合同版本。`false` 映射
Codex cached search，缺省/`true` 映射 live search；`required` 会在返回前验证至少出现一个
`webSearch` item，`none` 则为该 Attempt 强制关闭搜索且不要求 Web Search Provider/Build 能力或
App hosted-tool 权限。unary 与文本 SSE 都可
使用；SSE 的最终 `response.completed.response.output` 包含规范化结果。

成功响应把 App Server 的 `webSearch` 转为标准 `web_search_call`，action 名称规范化为
`search|open_page|find_in_page`，随后是普通 message。App Server 当前没有给 adapter 稳定的
Responses URL-annotation DTO，因此首版不伪造 `url_citation`；Consumer 必须允许未知响应字段，且
不能把空 annotations 理解为“没有使用 Web”。

Web Search 权限与 Intent、subscription access、cloud payload egress 三者独立。普通 App 默认
`allowed_builtin_tools=[]`；必须显式加入：

```toml
allowed_builtin_tools = ["web_search"]
```

缺少该 ACL 的请求会在建 Job、排队或启动 Codex 子进程前以 policy violation 拒绝。非 Web 请求的
Codex 进程继续以 `web_search="disabled"` 启动；Web 请求才按请求追加 cached/live 设置。Shell、
文件、MCP、Apps、computer use、delegation 与任意 server-initiated approval 继续 fail closed。
`image.generate` 仍是上文的独立精确合同，不能与 Web Search 混用。

## 10. 查询和解释自己的 Job

Consumer 可以读取、取消和解释自己创建的 Job，但不能看到其他 App 的数据：

```http
GET  /infer/v1/jobs
GET  /infer/v1/jobs/{response_id}
POST /infer/v1/jobs/{response_id}/cancel
GET  /infer/v1/explain/{response_id}
```

这些接口使用同一个 bearer token。`explain` 用于查看 Candidate Plan、排除原因和 Attempt
链；应用业务流程不应依赖其诊断文本。

Provider probe、资源 load/unload、eviction、maintenance lease、budget 和进程 metrics 属于
operator experimental surface，不是普通 consumer 合同。

## 11. RawNIND foundation（experimental）

`raw.materialize_foundation` 是本机、typed、无路径的大制品纵切，不是通用 tensor 或文件市场。
Consumer 通过官方 SDK / Infra Discovery 精确选择 `infer-runtime.consumer-core@20260813.1` 与
`infer-runtime.http-loopback`；短期 UDS endpoint 只在 bearer 鉴权成功的 lease grant 中返回，
不新增 Discovery offer。

官方 Rust client 已提供 `create_raw_foundation_lease(...)`、
`register_raw_foundation_handles(&File, &File)`、`execute_raw_foundation(...)` 与
`cancel_raw_foundation(...)`；应用不应自行拼接 UDS frame 或复制 generic handle transport。完整顺序固定为：

1. `POST /infer/v1/raw/foundations/leases` 创建 Job 与 30 秒、one-shot ticket；
2. 向返回的 owner-only UDS 发送 `infer.artifact-lease.register@20260811.1` 单行 JSON 和两个
   SCM_RIGHTS FD（只读 Bayer 输入、空的非 append 可写输出）；
3. `POST /infer/v1/raw/foundations` 只发送 `{job_id,lease_id}`；
4. 可用 `POST /infer/v1/raw/foundations/{job_id}/cancel` 取消并 revoke scope。

UDS 请求和响应均为最多 4096 bytes（含 LF）的单行 UTF-8 JSON；客户端发送后 half-close，
服务端返回一行并 EOF。App id 只能从 bearer 推导。路径、Bayer bytes、输出 bytes、ticket 和 lease
均不得进入 Job metadata、SQLite 或普通日志。

当前唯一允许的实验 Build 是 `rawnind_ort127_exp1`：CPU、FP32、ONNX Runtime 1.27.0。
Deployment 只有在 `raw_foundation.enabled=true`、graph/runtime 文件存在且 graph SHA-256 精确匹配、
owner-only socket directory 可建立、Provider/Build/Deployment 路由完整时才会随 daemon 启动；任一
条件失败都应让进程 fail closed，而不是静默回退到 legacy 1.24.4 或改写 Shadow cache identity。
成功响应的 `provenance` 还必须由 Consumer 严格核对
`provider=raw-foundation-local`、`deployment/model_build=rawnind_ort127_exp1`、exact model revision、
graph SHA-256、ORT version、actual EP、precision、implementation revision 与 cache identity。未知响应
字段允许忽略，但上述冻结身份缺失或漂移必须拒绝发布制品。pending 与 running Job 都可取消；运行中
取消返回 HTTP 409 + `error.code=cancelled`，最终 Job state 同样为 `cancelled`。

## 12. 接入完成门槛

在开始真实反馈测试前，至少确认：

1. consumer 使用独立 App/token，且 `resource_admin = false`；
2. 启动时读取并记录合同版本，不把 Console `8790` 当成 API；
3. 非流式、SSE、客户端取消各通过一次；
4. 400、401、409、429、503、504 能按 status + `error.code` 处理；
5. 隐私敏感任务显式发送 `local_only`，并验证无 cloud fallback；
6. 音频 consumer 验证文件上限、输出格式、取消和迟到结果；
7. 视觉实验 consumer 另行固定 daemon commit，验证 source/query revision 仲裁、embedding
   space、CPU/EP provenance、取消与 20 MiB/40M 双重上限，不把它当作 v0.1 stable contract；
8. 语义索引只比较完全相同 `embedding.space` 的向量，Build/space 改变时重建；
9. 反馈只带 response/job id、字段名、status、error code、合同版本和 daemon commit，不带
   token、prompt、音频、provider key 或 provider 原始错误。
10. 使用订阅 Provider 的 Consumer 必须显式拥有 subscription access class，并验证
    local-only/offline 请求零触达；图片输入另需 cloud image egress ACL。除 `image.generate` 的
    单个精确 tool 外不得发送 tools；订阅 bridge 不得使用 durable background。
11. 音频 streaming Consumer 必须验证 PCM descriptor、partial replacement、final、disconnect
    cancel 和 reservation 释放，并记录实际 `transcription_mode`，不得假设原生实时 ASR。
12. Qwen 视觉 Consumer 必须验证闭集 id 不外逸、foundational/capable 路由、取消与 source revision
    仲裁；proposal 只有在应用自己的用户接受链路后才能成为业务事实。

Golden request/response/error 示例位于
[contracts/consumer-core/20260813.1/fixtures](../contracts/consumer-core/20260813.1/fixtures)。
Rust Consumer 应使用官方 `infer-runtime-client`；其他语言 SDK 应从运行中 daemon 返回的 OpenAPI
生成 typed layer，并复用相同 Core conformance fixtures。
