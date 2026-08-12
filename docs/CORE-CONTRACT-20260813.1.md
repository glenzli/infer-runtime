# Infer Runtime Consumer Core `20260813.1`

状态：migration target；在所有登记 Consumer 完成适配前不替换当前运行服务。

迁移清单见 [MIGRATION-CONSUMER-CORE-20260813.1.md](MIGRATION-CONSUMER-CORE-20260813.1.md)。

## 身份

- Discovery protocol：`infer-runtime.consumer-core`
- Discovery version：`20260812.1`
- HTTP binding：`infer-runtime.http-loopback`
- 完整 Core 身份：`infer-runtime.consumer-core@20260813.1`
- 每个 Consumer control/data request 必须恰好携带：
  `Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1`
- 缺失、重复或其他值：HTTP 426，`error.code=consumer_core_unsupported`

Infra Discovery 使用 `infra.discovery.registration@20260812.1`。Runtime manifest 只发布上述
精确协议/version/binding；没有版本范围、candidate fallback、lease/heartbeat 或固定端口兜底。
连接失败后 Consumer 重读同一稳定 manifest；generation 或 offer 改变才允许重试新入口。

## Core 的稳定范围

Core 只承诺所有能力共同依赖的骨架：

1. Infra Discovery 选择和 loopback endpoint 安全规则；
2. exact Core header、Bearer Consumer 身份与公共错误 envelope；
3. App ACL/admission 的 fail-closed 语义；
4. Job、Attempt、routing explain、cancel 与 payload-free provenance；
5. `GET /infer/v1/contract`、`GET /infer/v1/capabilities` 和 OpenAPI bootstrap。

文本、音频、视觉、RAW、OCR 等请求/响应不进入 Core。它们由独立日期化 Capability Schema
拥有。新增 capability 不升级 Core；只有共同骨架改变，或者需要把一个新的 Core 字段/错误码写入
机器可读 OpenAPI 时，才发布新的 Core 日期版本。既有日期版本的 OpenAPI bytes 和 digest 永不原地
变化，新旧版本可在迁移窗口并行发布。

每个类型化能力请求还必须恰好发送一个 `Infer-Capability-Contract`，值为 Catalog 中选定的
完整能力身份，例如 `infer.audio.transcription@20260811.1`。Core-owned 的 Job/Explain/contract/
catalog 操作不发送该头。缺失、重复或错误能力身份返回 HTTP 426、
`error.code=capability_contract_unsupported`；这样某个能力 breaking 时只迁移使用该能力的
Consumer，不升级 Core，也不影响其他能力。

## Bootstrap

`GET /infer/v1/contract` 返回：

```json
{
  "schema": "infer-runtime.consumer-core",
  "schema_version": "20260813.1",
  "core_contract": "infer-runtime.consumer-core@20260813.1",
  "supported_core_contracts": ["infer-runtime.consumer-core@20260813.1"],
  "capability_catalog": {
    "schema": "infer-runtime.capability-catalog",
    "schema_version": "20260813.1",
    "url": "/infer/v1/capabilities"
  },
  "openapi_url": "/infer/v1/openapi.json",
  "openapi_sha256": "<64-lowercase-hex>",
  "error_codes": ["invalid_request_error", "..."],
  "consumer_routes": []
}
```

Consumer 解码器必须忽略未知响应字段，所以新 Runtime 可以在动态响应中携带额外诊断元数据；但
一旦某字段进入机器可读 OpenAPI，必须发布新的日期版本，不能改写既有 artifact。Consumer 请求仍
strict，未知字段或未知 `infer.*` metadata 返回稳定 4xx `error.code`。

机器 schema 对 response object 使用开放字段策略；required 字段与枚举仍是兼容底线。请求 object
保持 closed/strict。这样新增 response 诊断字段不要求租户升级，而删除 required 字段、收紧类型、
改变枚举语义仍必须发布新 Core/Capability 日期版本。

`error_codes` 只列 Core 共同骨架拥有的机器错误；能力专属错误属于对应 Capability schema，不因
新增一个 RAW/视觉错误而改写 Core 身份。新增、删除或改名机器错误都要发布拥有该错误的下一日期
版本；旧版本继续返回其已冻结集合。`openapi_sha256` 必须和
`/infer/v1/openapi.json` 原始 bytes 一致；Catalog 每项的 schema digest 则必须和该项独立的
`/infer/v1/capability-schemas/<id>/<version>/openapi.json` 原始 bytes 一致，不能复用聚合 Core 摘要。

## 兼容与迁移

这是一次刻意的 hard cut：最终 Runtime 不发布 `infer-runtime.consumer` candidate offer，也不接受
candidate.2/3/4 或缺失 header。旧 Job/Background 数据的内部读取兼容不构成旧 wire 支持。
历史 Job 的 `consumer_core_contract` 可以诚实保留旧身份或 `legacy-unknown`；只有新提交的 Job
保证等于当前 Core，并同时记录其 `capability_contract`。

官方 Rust SDK `infer-runtime-client` 是接入权威实现。租户不再各自实现 manifest 权限校验、
generation 重发现、proxy/redirect 禁用、token 文件读取或错误解析。

## 失败、重试与日志边界

- Consumer 只按 HTTP status 与 `error.code` 分支；`error.message` 是脱敏诊断，不是合同。
- SDK 仅在连接失败且 Discovery identity 发生变化时重新发现一次；能力请求会在新 generation
  重新读取 Catalog 并校验 schema digest，绝不把旧代校验结果用于新 endpoint。
- Runtime 对同一候选最多执行受 Attempt budget 限制的 retry；仅 rate-limit、timeout、unavailable
  可重试，采用有界指数退避和 deterministic jitter，并尊重秒数或 HTTP-date `Retry-After`（最长
  30 秒）。deadline/cancel 始终优先，`fallback=none` 不产生候选切换。
- Provider 原始 body、原生错误、路径、prompt、音频、像素和 embedding 不进入公共错误、Job、Attempt、
  audit 或普通日志；持久化只保存稳定错误 code 与脱敏摘要。
- SDK 对 Core/Capability JSON、错误、输入文件、unary 音频输出和 streaming 累计 bytes 都有硬上限；
  声明的 `Content-Length` 或实际 chunk 累计超限均 fail closed。
