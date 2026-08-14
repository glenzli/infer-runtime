# ADR-0018：本机 CLAP 音频-文本检索作为独立、实验性证据空间

- 状态：Accepted for inactive experimental baseline
- 日期：2026-08-15
- 决策者：infer-runtime maintainers

## 背景

Echo 需要把一段有界音频与自然语言查询投影到同一向量空间，用于可重建的检索候选。这不是
转写、摘要、SigLIP 文本证据，也不是 AudioSet/YAMNet 的时间区间事件事实：四者的模型语义、
召回方式与持久化边界不同，不能混用向量或互相补写事实。

Runtime 已有 typed audio worker、ArtifactStore、Job/Attempt、local-only admission 和
payload-free ledger。缺少一条能配对音频和文本、又不把物理 checkpoint、文件系统路径或通用
tensor surface 暴露给 Consumer 的窄 capability。

## 决定

1. 新增两个独立 Intent：`audio.embed`（有界音频）与 `audio.embed_text_query`（自然语言查询）；
   两者共享 `audio.embedding` 数据面，但不是通用 embedding/tensor API。稳定 capability identity
   为 `infer.audio.embedding@20260815.2`，路由为 `POST /v1/audio/embeddings` 与
   `POST /v1/audio/text-embeddings`。它是 additive、experimental capability，不修改 Consumer Core。
2. 请求强制 `placement=local_only`、`offline_required=true`、`fallback=none`；服务端补入这些
   约束，调用方不能放宽。音频是单个不超过 25 MiB 的 multipart 文件，worker 通过固定 ffmpeg
   解码为 48 kHz mono FP32 并拒绝超过 10 decoded seconds 的输入；文本最多 16 KiB，带必填
   `query_revision` 和 BCP-47-shaped `language`。长音频切片、Original 时间轴、索引和 stale
   仲裁都由 Consumer 规划和持有。
3. 第一条本机 Build 是 `laion/clap-htsat-unfused` revision
   `8fa0f1c6d0433df6e97c127f64b2a1d6c0dcda8a`，以 Runtime-managed local ArtifactStore
   的逐文件 SHA-256 manifest 发布。Provider 只读取这个 verified local root；它不在请求时下载、
   不向 Consumer/API 传递权重，也不接受 Consumer 指定模型路径或 physical model。
4. Build 使用 owner-installed Python 3.12、PyTorch 2.8.0、Transformers 4.56.2，强制 Apple
   MPS（`PYTORCH_ENABLE_MPS_FALLBACK=0`）。MPS 不可用、artifact 失配、ffmpeg 不可用、worker
   frame 错误、取消或 deadline 都 fail closed；没有 CPU fallback。persistent worker 在每次取消或
   协议失败后被终止，避免迟到输出和 decoder 子进程残留。
5. 输出始终是 512 维 finite、L2-normalized 向量，度量为 cosine。`embedding_space.identity`、
   artifact-set digest、runtime/precision、请求与实际 EP、fixed preprocessing 和 tokenizer
   identity 都在响应 provenance 中。Job/Attempt/普通日志不存音频、文本、向量、临时路径或
   source/query revision 的内容。
6. `RuntimeDownloadable` / `RuntimeBundled` 的合法性验证只要求 immutable upstream/revision/
   artifact identity；license 字段保留为 operator provenance，而不是“本机下载并提供本地访问”
   的运行时准入门。Runtime 不再因缺少 license-text receipt 阻止这种本机 Build；实际分发、
   再分发和产品法律承诺仍由单独政策决定。

## 实测与能力限制

在 Apple Silicon、强制 MPS、完全离线的 exact Build 上，首次加载约 1.3 s；合成 10 秒音频和两条
文本的首轮共同执行约 5.7 s；warm 10 秒音频平均约 104 ms、四文本 batch 平均约 95 ms。峰值由
PyTorch MPS driver 观测约 1.14 GB。音频与文本都返回 512 维、norm=1。

这不是中文语义检索验收：同一合成正弦音频对英文正确查询的 cosine 为约 0.58，对中文同义查询
仅约 0.14；错误英文查询约 -0.04。故第一条 Build 只能标为 generic English-first、provisional。
请求里的 `language` 是 Consumer 的查询证据，不是语言质量声明；`zh`/`zh-*` 的短查询在 exact
Build 配置了 `audio_text_query_normalizer` 时，先由本机
`ollama_qwen3_5_2b` / `qwen3_5_2b_mlx` 以
`infer.audio.zh-en-short-query@20260815.1` 规范化为 English，再进入 CLAP text tower；响应的
`query_normalizer` provenance 回显该 deployment/build/prompt 与 `zh`→`en`。normalizer unavailable、
输出越界或非 English 时本条查询 fail closed。Echo 不得把它作为中文自然语言
搜索承诺、不得用它替代已有 SigLIP2/YAMNet 证据。语音内容、方言与话语表达优先沿用
ASR、对齐、FTS 和既有文本语义路径；CLAP 只覆盖非语音、无文本或文字不足的原始声音。

## 未选择的方案

- 不把 CLAP 加入 `semantic.embed_text`，不把它的 512d space 与 SigLIP 768d 或其他 embedding
  space 混算。
- 不用 CLAP 分数伪装 AudioSet 事件概率，不以低相似度宣称某声音不存在。
- 不接受任意文件路径、URL、sample rate、tensor map、模型名或 backend 作为 Consumer 参数。
- 不因本机已下载权重就自动扩大 Echo ACL、发布 Deployment、重启 daemon 或允许 cloud fallback。
- 不用音乐专用的中英对齐 checkpoint 替换通用环境声/语音基线；它的训练域不同，须经单独 Build
  和 Echo 样本评估后才可能新增，而不是 silent replacement。
- `mispeech/GLAP` 被明确 deferred/rejected，而非下一阶段候选：即使不质疑其论文结果，它目前
  独立采用与运维生态不足（模型页约 531 月下载、11 likes、2 条社区讨论，官方仓库约 73 stars），
  exact Build 也约 3.43 GB，并要求执行 pinned custom model code（`trust_remote_code`）。本 ADR
  不下载、评测或接入 GLAP。也不选择 10.3 GB 的 MLCLAP checkpoint：公开制品过重且没有同机
  执行证据。
- 中文查询的产品路线不是把 CLAP 伪装成多语模型：待 Echo 选择、安装并验证一个本地 zh→en
  normalizer Build 后，`audio.embed_text_query` 可在该专属请求路径中先做有界查询规范化，再计算
  英文 CLAP text embedding。它不是公共 `text.translate` capability，不接受通用翻译请求，也不
  对 normalizer 不可用时回退到其它模型或云端；该查询分支应返回不可用错误，既有搜索路径继续独立
  工作。

## 激活门槛

在任何 App（包括 Echo）加入 `audio.embed` / `audio.embed_text_query` ACL 前，必须完成：

1. exact artifact manifest、MPS readiness、cold/warm/10-second throughput 和 cancellation/cleanup
   证据；
2. Consumer SDK 对 capability catalog、512d response、space/provenance、未知扩展字段和稳定
   errors 的 contract test；
3. Echo 领域中的英文检索精度/召回评估。中文场景只能在已冻结的本地 zh→en normalizer Build、
   明示其 source/target language 与 Build/version provenance 后走受控查询规范化；不得静默翻译、
   不得开放通用翻译能力，也不得宣称 CLAP 原生支持中文；
4. 明确的最小 App Intent ACL、真实 local-only/offline/no-fallback HTTP E2E，以及 payload-free
   Job/log audit。
