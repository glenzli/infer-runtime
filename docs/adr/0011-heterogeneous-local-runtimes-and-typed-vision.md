# ADR-0011：异构本地执行族共享控制平面，视觉能力保持类型化协议

- 状态：Accepted（ONNX foundation + 同步 face/SigLIP/QwenVL typed slices；视觉 durable 与通用资源池仍分别 gated）
- 日期：2026-08-11
- 决策者：infer-runtime owner；由 typed vision Consumer 需求与受控 Provider 验证触发复审
- 关联：D-106、D-107、D-108、D-109、D-110、ADR-0008、ADR-0009、ADR-0010

## 背景

Shadow 计划在 macOS/Windows 上增加本地照片 AI。当前候选包括 ONNX 形式的人脸检测、
人脸向量和图片向量模型，Ollama 中已有的 QwenVL，以及现有 MLX audio worker。若 Shadow
自行拥有 ONNX Session、队列和内存策略，而 infer-runtime 只管理 Ollama/MLX，就会在同一
Node 上形成多个互不知情的资源 owner：每一方都可能认为资源充足，同时装载重模型，并各自
实现取消、迟到结果、审计和模型身份。

现有设计已经接受以下基线，不需要为 ONNX 重新发明：

- ADR-0008 的 Intent、Model Profile、Model Build、Deployment、Provider、Node 身份链；
- ADR-0009 的“控制统一、数据分型”，以及 executor 保留原生生命周期；
- M2/M3 的 Job/Attempt、优先级、deadline、取消、reservation、审计和 Candidate Plan；
- M4 的 Resource Manager、模型 residency、load/unload、reload benchmark、eviction safety
  和 anti-thrashing。

本 ADR 先接受了严格收窄的 P0：ONNX Session owner、内容寻址 Build store、通用 native
controller 与同步 `vision.detect_faces` 类型化数据面；第二次复审按独立 SensitiveBiometric
边界接受 SFace 人脸向量。第三次复审接受 SigLIP 2 image/text encoder 的同步跨模态 embedding
slice；第四次复审接受 QwenVL 的有界描述/关键词与闭集分类复核 typed slices。它们仍不改变
冻结的 v0.1 Consumer contract；所有视觉路由列为 experimental。视觉 durable background、
通用 Resource Pool、Windows tolerance 与 stable promotion 继续保留独立门槛。

## 决定

### 统一控制平面，不统一 payload schema

ONNX、Ollama 与 MLX execution 继续共享：

- App、Job/Attempt、priority、deadline、Candidate Plan 和 admission；
- budget/capacity/lifecycle reservation、取消、终态仲裁和迟到结果隔离；
- Model Profile/Build/Deployment 身份、运行 provenance、metrics 和 audit；
- Node 级资源压力与 residency policy。

它们不共享万能 tensor-map 或大而全的 JSON payload。普通应用只调用稳定、类型化的能力
协议；tensor 名称、shape、dtype、预处理和输出解释由 provider 内的 capability adapter
拥有。管理员 benchmark/debug surface 可以固定 deployment 或采集原生诊断，但仍不能绕过
App、隐私、placement、reservation 和审计硬限制。

### ONNX 是新的本地 Provider/执行族，不进入核心调度器

候选结构为：

```text
ONNX Provider Runtime
  Session Registry
  artifact/build verification
  Session create / warm / drop
  Execution Provider selection and actual-route reporting
  residency estimate / reload benchmark / native error mapping
  cancellation and late-result isolation

Typed Capability Adapters
  YuNetAdapter
  SFaceAdapter
  DinoV2Adapter
  SigLip2Adapter
  future model-specific adapters
```

公共 ONNX provider owner 只管理 Session 和执行生命周期。每个 adapter 独占其预处理、tensor
编码、输出解释和模型语义。Provider 回答“如何执行/装卸”，Resource Manager 回答“此刻是否
允许”，Router/Scheduler 继续回答“选哪个合格 Candidate/何时轮到”。核心调度器不得依赖
ONNX tensor 或 Execution Provider 专有类型。

### 视觉使用独立类型化数据平面

