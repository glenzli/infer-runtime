# ADR-0003：TOML 配置与类型化 policy profiles

- 状态：Accepted
- 日期：2026-08-08
- 决策者：项目 Owner
- 关联：D-003

## 背景

local-first 工具需要可读、可备份、可版本控制的配置，同时运行时决策必须记录所用版本。通用 policy DSL 在需求尚少时会制造额外语言和安全边界。

## 决策

TOML 是配置真源，包含 Intent Profiles、Model Profiles、Model Builds、Deployments、Providers、App policies、quotas 和 policy profiles。CLI 负责验证、查看生效配置、原子 reload 和 explain。SQLite 只保存运行状态、配置版本引用和历史。

Policy 使用类型化 schema，不支持任意脚本。reload 先完整验证再原子发布；已 admission Job 固定原版本。

## 备选方案

- 纯数据库 + CLI：不利于版本控制和人工审阅。
- YAML：表达更宽松，也更容易出现隐式类型问题。
- Rego/CEL/脚本：早期能力过强，调试和安全成本过高。

## 影响

- 配置 schema 和 migration 成为稳定产品合同。
- 动态修改不会改变正在执行 Job 的语义。
- 未来若增加 UI，也必须操作同一类型化配置模型。

## 验证方式

golden config tests、无效配置拒绝、reload 原子性、旧 Job 固定旧版本和 explain 可重现性测试。

## 复审触发条件

类型化 profile 无法表达三个以上真实应用的必要策略，且规则组合已出现明确复用需求时评估 DSL。
