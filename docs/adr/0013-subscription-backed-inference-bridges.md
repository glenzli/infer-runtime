# ADR-0013：订阅式推理桥接使用 Provider 模型组与显式 App 授权

- 状态：Accepted（experimental Codex + Antigravity execution）
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
2. Provider-native model listing 是动态 inventory：Codex 使用 `model/list`，Antigravity 使用
   `agy models`。每个显式准入的上游模型仍建立独立 Model Profile、Build 和 Deployment；
   reasoning effort 是 Deployment 支持范围。发现不产生准入。
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
7. 第二个 Antigravity adapter 是独立 CLI owner，并采用受信任本机会话：`agy` 直接复用真实
   HOME/Keychain，Runtime 不读取、复制或轮换认证材料。Consumer prompt 经一次性 workspace 文档
   传递，argv 只有固定提示。首版只准入 Gemini 3.6 Flash low/medium/high 物理 variant 的 unary
   text 子集；CLI 自身仍可能维护账号级状态/history，因此该边界只面向显式授权的 experimental、
   非敏感 workload，不冒充 credential/history 隔离。
8. Codex 可额外暴露独立 `image.generate` Intent。它不是通用 tools 放行：请求必须是 unary、
   text-only，并且 `tools` 精确等于单个无参数 `image_generation`；adapter 先读取
   `modelProvider/capabilities/read`，只允许一个 `imageGeneration` item，返回前验证 Base64 PNG、
   20 MiB decoded 上限与 4096/16M pixel 尺寸边界。`savedPath` 不读取，stream、background、编辑和
   任意其他 tool 仍 fail-closed。

## 为什么不是 Provider Group 新实体

现有 Provider 已经拥有 endpoint/session、scheduler、quota 与健康边界，Deployment 已经表达
同一 Provider 下的多个可执行 Build。再增加 Provider Group 会重复这层关系。动态 inventory
只需要展示 `admitted`，Registry 仍由版本化配置控制。

同一 Provider 下的 Deployment 继续按 Intent rating、有效 `quality_floor` 与 `reasoning.effort`
独立筛选。普通策略使用 `quality_fit` 选择“刚好达到门槛”的 grade，避免 general 请求无意升级到
frontier；只有 `quality-first` 的 `quality` 排序明确追求最强合格模型。reasoning effort 始终是在
选定 Deployment 后交给上游的计算档位，不承担模型选择职责。

## 两种 bridge 的共同与独立边界

Provider SPI、动态模型组 DTO、static admission、scheduler/quota/access class、Job/Attempt 与
Responses normalization 仍只由 Codex execution slice 验证。进程协议并不是共性：Codex 继续拥有
JSON-RPC session/thread/item 生命周期，Antigravity 独立拥有 CLI inventory、认证与持久化边界。
没有抽象通用 CLI DSL 或 Agent protocol。

Antigravity 的 `stream-json` 已取得真实 init / user_input / agent_response / terminal result 样本。
动态 inventory 把 Gemini effort 暴露为物理 slug；CLI 会拒绝 effort-suffixed slug 与冲突的
`--effort`，因此 Runtime 用独立 Build/Deployment 把 Low、None/Medium、High 精确映射到
low/medium/high variant，且不额外传 `--effort`。底层模型支持图片或增量输出仍不构成 Runtime
图片/SSE 能力。

CLI 1.1.12 还观察到一个 `step_type="unknown"` 的无内容 bookkeeping frame。它不是通用扩展点：
adapter 仅在字段集合精确为 conversation identity、duration、state、step index 与 step type，且
identity 与 init 一致时忽略；任何额外字段、类型变化、内容或 tool/subagent payload 都回到
fail-closed。CLI 升级后的 wire drift 必须重新取证，而不能扩大该例外。

## 首版取舍

首版为每个 Attempt 启动一个可在 drop 时终止的子进程。Codex 会重复初始化 App Server 与
`model/list`；Antigravity 每次 unary Attempt 也启动一个 `agy` 子进程。代价是启动延迟，
收益是 discovery 不会自动扩大执行能力。持久连接或 inventory cache 只有
在 interrupt、并发隔离、崩溃恢复、credential/session rotation 与 drift 行为均有合同测试后才
允许替换。

## 后果与复审门

- 上游新增模型不会自动增加 Runtime 能力；需要 Build、评级、Deployment 和验收证据。
- `local_only`、`offline_required=true` 天然排除该 Provider；fallback 也不能突破 access class。
- Codex 当前支持收窄的文本 SSE、有界图片输入，以及独立单张 `image.generate`；Antigravity 当前
  只支持 unary text。除该精确 hosted image tool 外，两者均不支持 tools、conversation、durable
  background 或精确订阅窗口结算。图片输入 cloud egress 必须通过 ADR-0014 的独立 App ACL。
- 第二种订阅 bridge 继续证明 Provider inventory 模型足够；无需新增 Provider Group，也不设计万能
  CLI/Agent schema。
- 若 App Server 无法提供先于副作用的工具事件或可靠终止，标准推理 bridge 必须保持禁用。
- Antigravity 的 tool/subagent 事件是检测信号，不是副作用前置授权；首版同时使用 sandbox、
  request-review/无 stdin 与强制无工具提示，发现 tool/subagent 事件即失败。该证据仍不足以晋级
  stable，需继续验证 CLI 版本漂移、真实 tool-denial、history 与取消行为。

## 验证

- fake JSON-RPC server：model group、admitted 标记、usage normalization、文本模式非推理 item
  fail closed；生图模式验证动态 capability、单个 `imageGeneration`、重复 item 去重与 Base64 PNG
  边界；
- fake Antigravity CLI：tabular model group、authentication/error classification、输出上限与无敏感
  诊断错误；
- routing：standard-only App 得到 `provider_access_not_allowed`，显式 subscription App 才能准入；
- request narrowing：双授权 App 指定 `infer.provider_access_class` 后，另一 access class 以
  `provider_access_class_mismatch` 排除；越权请求在 planning 前拒绝；
- quality routing：`quality_fit` 选择最低充分 grade，`quality` 仍选择最强合格 grade；
- 受控订阅环境验证：`model/list` 发现模型组，一次低投入非流式文本调用成功；
- 2026-08-11 受控 Codex 订阅验证：`modelProvider/capabilities/read` 报告 image generation 可用，
  Luna 完成一次无敏感内容的单张生图，adapter 将结果验证为受限 PNG；测试未打印或落盘 payload；
- 真实 Antigravity：登录和 model group 已验证；payload 已移出 argv，真实 NDJSON 证明
  user_input/agent_response/terminal wire；Gemini Flash effort variants 已建立 provisional
  Deployment。继续覆盖 cancel、quota exhaustion、process crash、history、tool denial 与 model
  disappearance/upgrade drift；stream/image 仍未开放。
