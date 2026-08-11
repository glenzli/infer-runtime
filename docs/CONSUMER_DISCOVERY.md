# Infer Runtime Consumer Discovery

Infer Runtime 通过 Infra Discovery 发布本机 Consumer API 的位置。Discovery 只解决“这次
daemon 在哪里”；App 身份、Bearer 凭证、Intent ACL 和请求合同仍由 Infer Runtime 自己管理。

## 冻结的协议身份

| 项目 | 值 |
| --- | --- |
| Discovery schema | `infra.discovery.registration@20260812.1` |
| service kind | `infer-runtime` |
| Consumer protocol | `infer-runtime.consumer` |
| Consumer protocol version | `0.1.0-candidate.3` |
| binding | `infer-runtime.http-loopback` |
| endpoint | canonical numeric loopback URL，例如 `http://127.0.0.1:8787` |

从 `infra.discovery.registration@20260810.1` 迁移的 Consumer 必须精确接受新 document version，
删除 lease/TTL/manifest-renewal 判断。旧版本不能按兼容前缀解析；未升级的 Consumer 在迁移窗口内
只会使用显式 endpoint 或最后一级固定 fallback。

Consumer binding 只接受 `http://`、数值型 loopback 地址和显式非零端口；不接受 hostname、
通配地址、LAN 地址、路径、query、userinfo、redirect target 或尾随 `/`。IPv6 使用
`http://[::1]:<port>`。版本必须与 Consumer wire contract 精确匹配，不能做前缀推断。

同一份 registration 同时发布状态协议和推理入口；两者互不冒充兼容：

```json
{
  "schema": "infra.discovery.registration",
  "schema_version": "20260812.1",
  "service": {
    "kind": "infer-runtime",
    "instance_id": "local",
    "generation": "gen_example"
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
      "protocol_versions": ["0.1.0-candidate.3"],
      "binding": "infer-runtime.http-loopback",
      "endpoint": "http://127.0.0.1:8787"
    }
  ]
}
```

## Consumer 选择与迁移顺序

1. 显式的开发/诊断 endpoint override 优先；生产本机默认不要写死端口。
2. 按 Infra Discovery 规则选择当前用户拥有且 schema/version 合法的
   `kind=infer-runtime` registration。manifest 没有 lease，也不代表进程存活。
3. 精确选择 `infer-runtime.consumer@0.1.0-candidate.3` +
   `infer-runtime.http-loopback` offer，并再次校验 endpoint。
4. 缓存 `instance_id`、`generation` 和被选 offer；连接失败时先重读稳定 manifest，generation 或
   offer 改变时立即重新选择，不修复或删除 Provider manifest，也不在失败 endpoint 上无限重试。
5. 迁移期间可把 `http://127.0.0.1:8787` 保留为最后一级兼容 fallback；所有已登记 Consumer
   完成适配后删除 fallback。

Consumer 仍必须在每个业务请求中携带自己的 `Authorization: Bearer`。Registration 不包含
App id、credential id、token、ACL、Provider key 或 Console session。HTTP client 应禁用代理和
自动 redirect；重定向不能借此把敏感 payload 或 token 发出 loopback。`infer-runtime.status`
Unix socket 只服务只读设施观测，不能替代 Consumer API 或授权。

当前 publisher 随 `[observer].enabled=true` 一起启动，因为两个 offer 共享同一个 registration
generation；这不是在语义上把 Consumer API 归入 observer。publisher 在所有 endpoint 就绪后只
原子发布一次，不续租；退出时保留稳定 manifest，只清理进程私有 socket。未来若出现“关闭状态观测但
仍需自动发现”的真实部署，再把 publisher lifecycle 提升为独立配置 owner。

candidate.2 与 candidate.3 的 Intent/能力词汇不兼容。Consumer 必须完成
[candidate.3 migration](MIGRATION-0.1.0-candidate.3.md) 后才选择 candidate.3 offer；不得选择新
offer 却继续发送旧字段。迁移期间可以同时识别两个精确版本，但每次请求只能遵循被选 offer 的合同。
