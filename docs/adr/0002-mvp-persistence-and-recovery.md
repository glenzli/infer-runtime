# ADR-0002：MVP 持久化 metadata 和账本，不恢复执行

- 状态：Accepted；durable background 扩展见 ADR-0010
- 日期：2026-08-08
- 决策者：项目 Owner
- 关联：D-002

## 背景

预算与审计要求重启后仍可追踪，但首个交互型总结请求不要求跨 daemon 重启续流。过早实现 durable execution 会让第一个垂直切片承担幂等、重复计费和 provider 恢复等复杂语义。

## 决策

M1 使用内存状态。M3 使用 SQLite 持久化 Job/Attempt metadata、decision events、usage ledger 与 reservations，但不自动恢复普通未完成执行。普通请求重启中的 Attempt 进入 `unknown/interrupted`，所属 Job 失败。M4 只对调用方显式选择、具有加密 payload ownership 的 local background Job 增加受限恢复语义，见 ADR-0010。

## 备选方案

- 全内存 MVP：无法可靠管理跨应用预算与审计。
- 从 M1 开始 durable execution：复杂度与首个用例不匹配。
- 外部数据库：违背首版单用户 local-first 的部署目标。

## 影响

- v0.1 可以可靠保存账本和解释链，但交互请求需要客户端在明确失败后自行重提。
- 状态模型必须区分 provider 实际结果未知与 Job 对客户端的确定终态。

## 验证方式

在 queued、running、streaming 和 settling 阶段注入崩溃，验证恢复状态、账本守恒且不会自动产生第二次 provider 调用。

## 复审触发条件

若需要恢复 cloud、带外副作用或流式交互请求，必须重新评估幂等与结果仲裁；不得把 ADR-0010 的 local background 例外扩张为默认行为。
