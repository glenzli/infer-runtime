# infer-runtime v0.1 consumer contract

当前可供外部应用反馈测试的合同版本是 `0.1.0-candidate.2`；`candidate.1` 保持冻结。它不是正式发布标签，但已经有
固定的路由、请求字段、最小响应字段、错误 envelope、示例和运行时身份。机器可读规范见
[openapi.json](openapi.json)，示例见 [fixtures](fixtures)。第一次接入请从
[外部应用接入指南](../../docs/INTEGRATION.md) 开始；本文件仍是兼容边界的权威说明。

本机位置发现使用独立的
[`infer-runtime.consumer` Infra Discovery offer](../../docs/CONSUMER_DISCOVERY.md)。offer 的
protocol version 必须精确等于本合同版本；Discovery 只提供当前 loopback base URL，不改变本文件
冻结的 HTTP path、payload、鉴权或错误语义，也不携带 Consumer credential。

运行中的 daemon 无需凭证即可返回自身合同：

```bash
curl http://127.0.0.1:8787/infer/v1/contract
curl http://127.0.0.1:8787/infer/v1/openapi.json
```

## 稳定范围

| 数据面 | 当前公开合同 |
| --- | --- |
| 文本 | `POST /v1/responses`；非流式 JSON、SSE、受限 local background |
| Background | `GET /v1/responses/{response_id}`、`POST /v1/responses/{response_id}/cancel` |
| 文件音频 | transcription、alignment、speech、voice clone 四类 task-oriented endpoint |
| 流式音频 | speech 可选 chunked PCM server stream；duplex transcription 当前为 experimental WebSocket |
| Job 控制 | app-scoped list/get/cancel/explain |
| 基础状态 | `/health`、合同 manifest 与 OpenAPI |
| 错误 | `error.message/type/code`；程序只依赖 `code`，不要解析 `message` |

`metrics`、`budget`、provider probe、inventory、resource lifecycle、eviction 和 maintenance
lease 是 operator surface，当前仍为 `experimental`，不在 consumer 兼容承诺内。TOML 是严格的
operator 配置合同：未知键会使启动/校验失败，但它也不应由普通应用生成或修改。
本机 Web Console 的 Apps & Access credential lifecycle 同样属于 experimental operator
surface，不改变这里冻结的 bearer consumer wire contract。

## 兼容策略

- `0.1.0-candidate.1` 与当前 `0.1.0-candidate.2` 一旦交给 consumer 就视为不可变；规范、fixture 或 wire 行为变化必须产生新的 candidate revision。
- 新 candidate 默认只允许增加可选字段、响应字段或新 endpoint。Consumer 必须忽略响应中的未知字段。
- 若反馈期确实证明需要破坏性修正，必须提升 revision、写明 migration，并与已登记 consumer 协调；不得静默修改原 revision。
- 正式 `v0.1.0` 后，破坏公开 wire 的变化进入新的 API/合同版本。Provider SPI、CLI 文本输出和 operator experimental 路由不受此承诺约束。
- `error.code`、HTTP status 和 Job/Attempt 枚举是机器语义；`error.message`、SSE provider 原生扩展字段和 explain 中新增证据是诊断语义。

## Responses 子集

这是 OpenAI Responses 风格的无状态子集，不是对完整 OpenAI 平台行为的承诺：

- `model` 是稳定 Intent（如 `text.summarize`、`assistant.general`），不是物理模型名；
- 支持的请求字段以 OpenAPI 为准；未知顶层字段会返回 `400 invalid_request_error`；
- `previous_response_id`、`conversation`、`store=true` 不受支持；
- `background=true` 当前仅用于非流式、显式 `local_only` 的持久文本任务；
- `metadata` 中的 `infer.*` 表达 runtime 约束，普通 metadata 可按 provider capability 转发；
- SSE 使用 `text/event-stream`。事件集合取决于被选 provider 的 capability profile；runtime 会归一化 response identity，并用 `event: error` 表达自身流中错误。

