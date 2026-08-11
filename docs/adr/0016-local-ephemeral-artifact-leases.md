# ADR-0016：同机 Ephemeral Artifact Lease

- 状态：Accepted；Unix Phase 2 experimental activation
- 日期：2026-08-11
- 关联：D-009、D-102、D-106、D-107、D-108、ADR-0010、ADR-0011、ADR-0015

## 背景

RawNIND 一类执行会消费数十 MiB 的 provider-neutral Bayer staging，并产出约百 MiB 的线性 RGB
foundation。把这些内容上传/下载到普通 8787 multipart 会产生不必要的完整 materialization，
也会把路径、payload retention 和 cache owner 混入通用 Job。逐 tile RPC 同样错误：512×512 tile、
overlap、两遍 global gain 和 stripe sink 是一个 Build adapter 的原子像素合同，不能由 Consumer
跨 RPC 调度。

Shadow 继续拥有 decoder、source/Recipe revision、stale-result、未发布 partial、独立 verifier、
content-addressed cache、crash recovery 和最终 publish。Infer 在当前实验纵切中拥有 exact RawNIND Build、
preprocess/tensor/tiling/gain adapter、ONNX Session、admission、progress、cancel 和 Attempt provenance。
在接入模型前，双方先验证同机大制品的最小授权与生命周期边界。

## 决定

### 控制面与数据面分离

认证 Consumer 控制面先为一个 App/Job/daemon generation 签发短期 registration ticket。ticket
不是推理 Job，也不含路径或 payload。Consumer 随后连接 owner-only 本地数据面并提交 ticket：

- macOS/Linux：Unix socket，验证 peer effective UID，以 `SCM_RIGHTS` 传递两个 FD；
- Windows：未来使用 owner-only Named Pipe，验证 peer identity，以受限 `DuplicateHandle` 传递
  HANDLE；本阶段没有实现或通过 Windows 验证；
- mmap 只是收到句柄后的实现选择，不是协议字段或兼容承诺。

第一个 FD 必须是当前用户拥有、link count 为一的 read-only regular file。第二个必须是当前用户
拥有、link count 为一、长度为零、non-append、write-only 且可取得独占锁的 regular file。
Runtime 记录打开对象的 device/inode（Windows 对应 volume/file id），拒绝同一输入输出，并在整个
lease 生命周期持有同一个打开对象，绝不根据 Consumer path 重新解析。

ticket 转换出的 lease：

- 绑定 owner UID、App、Job、daemon generation 和绝对 expiry；
- opaque、one-shot；错误 App/Job/generation 不能消费；
- revoke、expiry、registry drop 或 daemon crash 都通过关闭句柄回收；
- 不写入 SQLite、Job metadata、普通日志或 durable spool；
- 不授予 rename、cache publish、模型选择或任意目录访问。

同一用户可以在另一个进程中修改自己拥有的普通文件；FD access mode 和 advisory lock 不能冒充
不可变存储。因此 adapter 必须在实际读取时计算输入 digest，Consumer 仍负责 source revision 与
stale-result，最终 receipt 绑定实际读取的 digest/size。需要更强不可变性的后续平台可以使用
sealed object，但不能改变公共 lease 语义。

Shadow Phase 0 baseline 使用产品协议 `infer-runtime.artifact-lease@20260811.1`。双方冻结的 transport
字段为：

- `input_handle = "read-only-stable-open-object"`；
- `input_integrity = "consumer-stability-promise-plus-runtime-identity-size-and-actual-read-digest"`；
- `output_handle = "empty-cooperative-exclusive-writable-open-object"`；
- `output_exclusivity = "cooperative-owner-only-not-adversarial-same-uid"`。

baseline validation receipt 同时暴露
`baseline_canonical_json_sha256=ca5ac508e20c84863796c9ae947800ea8d86d20ff83ba358027ae117561edc09`
（sorted keys + compact separators）与
`baseline_file_sha256=0e30a3e433bb8034ab955d9d61ab49021fe7945d1de3c62167e4cc8c8b3084fd`，
不得用一个未注明序列化方式的 digest 混淆两者。

### Phase 1 fixture（历史关闭门）

