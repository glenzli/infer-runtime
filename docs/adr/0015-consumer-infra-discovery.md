# ADR-0015：Consumer API 使用 Infra Discovery 定位

- 状态：Accepted
- 日期：2026-08-11
- 决策者：infer-runtime maintainers

## 背景

Infer Runtime 的 Consumer 目前普遍把 `127.0.0.1:8787` 当作默认地址。固定默认值便于早期
纵向切片，却无法区分实例、daemon generation 或动态端口，也使每个租户各自维护一套
“服务是否存在”的猜测逻辑。Runtime 已按 `infra.discovery.registration@20260812.1` 发布只读
状态协议，因此可以复用同一 service identity 和 publisher，而不扩张 Discovery 的职责。

## 决定

1. 同一 `infer-runtime` registration 发布 `infer-runtime.consumer-core@20260813.1` offer；Core
   只拥有跨能力共同骨架，typed schema 由 Capability Catalog 独立版本化。
2. Consumer 使用 Infer 自有 binding `infer-runtime.http-loopback`。它发布已经实际绑定的数值型
   loopback HTTP URL；不把 HTTP 请求、鉴权或错误语义塞进通用 Discovery 协议。
3. Registration 不携带 App id、token、credential id、ACL 或任何 Provider secret。发现地址后，
   Consumer 继续使用自己受限的 managed/environment bearer 身份。
4. 显式 endpoint override 仍用于开发和诊断；产品租户不保留固定地址 fallback。稳定接入保留
   Discovery generation 与被选 offer；manifest 没有 lease，当前可用性只由实际连接确定。
5. `local-operator` 是受保护的本机管理员身份，不是产品 Consumer。只有它以
   `resource_admin=true` 并显式 `allow_all_intents=true` 才自动覆盖完整 Intent registry；普通 App
   省略 `allowed_intents` 必须 fail closed。

## 后果与边界

- Echo、Shadow、Symbiont-d 可共享发现合同，但继续拥有不同 App 身份、Intent ACL 和 secret。
- `infer-runtime.status` 与 `infer-runtime.consumer-core` 共享 service generation，不共享 application
  framing、权限或数据面。
- Registration publisher 属于 Consumer 数据面生命周期，独立于 observer 开关。它在 endpoint
  就绪后原子发布，退出保留稳定 manifest；manifest 不携带 lease，也不作为心跳。
- 2026-09-12 运维补充：daemon 每 30 秒检查自身声明。内容完整时不写文件；仅在声明缺失、
  owner-only 目录仍有效且原 publisher lock 的文件身份不变时，原子补发同一 generation 和 offers。
  补发不得覆盖并发出现的声明。锁丢失、被替换、声明冲突或不安全路径会报告错误，交由受管重启恢复。
- Consumer 必须验证 registration owner/protocol/version/binding/endpoint，禁用 HTTP proxy 与
  redirect；连接失败先重读 manifest，并在 generation 或 offer 变化后重新选择。
- Discovery 只做精确 Core 版本选择。HTTP 再通过 `Infer-Capability-Contract` 精确选择单项 typed
  schema；新能力或单项 breaking 不再联动升级 Discovery/Core。
