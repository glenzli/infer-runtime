# ADR-0005：配置决定策略，请求在授权范围内动态覆盖

- 状态：Accepted
- 日期：2026-08-08
- 决策者：项目 Owner
- 关联：D-005

## 背景

local-first 描述控制平面的部署与所有权，不代表每次推理都必须优先本地。不同应用和请求可能分别重视隐私、延迟、成本或质量，系统不能用一个固定顺序替代用户策略。

## 决策

配置提供命名 policy profiles、全局/App 默认 profile、可选 provider 集和 request override 权限。请求可选择获准 profile、增加 hard constraints 或调整获准 preferences。

合并顺序为：系统安全不变量、全局/provider hard limits、App policy、Intent defaults、request overrides。请求不能放宽上层 hard constraints。profiles 采用类型化过滤条件和有序比较规则，并输出 reason codes。

提供 balanced、local-first、quality-first、latency-first、cost-first 模板，但均可配置；没有不可修改的全局“本地优先”。

## 备选方案

- 固定本地优先：无法适配质量或延迟优先请求。
- App 完全控制：容易绕过隐私、预算和 provider 边界。
- 单一加权分数：灵活但难解释，早期参数没有数据依据。

## 影响

- Policy schema、合并规则和 explain 输出是核心合同。
- App 可以偏好本地执行而不影响其他 App，Intent 本身不携带 placement。
- 请求动态性受 App 授权约束，不会把治理权交回任意客户端。

## 验证方式

组合测试覆盖每个配置层与 request override；属性测试证明后层永不放宽 hard constraints；golden explain tests 验证选择原因稳定。

## 复审触发条件

有真实数据证明有序规则无法满足质量/成本优化，且可解释评分能稳定优于 profiles 时评估评分模型。
