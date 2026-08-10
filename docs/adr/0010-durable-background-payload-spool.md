# ADR-0010：本地 durable background 使用 runtime 托管的加密 payload spool

- 状态：Accepted
- 日期：2026-08-09
- 决策者：项目 Owner
- 关联：D-105、ADR-0001、ADR-0002

## 背景

Job/Attempt metadata、配额账本和 Candidate Plan 已可持久化与分页，但仅有 metadata 无法在 daemon 重启后恢复实际推理输入。把 prompt 直接写入 SQLite 会扩大数据库备份、查询和运维路径的敏感数据边界；只接受调用方托管引用又会让首个 local Ollama consumer 场景依赖额外对象存储与引用租约。

OpenAI Responses 的 `background` 字段、Response retrieval 和 cancel endpoint 提供了调用方熟悉的异步生命周期；其官方数据控制说明也明确 background 依赖持久保存结果以供轮询。infer-runtime 需要兼容这组外部形状，同时保留自己的 App ownership、路由、资源 reservation 和恢复不变量。

## 决策

首个 M4 纵向切片支持非流式文本 `POST /v1/responses` 的 `background: true`，并提供：

- `GET /v1/responses/{response_id}`：App-scoped 状态或最终结果；
- `POST /v1/responses/{response_id}/cancel`：取消仍可终止的后台任务；
- CLI `infer run --background`、`infer response`、`infer cancel-response`。

该切片只允许 `local_only` placement。runtime 在调用 provider 前移除 `background`，因为 durability 是本控制平面的职责，不应同时要求 Ollama 或其他上游再创建第二个后台生命周期。云端、可信远程节点、音频和 batch 不在本决定范围内。

payload 由新的 `infer-payload` owner 管理，SQLite 仅保存 `DurablePayloadRef`：随机 blob ID、类型、带密钥 HMAC-SHA256 identity 与明文大小。spool 采用 AES-256-GCM、随机 96-bit nonce 和如下认证上下文：

```text
format version + App ID + payload kind + blob ID + HMAC digest + plaintext length
```

32-byte master key 只从配置指定的环境变量读取；它通过 domain separation 派生独立的 AEAD 与 HMAC key，配置、SQLite、日志和审计事件都不保存 key value。Unix 上目录为 `0700`、blob 为 `0600`；写入使用同目录临时文件、sync、原子 rename 和目录 sync。实现使用 Rust `ring` 的 AEAD/HMAC/SystemRandom primitives，并对 key 与中间明文/密文 buffer 做 zeroize。

提交顺序为：加密输入 → 原子持久化 queued Job/引用/audit → 返回 Response ID → 调度。若 admission 失败，未发布 blob 立即删除。完成顺序为：加密结果 → 原子持久化 succeeded Job/结果引用并清除输入引用 → 删除输入 blob。失败或取消只在终态已经持久化后回收输入；若 metadata 发布失败，密文输入保留给下次启动恢复。结果按配置 retention 过期。

启动恢复发生在监听端口前：

1. 普通 queued/running Job 继续按 ADR-0002 失败且不重放；
2. durable background running Attempt 标为 `interrupted`，未结算 reservation 以 interrupted 结算；
3. 相同 Job/Response ID 重新排队，重用 admission 时持久化的 Candidate Plan 与 config fingerprint；
4. 新 Attempt 的 trigger 为 `recovery`，且总 Attempt budget 与 `max_recovery_replays` 同时生效；
5. config fingerprint 变化、重放超限或 payload 认证失败时 fail closed。

启用 background 时 key 缺失或格式错误会阻止 daemon 启动。恢复 metadata 前还会只读认证所有 pending 输入；格式正确但内容错误的 key、缺失 blob 或认证失败同样阻止启动，不增加 replay 计数，也不删除输入。关闭配置但数据库仍有 pending durable Job 也会阻止启动，避免通过关闭功能遗弃密文。启动 reconciliation 只删除符合 spool 自有命名/格式且不被数据库引用的 blob/temp 文件，不触碰目录内其他文件。

## 备选方案

- **prompt 与结果直接存 SQLite**：事务简单，但让通用 metadata/backup/query owner 持有敏感正文，扩大误导出和运维暴露面。
- **只接受调用方 immutable reference**：适合未来大型音视频和零复制，但首个本地文本场景需额外引用服务、授权与租约协议。
- **直接转发上游 `background`**：无法统一本地 Ollama 行为，也不能让 runtime 自己持有 Candidate Plan、配额与资源恢复语义。
- **自动恢复所有未完成 Job**：可能重复云端计费、工具副作用或已产生但未观测的结果，违反 ADR-0002。

## 影响

### 正面

- 调用方可使用 Responses 风格的 create/retrieve/cancel 生命周期；
- 本地长任务可在 daemon 重启后保留同一 ID 和完整 Attempt 审计；
- SQLite、Job 分页和默认日志继续保持 payload-free；
- payload crypto、文件权限、orphan cleanup 和 retention 有单一语义 owner。

### 代价与风险

- 操作者必须可靠保管 key；丢失或轮换错误会让已有 payload 无法恢复；
- “删除”是文件系统层 best effort，不承诺闪存介质上的物理覆写；
- local provider 中断后的真实上游结果仍可能未知，恢复意味着重新计算；
- 当前单机 spool 不是 HA/object storage，数据库与 payload 目录必须作为同一恢复单元备份。

## 验证方式

- fake provider 覆盖加密提交、App isolation、取消、结果原子发布、同 ID restart recovery 和 `initial/interrupted → recovery/succeeded` Attempt 链；
- payload tests 覆盖随机 nonce roundtrip、跨 App/tamper 拒绝、size/path/reference 校验、orphan reconciliation 与权限；
- SQLite tests 覆盖 schema migration、pending recovery、reservation settlement、config drift、重放上限、结果 retention；
- SQLite trigger 故障注入覆盖“结果 blob 已写入但 metadata 发布失败”和“输入引用清除失败”：只要引用事务未提交，密文输入不得删除；修复故障并重启后以同一 ID 完成；
- 512 个 running Job 连续经历三轮 recovery：前两轮到达 replay 1/2，第三轮按上限全部终止，reservation 只结算一次且最终无 pending/reference 泄漏；
- 受控本地 Responses deployment 验证正常 background 与执行中终止 daemon 后恢复；对 spool/SQLite/WAL 搜索输入和输出明文不得命中。

## 复审触发条件

接入 cloud/可信节点恢复、background 音频/图像、batch、key rotation、多个 daemon 共享队列或外部 immutable reference 时复审。任何扩展都必须先定义幂等键、payload 授权/租约、结果仲裁和删除责任。
