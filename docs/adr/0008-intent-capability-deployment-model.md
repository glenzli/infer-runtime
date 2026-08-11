# ADR-0008：Intent、能力评级、推理投入与 Placement 模型

- 状态：Accepted
- 日期：2026-08-08
- 决策者：项目 Owner
- 关联：D-008；替代 D-007 的结构部分

## 背景

应用别名、任务类型、模型规模、本地/云位置、能力等级、tool 权限和 reasoning effort 曾被混在
route 与单一 `quality` 数字中。这无法表达“小模型足够完成总结，但不适合复杂推理”，也无法
表达“一个语言响应是否允许搜索或宿主机执行”。

## 决策

Responses `model` 表示稳定 Intent Profile，例如 `text.summarize`、`language.respond`、
`reasoning.solve`、`multimodal.respond`。应用名不进入 runtime 公共分类；Intent 只表达任务和数据
合同，不表达 capability level、reasoning effort 或 tool 权限。`assistant.*` 被移除，因为它无法
说明是否具有搜索、function execution 或 Agent 权限。

模型注册分层为：

```text
Intent Profile
      |
      | workload-specific rating
      v
Model Profile -> Model Build -> Deployment -> Provider -> Node
```

每个 Model Profile 按 Intent 独立记录 capability scale `20260811.1` 的
`foundational/capable/advanced/expert/exceptional`，并标记为 `provisional` 或带 eval profile 的
`benchmarked`。等级由固定任务包络决定，不是当前 fleet 百分位；未来的小模型通过 `expert` eval
就登记为 `expert`。参数量、provider、placement、价格和 reasoning effort 均不能自动推导能力等级。
缺少 rating 表示 `unassessed` 并拒绝路由，不再被解释为确定不支持。

请求将以下维度正交表达：

- `capability_floor`：候选模型必须达到的 Intent-specific 能力等级；
- `reasoning.effort`：选中模型后投入的计算强度，公共值包括 `ultra`，每个 Deployment 只声明其支持子集；
- `tools`：可选的 provider-hosted 或 function-call 能力；它不由 `language.respond` 暗示；
- `placement`：`local/trusted_node/cloud` 的允许集合；
- `prefer`：允许集合内的位置偏好；
- deadline、cost、priority 和 fallback：服务约束。

Deployment 的 `resource_class` 独立描述运行负担，用于在多个均满足能力下限的本地候选间优先
轻量制品；它不得参与能力评级。

## 影响

- 删除 `app.summary`、`local.general` 等应用或位置耦合 route；
- 删除没有 workload/eval 含义的全局质量数字；
- fallback 不得突破 Intent、feature、capability floor、placement、deadline 或预算；
- 本机、可信 DGX 和云端模型使用同一候选模型，不因位置被预设能力上限。

## 验证

- 弱本机模型不能满足默认的 `reasoning.solve + advanced`；
- `language.respond` 在没有 `tools` 时不获得搜索、function execution 或 Agent 执行能力；
- 请求 `effort=ultra` 时，不支持该值的 Deployment 在 dispatch 前被排除；
- `local_only` 不会因云模型能力更高而越界；
- `app.summary` 被视为未知 Intent；
- Intent 名不会作为物理模型名发送给 provider。
