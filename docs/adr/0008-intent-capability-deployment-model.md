# ADR-0008：Intent、能力评级、推理投入与 Placement 模型

- 状态：Accepted
- 日期：2026-08-08
- 决策者：项目 Owner
- 关联：D-008；替代 D-007 的结构部分

## 背景

应用别名、任务类型、模型规模、本地/云位置、质量档位和 reasoning effort 曾被混在 route 与单一 `quality` 数字中。这无法表达“小模型足够完成总结，但不适合复杂推理”，也无法表达“可信 DGX 上可以部署高能力模型”。

## 决策

Responses `model` 表示稳定 Intent Profile，例如 `text.summarize`、`assistant.general`、`reasoning.deep`、`vision.understand`。应用名不进入 runtime 公共分类。

模型注册分层为：

```text
Intent Profile
      |
      | workload-specific rating
      v
Model Profile -> Model Build -> Deployment -> Provider -> Node
```

每个 Model Profile 按 Intent 独立记录 `basic/general/advanced/frontier`，并标记为 `provisional` 或带 eval profile 的 `benchmarked`。参数量、provider、placement 和 reasoning effort 均不能自动推导能力等级。

请求将以下维度正交表达：

- `quality_floor`：候选模型必须达到的 workload 等级；
- `reasoning.effort`：选中模型后投入的计算强度；
- `placement`：`local/trusted_node/cloud` 的允许集合；
- `prefer`：允许集合内的位置偏好；
- deadline、cost、priority 和 fallback：服务约束。

Deployment 的 `resource_class` 独立描述运行负担，用于在多个均满足质量下限的本地候选间优先轻量制品；它不得参与能力评级。

## 影响

- 删除 `app.summary`、`local.general` 等应用或位置耦合 route；
- 删除没有 workload/eval 含义的全局质量数字；
- fallback 不得突破 Intent、feature、quality floor、placement、deadline 或预算；
- 本机、可信 DGX 和云端模型使用同一候选模型，不因位置被预设能力上限。

## 验证

- 弱本机模型不能满足 `reasoning.deep + advanced`；
- `local_only` 不会因云模型能力更高而越界；
- `app.summary` 被视为未知 Intent；
- Intent 名不会作为物理模型名发送给 provider。
