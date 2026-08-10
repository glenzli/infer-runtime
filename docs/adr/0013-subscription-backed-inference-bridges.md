# ADR-0013：订阅式推理桥接使用 Provider 模型组与显式 App 授权

- 状态：Accepted（experimental Codex slice）
- 日期：2026-08-10
- 决策：D-012

## 背景

Codex App Server、Claude Code server 或 Antigravity CLI wrapper 能把用户已有订阅外露为本机
可调用入口。它们与普通云 API 不同：认证和 quota 绑定交互产品账号，传输可能是本机 stdio，
协议通常包含 thread、工具、工作区和审批等 Agent 能力；同时一个入口会暴露一组模型而不是
单个模型。

infer-runtime 只需要其中的标准推理能力，不应复制完整 Agent runtime，也不能因为 socket 在
本机就把隐私、offline 或 local-only 请求送入云端。

## 决定

1. 一个账号/登录会话对应一个 `subscription` Provider。它拥有共享 scheduler、并发与 quota
   边界；不同账号将形成不同 Provider 实例。
2. `model/list` 是动态 inventory。每个显式准入的上游模型仍建立独立 Model Profile、Build 和
   Deployment；reasoning effort 是 Deployment 支持范围。发现不产生准入。
3. Provider 固定 `placement=cloud`。App 默认只允许 `standard` access class，必须显式授权
   `subscription` 才能把其 Deployment 纳入 Candidate Plan。已同时获得多种 access class 的 App
   可以用 `infer.provider_access_class=standard|subscription` 把单次请求硬性收窄到其中一类；该字段
   不能授予 App ACL 之外的权限。
4. 首个 Codex adapter 是独立 JSON-RPC semantic owner，不进入 HTTP Responses adapter 的条件
   分支。初始纵切只翻译 text/non-streaming；ADR-0014 后续在同一边界内增加有界 text/image 输入
   和文本 unary/SSE，仍只保留 instructions、reasoning-effort 的无状态 Responses 子集。
5. 每次调用使用空 ephemeral workspace、read-only sandbox、`approvalPolicy=never`，并关闭
   shell、unified exec、plugins、apps、multi-agent、computer use、web search、history 和 memory。
   事件中出现 command、file change、MCP、dynamic tool、web search、delegation 等 item 时失败。
6. 公开 response 只包含规范化文本、usage 和 Runtime Job/Attempt provenance；账号 rate-limit
   等信息只允许进入脱敏 operator telemetry，不进入普通 Consumer response/log。

## 为什么不是 Provider Group 新实体

现有 Provider 已经拥有 endpoint/session、scheduler、quota 与健康边界，Deployment 已经表达
同一 Provider 下的多个可执行 Build。再增加 Provider Group 会重复这层关系。动态 inventory
只需要展示 `admitted`，Registry 仍由版本化配置控制。

同一 Provider 下的 Deployment 继续按 Intent rating、有效 `quality_floor` 与 `reasoning.effort`
独立筛选。普通策略使用 `quality_fit` 选择“刚好达到门槛”的 grade，避免 general 请求无意升级到
frontier；只有 `quality-first` 的 `quality` 排序明确追求最强合格模型。reasoning effort 始终是在
选定 Deployment 后交给上游的计算档位，不承担模型选择职责。

## 首版取舍

首版为每个 Attempt 启动一个可在 drop 时终止的 App Server 子进程。代价是启动延迟与重复
`model/list`，收益是取消、deadline、协议错误或提示污染时可以丢弃整个进程。持久连接只有在
turn interrupt、并发 multiplex、崩溃恢复、credential/session rotation 和状态隔离均有合同
测试后才允许替换。

## 后果与复审门

- 上游新增模型不会自动增加 Runtime 能力；需要 Build、评级、Deployment 和验收证据。
- `local_only`、`offline_required=true` 天然排除该 Provider；fallback 也不能突破 access class。
- 当前支持收窄的文本 SSE 与有界图片输入；仍不支持 tools、conversation、durable background 或
  精确订阅窗口结算。图片 cloud egress 必须通过 ADR-0014 的独立 App ACL。
- 第二种订阅 bridge 出现时复审共性；在此之前不设计万能 CLI/Agent schema。
- 若 App Server 无法提供先于副作用的工具事件或可靠终止，标准推理 bridge 必须保持禁用。

## 验证

- fake JSON-RPC server：model group、admitted 标记、usage normalization、非推理 item fail closed；
- routing：standard-only App 得到 `provider_access_not_allowed`，显式 subscription App 才能准入；
- request narrowing：双授权 App 指定 `infer.provider_access_class` 后，另一 access class 以
  `provider_access_class_mismatch` 排除；越权请求在 planning 前拒绝；
- quality routing：`quality_fit` 选择最低充分 grade，`quality` 仍选择最强合格 grade；
- 受控订阅环境验证：`model/list` 发现模型组，一次低投入非流式文本调用成功；
- 后续：真实 stream/image cancel 与慢消费者 soak、quota exhaustion、process crash、model
  disappearance/upgrade drift、第二种 bridge。
