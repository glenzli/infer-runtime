# Consumer Core 外部合同审计

日期：2026-08-11

基线：`infer-runtime.consumer-core@20260813.1` + 日期化 Capability Catalog
结论：consumer 共同骨架已从 candidate 总包收敛为稳定 Core；正式发布仍由
长期运行、真实 consumer 和 SLO 证据决定。

## 审计范围与 owner

| 范围 | 语义 owner | Wire/制品 owner | 稳定性 |
| --- | --- | --- | --- |
| Responses 请求、Intent 约束 | `infer-core::request` | Capability schema + official SDK | stable capability |
| 音频任务合同 | `infer-core::audio` + `infer-core::streaming` | 独立日期化 Capability；`infer-api` multipart/JSON/chunked PCM，duplex WebSocket experimental | stable + experimental capabilities |
| Job/Attempt/Candidate projection | `infer-core::job` | `/infer/v1/jobs`、`explain` | Core |
| HTTP auth、状态码、error envelope | `infer-api::contract` | dated OpenAPI + SDK tests | Core |
| Runtime TOML | `infer-core::config` | `config/infer.example.toml` + config tests; ignored `config/infer.toml` at runtime | strict operator contract |
| metrics/provider/resource 管理 | control/resource owner | `/infer/v1/*` operator routes | experimental |
| ONNX/视觉 | `infer-core::vision` + ADR-0011 | 实验 face/SigLIP typed routes；列入 manifest 但不进入 stable consumer routes | experimental |

最初 v0.1 source-cohesion growth review 的结论是：请求和 Job 类型继续留在 `infer-core`，HTTP
合同身份、公开错误和机器规范抽到 `infer-api::contract`，且 provider/resource 运维 shape 不进入
核心领域模型。后续 ONNX P0 为具有独立权限与发布生命周期的制品增加 `infer-artifact`，Session 与
adapter 留在 `infer-provider::onnx`，类型化视觉 wire 留在独立 API module；仍未扩张 Responses schema。

## 发现与处理

| ID | 发现 | 风险 | 处理 |
| --- | --- | --- | --- |
| C-001 | Responses/Speech JSON 会忽略未知顶层字段 | SDK 参数或拼写错误看似成功 | 根请求启用 `deny_unknown_fields`；reasoning provider 扩展仍有意保留 |
| C-002 | Axum JSON rejection 不经过公开 error mapper | 同一 API 出现框架文本/422 | 稳定 JSON endpoint 使用 strict extractor adapter，统一为 400 error envelope |
| C-003 | 音频 multipart 不区分端点字段且覆盖重复项 | 错文件字段、错拼和 metadata 冲突被吞掉 | endpoint-specific allowlist、文件名、重复字段和 metadata collision 校验 |
| C-004 | Job list query 会忽略未知参数 | `stat` 等错拼造成无过滤结果 | query 严格反序列化并统一 400 envelope |
| C-005 | 没有机器可读的外部合同身份 | Consumer 无法判断实际兼容范围 | 固定 OpenAPI、fixtures、`/infer/v1/contract`、`/infer/v1/openapi.json` |
| C-006 | TOML 语义严格但未知键会被忽略 | 运维配置错拼后以默认值运行 | 所有 config struct 逐层拒绝未知键并增加 root/nested tests |
| C-007 | Consumer 与 operator API 混在同一路由前缀描述中 | 过早冻结资源治理 shape | manifest/OpenAPI 只承诺 consumer surface，operator routes 明确 experimental |
| C-008 | Catalog 的旧版本数组只有一份 schema 引用，且 SDK 禁止同 route 多版本 | 单能力升级仍会退化成所有租户同时切换 | 冻结为单值 `schema_version`，一 record 一版本；允许同 id/route 的旧新 record overlap，SDK 按有序精确交集选择 |
| C-009 | Catalog schema URL、Job optional provenance、named route 与真实 wire 不一致 | 生成器/Consumer 严格校验会错误拒绝合法响应 | 修正机器 schema，并由 Rust fixture/枚举测试锁定 |
| C-010 | 文档要求忽略未知响应字段，但部分 response schema closed | additive 响应字段会被生成客户端视为 breaking | response object 开放 additive 字段；request object 继续 strict |
| C-011 | 本机预发布 daemon 曾以 `20260812.1` 发布较早的 Core/Catalog 草案，最终 source 已包含不同 bytes 与更完整字段 | 同一不可变身份对应两套 wire，Consumer 无法安全校验 digest | 最终合同改用 `20260813.1`；不覆盖、重标或继续支持预发布草案，切换时必须更新 Discovery generation |
| C-012 | 声音事件的空事件列表、无人声和未完整分析容易被合并成同一“空”语义 | Echo 会把缺失证据误当否定证据 | 独立 `infer.audio.event-detection@20260813.2` 强制返回 events、speech_presence、coverage、ontology、policy、provenance；只有 full coverage + 低于版本化阈值才能声明 absent |