首个纵向切片选择 `vision.detect_faces`，数据平面为 `vision.face_detection`，实验路由为
`POST /infer/v1/vision/face-detections`。第二个切片选择 `vision.embed_face`，数据平面为
`vision.face_embedding`，实验路由为 `POST /infer/v1/vision/face-embeddings`。它接收同一
输入像素坐标系中的原图与命名五点，不接受 tensor、物理 backend 或 Consumer 自对齐 crop。
第三个切片开放 `vision.embed_image` / `vision.image_embedding` 与 `vision.embed_text` /
`vision.text_embedding`，实验路由分别为 `POST /infer/v1/vision/image-embeddings` 与
`POST /infer/v1/vision/text-embeddings`。两者由同一 SigLIP checkpoint 的 image/text encoder
满足，必须返回相同 embedding-space identity。第四个 slice 开放 `vision.describe_image` /
`vision.image_description` 与 `vision.review_classification` / `vision.classification_review`，实验
路由分别为 `POST /infer/v1/vision/image-descriptions` 与
`POST /infer/v1/vision/classification-reviews`。它们通过 Ollama QwenVL adapter 执行，但公共
合同不暴露 chat schema、物理 tag 或自由 tensor map。

候选合同方向：

- Face detection 返回 boxes、five-point landmarks、confidence 和 model/preprocess provenance；
- Face embedding 返回 normalized vector、quality/eligibility evidence、embedding-space identity
  和 model/preprocess provenance；
- Image embedding 返回 vector、embedding-space identity、input artifact/source revision 和
  preprocess provenance；
- Text embedding 返回同一 space 的 vector、query revision、language 与 tokenizer provenance；
- Image description 返回有界 description 与 keyword suggestions，均只是 Consumer 可重建
  proposal；
- Classification review 只在 Consumer 提供的版本化闭集内返回 `matched|none|uncertain`，不把
  模型自报概率伪装成校准 confidence，也不直接形成用户事实；
- Caption/复杂视觉理解由 Ollama VLM typed adapter 满足，不因新增 ONNX provider 改写
  Responses/Ollama 的原生执行路径。

普通应用不能传递 ONNX tensor 名、物理 backend 或任意模型路径。

### Model Build 必须固定执行语义与许可事实

ONNX Build manifest 至少需要版本化并校验：

- exact artifact/checkpoint/export digest、ONNX opset 和 export revision；
- 输入/输出 tensor contract；
- orientation、resize/crop、color space、RGB/BGR、layout、mean/std 和 dtype；
- 人脸对齐模板、关键点顺序、输出归一化和距离定义；
- tokenizer/vocabulary 及其 digest；
- 允许的 Execution Provider、precision、fallback policy 和已验证平台；
- code、weights、training data 的已知许可与 redistribution facts。

这些字段共同决定 Build 和 embedding-space identity。不同 space/revision 的向量不得直接比较。
模型输出是可重建证据；Shadow 中用户确认的人物合并、拆分和命名仍是应用持久事实。

跨平台核心模型来源以我们可固定 artifact、预处理、输出合同和版本身份的 Build 为准；Apple
Vision 不作为 macOS/Windows 共同的模型语义来源。macOS 的 Core ML、Windows 的 WinML 或
其他 ONNX Runtime Execution Provider 可以作为候选执行后端，但必须通过同一 Build 合同、
实际 route disclosure 和平台 tolerance 验证。

### 责任边界

Shadow 继续负责照片选择、viewport/background 到 priority 的映射、source/Recipe revision、
stale-result 判定、Catalog 派生证据、用户事实和人脸数据产品政策。infer-runtime 负责执行
时机、合格 Deployment、residency、跨 App/provider 背压与公平性、Attempt/取消/审计和运行
provenance。模型 provider/adapter 负责 runtime 调用、预处理、tensor/postprocessing、原生
生命周期和错误归类。

## 已关闭的 P0 决策与剩余开放项

### OD-1：首个视觉纵向切片（已关闭）

选择 `vision.detect_faces`：YuNet 制品小、结果可解释、不产生持久 biometric vector，能够先
验证公共 Session/Build/取消/资源边界。该切片稳定后，SFace 作为第二个独立实验切片开放：
Runtime 按固定 OpenCV SFace 五点模板执行 112×112 similarity alignment，产生有限的 128 维
向量并 L2 归一化；响应绑定 exact Build/artifact/postprocess space identity。

