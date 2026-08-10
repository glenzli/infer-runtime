# ADR-0014：执行模式正交化与类型化流式数据面

- 状态：Accepted for candidate.2 experimental slices
- 日期：2026-08-10
- 决策者：infer-runtime maintainers

## 背景

单一 `stream: bool` 只能描述“是否逐步返回”，无法表达文本只出不进、TTS 音频只出不进，和
实时转写同时收发。Codex App Server 又同时提供图像输入和文本 delta；把这些能力都压进一个
Responses/Tensor/JSON 万能协议，会让 payload、取消、回压和隐私边界互相污染。

## 决定

1. 控制平面使用三个正交执行模式：`unary`、`server_stream`、`duplex`。Responses 的兼容
   `stream=true` 只映射为 `server_stream`，不成为全系统的唯一流式抽象。
2. Deployment 声明实际验证过的 execution modes。Responses/Codex 的 `server_stream` 还必须
   通过 provider capability；音频的 `server_stream/duplex` 由类型化 executor 合同承载。
3. 文本仍使用 Responses object/SSE；TTS 使用 HTTP chunked `pcm_s16le`；ASR 使用 WebSocket，
   二进制帧输入 PCM，JSON 事件输出 revisioned partial/final transcript。共享的是 Job/Attempt、
   sequence/revision、deadline/cancel、reservation、终态和审计，不共享 payload schema。
4. partial transcript 是完整替换语义，revision 单调递增；只有 final 可视为终态结果。当前
   Qwen3-ASR 没有已验证的原生增量接口，因此 adapter 明确返回
   `transcription_mode=commit_redecode` 和 `stream_semantics=revisable`，不声称原生实时能力。
5. Codex bridge 接受有界 JPEG/PNG data URL 或 HTTPS 图片，并翻译为 App Server 的
   `localImage/image`；文本 delta 翻译为 Responses SSE，`item/completed`/`turn/completed` 仍是
   权威完成依据。工具、Shell、MCP、文件路径、memory 和 agent loop 继续 fail closed。
6. App 的 provider access class 与 cloud payload egress 正交。默认
   `allowed_cloud_input_modalities=["text"]`；即使 App 获得 `subscription`，图片也只有在单独
   授权后才可进入 cloud Candidate。拒绝原因是 `cloud_input_modality_not_allowed`。

## 取消、失败和回退

- `server_stream` 在首个可见输出前可以按既有规则 retry/fallback；之后 Candidate 固定。
- duplex session 从成功 admission 到 final/cancel/disconnect 持有 App、scheduler、quota 和模型
  reservation；断开连接进入唯一 cancelled 终态。
- TTS body 已发送后发生的 provider 错误以连接失败结束，并在 Job/Attempt 中记录；不能改发另一个
  模型的音频。
- WebSocket error 使用稳定 `error.code`，不回显音频、transcript 或 provider 原始 payload。

## 后果与复审门槛

- 当前 `commit_redecode` 能支持边录边提交和修订，但计算量随已提交前缀增长；它不是长期低延迟
  保证。只有原生增量 ASR 或有证据的滑窗/稳定前缀算法通过延迟、重叠词和决策稳定性测试后，才
  新增另一 `transcription_mode`。
- PCM 是首个 TTS streaming 编码，避免需要尾部索引的容器；Opus 等分块编码要作为独立 codec
  capability 加入。
- 图片 URL 只允许 HTTPS；本地路径永不进入公共请求。远程下载策略、image reference 租约和
  durable multimodal payload 仍是独立决策。
- WebSocket ASR、Codex multimodal 和音频 server stream 先保持 experimental；真实 Consumer
  soak、取消/背压、长会话内存和模型吞吐达到门槛后再提升稳定级别。