## 已有设计覆盖与本轮新增

已有设计已经覆盖：Responses 与音频分型、Intent 而非 deployment、`infer.*` 约束、App 隔离、
Candidate/Attempt、取消、deadline、错误归类、provider capability probe、Job provenance、payload
不进入通用 metadata/log。此次没有改变这些核心边界。

历史 candidate 建立了精确版本、Responses/音频分型、流式能力、Intent taxonomy 与具名 routing
ACL。`20260813.1` hard migration 将这些已验证语义拆为日期化 Consumer Core 与独立 Capability
Catalog：Core 只冻结 discovery/auth/error/Job/routing/cancel/provenance，typed data plane 用
`Infer-Capability-Contract` 独立协商。Intent 仍是 `model`，App 全局安全边界仍为硬上限；远程节点与
operator resource schema 仍不进入 Consumer Core。

## 自动化证据

- OpenAPI 必须能被 JSON parser 读取，`info.version` 必须与代码常量一致；
- `tools/generate_capability_schemas.py --check` 必须证明聚合 authoring source 与所有发布制品 byte-for-byte 一致；
- `tools/validate_contract_artifacts.py` 必须用 Draft 2020-12 校验 canonical fixtures、解析全部 local `$ref`、
  检查 operationId 唯一性并核对不可变 digest lock；
- manifest 中每条 consumer route 必须出现在 OpenAPI；
- fixtures 必须保持合法 JSON；
- route-level tests 覆盖 manifest/OpenAPI、JSON unknown field、speech JSON、multipart unknown/wrong
  file、unknown Job query 和统一鉴权错误；
- typed fake worker tests 覆盖 TTS chunk、ASR revision、Codex image staging 与文本 delta；
- audio-event fixtures 分别覆盖事件存在、合法空事件 + speech absent、partial coverage + unknown；Core
  与官方 SDK 额外拒绝 partial coverage 上的 absent；
- core tests 覆盖 Responses/Speech 严格字段与 reasoning extension 保留；
- config tests 覆盖根与嵌套 unknown key，完整 checked-in registry 仍必须通过；
- workspace format、Clippy 与 tests 是每次 Core/Capability 变更的最低门槛。

## 已关闭的发布阻断项

1. Catalog 每个能力现在都引用自己独立的不可变 OpenAPI URL + SHA-256，Core OpenAPI 也只包含共同
   控制面。所有登记 route 必须存在于能力专属制品；新增无关能力不会改写既有 Core 或能力摘要，
   稳定音频 JSON 也有具体 response schema。
2. Consumer Discovery 已从 observer 生命周期解耦；关闭状态观测不会让 Consumer API 消失。
3. 官方 SDK 的 unary 方法会在发送前拒绝 Responses/Speech 流模式；Catalog handshake 先做精确
   capability identity 交集，不再盲发编译期常量。
4. 公共 error code 已进入 Core manifest/OpenAPI 枚举，并检查唯一性。

## 尚未关闭但不阻塞反馈测试

1. 官方 Rust SDK 已冻结并覆盖当前登记 Consumer 的主要能力；其他语言绑定及其版本矩阵尚未建立，
   只能由真实非 Rust Consumer 需求触发。
2. SSE 与 PCM 已由官方 SDK 的独立有界 session API 承载；双工 ASR WebSocket 仍是 experimental，
   需要完整 framing/cancel/disconnect client tests 后再进入稳定 SDK 面。
3. Aggregate schema source 是 authoring 真源，生成出的 Core/Capability OpenAPI 已由不可变 digest
   lock 防止原位覆盖，并通过 Draft 2020-12 validator；正式发布门前仍应让至少一个独立 OpenAPI
   client generator 读取发布制品，确认第三方工具兼容性。
4. Windows Discovery/credential ACL 尚未实现；当前 SDK 明确只支持 Unix。
5. 24 小时混合 soak、完整 trace、真实 consumer 连续使用和 SLO 仍是正式发布证据，不因合同日期化而跳过。
6. 当前能力只有单一已实现版本；Catalog/SDK 的有序精确交集、同 route 多 record、Runtime route
   admission 与 request-scoped Job provenance 已具备。首个 capability v2 仍必须补旧新两份真实 HTTP
   fixture 和同一 generation soak，才能证明具体 adapter 同时满足两份 wire，而不是只证明协商机制。

## Consumer 接入门槛

允许开始反馈测试的门槛是：daemon 报告预期 contract revision、外部请求只使用 Core 与所选能力的
发布 OpenAPI、错误只
依赖 HTTP status + `error.code`、App token 独立配置、consumer 默认不获得 resource-admin 权限。
发现合同歧义时先更新 audit/fixture；Core breaking 发布新 Core 日期版本，单能力 breaking 只发布
新的 Capability 日期版本。不得让 Consumer 依赖未登记字段或 experimental operator shape。