第三个切片由 Shadow 的跨模态检索需求触发：SigLIP image encoder 单独不能满足文本查询，
因此 image/text encoder 必须一起交付，并绑定同一个 exact checkpoint、preprocess/tokenizer
与 768d space。它仍是两个分型请求合同，不把任意 tensor 或通用 embedding API 暴露给应用。

### OD-2：Node Resource Pool 的最小合同（同步 SigLIP 复审后仍不扩张）

P0 选择“不预建通用多资源池”：YuNet/SFace 属于 light deployment，继续使用 per-provider
有界队列、全机 pressure、统一 deployment lifecycle reservation 与通用 `NativeModelController`。
Resource Manager 已不再硬编码 Ollama，ONNX Session 与 Ollama native lifecycle 由同一 owner
仲裁。SigLIP 的真实 release 实测显示 image Session 增量约 425.5 MiB，image+text 合计约
1,916.4 MiB；两个 deployment 因此登记为 heavy，并继续受全机 pressure、provider capacity
和 lifecycle reservation 约束。这个数字证明了 admission/residency 需要保守治理，但当前
同步、串行 Consumer slice 尚未证明核心必须一次原子预留多种资源。以下两种方案的完整比较
推迟到 background SigLIP 与 interactive QwenVL/MLX 的真实混合负载、视觉 durable 或跨节点
容量出现时：

1. 保持 provider 本地并发/驻留 owner，仅增加统一内存压力和 admission estimate；
2. 增加显式 Node Resource Pool，例如 system memory、unified/GPU memory、CPU slots、
   accelerator/device queue 和 resident-model budget。

评审必须定义多资源原子 reservation、估算误差、动态回收、未知容量的 fail-closed 行为，
以及它与 M5 远程 Node capability/resource 声明的共同 schema。不能仅因存在多个 runtime 就
提前建设通用集群调度器。

### OD-3：视觉与 SensitiveBiometric payload ownership（同步 face/semantic/understanding 已关闭，引用与 durable 开放）

同步检测与向量只接受 20 MiB 内的 JPEG/PNG multipart，服务器强制 `local_only`、
`offline_required=true`、`fallback=none`；像素只存在于请求和 executor 内存。SFace embedding
标记为 `sensitive_biometric`，只进入同步响应，不进入 Job、默认日志、audit 或 durable spool。
调用必须携带 `source_revision`，结果原样绑定该 revision，最终是否发布及 stale 判断由 Consumer
所有。持久输入引用、向量持久化策略与视觉 durable 仍需新的独立决策。

SigLIP image 路由只接受 Consumer 已做 orientation normalization 的 display-sized JPEG/PNG，
并要求精确 orientation 标识；text 路由接受有界查询、`query_revision` 和可选语言标签。图片、
查询文本与 768d 向量同样只存在于同步请求/响应，不进入通用 Job metadata、日志、audit 或
文本 durable spool。图库队列、checkpoint/resubmit、索引和 stale-result 仲裁由 Shadow 所有。

QwenVL typed routes 复用相同的 raster/source revision 下限。Consumer 提供的闭集和模型生成的
描述、关键词、分类 proposal 只存在于单次请求/响应，不进入通用 Job metadata、默认日志、
audit 或 durable spool；用户接受、关键字写入和 adaptation feedback 继续完全由 Shadow 所有。

视觉 durable background 不能默认复用 ADR-0010 的文本 spool。只有在重新定义 payload 类型、
大小/引用 owner、加密、恢复幂等、结果发布仲裁和删除语义并完成隐私评审后，才能单独启用。

### OD-4：Execution Provider fallback 与跨平台数值门槛（macOS 当前 Builds 已关闭）

Session 先以请求 EP 严格禁用 CPU fallback 创建；失败时，只有 Build 与 runtime 同时显式允许
CPU 才重新创建一个纯 CPU Session。结果记录 requested/actual EP、稳定 fallback reason、
precision、runtime version 和 Build/preprocess identity，不把混合或失败 Session 伪装成 Core
ML。实测 ONNX Runtime 1.27.0 下 YuNet 与 SFace 的严格 Core ML 均不能完全接管，因此当前实际
route 为已披露的 CPU；Windows EP 与跨平台 tolerance 仍待验证。

