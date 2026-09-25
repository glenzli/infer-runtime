# ADR-0020：独立的文件型 Agent 任务能力

- 状态：合同与拒绝路径已实现；执行阶段阻断
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
输出路径；单个输入最多 8 MiB，总输入最多 16 MiB。未来执行器读取实际 staging bytes 时仍须
重算摘要，并只收集声明过的输出路径，限制返回文件大小，拒绝 symlink、hardlink 和特殊文件。

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

成功 Job 应使用现有 `/infer/v1/jobs/{job_id}` 与取消接口记录状态，Attempt 绑定所选 Deployment、
Provider、实际工具权限和 Codex thread/turn。Agent 可能修改工作区文件，因此一旦 `turn/start` 已被
上游接受，传输断开只能记为结果未知，不能自动重试或 fallback。输出交给 Shape 后仍是候选，
Infer 不替 Shape 接受或发布创作内容。

## 当前门槛与行为

现有订阅桥按 ADR-0013 禁用 shell/unified exec，并在非推理 item 出现时拒绝 Attempt。
本机 Codex App Server schema 确认 `thread/start`、`turn/start`、`workspaceWrite.writableRoots`
与审批字段的形状，却没有提供“只能读取这些输入文件”的 per-task 读权限。只改变 cwd 或传入
`runtimeWorkspaceRoots` 不能证明文件隔离；拒绝交互审批也只能防止越权升级，不能限制已允许
的本机文件读取。官方 sandbox 文档将 writable roots 说明为写权限扩展，而非读权限白名单。

因此当前路由完成双层合同、认证、独立 App ACL 和严格 payload 验证后，固定返回
`503 agent_task_unavailable`，不创建 Job、不保留文件、不启动 Codex。未授权 App 返回
`403 agent_task_forbidden`。Provider profile 可以解析 `agent_task` 声明，但只有 Codex App Server
家族允许声明，显式 Provider probe 会失败；示例订阅 Provider 不声明它。Catalog 公布 experimental
schema 是协商和集成准备，不表示本机已经有可用执行器。

开放执行前必须提供一个真正只暴露所提交文件的隔离执行环境，验证 Codex 子进程与其工具均受
同一读/写/网络边界约束，再覆盖拒绝外部读取、越界写入、审批请求、取消、上游结果未知、
输出收集与 Job/Attempt 持久化的端到端测试。不能以 prompt、通知事后检查、cwd 或
`workspaceWrite` 单独替代这些证据。若成功结构或这些语义需要变动，应发行新的日期化 capability
版本，不覆盖本版 schema bytes。
