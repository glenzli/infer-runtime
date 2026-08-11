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

1. 同一 `infer-runtime` registration 新增 `infer-runtime.consumer` offer；其版本精确等于冻结的
   Consumer contract，当前为 `0.1.0-candidate.3`。
2. Consumer 使用 Infer 自有 binding `infer-runtime.http-loopback`。它发布已经实际绑定的数值型
   loopback HTTP URL；不把 HTTP 请求、鉴权或错误语义塞进通用 Discovery 协议。
3. Registration 不携带 App id、token、credential id、ACL 或任何 Provider secret。发现地址后，
   Consumer 继续使用自己受限的 managed/environment bearer 身份。
4. 显式 endpoint override 仍用于开发和诊断；租户迁移期可保留固定地址 fallback。稳定接入保留
   Discovery generation 与被选 offer；manifest 没有 lease，当前可用性只由实际连接确定。
5. `local-operator` 是受保护的本机管理员身份，不是产品 Consumer。其省略
   `allowed_intents` 是为了自动覆盖完整 Intent registry；普通旧配置省略该字段才称为兼容配置。

## 后果与边界

- Echo、Shadow、Symbiont-d 可共享发现合同，但继续拥有不同 App 身份、Intent ACL 和 secret。
- `infer-runtime.status` 与 `infer-runtime.consumer` 共享 service generation，不共享 application
  framing、权限或数据面。
- 当前 registration publisher 随 observer 开关启动；没有真实部署需求前不增加第二套 publisher
  或新配置层。它在 endpoint 就绪后原子发布一次，退出保留稳定 manifest，不运行 heartbeat。
- Consumer 必须验证 registration owner/protocol/version/binding/endpoint，禁用 HTTP proxy 与
  redirect；连接失败先重读 manifest，并在 generation 或 offer 变化后重新选择。
- 本决定不改变已冻结 candidate revision 的 HTTP path、payload、错误 envelope 或兼容承诺。
  candidate.3 的 Intent/能力 breaking change 由独立 migration 管理，Discovery 只做精确版本选择。