Phase 1 当时没有 RawNIND Intent、endpoint、Build、Deployment、ACL、模型或 daemon wiring。测试 owner
用一个确定性大文件执行 bounded copy/hash：只分配 64 KiB stripe buffer，直接从 leased input
写入 leased output，并报告 bytes、SHA-256、maximum explicit buffer 与 algorithmic full-payload
buffer count。
这是 lifecycle/measurement harness，不是“零拷贝推理”或已发布模型能力。

Shadow 另保留一份不含用户内容的当前机器 legacy 比较点：deterministic synthetic 2048×2048
Bayer staging、packed 1024×1024、两遍共 32 次 tile inference；plan 0.65s、完整 CPU sidecar
13.36s、验证后 artifact 50,335,372 bytes，SIGINT-to-exit 约 0.14s 且 unpublished partial 已删除。
外部 evidence receipt SHA-256 为
`b1aa6e293002a5411a4423b3f24407ce93fc6d5996f3f35a10e23e258d584d74`。该数字只作为 Phase 2
同机回归起点，不是 warm tile、跨平台或产品 SLO；fixture/artifact 不复制进任何仓库。

机械门槛：

- 请求 wire 不接受路径且 strict-reject unknown fields；
- socket parent 0700、socket 0600、same effective UID；
- descriptor type/owner/link/open flags、same-file 与 empty output fail closed；
- ticket/lease TTL、App/Job/generation、one-shot、revoke、expiry 和 registry drop 有确定性测试；
- fixture 的 working buffer 固定 64 KiB，算法不分配 full input/output；这不是 allocator/RSS 证明；
- 后续真实 benchmark 仍须测 OS copy volume、private dirty footprint、warm overhead 和 cancel latency。

## RawNIND 后续责任边界

进入 Phase 2 后，RGGB mapping、逐 CFA normalization、`[1,4,512,512]` tensor、reflect、halo/step、
crossfade/blending、两遍 global gain、stripe accumulation、output tensor interpretation 与质量 gate
必须共同进入 immutable RawNIND Build adapter。Shadow 不传物理模型名，也不逐 tile 调用。

Shadow 继续先查并独立验证 cache；cache hit 不需要 Infer 在线。cache miss 且 Infer 不可用时返回
deferred/unavailable，不静默执行旧 sidecar。迁移期 legacy 与 Infer route 使用不同 identity，除非
完整 artifact parity 证明允许共享。

## 非目标

- 通用大制品市场、对象仓库或任意 filesystem path broker；
- 跨节点或云端 payload transport；
- 万能 tensor-map、逐 tile RPC 或通用共享内存 ABI；
- Runtime 接管 Shadow cache、Recipe、Catalog、decoder 或 publish；
- 宣称 Windows、mmap、零拷贝或跨模型通用执行已经完成。

## 后果与复审

新增 `infer-artifact-lease` 独立 owner，避免扩大模型 artifact store、HTTP router、ONNX provider 或
durable payload spool。Phase 1 通过后，typed `raw.materialize_foundation`、RawNIND Build/adapter、
receipt、资源 estimate、Shadow ACL 和真实 HTTP + local data-plane E2E 已作为独立 Phase 2 门批准并
验证；该批准只适用于本 ADR 冻结的单一能力。

### Phase 2 contract checkpoint

`infer.raw.foundation@20260811.1` 保持单 input lease：HTTP 控制请求携带 strict、bounded
`infer.raw-foundation-staging@20260811.1` descriptor，输入 FD 只含 `<u2` row-major active Bayer
samples，输出 FD 只含新的未发布 `.shadowrawf` partial。descriptor 不复制 Shadow manifest 文本、
路径、orientation、colour matrix 或 opcode facts；Runtime 逐行读取并校验实际 bytes/digest，再派生
RGGB crop、normalization 与 packed tensor。

本机 graph/package digest 与冻结 Build 一致；Infer 当前受管 ONNX Runtime 1.27.0 使用独立
experimental immutable Build：implementation revision
`rawnind-public-bayer-foundation-ort127-exp1`、cache identity `rawnind-cpu-ort127-exp1`。旧
`rawnind-public-bayer-foundation-v1` 继续严格绑定 1.24.4，不能被重标、覆盖或共享 cache key。

