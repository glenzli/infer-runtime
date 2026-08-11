# 迁移到 infer-runtime `0.1.0-candidate.3`

`candidate.3` 是一次刻意的、范围受限的 breaking candidate revision。它只重整 Intent、能力等级、
reasoning effort 和相关 provenance 名称；HTTP endpoint、Bearer credential、App identity、Provider
access class、placement 和 payload schema 均不改变。Consumer contract revision 与 Infra
Discovery document version 独立；当前 publisher 已迁移到 `infra.discovery.registration@20260812.1`。

不要把旧值静默按同名解释。尤其旧 `advanced` 是新 `expert`，而新 `advanced` 是 candidate.3
新增的中间能力层。

## 不变的合同

- Consumer base URL 由 `infra.discovery.registration@20260812.1` 发现；manifest 无 lease，连接失败
  时重读 generation 与 offer；
- binding 仍是 `infer-runtime.http-loopback`，endpoint 仍为 canonical numeric loopback origin；
- HTTP 路径和顶层请求 envelope 不变；`model`、`infer.*` metadata 和 provenance 名称按下文替换；
- 每个 App 继续使用原 managed bearer token，不需要创建、轮换或迁移 secret；
- `app_id`、provider access class、cloud modality ACL、priority、placement、cost、deadline 和 fallback
  的安全语义不变；Intent ACL 和 fallback 枚举值只做下文的一次性名称迁移；
- typed audio/vision endpoint 路径不变；
- Consumer 仍以 HTTP status + `error.code` 分支，并忽略未知响应字段。

## Discovery 与版本选择

Runtime 重启到新构建后发布：

```text
infer-runtime.consumer@0.1.0-candidate.3
binding = infer-runtime.http-loopback
```

迁移期 Consumer 推荐同时识别 candidate.2 与 candidate.3，并根据实际选中的 protocol version 生成
对应 vocabulary；不得把 candidate.2 请求发送给 candidate.3。全部 Consumer 完成迁移并通过 smoke
后，可删除 candidate.2 选择分支和固定 `127.0.0.1:8787` fallback。

如果采用同步维护窗口，也可以先停请求，更新所有 Consumer，再重启 Runtime；凭证不变。

## Intent 映射

| candidate.2 | candidate.3 |
| --- | --- |
| `assistant.general` | `language.respond` |
| `assistant.multimodal` | `multimodal.respond` |
| `vision.understand` | `multimodal.respond` |
| `reasoning.deep` | `reasoning.solve` |
| `vision.review_classification` | `vision.classify_closed_set` |
| `vision.embed_image` | `semantic.embed_image` |
| `vision.embed_text` | `semantic.embed_text` |
| `speech.voice_design` | `speech.design_voice` |
| `speech.voice_clone` | `speech.clone_voice` |

以下 Intent 不变：

```text
text.summarize
text.proofread
image.generate
vision.describe_image
vision.detect_faces
vision.embed_face
audio.transcribe
audio.align
speech.synthesize
```

`language.respond` 和 `multimodal.respond` 只承诺生成响应，不暗示搜索、function execution、shell、
文件访问、computer use 或 agent loop。Web Search 继续通过标准 `tools=[{"type":"web_search"}]`
显式请求，并同时受 App tool ACL 和 Provider capability 约束。

## Capability 字段与枚举

### 请求 metadata

```text
infer.quality_floor     -> infer.capability_floor
```

旧值到新值的安全迁移：

| candidate.2 | candidate.3 |
| --- | --- |
| `basic` | `foundational` |
| `general` | `capable` |
| — | `advanced`（新增中间层） |
| `advanced` | `expert` |
| `frontier` | `exceptional` |

能力 scale identity 为 `20260811.1`，由 `/infer/v1/contract` 的
`capability_scale_version` 返回。等级是按 Intent 评估的固定任务包络，不按模型价格、参数量、
placement 或当前 fleet 百分位计算。

### 路由 policy 与 fallback

```text
quality-first          -> capability-first
allow_lower_quality    -> allow_lower_capability
```

Operator policy order 中：

```text
quality                -> capability
quality_fit            -> capability_fit
```

### Job / explain provenance

```text
quality_grade          -> capability_level
rating_status          -> evaluation_status
routing.quality_floor  -> routing.capability_floor
quality_below_floor    -> capability_below_floor
```

新增候选拒绝原因：

```text
intent_unassessed
```

它表示该 Model Profile 没有当前 Intent 的 rating，因此未获路由准入；不等于物理模型一定无法处理
该模态。物理支持仍由 Build modalities/features 表达。

### Operator TOML

```text
default_quality_floor  -> default_capability_floor
quality_floor          -> capability_floor
grade                  -> level
```