SigLIP image/text 当前 graph 在相同组合下也不能完整交给 Core ML：严格请求后分别约 17.5 秒、
223.5 秒才回退 CPU。由于探测代价本身不可接受，当前 immutable SigLIP Builds 只允许 CPU，
requested/actual 均记录为 `cpu` 且无 fallback；未来更换 export/ORT/EP 必须形成新 Build，不能
把 fallback 后成功解释为 Core ML 支持。

验收不承诺浮点逐位一致；应按任务定义数值 tolerance 和 decision-stability 门槛，例如检测
框/阈值决策、近邻顺序或分类 top-k 的稳定性。超出门槛的 backend 不能借用其他平台的
`benchmarked` 状态。

### OD-5：模型许可、转换与分发

YuNet 2026May 与 SFace 2021dec 已固定 exact official artifact 并形成 experimental/light
Deployment；YuNet 与 SFace 只承诺当前实验 typed slice。取得合法授权的
ArcFace/InsightFace build 与 DINOv2 仍只是候选。SigLIP 2 已固定
official Base patch16 224 checkpoint revision
`75de2d55ec2d0b4efc50b3e9ad70dba96a7b2fa2`、Apache-2.0、checkpoint/graph/tokenizer digest、
opset 17 与 export toolchain，并以 image/text 两个 Build 共享一个 768d space。它只是本机
experimental Deployment；Windows tolerance 和 Shadow 照片域检索质量通过前不得提升为
stable。当前实现也不代表 infer-runtime 提供通用自动下载或模型市场。

QwenVL typed routes 当前使用 operator 已管理的 Ollama 4B/8B Builds；Runtime 记录实际 Build、
physical model 与 runtime provenance，但不复制、下载或重新分发 Ollama 管理的权重。物理 tag、
模板或 prompt revision 改变时必须形成新的可审计 Build/adapter identity，不能继承旧质量结论。

### OD-6：SigLIP 跨模态合同（已关闭）

采用两个 typed Intent/endpoints，而不是万能 embedding/tensor endpoint：image 请求绑定
orientation-normalized artifact 与 `source_revision`；text 请求绑定有界查询、
`query_revision` 和 tokenizer identity。两种响应返回 768d L2-normalized vector、cosine、
完全相同的 space、Build/artifact/preprocess/tokenizer/EP provenance。Consumer 必须按 space
分区索引，并在 identity 改变时重建；Runtime 不持有图库、索引或用户关键字事实。

### OD-7：QwenVL typed understanding（已关闭为 experimental slice）

采用两个职责分离的 Intent，而不是让一个自由生成 endpoint 同时解释分类和描述。
`vision.describe_image` 接受 orientation-normalized display raster、`source_revision` 和输出语言，
返回 bounded short description 与去重 keyword suggestions。`vision.review_classification` 另接受
`taxonomy_revision` 和最多 64 个 id/name/description 类别，只允许返回闭集 id 或
`none|uncertain`。proposal 永远不是 adaptation feedback 或 `AiAccepted`。

Consumer 以 quality floor 选择能力下限，不传 Ollama tag：当前 4B Build 为 basic/standard，
用于 bulk/background；8B Build 为 general/heavy，用于 explicit/review。描述默认 basic，分类
复核默认 general。两条路由都强制 local-only/offline/no-fallback，复用统一 Job/Attempt、
deadline、取消、provider capacity 和全机 pressure；首版同步返回，background 只表达优先级，
不形成 durable photo queue。

Provider adapter 使用 Ollama native vision transport，但只接受 revisioned prompt 的严格 JSON
结果并再次执行闭集/长度校验。真实 provider 验证显示当前 `format` JSON Schema 与 Qwen thinking
组合不能稳定把结果放入 public content，因此本 Build 不依赖该开关；这一兼容细节只存在于
adapter，schema/prompt revision 与实际 physical model 仍进入 provenance。

## 备选方案

- **Shadow 内自建 ONNX scheduler**：轻模型起步快，但形成第二个硬件资源 owner，并复制取消、
  审计、版本与迟到结果语义；在多 App/多 runtime 并存时不可取。