Phase 2 synthetic comparator 的两遍 32-tile 执行得到与旧 1.24.4 完全相同的 payload/stripe/pixel：
sequence SHA-256 仍为
`1192be285728e479c1fce1a799046bd2252593f7f23f782a9c92d94abc10050a`，max-abs 与 RMSE 均为 0；
新 artifact 因 truthful runtime/implementation provenance 拥有不同的 file/cache/identity。滚动
writer 不创建显式整图输出 buffer，2048-row fixture 的最大 accumulator 为 824 rows。最终流式 route
三次 wall 为 13.247s、13.317s 与 14.182s，同轮 legacy comparator 13.411s；中位数低于 comparator，
但最慢样本约 +5.7%，样本尚少，不能视为已通过 warm p50 promotion gate。流式 route 一次实测 peak RSS 586,416,128 bytes，legacy comparator 574,373,888
bytes（约 +2.1%，ORT/tile buffer 主导且仍需重复采样）；single-tile in-flight cancel stop latency
1.7ms。endpoint/ACL/config/Deployment 已在 candidate.3 experimental 边界内开放；其他 App 默认无权
调用，Windows 仍 fail closed。

若 Unix 与 Windows 不能维持相同的 handle/TTL/one-shot/owner 语义，或实现必须退化为任意路径，
停止迁移并保留 Shadow 当前 sidecar owner。

### Consumer assembly checkpoint

该 assembly 已进入 production composition，但只在 exact `raw_foundation` graph/runtime/digest、
owner-only socket、Build/Deployment 和 App ACL 全部配置后由 `inferd` 挂载。配置验证还会拒绝
缺失或漂移的 Intent、rating evidence、Build feature、专用 Provider 和 unary Deployment。冻结顺序如下：

1. Bearer-authenticated `POST /infer/v1/raw/foundations/leases` 接受 strict
   `RawFoundationLeaseRequest`；App scope 只从 bearer 推导。Runtime 以强制
   local-first/local-only/offline/no-fallback/zero-cost constraints 先建立 queued Job，再签发 30 秒、
   App/Job/daemon-generation-bound registration ticket。
2. `201 raw.foundation.lease` 返回 `job_id`、one-shot `ticket_id`、绝对到期时间、generation，以及
   `{contract="infer-runtime.artifact-lease@20260811.1",transport="uds-scm-rights",endpoint}`。
   Consumer 向该 owner-only UDS 发送一行 strict
   `infer.artifact-lease.register@20260811.1` + exactly two SCM_RIGHTS FDs；返回 one-shot `lease_id`。
3. Bearer-authenticated `POST /infer/v1/raw/foundations` 仅接受 `{job_id,lease_id}`，再次以服务端 App
   scope 消费 lease，同步返回小型 `raw.foundation` receipt。Job/Attempt/progress 继续通过既有
   `/infer/v1/jobs/{job_id}`；typed cancel 为
   `POST /infer/v1/raw/foundations/{job_id}/cancel`，同时取消 Job 并按 scope revoke ticket/lease。

请求不存在 `app_id`、路径、manifest 或像素字段；未知/重复字段 fail closed。稳定错误包括
`invalid_api_key`、`invalid_request_error`、`raw_descriptor_invalid`、`raw_job_not_found`、
`artifact_lease_scope_mismatch`、`daemon_generation_changed`、`artifact_lease_invalid`、
`artifact_descriptor_invalid`、`cancelled`、`deadline_exceeded`、`no_candidate` 和既有 admission/
queue codes。TTL 清理将 abandoned queued Job 标记 expired；generation 改变使旧 capability 失效。

真实 E2E 已覆盖 auth、strict schema、server-derived App scope、queued snapshot、UDS/SCM_RIGHTS、
one-shot、跨 App 隔离、真实 ORT 1.27 execution、Attempt provenance、pending/in-flight cancel、正确
HTTP error code、终态 `cancelled` 和空 pending partial。响应 provenance 额外绑定 exact revision 与
graph SHA-256；Shadow strict client 同时验证专用 Provider 与这些 immutable identities。production
assembly 只把上述已验证 wire 接到 composition root；不改变 Shadow cache-before-runtime、独立
verifier/publisher 或 stale-result owner。
