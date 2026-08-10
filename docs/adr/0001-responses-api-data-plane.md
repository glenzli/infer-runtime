# ADR-0001：文本数据面采用 OpenAI Responses API 兼容合同

- 状态：Accepted
- 日期：2026-08-08
- 决策者：项目 Owner
- 关联：D-001

## 背景

Ollama 与大多数目标云服务都提供某种 OpenAI-compatible API。自定义一套文本生成协议会增加 Client SDK、adapter 和迁移成本。与此同时，runtime 还需要表达 Intent、priority、placement、quality floor、queue、budget 和 routing，这些不是物理模型 API 自然拥有的语义。

## 决策

文本数据面公开 OpenAI Responses API 兼容的 `POST /v1/responses`。非流式响应和 SSE 事件保持兼容；管理面使用独立 `/infer/v1/*` API。

标准 `model` 字段解释为 Intent Profile，而不是物理模型。Responses `metadata` 中的 `infer.*` keys 承载请求级动态约束，标准 `reasoning.effort` 保持其独立含义。普通/上游执行只支持无状态 Responses 子集；不支持字段返回明确错误。M4 对标准 `background: true` 增加 runtime-managed create/retrieve/cancel 生命周期，但转发到 provider 的请求仍是无状态调用，具体 payload 与恢复边界见 ADR-0010。

Responses function tools 只在数据面传递 tool definitions/tool calls；`infer-runtime` 不执行工具，也不拥有 tool loop。

## 备选方案

- 自定义 Job HTTP API：表达能力最强，但破坏现成 SDK 兼容。
- Chat Completions：覆盖广，但方向偏旧且不适合作为未来能力基线。
- 完整复制 OpenAI 平台行为：无法保证所有状态、存储和内置工具语义，容易形成虚假兼容承诺。

## 影响

### 正面

- 外部应用可复用 OpenAI SDK，只需改变 base URL 并选择 Intent Profile。
- Ollama 与云 provider 可共享 Responses 执行 adapter。
- 保留独立控制平面，不让 provider schema 决定 scheduler 架构。

### 代价与风险

- `model` 的 Intent 语义与直接调用 OpenAI 时不同，必须清晰文档化。
- 兼容范围需要 profile 与合同测试；不能默认所有服务完整支持 Responses。
- OpenAI Responses API 演进时，需要显式版本/兼容性评审。

## 验证方式

- 使用官方 OpenAI SDK 对 `inferd` 执行非流式与流式合同测试；
- 对未知/不支持字段验证稳定错误；
- 证明 `infer.*` metadata 不会泄露到 provider；
- 证明普通请求不能用 `model` 绕过 Intent 指定物理 deployment。

## 复审触发条件

Responses 合同无法表达新协议族，或三个以上关键 provider 无法实现所需子集时复审；多模态可以采用独立数据协议，不自动推翻本决定。
