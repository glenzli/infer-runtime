# 迁移到 infer-runtime `0.1.0-candidate.4`

candidate.4 的请求词汇相对 candidate.3 是 additive，但当前 Runtime 只发布并接受 candidate.4。
旧租户必须先升级 Consumer；不能依赖 candidate.3 wire 协商继续连接。

## 精确兼容握手

Discovery offer 精确发布 `["0.1.0-candidate.4"]`。合同 probe 和所有受保护 Consumer 请求
必须恰好发送一个 `Infer-Consumer-Contract: 0.1.0-candidate.4`。`GET /infer/v1/contract`
固定返回 `contract_version=0.1.0-candidate.4` 与单元素
`supported_contract_versions=[0.1.0-candidate.4]`。缺少、重复、candidate.3、candidate.2 或未知值
统一返回 HTTP 426、`error.code=consumer_contract_unsupported`，调用方应通知租户升级。

## 请求合同

`model` 继续表示稳定 Intent。具名申请使用且只能使用下列一个 metadata key：

```json
{
  "model": "text.summarize",
  "metadata": {
    "infer.deployment_ids": "preferred_deployment,authorized_backup",
    "infer.fallback": "equivalent"
  }
}
```

- `infer.deployment_ids`：按顺序申请精确 Runtime Deployment；
- `infer.model_profile_ids`：按顺序申请 Runtime Model Profile；
- 两者互斥；每项最多 16 个唯一、path-safe ID；
- 不接受 Build id、Provider id 或 provider-native physical model；
- 请求列表必须是 effective Intent routing grant 的子集；授权在 Job、Provider 和 quota admission
  前完成；
- 已授权目标不存在于当前 Candidate Plan、不可用或不满足硬约束时返回 `no_candidate`；
- 未授权或隐藏目标返回 `route_target_forbidden`，Consumer 不应从 message 推断 inventory；
- `infer.fallback=none` 只允许列表第一项，第一项在 planning 或执行时不可用都不会启用后续项；
  `equivalent` 才按已授权有序候选继续 Attempt；
  placement、offline、provider class、cloud modality、capability floor、cost 与 deadline 永不放宽。

## App 配置合同

App 的 global security ceiling 始终由 `allowed_intents`、provider access class、cloud input
modalities、policy、placement、cost 与其他 override grants 给出。`routing` 不是这些边界的替代品。

```toml
[apps.example.routing]
deployment_ids = ["default_deployment"]
model_profile_ids = ["default_profile"]

[apps.example.routing.intents."text.summarize"]
deployment_ids = ["summary_primary", "summary_backup"]
model_profile_ids = []

[apps.example.routing.intents."reasoning.solve"]
deployment_ids = []
model_profile_ids = []
```

`apps.example.routing` 的 deployment/profile grant 是 routing default，只在某 Intent 没有专属
rule 时使用。Intent rule 一旦存在便完整替换 routing default；空 rule 表示该 Intent deny all，
不可退回 routing default。effective Intent routing grant 仍必须位于 global security ceiling 内，
单次具名请求又必须是 effective grant 的严格收窄。

旧 App 没有 `routing` 时，非具名请求仍使用 capability routing，但它的 Consumer 也必须完成 c4
header 升级；具名请求默认拒绝。Console 普通 App 编辑暂不创建或扩大 routing grants；更新其他
ACL 时保留已有 routing 配置。

## Job / explain 投影

成功 admission 后，Job 的 `constraints.named_route` 与 `routing.named_route` 使用相同的稳定形状：

```json
{
  "kind": "deployment",
  "ordered_ids": ["preferred_deployment", "authorized_backup"]
}
```

`kind` 只能是 `deployment` 或 `model_profile`。`routing.candidates[*].reason_codes` 可包含
`routing_grant_excluded`、`named_route_mismatch` 或 `named_route_fallback_disabled`；Consumer 只应依赖枚举值，不从 message 推断
inventory。普通 capability 请求的两个 `named_route` 字段均省略。
Job 与 explain 另记录 `consumer_contract_version`；background 持久化/恢复不得丢失它。

## 首个 tracked Consumer 形状

受管示例冻结 `text.edit`：纯文本 input/output、默认 `foundational`，首个执行面为 Model Profile
`qwen3_5_4b` / Deployment `ollama_qwen3_5_4b`。请求使用
`infer.deployment_ids=ollama_qwen3_5_4b`，并维持 `local_only`、offline、zero-cost 与
`fallback=none`。这只是 tracked example；不会修改 ignored live App、credential 或 daemon。