当前保留 routing metadata（所有值都是 JSON string）：

| Key | 值 |
| --- | --- |
| `infer.policy` | App 获准使用的 policy profile |
| `infer.priority` | `interactive` / `normal` / `background` |
| `infer.placement` | `local_only` / `private` / `anywhere` / `cloud_only` |
| `infer.provider_access_class` | `standard` / `subscription`（仅可缩窄 App ACL） |
| `infer.prefer` | `local` / `trusted_node` / `cloud` |
| `infer.offline_required` | `true` / `false` |
| `infer.quality_floor` | `basic` / `general` / `advanced` / `frontier` |
| `infer.latency` | `interactive` / `balanced` / `throughput` |
| `infer.max_cost_usd` | 非负小数 |
| `infer.fallback` | `none` / `equivalent` / `allow_lower_quality` |
| `infer.deadline_ms` | 正整数毫秒 |

最小调用：

```bash
curl http://127.0.0.1:8787/v1/responses \
  -H "Authorization: Bearer $INFER_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "text.summarize",
    "input": "需要总结的内容",
    "metadata": {"infer.placement": "local_only"}
  }'
```

外部应用可复用 Responses-shaped client，并把 base URL 改为本机 daemon；必须把本文件描述的
兼容子集而非某个云平台的完整参数集作为生成请求的依据。音频和 `/infer/v1/*` 控制面当前直接按
OpenAPI 调用，等第二个真实 consumer 证明重复需求后再决定是否维护轻量 Client SDK。

## 音频字段纪律

Multipart 端点按任务分别接受字段，拼写错误、重复字段、错误的文件字段都会确定返回 400：

- transcription：`model`、`file`、`language`、`prompt`、`response_format`、`temperature`；
- alignment：`model`、`file`、`text`、`language`；
- voice clone：`model`、`input`、`reference_audio`、`reference_text`、`language`、`response_format`；
- 三者都可额外使用一个 JSON-string `metadata` 字段或独立 `infer.*` 字段，但同名 key 不得冲突；
- speech 使用严格 JSON，字段见 OpenAPI；所有上传仍受 25 MiB 单文件上限约束。
- speech 的 `execution_mode=server_stream` 只与 `response_format=pcm` 组合，body 是 append-only
  `pcm_s16le`；采样率、声道与 Job 身份由响应 header 给出。
- duplex transcription 使用 `GET /v1/audio/transcriptions/stream` WebSocket；binary frame 是 PCM，
  JSON partial 按单调 `revision` 完整替换。当前 `commit_redecode` 是实验 transport，不承诺原生
  增量 ASR 的固定低延迟。

## 反馈测试最小清单

Consumer 开始接入时记录 daemon commit 与 `contract_version`，至少反馈：

1. 非流式成功、SSE 成功和客户端取消；
2. Intent/placement/quality/fallback 的实际使用组合；
3. 400、401、409、429、503、504 是否能只依赖 status + `error.code` 正确处理；
4. background create/retrieve/cancel（若启用）；
5. Job list/get/explain 能否支撑问题定位；
6. SDK 是否自动发送了当前子集之外的字段；
7. 音频使用方再覆盖文件上限、格式、TTS chunk/取消，以及 ASR revision/断开/迟到结果。

反馈应包含 request 字段名、HTTP status、error code、response/job id 和合同版本；不要附带原始
prompt、音频、API token、provider 原始错误或 background 加密密钥。

Consumer 对未知 `error.code` 必须退回 HTTP status 处理。常见核心 code 包括：
`invalid_api_key`、`invalid_request_error`、`policy_violation`、`intent_forbidden`、`not_found`、`no_candidate`、
`cancelled`、`background_unavailable`、`queue_full`、`app_queue_full`、`quota_exceeded`、
`deadline_exceeded`、`provider_unavailable` 以及 `upstream_*`。新增 code 属于兼容性扩展；
operator experimental 路由还会有自己的管理类 code。
