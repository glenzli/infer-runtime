# ADR-0013：Codex 订阅式推理桥接使用 Provider 模型组与显式 App 授权

- 状态：Accepted（Codex bounded execution）
- 日期：2026-08-10
- 决策：D-012

## 背景

Codex App Server 能把用户已有订阅外露为本机可调用入口。它与普通云 API 不同：认证和 quota
绑定交互产品账号，stdio 协议包含 thread、工具、工作区和审批等 Agent 能力，同时一个入口会
暴露一组模型而不是单个模型。

infer-runtime 只需要其中的标准推理能力，不复制完整 Agent runtime，也不能因为 transport 在
本机就把隐私、offline 或 local-only 请求送入云端。

## 决定

1. 一个登录会话对应一个 `subscription` Provider，共享 scheduler、并发与 quota 边界。
2. `model/list` 只形成动态 operator inventory。每个可路由模型仍需显式 Model Profile、Build 和
   Deployment；reasoning effort 是 Deployment 支持范围，发现不产生准入。
3. Provider 固定 `placement=cloud`。App 默认只允许 `standard` access class，必须显式授权
   `subscription`；单次请求只能通过 `infer.provider_access_class` 收窄已有权限。
4. Codex adapter 是独立 JSON-RPC semantic owner，不进入 HTTP Responses adapter 的条件分支。
   它支持有界 text/image 输入、文本 unary/SSE、reasoning effort，以及标准 Responses
   `tools:[{"type":"web_search"}] + tool_choice` 的有界 experimental 子集。
5. 每次调用使用空 ephemeral workspace、read-only sandbox、`approvalPolicy=never`，并关闭
   shell、unified exec、plugins、apps、multi-agent、computer use、history 和 memory。Web Search
   默认 disabled，只有已授权请求才按 Attempt 开为 cached/live；其他 tool item 或
   server-initiated approval 均失败。
6. 公开 response 只包含规范化结果、usage 和 Runtime Job/Attempt provenance；账号级信息只允许
   进入脱敏 operator telemetry。
7. `image.generate` 是独立精确 Intent，只接受 unary text prompt 和一个无参数
   `image_generation` tool；返回前验证 PNG、decoded size 和像素边界，不读取 `savedPath`。
8. Hosted Web Search 使用独立 App ACL `allowed_builtin_tools=["web_search"]`。首版只接受单个
   `web_search`、可选 `external_web_access: bool` 和字符串 `tool_choice=auto|required|none`。
   该有界形状进入 candidate.3 OpenAPI；未来扩大 tool schema 仍需新的合同 revision。

## 为什么不是 Provider Group 新实体

现有 Provider 已拥有 endpoint/session、scheduler、quota 与健康边界，Deployment 已表达同一
Provider 下的多个 Build。动态 inventory 只需展示 `admitted`，Registry 仍由版本化配置控制。
模型按 Intent rating、有效 `capability_floor` 与 `reasoning.effort` 独立筛选；reasoning effort 不承担
模型选择职责。

## 被拒绝的替代方案

Antigravity CLI 曾作为第二种订阅桥接候选接受实验验证，但其 permission JSON 无法提供稳定的
调用级 `tools=[]`：headless turn 可能主动选择工具后因不能授权而无输出；全局与项目规则还会合并。
`--dangerously-skip-permissions` 会破坏权限边界，公开报告也显示它与 sandbox 组合时可能批准
sandbox bypass。因此 infer-runtime 不保留对应 Provider protocol、adapter、配置入口、Build 或
Deployment。未来若上游出现可验证的 fail-closed 无工具协议，必须以新的 ADR 和实现重新评审，
不能复活本决定之前的实验代码。

## 验证

- fake JSON-RPC server 覆盖 model group、admitted、usage、malformed event 和非推理 item；
- routing 覆盖 subscription entitlement、请求级收窄、local-only/offline 零触达；
- Codex image generation 覆盖 capability、单图、重复 item 与 Base64 PNG 边界；
- Web Search 覆盖 App ACL、candidate capability、cached/live/disabled、required postcondition、
  action normalization、未授权 item 与交互 approval fail-closed；
- 真实订阅调用仅作为显式、低风险、可能计费的 operator acceptance，不属于默认 CI。

## 外部依据

- OpenAI Responses Web Search：<https://developers.openai.com/api/docs/guides/tools-web-search>
- Codex App Server items/approvals：<https://learn.chatgpt.com/docs/app-server>
- Codex web_search 配置：<https://learn.chatgpt.com/docs/config-file/config-reference>
- Antigravity permissions：<https://antigravity.google/docs/cli/permissions>
- Antigravity sandbox：<https://antigravity.google/docs/cli/sandbox>
- sandbox bypass 报告：<https://github.com/google-antigravity/antigravity-cli/issues/36>
