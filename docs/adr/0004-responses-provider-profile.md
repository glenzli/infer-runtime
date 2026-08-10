# ADR-0004：公开 Responses 合同与窄上游协议 adapter

- 状态：Accepted
- 日期：2026-08-08
- 决策者：项目 Owner
- 关联：D-004

## 背景

OpenAI、Ollama 及目标服务都具有某种 OpenAI 风格接口，但具体路径、字段、状态能力、工具和 usage 行为并不完全相同。DeepSeek V4 Flash 现已公开 stateless `/responses`，而 V4 Pro 尚未支持该端点。

## 决策

`inferd` 的 provider 执行合同固定为无状态 Responses 子集。MVP 保留一个 Responses-compatible 执行 adapter，供 Ollama 与 DeepSeek V4 Flash 等原生 `/responses` 服务使用。每个 provider instance 声明并探测 capability profile；不支持字段显式失败。M4 的 runtime-managed local background 在 adapter 前移除 `background`，因此不改变上游 capability profile；见 ADR-0010。

Ollama MVP profile 是无状态 Responses：支持 input、instructions、streaming、function tools、基础生成参数；不使用 `previous_response_id` 或 `conversation`。

DeepSeek Flash 的 Responses profile 支持无状态文本 input/instructions、streaming、function tools、temperature、top_p、max_output_tokens 和 usage；其 `reasoning.effort` 原生包含在该合同中。V4 Pro 仅保留 catalog registration，直到官方说明其 `/responses` 支持可用。

外部合同基线（2026-08-08 核对）：

- [OpenAI Responses API reference](https://platform.openai.com/docs/api-reference/responses)
- [Ollama OpenAI compatibility](https://docs.ollama.com/api/openai-compatibility)
- [DeepSeek Responses API](https://api-docs.deepseek.com/api/create-response)

## 备选方案

- 每个 provider 独立完整 adapter：重复大量执行和 stream 解析逻辑。
- 为尚不支持的 V4 Pro 假设 `/responses`：会把部署错误推迟到运行时。
- 为单一 provider 预先建立通用 Chat Completions SPI：在第二个独立需求出现前会产生未验证的抽象与合同负担。
- 假设完全兼容：会静默产生功能和状态语义错误。

## 影响

- 公开合同稳定，当前执行 adapter 数量保持最小；新增上游协议时必须以独立合同测试证明其必要性。
- provider-specific 控制面可以独立演进，而不会污染数据面。

## 验证方式

对 Responses-compatible fake server、Ollama 和 DeepSeek Flash 运行同一公共 contract suite，再为每个 profile 运行 SSE 与能力特定测试。默认 CI 不需要云凭证；DeepSeek 配置声明 `requires_api_key = true`，所以 `DEEPSEEK_API_KEY` 缺失时该 deployment 不进入候选。仅在该凭证存在时运行付费 smoke test。

## 复审触发条件

出现第二个必须走 Chat Completions 的 provider，或 native Responses profile 不能满足其真实需求时复审通用 Chat Completions SPI。