- **用万能 tensor-map 作为公共 API**：便于快速试验，但把模型物理合同泄漏给应用，使 Build
  升级、隐私审查和跨平台兼容都失去边界。
- **把所有本地模型塞进 Ollama 或 MLX**：不能覆盖 ONNX 图和平台 Execution Provider 的真实
  生命周期，也会把执行机制误当作能力协议。
- **现在就建设完整 Resource Pool 和全部视觉能力**：缺少真实负载证据，容易扩大核心调度器并
  推迟 v0.1；本提案明确拒绝。

## 影响

### 正面

- 保留一个跨 App/provider 的调度与资源真源；
- 新视觉模型不会污染文本、音频或核心 Job payload；
- artifact、预处理、embedding space、实际 backend 和结果可以完整追溯；
- macOS/Windows 可以共享模型语义，同时允许平台后端有明确、可测试的数值差异。

### 代价与风险

- Build manifest、provider contract 和真实跨平台矩阵会增加发布成本；
- ONNX Session 内存与实际 Execution Provider 行为可能无法准确静态估算；
- embedding 与人脸数据扩大本地敏感数据面；
- Resource Pool 若没有真实负载驱动，可能成为过度设计。

## 验证方式与当前证据

基础与四组 slice 已通过公共制品 publish/reverify、真实 YuNet CPU Session/推理、官方示例人像框与五点、
SFace CPU load/inventory/unload/对齐/归一化、严格 Core ML 拒绝/CPU fallback disclosure，
以及 face 路由完整 auth → ACL → HTTP → Job/Attempt → provider 端到端测试。SigLIP 另通过
固定 checkpoint/export、image/text graph/tokenizer 内容寻址校验、中文文本、共享 space、
768d L2 normalization、CPU-only 实际 route 和 release 内存/吞吐测量；Shadow ACL 的真实
managed credential 已完成两条 HTTP E2E。QwenVL 另通过 strict multipart/JSON、4B/8B quality
routing、闭集 id 仲裁、取消与本机 native Ollama HTTP E2E；图片、类别、描述和关键词不会进入
Job metadata 或 Console log。当前机器一次真实采样中，4B basic 描述约 32.5 秒（load 2.8 秒）、
8B general 描述约 51.0 秒（load 6.2 秒）、warm 8B 闭集复核约 9.4 秒，8B 驻留约 7.81 GB；
这些数据只用于 provisional admission/SLO 起点，不是跨机器承诺。所有 E2E 都验证通用 Job
响应不包含 image、query、embedding 或生成文本。默认
CI 继续用不依赖权重的合同测试。后续
推广为稳定 Consumer contract 前仍必须通过：

- artifact/preprocess/tensor/tokenizer/vocabulary/license identity 校验；
- Session load/unload 与 lifecycle/capacity reservation race；
- queued/running cancellation、迟到结果丢弃和唯一终态；
- Execution Provider fallback 的实际 route/provenance disclosure；
- macOS 与 Windows 的数值 tolerance、decision stability 和性能/内存基线；
- fake provider 在默认 CI 中覆盖失败语义，不要求模型、GPU 或下载；
- Shadow 只使用类型化协议，在 source/Recipe revision 不匹配时拒绝发布结果；
- local-only/SensitiveBiometric 场景对 cloud fake endpoint 零触达，默认日志/Job metadata
  不含像素、crop 或 embedding。

## 明确非目标

- 不因已批准同步视觉 slices 而批准持久化、视觉 durable background、开放词表自动写入或通用 VLM chat proxy；
- 不自动下载、采购、转换或对外分发候选模型；本机 operator import 只发布已校验制品；
- 不用 ONNX 替代 Ollama VLM 或 MLX audio；
- 不接管 Shadow 的 Catalog、人物命名、聚类确认或照片分析计划；
- 不承诺不同 Execution Provider 浮点逐位一致；
- 不把 infer-runtime 变成模型市场、权重仓库或通用 tensor service。

## 复审触发条件

真实 Consumer 开始批量照片检测/向量与 interactive QwenVL/MLX 形成混合压力、Windows EP、
视觉 background 或跨节点 payload 任一进入实施时重新评审对应子门；不得把本次 Accepted
外推成整个视觉协议族已经稳定。
