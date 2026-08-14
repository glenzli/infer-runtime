# Infer Runtime Capability Catalog `20260813.1`

Catalog schema：`infer-runtime.capability-catalog@20260813.1`。

它回答“这个 endpoint 实现哪些类型化数据平面合同”，不回答“某个物理模型当前是否驻留”，也不
替代 App ACL、routing candidate、Deployment inventory 或 provider health。

Consumer 选择某个条目的精确 `id@schema_version` 后，在该能力请求上发送
`Infer-Capability-Contract: <id>@<schema_version>`。Runtime 在鉴权、请求解析和 Provider dispatch
之前精确校验；不能用日期大小、前缀或版本范围推断兼容。

```json
{
  "schema": "infer-runtime.capability-catalog",
  "schema_version": "20260813.1",
  "core_contract": "infer-runtime.consumer-core@20260813.1",
  "capabilities": [
    {
      "id": "infer.responses",
      "schema_version": "20260812.1",
      "stability": "stable",
      "schema": {
        "format": "openapi-3.1",
        "url": "/infer/v1/capability-schemas/infer.responses/20260812.1/openapi.json",
        "sha256": "<64-lowercase-hex>"
      },
      "routes": [
        {"method": "POST", "path": "/v1/responses", "execution_modes": ["unary", "server_stream"]}
      ]
    }
  ]
}
```

## 首次冻结的能力身份

| Capability id | schema version | 主要入口 | stability |
| --- | --- | --- | --- |
| `infer.responses` | `20260812.1` | `/v1/responses` | stable |
| `infer.audio.transcription` | `20260811.1` | `/v1/audio/transcriptions` | stable |
| `infer.audio.event-detection` | `20260813.2` | `/v1/audio/event-detections` | stable |
| `infer.audio.embedding` | `20260815.2` | `/v1/audio/embeddings`、`/v1/audio/text-embeddings` | experimental |
| `infer.audio.alignment` | `20260811.1` | `/v1/audio/alignments` | stable |
| `infer.audio.speech` | `20260811.1` | `/v1/audio/speech` | stable |
| `infer.audio.voice-clone` | `20260811.1` | `/v1/audio/voice-clones` | experimental |
| `infer.audio.transcription-stream` | `20260811.1` | `/v1/audio/transcriptions/stream` | experimental |
| `infer.vision.face-detection` | `20260811.1` | `/infer/v1/vision/face-detections` | experimental |
| `infer.vision.face-embedding` | `20260811.1` | `/infer/v1/vision/face-embeddings` | experimental |
| `infer.vision.subject-segmentation` | `20260813.1` | `/infer/v1/vision/subject-segmentations` | experimental |
| `infer.vision.subject-segmentation-soft-mask` | `20260814.1` | `/infer/v1/vision/subject-segmentations/soft-mask` | experimental |
| `infer.vision.image-embedding` | `20260811.1` | `/infer/v1/vision/image-embeddings` | experimental |
| `infer.vision.text-embedding` | `20260811.1` | `/infer/v1/vision/text-embeddings` | experimental |
| `infer.vision.image-description` | `20260811.1` | `/infer/v1/vision/image-descriptions` | experimental |
| `infer.vision.classification-review` | `20260811.1` | `/infer/v1/vision/classification-reviews` | experimental |
| `infer.text.embedding` | `20260812.1` | `/infer/v1/text/query-embeddings`、`/infer/v1/text/document-embeddings` | experimental |
| `infer.text.rerank` | `20260812.1` | `/infer/v1/text/rerank` | experimental |
| `infer.document.ocr` | `20260812.1` | `/infer/v1/documents/ocr` | experimental |
| `infer.raw-foundation` | `20260811.1` | `/infer/v1/raw/foundations*` | experimental |

Job/Explain 和具名路由是 Core 的共同控制面语义，不作为 capability 重复登记。新的图像生成或
图像编辑能力只有在真实 provider/Build/ACL/E2E 通过并拥有自己的
schema 后才进入 catalog。登记条目不等同于本机一定存在候选 Deployment。

## 版本规则

- Consumer 解码器忽略未知 response 字段；但字段一旦进入机器可读 schema，必须发布新的 capability
  日期版本。既有 OpenAPI artifact 和 digest永不原地修改。
- breaking request/response 语义：只升级该 capability 日期版本。
- 新 capability：Catalog 动态增加独立条目，Core 不升级；老 SDK 忽略不支持的条目。
- Job/Auth/Discovery/Error 的共同 breaking change：才升级 Consumer Core。
- SDK crate 使用 SemVer；SDK minor 可新增 capability 模块，SDK major 仅用于 Rust API breaking。

迁移窗口内，同一个 capability `id` 可以出现多条 Catalog record，但每条 record 的
`schema_version` 必须恰好标识一个版本，并拥有自己的 URL、digest、routes 与 stability。旧、新版本
可声明相同 route；HTTP admission 根据请求的完整 `id@version` 选择实现。禁止一条 record 同时列出
多个版本，因为单个 `schema` 引用无法同时证明多份不可变制品。SDK 按自身有序支持列表取第一个精确
交集，不比较日期大小；没有交集则在发送 payload 前失败。

每个登记版本必须由机器可读 `schema` 引用闭合：URL 指向当前 Runtime 实际服务的能力专属不可变
制品，SHA-256 必须与响应 bytes 精确一致，而且 Catalog 的每一条 route/method 都必须存在于该
OpenAPI。能力制品只包含本能力 routes 及其传递依赖的 components；共同鉴权与 error envelope 仍由
Core OpenAPI 拥有，不复制进能力制品。能力制品不能引用整份 Core OpenAPI，否则新增无关能力或
Core error code 会改变所有既有能力的摘要，破坏独立版本身份。
缺少 schema、route、SDK/fixture 或稳定错误集合时不得登记为 stable；CI 对缺失项 fail closed，不能
静默跳过。SDK 在发送业务 payload 前读取并按完整 Discovery endpoint/generation 缓存 Catalog，
实际拉取能力 OpenAPI bytes 并校验 SHA-256，再使用自己的有序支持列表与 `schema_version` 做精确
交集；无交集不发业务请求。

Core OpenAPI 位于 `contracts/consumer-core/20260813.1/openapi.json`，能力专属制品位于
`contracts/capabilities/<id>/<version>/openapi.json`。两者由
`tools/generate_capability_schemas.py` 从不对外发布的 aggregate schema source 机械提取；运行后若
既有能力摘要变化，必须升级该能力版本或还原不兼容修改，不能只更新摘要常量。新增能力允许改变
aggregate source 和 Catalog 内容，但不得改变既有 Core/Capability 制品摘要。Catalog 本身是
generation-scoped 的能力清单，不是所有能力 schema bytes 的合并身份。

冻结前用 `python3 tools/generate_capability_schemas.py` 生成，之后用
`python3 tools/generate_capability_schemas.py --check` 做只读一致性检查。发布后的日期目录不允许再由
普通生成覆盖；任何 byte 变化必须先创建下一日期版本，再并行登记旧、新 record。
