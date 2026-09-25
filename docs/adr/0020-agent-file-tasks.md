# ADR-0020：独立的文件型 Agent 任务能力

- 状态：合同与受限执行已实现；产品 App 仍须单独授权
- 日期：2026-09-25
- 关联：D-008、D-113、ADR-0013

## 决定与责任

`infer.agent.task@20260925.1` 是独立的 experimental 能力，入口为
`POST /infer/v1/agent/tasks`。请求使用精确 Consumer Core 与 Capability 双层握手、Bearer App
身份和独立的 `allow_agent_file_tasks` ACL。`agent.file_task` 是该数据面的稳定 Intent；
`text.edit`、`language.respond`、Responses `tools` 和订阅 Provider 访问权都不授予文件型 Agent 权限。
普通 Responses 输入中的路径字符串仍只是文本，不触发文件读取。

Shape 负责创作工作流、候选预览、过期判断和接受。Infer 应负责 App 准入、受限输入/输出工作区、
模型与资源选择、Job/Attempt、取消、传输和 provenance。Agent 运行时负责模型与工具循环；
Infer 不重新实现该循环。

## 冻结的请求与结果形状

请求 JSON 严格拒绝未知字段：

```json
{
  "model": "agent.file_task",
  "instruction": "Read input/scene.txt and write output/revision.txt",
  "input_files": [{
    "path": "scene.txt",
    "content_base64": "aGk=",
    "sha256": "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4"
  }],
  "output_paths": ["revision.txt"]
}
```

`input_files[].path` 相对于私有 `input/`；`output_paths[]` 相对于私有 `output/`。没有 App ID、
本机绝对路径、现有目录、Provider ID 或物理模型字段。路径只接受 ASCII 字母数字、`-_.` 和目录分隔符。
当前解析器拒绝路径穿越、盘符、反斜线、隐藏组件、大小写折叠后的重复路径、未知字段、
非规范 Base64、摘要不符及超额内容。最多 16 个输入文件、16 个
输出路径；单个输入最多 8 MiB，总输入最多 16 MiB。执行器重算 staging bytes 摘要；每个
输出文件最多 8 MiB、总输出最多 16 MiB，只返回声明过的路径，拒绝 symlink、hardlink 和特殊文件。

完整执行后的预留成功结构是：

```json
{
  "job_id": "agent_...",
  "state": "completed",
  "answer": "...",
  "outputs": [{"path": "revision.txt", "content_base64": "...", "sha256": "..."}],
  "provenance": {
    "capability_contract": "infer.agent.task@20260925.1",
    "provider": "...",
    "deployment": "...",
    "model_build": "...",
    "attempt_number": 1,
    "codex_thread_id": "...",
    "codex_turn_id": "...",
    "sandbox_profile": "...",
    "tool_policy": "..."
  }
}
```

成功 Job 使用现有 `/infer/v1/jobs/{job_id}` 与取消接口记录状态，Attempt 记录所选 Deployment
和 Provider；成功响应的 provenance 给出实际工具策略以及 Codex thread/turn。Agent 可能修改工作区文件，因此一旦 `turn/start` 已被
上游接受，传输断开只能记为结果未知，不能自动重试或 fallback。输出交给 Shape 后仍是候选，
Infer 不替 Shape 接受或发布创作内容。

## 执行边界

普通订阅 Responses 桥继续禁用 shell/unified exec。独立 `codex-agent` Provider 在每次请求时
创建新的临时工作区和 `CODEX_HOME`，只把 Consumer 提交的已验证字节放进 `input/`，并预建
声明的 `output/` 文件。Codex 登录信息仅供宿主 App Server 进程使用；Agent 工具不能读取
`CODEX_HOME`。运行时向 Codex App Server 同时提交命名权限 profile 和 `approvalPolicy=never`，
然后回读配置，确认权限和 MCP/plugin 空集合。profile 默认拒绝宿主文件，只开放工作区读权限和
声明的输出文件写权限。额外审批请求被拒绝，Attempt 失败；首版没有交互式审批或动态扩权。
该隔离不靠提示词或 cwd。Agent 的模型调用使用 Codex 订阅云端；所选输入内容可能进入模型上下文。

`local-operator` 示例身份显式允许此能力；Shape 示例仍为 `allow_agent_file_tasks=false`，
生产 App 必须同时具备 Intent、订阅 Provider、云端文本输入和独立 Agent ACL 授权。
没有可用执行器时返回 `503 agent_task_unavailable`；未授权 App 返回 `403 agent_task_forbidden`。
显式 Provider probe 会运行一项合成文件任务，因此会消耗订阅额度。运行时只尝试一个
Deployment/Attempt，任务总期限为 5 分钟，不在不确定结果后重试或 fallback。成功 Job 及 Attempt 可通过现有 Job
接口查询。此实现目前验证了本机 Codex 0.156.0 的真实 Agent turn、未授权文件读取拒绝、
声明输出文件写入，以及 HTTP 到 Job 的合成任务闭环；部署中的版本和产品集成仍须各自验证。
