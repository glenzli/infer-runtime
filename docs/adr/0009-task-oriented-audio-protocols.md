# ADR-0009：音频按任务协议族归并，并以本地常驻 executor 执行

- 状态：Accepted
- 日期：2026-08-08
- 决策者：Project Owner / infer-runtime maintainers

## 背景

本机已经缓存 Qwen3 ASR、ForcedAligner 和三种 Qwen3 TTS 权重。它们共享音频资源治理需求，但输入、输出和必需参数不同：转写接收文件并返回文本，强制对齐还要求既有 transcript，普通 TTS 接收文字与音色，VoiceDesign 需要自然语言声音描述，Base 声音克隆需要参考音频与参考文本。

把这些能力塞入 Responses 会制造失真的大一统 schema；为每个模型创建 endpoint 又会把物理部署泄漏为公共合同。

## 决定

公共 API 按稳定任务 Intent 与数据合同归并：

- `audio.transcribe` → `/v1/audio/transcriptions`；
- `audio.align` → `/v1/audio/alignments`；
- `speech.synthesize`、`speech.voice_design` → `/v1/audio/speech`；
- `speech.voice_clone` → `/v1/audio/voice-clones`。

这些数据面共享现有 App admission、Intent/Model/Build/Deployment registry、placement/quality constraints、provider queue、deadline、Job、cancel、metrics 和 explain。物理模型仅存在于 Build/Deployment。

本机执行采用进程外、常驻的 JSON-lines MLX worker。worker 懒加载模型，默认最多缓存一个，以免五个音频模型同时占用统一内存。request id 隔离 deadline/cancel 后可能迟到的 worker 响应。

上传文件限制为 25 MiB。音频 payload 不进入 Job metadata；executor 为每次调用创建临时目录，拥有输入、参考音频和输出的完整生命周期，返回后清理。精确 snapshot 路径和 HF offline 模式保证本地 deployment 不在请求时隐式下载。

## 后果

- 新模型若满足既有任务合同，只需增加 Profile/Build/Deployment 和 adapter 能力，不增加 endpoint。
- 新任务若拥有不同 payload 生命周期或失败语义，应增加协议 owner，而不是扩张 Responses 或堆 optional 字段。
- 当前文件协议本身不承诺实时音频。后续 ADR-0014 已增加类型化 PCM TTS server-stream 与
  commit-redecode ASR duplex；原生低延迟增量 ASR 仍需要独立模型能力与 SLO 证据。
- SenseVoice 只有 Model Profile/Build，没有 Deployment；安装并验证 FunASR executor 前不可路由。
- 本地单模型缓存只是局部保护；全局资源感知、跨 provider eviction 和 anti-thrashing 仍属于 M4。

## 验证

受控集成链路已覆盖语音生成 → ASR 转写 → ForcedAligner，以及 voice design 和 voice clone。
克隆结果再次经 ASR 回读，与目标文本一致。ADR-0014 之后又验证了原生 generator 能连续产生
多个 PCM chunk；默认 CI 仍使用不加载真实权重的合同与路由测试。
