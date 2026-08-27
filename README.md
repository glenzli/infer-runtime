# infer-runtime

[中文](README.md) · [English](README.en.md)

`infer-runtime` 是一个面向本机应用的 AI 推理运行时。Consumer 提交 Intent、能力下限、
延迟、位置、隐私和回退约束；Runtime 负责选择 Provider 与 Deployment，并处理排队、配额、
模型驻留、取消、故障切换和执行记录。

项目只统一控制平面。文本、音频和视觉能力保留各自的类型化协议，不被包装成通用的
JSON 或 Tensor 接口。`infer-runtime` 也不提供模型市场、Agent 编排或具体 AI 产品功能。

## 当前实现

| 范围 | 已实现 | 状态 |
| --- | --- | --- |
| 文本 | Responses 风格的普通请求与 SSE、本地加密 background；本地、云端和订阅式 Provider | `infer.responses@20260812.1` 已冻结；订阅桥仍为 experimental |
| 音频 | 转写、强制对齐、语音合成、声音生成、声音克隆、AudioSet 事件检测和 audio-text retrieval | 基础音频与事件检测合同稳定；流式和部分生成、检索能力仍为 experimental |
| 视觉 | ONNX/Core ML Provider；人脸检测与向量、图文向量、点击式主体分割和人脸解析 | 当前均为范围受限的 experimental 能力；部分权重仅限研究用途 |
| 调度 | Intent 路由、优先队列、deadline、cancel、retry/fallback、熔断和配额 | 已形成可运行闭环 |
| 本地资源 | Ollama/ONNX 生命周期、压力采样、加载基准、eviction 建议和维护租约 | 自动 eviction 默认关闭 |
| 接入与运维 | per-App ACL、managed credential、Infra Discovery、状态接口和 Web Console | 供本机 Consumer 与 Operator 使用 |

具体模型和 Provider 由本机配置决定。仓库不附带模型文件，也不维护开发机上的模型清单。
当前版本仍是开发预览；发布门槛和实验能力的进度见 [ROADMAP.md](ROADMAP.md)。

![Infer Console 运行总览（合成演示数据）](docs/images/console-overview-demo.png)

*截图中的 Provider、Deployment、指标和实例身份均为合成演示数据。*

## 快速启动

基础开发环境需要 Rust。示例配置包含 Ollama 的配置结构，音频、ONNX、云端和订阅式
Provider 均为可选项。

```bash
cp config/infer.example.toml config/infer.toml
cargo build -p infer -p inferd
target/debug/infer console --spawn
```

Console 默认打开 `http://127.0.0.1:8790/`，并启动它所管理的 `inferd`。首次运行时会生成本机
`local-operator` credential，无需把 API key 写入配置。Console 不接管外部启动的 daemon。

也可以单独启动 daemon：

```bash
cargo run -p inferd
```

然后从另一个终端提交开发请求：

```bash
cargo run -p infer -- run \
  --input '解释一下这段设计的核心取舍。' \
  --stream
```

CLI 默认连接 `http://127.0.0.1:8787`，可通过 `--server` 或 `INFER_URL` 覆盖。产品 Consumer
应使用 Infra Discovery，不应依赖固定端口。

可选能力还需要相应的 Provider、Build/Deployment、运行环境、模型文件和 App ACL。Runtime
不会自动下载模型或放宽 placement。完整准备流程见 [运维文档](docs/OPERATIONS.md)。

## Web Console

Console 提供总览、统计、任务、模型与资源、Apps 与访问、日志和配置页面，可用于查看运行状态、
管理模型生命周期、创建 Consumer credential 和应用配置。资源控制与凭证管理只向 Operator 开放；
普通 Consumer 只能访问自己的任务。

浏览器只接收随机生成的 Console session proof，不接收 `local-operator` bearer credential。

![Infer Console 模型与资源（合成演示数据）](docs/images/console-models-demo.png)

## Consumer 接入

本机应用从 [Infra Discovery](docs/CONSUMER_DISCOVERY.md) 选择以下 offer：

