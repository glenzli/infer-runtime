# ADR-0012：Runtime-managed local operator credential

- 状态：Accepted
- 日期：2026-08-09
- 决策者：项目 Owner
- 关联：D-010

## 背景

早期开发配置把一个从未接入的候选应用当作默认 App，并要求用户在启动 daemon 和 CLI
前手工设置它的 token。该身份甚至拥有资源管理权限，混淆了外部 consumer 与本机
operator。完全取消 loopback 认证同样不安全：同一用户会话中的其他进程可以访问
loopback，进而消耗云额度、读取控制面 metadata 或执行模型 lifecycle action。

## 决策

默认配置只注册通用 `local-operator`。它的 credential source 为 `managed`：runtime auth
owner 首次使用时生成 32 个随机字节，以 64 位小写十六进制保存；daemon 与 CLI 从同一
credential directory 解析。CLI 无需日常传入 API key，但仍可用 `--api-key` 或
`INFER_API_KEY` 显式连接其他实例。

外部 consumer 不预置产品名称。接入时新增独立 `[apps.<id>]`。已有 secret provisioning 的
应用可使用 `credential = { source = "environment", variable = "..." }`，由 daemon 启动环境
注入；本机 operator 也可通过 Apps & Access 创建 `managed` Consumer credential。普通
consumer 的 `resource_admin` 为 false。credential 在启动时解析为独立认证表，请求路径不
读取或改变进程环境。

Unix 上 managed directory/token file 分别强制为 `0700`/`0600`，拒绝 token symlink、
非普通文件、宽松权限和格式错误。token 不进入 RuntimeConfig、SQLite snapshot、日志或
错误；Apps & Access 仅在创建/轮换成功时以 `no-store` 响应显示一次，此后只显示 SHA-256
短指纹。认证使用固定长度 HMAC tag 比较，避免直接 secret 字符串比较。

## 备选方案

- **每次手工设置环境变量**：secret 不落盘，但启动体验差，并把本地 operator 错写成外部 App。
- **loopback 免认证**：没有安全的 App/权限/审计边界，拒绝。
- **立即接入各平台 credential manager**：长期更强，但 macOS Keychain、Windows Credential
  Manager 和 service account 生命周期需要单独跨平台验证；当前 file owner 保持可替换。
- **把 token 写进 TOML 或 SQLite**：会进入版本控制、config snapshot、备份和通用查询边界，拒绝。

## 影响

- `inferd`、`infer console --spawn` 和普通本机 CLI 首次运行会自动创建 credential。
- 删除 credential file 会使下次启动生成新 token，已运行 daemon 仍持有旧 token，需重启后统一。
- 自定义 credential directory 的 CLI 必须使用同一 `--config`/`INFER_CONFIG`；连接远端实例时
  使用显式 API key。
- Apps & Access 的 create/update/rotate/revoke 是 loopback experimental operator surface，
  修改配置与 owner-only credential file；运行中认证表仍在 daemon 启动时固定，必须重启后
  生效。`local-operator` 不允许由该 surface 修改或删除。
- OS credential manager、无重启认证 reload 和多用户权限属于后续增强，不改变 App
  credential source 与认证表边界。

## 验证方式

- 首次生成、重复读取、权限、symlink/格式拒绝和重复 token 测试；
- Consumer managed credential 的 exclusive create、一次性返回、指纹、安全替换/回滚、撤销、
  path traversal 拒绝与配置事务测试；
- 无 API key 的 daemon + CLI/console 真实 loopback 启动；
- 错误 token 返回稳定 401，普通 consumer 无 `resource_admin`；
- 全仓搜索确认没有把外部产品身份当作默认 operator；任何真实 consumer（当前为 Echo）都以
  独立、最小权限 App 显式登记。
