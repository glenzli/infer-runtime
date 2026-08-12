# ADR-0006：首个真实 consumer 验收与 v0.1 SLO

- 状态：Accepted
- 日期：2026-08-08
- 决策者：项目 Owner
- 关联：D-006

## 背景

没有真实 consumer 会使 route、SDK 和 constraints 按想象设计。早期讨论曾以一个候选应用的文本总结需求作为切入点，但该应用没有实际接入；讨论起点不能被误写成 runtime 已注册的产品身份或验收事实。

## 决策

runtime 不预注册具名外部 consumer。接入方在自己的领域内把动作映射到公共 Intent，例如把文本总结映射到 `text.summarize`；runtime 不注册应用专用 route。参考环境是 macOS Apple Silicon + Ollama，候选由 workload rating、placement 和请求约束产生，具体结构见 ADR-0008。

v0.1 SLO：provider 空闲时 admission-to-dispatch p95 ≤ 25 ms；首个 provider 事件的额外转发延迟 p95 ≤ 20 ms；取消在 100 ms 内完成控制面终态仲裁，并在 provider 支持时 2 s 内释放 reservation。隐私越界、预算超卖、重复终态和 reservation 泄漏为零容忍。

## 备选方案

- 先做纯 CLI demo：不能验证真实 App 合同。
- 先做后台批处理：会过早引入 durable execution。
- 先接云模型：无法验证本项目最有区别度的本地资源路径。

## 影响

- M1-M3 先以通用 consumer fixture 验证合同，首个真实接入方再提供连续使用证据。
- CI 继续使用 fake provider，真实 Ollama 只作为可选集成与验收环境。

## 验证方式

首个真实 consumer 使用标准 OpenAI SDK 指向 `inferd`，以公共 Intent 完成非流式、流式、取消、fallback policy 和 explain 端到端测试；在此之前不宣称已有 consumer acceptance。

## 复审触发条件

若真实 Consumer 暴露当前 schema 无法表达的需求，按 owner 提升对应的日期化 Capability；只有
Discovery/Auth/Job/Error 等共同骨架发生 breaking change 才提升 Consumer Core。不得用应用专用
alias 绕过公共 Intent 和约束模型。
