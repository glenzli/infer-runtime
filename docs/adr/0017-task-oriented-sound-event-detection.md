# ADR-0017：通用声音事件检测使用任务级合同与精确 YAMNet Build

- 状态：Accepted
- 日期：2026-08-13
- 决策者：infer-runtime maintainers

## 背景

Echo 在转写没有产生可用文字时，仍需理解雨、列车、鸟、车辆、家居、工具和音乐等真实声音。
“没有 transcript”只说明转写路径没有文字结果，不能推出无人声，也不能作为某个声音不存在的
否定证据。现有 SenseVoice 事件标签只覆盖 Speech、BGM、掌声、笑声、哭声、喷嚏、呼吸和
咳嗽；它适合作为窄域补充，不能冒充通用环境声分类器。

Runtime 已有 task-oriented 音频、Intent → Model Profile → Build → Deployment → Provider、
App ACL、Job/Attempt、25 MiB 临时 payload 和 fail-closed placement 边界。缺少的是一条消费者
可直接调用、且不泄漏物理模型身份的通用事件纵向切片。

## 决定

1. 稳定 Intent 使用 `audio.detect_events`，数据面标识为 `audio.event_detection`，公开入口为
   `POST /v1/audio/event-detections`。该名称符合既有 `<domain>.<action>` 规则，表达任务而非
   YAMNet、AudioSet 或 Echo 产品名；模型替换不改变 Consumer 请求。
2. 请求是严格 multipart，只接受 `model=audio.detect_events`、单个 `file`、可选 JSON-string
   `metadata`/`infer.*`。共享单文件上限保持 25 MiB。服务端固定
   `local_only`、`offline_required=true`、`fallback=none`；调用方尝试放宽时明确失败。
3. 首个真实 Deployment 使用官方 TF Hub `google/yamnet/1` SavedModel、TensorFlow 2.20.0 和
   ffmpeg 单声道 16 kHz 解码。Build 固定下载 archive 的大小 `14242921` 与 SHA-256
   `b80da2a1a56926fb0767205051a200dd7b3beaf3ea1ea126c42a53943996e5e0`，并在加载前验证
   SavedModel、variables 与 521-class map 的逐文件 digest。文件先发布到 Runtime 的
   content-addressed ArtifactStore，再由 typed Provider 解析为只读 Build root；配置和 Consumer
   都不能提供任意模型路径。Provider 为 `yamnet-local`，
   Deployment 为 `yamnet_audio_events_tfhub_v1`；worker serving 时不联网下载。
4. ontology 使用 artifact 内的 AudioSet 521 class map，公开 ID 是稳定 MID（namespace
   `audioset_mid`），不是模型输出 index；class-map SHA-256 为
   `cdf24d193e196d9e95912a2667051ae203e92a2ba09449218ccb40ef787c6df2`。模型 artifact 记录
   Apache-2.0，训练数据记录 CC-BY-4.0，ontology 记录 CC-BY-SA-4.0。安装器只接受上述精确
   archive 和成员，不把未验证候选登记为可用。
5. YAMNet 原生使用 0.96 秒窗口和 0.48 秒 hop，输出 521 个独立 sigmoid score。本政策
   `yamnet-audioset-event-policy-v1` 对每个窗口保留多标签，不用 Top-1 代表整段录音；使用
   3-frame centered median（edge padded）平滑，以 `0.10` 阈值激活并合并同类相邻窗口。
   每窗口最多保留 12 类，最终事件有界为 10000 条。speech class set、阈值、平滑方法与窗口、
   最大音频时长、窗口参数和 ontology revision 全部进入响应和 Build 合同。
6. 响应包含 `events[]` 的 MID、展示标签、start/end、score，以及完整 `coverage`、
   `speech_presence`、`ontology`、`policy` 和模型/运行时/decoder/preprocessing provenance。
   `speech_presence=present` 要求 speech-family 最大 score `>=0.30`；`absent` 只允许在完整覆盖且
   score `<=0.05` 时返回；两者之间或覆盖不完整时为 `unknown`。这是版本化模型证据，不是从
   transcript 缺失推断，也不是真实世界无人声的绝对证明。
7. Provider 输出先按 typed result 校验；control plane 再与选中 Build 的 artifact、license、
   runtime、ontology 和 policy identity 精确比对，漂移时 Attempt 失败。成功仍使用统一 admission、
   queue、Job/Attempt/provenance；音频只存在于请求与 executor 临时目录，Job/日志不保存 payload。
8. Consumer Core `infer-runtime.consumer-core@20260813.1` 保持冻结；新增数据面发布为独立、不可变
   Capability `infer.audio.event-detection@20260813.1`，并通过同日期 generation-scoped Catalog
   增量登记。官方 SDK 只在发现、拉取并校验该精确能力 schema 后发送音频。
9. worker 请求与响应帧有硬上限；调用 future 取消、deadline 或协议失败会丢弃并终止该次
   persistent worker（包括其 ffmpeg 子进程），下次请求只会使用新进程。stderr 不继承到 daemon，
   错误只返回固定代码，不输出音频、临时路径、转写或 token。

## 未选择的方案

- 不把 SenseVoice 的窄标签集合宣称为通用事件检测。未来可在独立 Profile/Build 证明质量后作为
  speech/BGM 人体事件补充，但其输出必须与 ontology、coverage 和证据语义一致。
- 不使用整段 Top-1 分类；它会丢失短暂及重叠事件，也不能说明分析覆盖。
- 本里程碑不实现 CLAP 或 audio↔text embedding。检索空间的相似度不是校准的事件检测分数，
  也不能提供某事件不存在的否定证据。未来若有检索需求，使用独立 Intent、embedding-space
  identity、质量评估和 ADR，不复用本合同。

## 后果与复审条件

- Echo 可在转写为空、失败或业务上不可用时独立调用事件检测，但必须保留
  `speech_presence=unknown`，不能自行把空 transcript 改写为 absent。
- YAMNet 的公开基准 mAP 和 raw sigmoid 分数表明它是广谱轻量基线，不是完成域校准。当前评分
  为 provisional；雨/列车/鸟/车辆/家居/工具/音乐的 Echo 真实样本集、阈值精度/召回、短事件
  边界和混合声音误报仍需持续验收。
- TensorFlow wheel 与系统 ffmpeg 是 Deployment 前置依赖；实际版本进入响应 provenance。顶层
  TensorFlow pin 不是完整 transitive lock，部署 receipt 仍需记录 Python、平台、NumPy 与 ffmpeg。
- 本仓库记录的模型、训练数据和 ontology SPDX 是 operator 声明；只有 license text receipt、摘要和
  审核日期齐全时，Build license status 才能从 `declared` 升为 `verified`。
- 修改 archive、class map、TensorFlow major/minor、预处理、阈值、speech class set 或平滑策略
  必须产生新的 Build/policy/ontology revision，并通过合同与真实 acceptance 后才能 admission。
