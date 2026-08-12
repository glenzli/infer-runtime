# Infer Runtime Consumer Discovery

Infer Runtime 通过 Infra Discovery 发布本机 Consumer API 的位置。Discovery 只解决“这次
daemon 在哪里”；App 身份、Bearer 凭证、Intent ACL 和请求合同仍由 Infer Runtime 自己管理。

## 冻结的协议身份

| 项目 | 值 |
| --- | --- |
| Discovery schema | `infra.discovery.registration@20260812.1` |
| service kind | `infer-runtime` |
| Consumer protocol | `infer-runtime.consumer-core` |
| Consumer protocol versions | `20260813.1`（单元素精确集合） |
| binding | `infer-runtime.http-loopback` |
| endpoint | canonical numeric loopback URL，例如 `http://127.0.0.1:8787` |

从 `infra.discovery.registration@20260810.1` 迁移的 Consumer 必须精确接受新 document version，
删除 lease/TTL/manifest-renewal 判断。旧版本不能按兼容前缀解析；本次 hard cut 结束后也不保留
固定 endpoint fallback。

Consumer binding 只接受 `http://`、数值型 loopback 地址和显式非零端口；不接受 hostname、
通配地址、LAN 地址、路径、query、userinfo、redirect target 或尾随 `/`。IPv6 使用
`http://[::1]:<port>`。版本必须精确匹配，不能做前缀、范围、语义版本或词法推断。

同一份 registration 始终发布推理入口；启用只读设施观测时才附加状态协议。两者互不冒充兼容：

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
      "protocol": "infer-runtime.consumer-core",
      "protocol_versions": ["20260813.1"],
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
3. Consumer 必须精确支持 `infer-runtime.consumer-core@20260813.1`；无交集即 incompatible。不能用
   版本范围、前缀、candidate 兼容或日期大小推断继续连接。
4. 对 `/infer/v1/contract` 及每个 Consumer 请求发送
   `Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1`，并交叉核对 HTTP 的
   `core_contract`、`supported_core_contracts` 与 Discovery offer。
5. 从 Capability Catalog 为每个 typed 请求选择精确能力身份，并发送唯一的
   `Infer-Capability-Contract`。Job/Explain 等 Core control 请求不发送能力头。
6. 缓存 `instance_id`、`generation` 和被选 offer；连接失败时先重读稳定 manifest，generation 或
   offer 改变时立即重新选择，不修复或删除 Provider manifest，也不在失败 endpoint 上无限重试。
7. 产品 Consumer 不保留固定 `http://127.0.0.1:8787` fallback；显式 endpoint 只用于开发/诊断。

Consumer 仍必须在每个业务请求中携带自己的 `Authorization: Bearer`。Registration 不包含
App id、credential id、token、ACL、Provider key 或 Console session。HTTP client 应禁用代理和
自动 redirect；重定向不能借此把敏感 payload 或 token 发出 loopback。`infer-runtime.status`
Unix socket 只服务只读设施观测，不能替代 Consumer API 或授权。

Consumer publisher 属于 inferd 数据面生命周期，不受 `[observer].enabled` 控制；关闭 observer
只会移除 `infer-runtime.status` offer 和它的 Unix socket。publisher 在所有将要发布的 endpoint
就绪后原子发布一次，不续租；退出时保留稳定 manifest，只清理进程私有 socket。offer 集合变化和
daemon 重启都会产生新的 generation。

缺少、重复、裸 `20260813.1` 或旧 candidate 的 `Infer-Consumer-Contract` 都返回 HTTP 426、
`error.code=consumer_core_unsupported`。能力版本通过 Capability Catalog 独立协商，不拼进 Core
header，也不会触发整个 Consumer 连接层升级。
