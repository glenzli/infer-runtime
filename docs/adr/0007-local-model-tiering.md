# ADR-0007：大型本地模型的能力分层与短总结路由

- 状态：Superseded by ADR-0008
- 日期：2026-08-08
- 决策者：项目 Owner
- 关联：D-007

## 背景

早期垂直链路曾把一个资源等级明显过高的本地模型映射到应用专用 summary alias。这会使
高频、短文本总结占用不必要的本地资源，也会将“可运行”误表述为“适合默认部署”。具体模型、
体积和主机测量属于本地 inventory，不构成本 ADR 的前提。

## 决策

该决策正确确认参数规模不等同于任务能力，但 `local.general` 与应用专用 summary alias 仍耦合了
placement 和应用名。ADR-0008 已用 Intent 与 workload-specific rating 替代这一结构。

## 影响

- 历史判断保留；当前合同以 ADR-0008 为准。
