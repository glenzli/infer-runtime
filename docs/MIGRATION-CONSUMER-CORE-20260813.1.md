# 迁移到 Consumer Core `20260813.1`

这是一次有意的 hard cut。目标是把历史 candidate 协议、每个产品重复的 Discovery/鉴权代码和
整套协议联动升级，一次性替换为稳定 Core + 独立 Capability + 官方 SDK。迁移完成后不保留
candidate offer、缺 header 兼容、固定 `127.0.0.1:8787` 产品 fallback 或租户自有 manifest parser。

## 新身份

| 层 | 精确身份 |
| --- | --- |
| Discovery document | `infra.discovery.registration@20260812.1` |
| Consumer Core offer | `infer-runtime.consumer-core` / `20260813.1` |
| HTTP Core header | `Infer-Consumer-Contract: infer-runtime.consumer-core@20260813.1` |
| Capability catalog | `infer-runtime.capability-catalog@20260813.1` |
| Capability header | `Infer-Capability-Contract: <capability-id>@<schema-version>` |

`model` 仍是 Intent。具名申请只通过 App routing ACL 授权的 `infer.deployment_ids` 或
`infer.model_profile_ids` 收窄，不把物理模型名放进 `model`。

本机曾运行过一个以 `20260812.1` 标记的预发布草案；它没有最终 Core/Catalog 的相同 bytes、
schema references 与 digest 集合，因此不属于受支持版本，也不得被重标为本版本。Consumer 只依赖
本提交冻结的 `20260813.1` artifact 与 SDK；正式切换必须生成新的 Discovery generation。

## 租户改造

Rust Consumer 依赖官方 SDK 的不可变 Git revision：

```toml
[dependencies]
infer-runtime-client = { git = "https://github.com/glenzli/infer-runtime.git", rev = "<migration-commit>" }
```

每个 Consumer：

1. 删除自有 Infra Discovery schema、权限、generation 和 loopback URL parser；调用 SDK resolver；
2. 删除固定端口 product fallback、proxy 和 redirect 行为；开发 override 仍可显式保留；
3. 继续使用原有 app id、managed token 文件和最小 ACL，不创建或轮换凭据；
4. 将业务调用迁到 SDK 对应能力模块；SDK 自动发送 Core 与 Capability 两个精确 header；
5. 按 `error.code` 分支，忽略 response 新增字段，不解析 `error.message`；
6. 保存 Job 的 `consumer_core_contract` 和 `capability_contract` 作为 payload-free provenance；
7. 在 Runtime 切换前，用 SDK fixtures 完成 Discovery、header、错误和 ACL 形状测试；真实业务
   smoke 留在统一切换后，使用原 credential 执行。

## 切换顺序

1. Runtime 合同、SDK、OpenAPI、fixtures 和全量测试冻结；
2. Echo、Shadow、Shape、Symbiont-d 分别迁移并提交，但当前 daemon 仍运行旧合同；
3. 核对所有登记 App 的新 SDK 构建已就绪，token/ACL 未改变；
4. 构建 release，确认无 running Job，通过 Console-owned 单实例流程平滑重启；
5. 验证 Discovery generation 更新且只发布 Core offer；
6. 每个租户用原 credential 跑真实最小 smoke；全部通过后结束迁移。

### 持久化 Job 与回滚门

切换前先停止新 background submission、等待 running Job 归零，并备份 SQLite store。新 Runtime
读取历史 Job 时将缺失的 `consumer_core_contract` 明确投影为 `legacy-unknown`，而不是伪装为当前
Core；历史 candidate 字段 `consumer_contract_version` 只作为读取 alias，重新序列化只写新字段。
旧 Runtime 的 Job projection 会忽略新增 provenance 字段，因此允许二进制回滚，但回滚后不得提交
依赖新 Capability 的任务。

发布演练必须在生产副本上机械验证：旧 store → 新二进制启动/读取/提交一条非敏感任务 → 停止并用
旧二进制只读打开同一副本。任何 SQLite migration、Job decode 或 background recovery 错误都终止
切换并从切换前备份恢复；不得在原库上反复试错。Core release 证据记录备份摘要、两个二进制版本、
迁移前后 Job 计数和回滚只读结果，不记录 payload。

实验性 RAW foundation 不在首批 SDK 稳定面内。Shadow 可以继续保留未激活的专用实现，但在
生产激活前必须迁到官方 SDK 的专用 RAW 模块；不得保留第二套 Core Discovery parser。

任一租户未就绪都不重启生产 daemon。切换后旧 Consumer 得到 HTTP 426：Core 不匹配为
`consumer_core_unsupported`，能力不匹配为 `capability_contract_unsupported`。不静默降级。

## 后续升级规则

- Core 骨架 breaking：发布新的日期化 Core，并统一迁移；这是低频事件。
- 单能力 breaking：只升级该 capability 日期版本和 SDK 对应模块，只迁移实际使用者。
- 新 capability：Catalog/SDK additive 更新，不升级 Core。
- 已有 Core/Capability 的 additive response 字段若需要写入 OpenAPI：发布该 owner 的下一日期
  版本；旧 SDK 因忽略未知字段可继续工作，但既有 artifact/digest 不被改写。
- SDK 自身遵循 SemVer；发布 registry package 后 Consumer 可用普通依赖更新工具管理。

单能力升级采用 overlap，而不是 flag day：Runtime 在同一 generation 同时发布该 capability 的旧、新
单版本 record；新 SDK 的有序支持列表优先新版本，旧 SDK 继续精确选择旧版本。完成真实 Consumer
soak 后才移除旧 record，移除会更新 Discovery generation。Runtime 必须在请求 admission 与 Job
provenance 中保留实际 header 选择，不能把旧请求重标成新版本。

每个 SDK capability 模块声明一个按偏好排序的精确身份列表；增加 v2 时只在对应模块把 v2 放到 v1
之前。SDK 先读取 Catalog，再下载并校验所选 schema digest，最后才携带所选 header 与 bearer 发业务
请求。Runtime admission 只接受该 route 的 Catalog record，并把实际选择绑定到当前请求的异步作用域；
Job 创建时持久化该值，background recovery 继续读取持久化 provenance，禁止改写成进程默认版本。