```text
protocol  = infer-runtime.consumer-core
versions  = [20260813.1]
binding   = infer-runtime.http-loopback
```

Discovery manifest 提供 service identity、generation 和 endpoint offer；实际可用性仍以连接和
合同握手为准。推荐使用 [`infer-runtime-client`](crates/infer-runtime-client/README.md)，由 SDK
处理发现、generation 变化、token、Core/Capability header 和错误解析。

在 Console 的 **Apps 与访问** 页面为每个 Consumer 创建独立身份，只授予所需的 Intent、
placement、priority、capability、fallback 和成本范围，普通应用保持 `resource_admin = false`。
不要把 `local-operator` token 交给产品应用。Managed token 只在创建或轮换时显示一次，应写入
Consumer 自己的 owner-only secret store。

请求中的 `model` 是 Intent，不是物理模型名：

```bash
curl "$INFER_BASE_URL/v1/responses" \
  -H "Authorization: Bearer $INFER_API_KEY" \
  -H 'Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1' \
  -H 'Infer-Capability-Contract: infer.responses@20260812.1' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "text.summarize",
    "input": "需要总结的内容",
    "metadata": {
      "infer.placement": "local_only",
      "infer.fallback": "none"
    }
  }'
```

程序应根据 HTTP status 和 `error.code` 处理失败，不解析 `error.message`。完整字段、SSE、
background、音频和视觉协议见 [接入文档](docs/INTEGRATION.md) 与
[`consumer-core@20260813.1`](contracts/consumer-core/20260813.1/README.md)。

## 工程边界

- Intent 与物理模型分离；Runtime 根据约束选择 Provider、Build 和 Deployment。
- `local_only`、offline、cloud modality ACL 和 fallback 是硬约束，不会静默放宽。
- prompt、音频、像素、生物 embedding 和 Provider secret 默认不写入普通 Job metadata 或日志。
- Provider 保留各自的生命周期和数据协议，共用 admission、reservation 与资源压力策略。
- Provider probe 需要 Operator 显式执行，可能产生实际费用；自动 eviction 默认关闭。
- 模型、凭据、本机配置和运行状态不进入公共仓库。

## 相关项目

- [Infra Protocol](https://github.com/glenzli/infra-protocol)：本机服务发现合同。
- [Infra Sentinel](https://github.com/glenzli/infra-sentinel)：读取脱敏 status snapshot 的设施观测工具。
- [Shadow](https://github.com/glenzli/shadow)：视觉 Consumer。
- [Symbiont-d](https://github.com/glenzli/symbiont-d)：语音转写 Consumer。

## 文档

| 文档 | 内容 |
| --- | --- |
| [DESIGN.md](DESIGN.md) | 产品边界、领域模型和组件职责 |
| [ROADMAP.md](ROADMAP.md) | 当前阶段与发布门槛 |
| [docs/INTEGRATION.md](docs/INTEGRATION.md) | Consumer 接入和各数据平面示例 |
| [docs/OPERATIONS.md](docs/OPERATIONS.md) | Console、模型资源和后台任务运维 |
| [docs/CONSUMER_DISCOVERY.md](docs/CONSUMER_DISCOVERY.md) | Infra Discovery 合同 |
| [contracts/consumer-core/20260813.1](contracts/consumer-core/20260813.1/README.md) | 当前 Core OpenAPI、fixtures 和机器合同 |
| [docs/DECISIONS.md](docs/DECISIONS.md) | 已接受决策与 ADR 入口 |
| [docs/STATUS_PROTOCOL.md](docs/STATUS_PROTOCOL.md) | 只读状态接口与 Discovery offer |
| [docs/REPOSITORY_BOUNDARY.md](docs/REPOSITORY_BOUNDARY.md) | 源码与本机资产的仓库边界 |

## 开发检查

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

音频、视觉、模型 benchmark 和 background 操作示例集中在
[docs/OPERATIONS.md](docs/OPERATIONS.md) 与 [docs/INTEGRATION.md](docs/INTEGRATION.md)。
