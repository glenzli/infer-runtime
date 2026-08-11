# v0.1 外部合同审计

日期：2026-08-11

基线：`0.1.0-candidate.3`（`candidate.1`、`candidate.2` 保持冻结）
结论：consumer surface 已形成可供外部应用开始反馈测试的版本化 candidate；正式发布仍由
长期运行、真实 consumer 和 SLO 证据决定。

## 审计范围与 owner

| 范围 | 语义 owner | Wire/制品 owner | 稳定性 |
| --- | --- | --- | --- |
| Responses 请求、Intent 约束 | `infer-core::request` | `infer-api` + `contracts/v0.1` | candidate |
| 音频任务合同 | `infer-core::audio` + `infer-core::streaming` | `infer-api` multipart/JSON/chunked PCM；duplex WebSocket experimental | candidate + experimental |
| Job/Attempt/Candidate projection | `infer-core::job` | `/infer/v1/jobs`、`explain` | candidate |
| HTTP auth、状态码、error envelope | `infer-api::contract` | OpenAPI + route tests | candidate |
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

## 已有设计覆盖与本轮新增

已有设计已经覆盖：Responses 与音频分型、Intent 而非 deployment、`infer.*` 约束、App 隔离、
Candidate/Attempt、取消、deadline、错误归类、provider capability probe、Job provenance、payload
不进入通用 metadata/log。此次没有改变这些核心边界。

candidate.1 新增的是它们的外部发布形态：精确版本、路由清单、最小 JSON schema、golden fixtures、
严格失败纪律和兼容策略。candidate.2 以 additive 方式增加 speech `execution_mode`、PCM
server-stream 描述和 experimental duplex ASR 路由，同时在 Responses 后端允许已授权的 Codex
text/image + SSE。candidate.3 通过显式 migration 更换 Intent、capability、policy/fallback 与
Job provenance 词汇，并增加 `reasoning.effort=ultra`；它不修改 bearer、HTTP path、typed payload
或错误 envelope。ONNX/视觉 typed slices 只列在 manifest `experimental_routes`，不进入 stable
consumer routes；远程节点与 operator resource schema 同样没有进入。

## 自动化证据

- OpenAPI 必须能被 JSON parser 读取，`info.version` 必须与代码常量一致；
- manifest 中每条 consumer route 必须出现在 OpenAPI；
- fixtures 必须保持合法 JSON；
- route-level tests 覆盖 manifest/OpenAPI、JSON unknown field、speech JSON、multipart unknown/wrong
  file、unknown Job query 和统一鉴权错误；
- typed fake worker tests 覆盖 TTS chunk、ASR revision、Codex image staging 与文本 delta；
- core tests 覆盖 Responses/Speech 严格字段与 reasoning extension 保留；
- config tests 覆盖根与嵌套 unknown key，完整 checked-in registry 仍必须通过；
- workspace format、Clippy 与 tests 是每次 candidate 变更的最低门槛。

## 尚未关闭但不阻塞反馈测试

1. 官方 SDK 的多语言版本矩阵还未自动化；先由首个真实 consumer 的实际 SDK 固化第一条 acceptance fixture。
2. SSE 的公共最小 event 集合仍需结合 Ollama 与 cloud provider 实测后收窄；当前只承诺 SSE framing、
   response identity normalization、可见输出后不 fallback 以及 runtime `error` event。
3. `error.code` inventory 目前由代码映射与 route tests 共同约束；正式 v0.1 前可从 Rust 常量进一步生成，
   避免手写文档漂移。
4. OpenAPI 是 schema/示例的真源，但尚未引入完整 JSON Schema validator。正式发布门前应让至少一个
   外部 validator/client generator 读取该制品，确认工具兼容性。
5. 24 小时混合 soak、完整 trace、真实 consumer 连续使用和 SLO 仍是正式发布证据，不因合同 candidate 而跳过。

## Consumer 接入门槛

允许开始反馈测试的门槛是：daemon 报告预期 contract revision、外部请求只使用该 OpenAPI、错误只
依赖 HTTP status + `error.code`、App token 独立配置、consumer 默认不获得 resource-admin 权限。
发现合同歧义时先更新 audit/fixture 并提升 candidate revision，再改实现；不得让 consumer 依赖未登记
字段或 experimental operator shape。
