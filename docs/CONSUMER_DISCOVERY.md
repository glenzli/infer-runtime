# Infer Runtime Consumer Discovery

Infer Runtime 通过 Infra Discovery 发布本机 Consumer API 的位置。Discovery 只解决“这次
daemon 在哪里”；App 身份、Bearer 凭证、Intent ACL 和请求合同仍由 Infer Runtime 自己管理。

## 冻结的协议身份

| 项目 | 值 |
| --- | --- |
| Discovery schema | `infra.discovery.registration@20260810.1` |
| service kind | `infer-runtime` |
| Consumer protocol | `infer-runtime.consumer` |
| Consumer protocol version | `0.1.0-candidate.2` |
| binding | `infer-runtime.http-loopback` |
| endpoint | canonical numeric loopback URL，例如 `http://127.0.0.1:8787` |

Consumer binding 只接受 `http://`、数值型 loopback 地址和显式非零端口；不接受 hostname、
通配地址、LAN 地址、路径、query、userinfo、redirect target 或尾随 `/`。IPv6 使用
`http://[::1]:<port>`。版本必须与 Consumer wire contract 精确匹配，不能做前缀推断。

同一份 registration 同时发布状态协议和推理入口；两者互不冒充兼容：

```json
{
  "schema": "infra.discovery.registration",
  "schema_version": "20260810.1",
  "service": {
    "kind": "infer-runtime",
    "instance_id": "local",
    "generation": "gen_example"
  },
  "lease": {
    "renewed_at": "2026-08-11T08:00:00Z",
    "expires_at": "2026-08-11T08:00:45Z"
  },
  "offers": [
    {
      "protocol": "infer-runtime.status",
      "protocol_versions": ["20260810.1"],
      "binding": "infra.local.unix-socket",
      "endpoint": "sockets/example.sock"
    },
    {
      "protocol": "infer-runtime.consumer",
      "protocol_versions": ["0.1.0-candidate.2"],
      "binding": "infer-runtime.http-loopback",
      "endpoint": "http://127.0.0.1:8787"
    }
  ]
}
```

## Consumer 选择与迁移顺序

1. 显式的开发/诊断 endpoint override 优先；生产本机默认不要写死端口。
2. 按 Infra Discovery 规则选择当前用户拥有、未过期且 schema/version 合法的
   `kind=infer-runtime` registration。
3. 精确选择 `infer-runtime.consumer@0.1.0-candidate.2` +
   `infer-runtime.http-loopback` offer，并再次校验 endpoint。
4. 缓存 `instance_id`、`generation` 和 lease；generation 改变、lease 到期或连接失败时重新发现，
   不在旧 endpoint 上无限重试。
5. 迁移期间可把 `http://127.0.0.1:8787` 保留为最后一级兼容 fallback；所有已登记 Consumer
   完成适配后删除 fallback。

Consumer 仍必须在每个业务请求中携带自己的 `Authorization: Bearer`。Registration 不包含
App id、credential id、token、ACL、Provider key 或 Console session。HTTP client 应禁用代理和
自动 redirect；重定向不能借此把敏感 payload 或 token 发出 loopback。`infer-runtime.status`
Unix socket 只服务只读设施观测，不能替代 Consumer API 或授权。

当前 publisher 随 `[observer].enabled=true` 一起启动，因为两个 offer 共享同一个 registration
lease 和 generation；这不是在语义上把 Consumer API 归入 observer。未来若出现“关闭状态观测但
仍需自动发现”的真实部署，再把 publisher lifecycle 提升为独立配置 owner。
