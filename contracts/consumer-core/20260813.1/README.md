# Infer Runtime Consumer Core `20260812.1`

本目录是日期化 Core Contract 的机器可读发布面：

- `openapi.json`：只包含 Core 共同骨架；manifest 发布该文件原始 bytes 的 SHA-256；
- `fixtures/`：Responses、完整 Job provenance、Job 列表与公共错误的严格回归样例。

完整能力库存及其独立 schema URL/SHA-256 以运行时 Capability Catalog 和
`contracts/capabilities/` 为准，官方 Rust SDK 是当前首选绑定。SDK 在业务调用前对 Catalog 做
精确 capability identity 交集。实验性 RAW
句柄租约仍保留专用协议；在其 SDK 模块和激活门槛完成前，不得让 Consumer 自行复刻 Core
Discovery 或把它宣称为稳定公开能力。

协议语义见 [`docs/CORE-CONTRACT-20260813.1.md`](../../../docs/CORE-CONTRACT-20260813.1.md)，
能力目录规则见
[`docs/CAPABILITY-CATALOG-20260813.1.md`](../../../docs/CAPABILITY-CATALOG-20260813.1.md)。

历史 candidate 制品只存在于 Git 历史和 migration 文档；Runtime 不再从旧目录嵌入或发布活动合同。