`status = "provisional" | "benchmarked"` 保留，但公开 Job 字段改名为 `evaluation_status`。

## Reasoning effort

candidate.3 的公共集合为：

```text
none | low | medium | high | xhigh | max | ultra
```

省略 `reasoning.effort` 仍表示使用 Deployment/provider 默认值。请求 `ultra` 时，未声明支持的
Deployment 在 dispatch 前以 `reasoning_effort_unsupported` 排除；Runtime 不会降成 `max`。
Capability level 与 effort 仍是正交维度。

## 请求示例

candidate.2：

```json
{
  "model": "reasoning.deep",
  "input": "比较两个方案。",
  "reasoning": {"effort": "high"},
  "metadata": {
    "infer.quality_floor": "advanced",
    "infer.fallback": "none"
  }
}
```

candidate.3：

```json
{
  "model": "reasoning.solve",
  "input": "比较两个方案。",
  "reasoning": {"effort": "high"},
  "metadata": {
    "infer.capability_floor": "expert",
    "infer.fallback": "none"
  }
}
```

旧 `infer.quality_floor` 是未知保留 key，candidate.3 会返回 `400 invalid_request_error`；旧 Intent
同样不会被别名自动接收。这是为了避免把两个不同含义的 `advanced` 静默混用。

## 已登记 Consumer 的最小修改

### Echo

- `vision.embed_text` → `semantic.embed_text`；
- typed endpoint 仍为 `POST /infer/v1/vision/text-embeddings`；
- audio Intent 与 `text.summarize` 不变；token、local-only/offline/no-fallback policy 不变。

### Shadow

- `vision.embed_image` → `semantic.embed_image`；
- `vision.embed_text` → `semantic.embed_text`；
- `vision.review_classification` → `vision.classify_closed_set`；
- 所有 typed endpoint、source revision、biometric/privacy 合同不变；token 不变。

### Symbiont-d

- `assistant.general` → `language.respond`；
- `reasoning.deep` → `reasoning.solve`；
- `audio.transcribe`、`text.summarize`、`image.generate` 不变；token 不变；
- 若提交 capability floor，按上表迁移；若请求 `ultra`，必须接受无合格 Deployment 的确定失败。

### Shape

- `assistant.general` → `language.respond`；
- `speech.synthesize` 不变；token、local-only/offline/no-fallback policy 不变。

## 推荐发布顺序

1. 确认 Runtime 没有 running/queued Job；durable background 若启用，也必须先完成或取消；
2. 发布能同时识别 candidate.2/candidate.3 Discovery offer 的 Consumer；
3. Consumer 根据选中版本切换 Intent、capability metadata 和 provenance 解码；
4. 构建并平滑重启 Console-owned inferd；确认 Discovery generation 更新并发布 candidate.3；
5. 每个 Consumer 使用现有 credential 做一个无敏感 payload smoke；
6. 验证 Job 的 `intent`、`capability_level`、`evaluation_status`、provider/deployment/build provenance；
7. soak 完成后删除 candidate.2 和固定 endpoint fallback。

最小验收应覆盖：

- candidate.3 Discovery 命中；连接失败会重读 registration，generation/offer 变化会触发重选；
- 旧 Intent 和 `infer.quality_floor` 明确失败，而不是误路由；
- `language.respond` 无 tools 时不产生 tool call；
- `reasoning.solve` 的 capability floor 与 effort 分别生效；
- local-only/offline/no-fallback 仍零云触达；
- 所有现有 token 继续通过认证，无 401/403 回归。

## 可直接发送给 Consumer 的通知模板

```text
Infer Runtime 将从 0.1.0-candidate.2 升级到 0.1.0-candidate.3。

这是一次只涉及 Intent/capability/reasoning vocabulary 与 Job provenance 字段的 breaking
migration。Infra Discovery schema、HTTP endpoint/path、App ID、现有 managed bearer token、
typed payload、privacy/placement/fallback 安全语义均不改变，也不需要重新授权或轮换凭证。

请按 docs/MIGRATION-0.1.0-candidate.3.md 完成：
1. 同时识别 infer-runtime.consumer candidate.2/candidate.3，并按实际 offer 选择对应词汇；
2. 替换本应用涉及的 Intent 和 infer.capability_floor；
3. 更新 capability_level/evaluation_status/routing.capability_floor provenance 解码；
4. 保持未知响应字段可忽略，错误仍按 HTTP status + error.code 处理；
5. 告知 Runtime 侧可以切换后，使用原 token 完成一次无敏感 payload smoke。

不要把 candidate.2 请求发送给 candidate.3，也不要为本次迁移创建或轮换 token。
```
