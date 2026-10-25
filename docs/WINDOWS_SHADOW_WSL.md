# Windows Shadow / WSL2 CUDA

Shadow 在 Windows 原生进程中使用官方 Rust SDK；Infer Runtime、ONNX Runtime 和
Ollama 在 Ubuntu 24.04 / WSL2 中运行。产品只依赖 Discovery、私有 credential 和类型化
Consumer API。模型文件、Linux 动态库和模型缓存留在 Linux 用户目录，不进入源码树。

## 准备

WSL 需要 Python 3.12、venv、curl、zstd、构建工具和 Rust 1.97.1。GPU 使用 Windows
NVIDIA 驱动经 WSL 提供的 `/usr/lib/wsl/lib/libcuda.so.1`；这里的安装脚本只安装用户级
计算库。先确认 `/usr/lib/wsl/lib/nvidia-smi` 能看到 GPU。

以下命令在 WSL 中运行。调整源码路径，保留 Runtime 和构建目录在 Linux 文件系统：

```bash
repo_root=/mnt/d/Lab/infer-runtime
runtime_root="$HOME/.local/share/infer-runtime"
export CARGO_TARGET_DIR="$HOME/.cache/infer-runtime/target"
source "$HOME/.cargo/env"
cd "$repo_root"
cargo +1.97.1 build --locked -p infer -p inferd
bash tools/install_onnx_cuda.sh "$runtime_root"
bash tools/install_ollama_wsl.sh "$runtime_root"
```

ONNX 安装脚本固定 ORT 1.27.0 的官方 CUDA 12 archive 和 SHA-256，安装匹配的
CUDA 12.8 / cuDNN 9 用户级依赖。直接安装 PyPI 的 `onnxruntime-gpu==1.27.0`
会得到 CUDA 13 构建，不适用于这套 CUDA 12 库。Ollama 固定 v0.40.11 并验证官方
archive 摘要；不创建系统服务。

## 模型和配置

```bash
stage="$runtime_root/.infer-runtime/staging/shadow-vision"
python3 tools/prepare_shadow_vision.py --staging-root "$stage"
python3 -m venv "$runtime_root/.infer-runtime/runtimes/siglip-export"
export_python="$runtime_root/.infer-runtime/runtimes/siglip-export/bin/python"
"$export_python" -m pip install -r tools/requirements-siglip2-export.txt
"$export_python" tools/export_siglip2.py \
  --checkpoint "$stage/siglip2-source" --output-directory "$stage/siglip2-onnx"
```

下载器从现有固定 Build 和导出器读取来源摘要，校验后才发布 staging 文件。SigLIP
图像/文本导出必须分别匹配示例配置的 SHA-256。CUDA Build 使用独立 ID；原 CPU
Build、图、预处理、tokenizer 和语义空间定义不变。

首次安装时，从 `config/shadow-wsl.example.toml` 创建
`$runtime_root/config/infer.toml`，把 `@RUNTIME_ROOT@` 替换为上面的 Linux **绝对路径**。
该配置包含最小 Shadow ACL、四个 CUDA Build 和 Qwen3-VL 4B/8B。已有配置时合并需要的
条目；保留自己的 App 和 credential 设置。配置文件设为 `0600`，父目录设为 `0700`。

```bash
cd "$runtime_root"
cli="$CARGO_TARGET_DIR/debug/infer"
config="$runtime_root/config/infer.toml"
"$cli" --config "$config" import-onnx yunet_2026may_onnx_cuda_v1 \
  --file "$stage/yunet_2026may_onnx_cpu_v1.onnx"
"$cli" --config "$config" import-onnx sface_2021dec_onnx_cuda_v1 \
  --file "$stage/sface_2021dec_onnx_cpu_v1.onnx"
"$cli" --config "$config" import-onnx siglip2_base_patch16_224_image_onnx_cuda_v1 \
  --file "$stage/siglip2-onnx/siglip2-base-patch16-224-image.onnx"
"$cli" --config "$config" import-onnx siglip2_base_patch16_224_text_onnx_cuda_v1 \
  --file "$stage/siglip2-onnx/siglip2-base-patch16-224-text.onnx"
"$cli" --config "$config" import-onnx-auxiliary siglip2_base_patch16_224_text_onnx_cuda_v1 \
  tokenizer --file "$stage/siglip2-source/tokenizer.json"
```

## 启动与接入

在源码目录的 Windows PowerShell 7 中启动：

```powershell
.\tools\start_shadow_wsl.ps1
```

启动器管理本次 Console/Runtime 和自己创建的 Ollama 子进程，并发布当前 Windows
Discovery generation。退出启动器会结束它管理的服务。已有独立 Ollama 服务会保留。
Console 为 `http://127.0.0.1:8790/`，Consumer 当前监听 `127.0.0.1:8787`。
Ollama 使用显式内部端口 `12434`，避开本机 Hyper-V 保留的默认端口范围；修改端口时
同步修改启动器和 Provider 配置。

首次运行后，在另一个 Windows PowerShell 中复制已生成的 Shadow 私有 credential：

```powershell
.\tools\connect_wsl_shadow.ps1
```

该操作验证 WSL 文件的 UID/0600 权限、服务身份和健康状态，将 Consumer Core offer
发布到 `%LOCALAPPDATA%\infra-protocol`，将 token 写到
`%LOCALAPPDATA%\Shadow\credentials\infer-runtime-shadow.token`。
Windows 目录/文件使用当前用户 + SYSTEM 的私有 ACL，拒绝重解析路径。token 不进入
Discovery、命令行或日志。Credential 轮换后重新执行此命令；单纯重启由启动器刷新
Discovery，不复制 token。Shadow 使用包含 Windows ACL 支持的 SDK revision，沿现有
credential/Discovery 默认路径连接，不需要设置固定 endpoint override。

在 WSL 的另一个终端中补齐 Qwen 模型；运行时不自动下载模型：

```bash
export OLLAMA_HOST=127.0.0.1:12434
ollama="$runtime_root/.infer-runtime/runtimes/ollama-v0.40.11/bin/ollama"
"$ollama" pull qwen3-vl:4b
"$ollama" pull qwen3-vl:8b
```

4B 对应基础质量档，8B 对应通用质量档。启动器禁用 Ollama cloud，固定 8192 context、
单请求和同时最多一个 Ollama 模型驻留。ONNX 会话和 Ollama 是不同内存所有者；这不构成
跨进程显存总预算。Linux 压力采样尚未实现，不能依赖自动显存驱逐。

## 验证

Windows 原生 SDK 可先验证完整 Core/Capability 握手和真实 App 鉴权：

```powershell
cargo run -p infer-runtime-client --example shadow_vision -- `
  "$env:LOCALAPPDATA\Shadow\credentials\infer-runtime-shadow.token" `
  "$env:LOCALAPPDATA\infra-protocol" connect
```

使用一张明确选定的公开人像 JPEG 替换 `<public-face.jpg>`，再执行 `all`：

```powershell
$env:INFER_SMOKE_REQUIRE_CUDA = '1'
cargo run -p infer-runtime-client --example shadow_vision -- `
  "$env:LOCALAPPDATA\Shadow\credentials\infer-runtime-shadow.token" `
  "$env:LOCALAPPDATA\infra-protocol" all '<public-face.jpg>'
```

例子只输出连接与执行来源，检查 YuNet/SFace 实际 CUDA、合格 128d 人脸向量，以及
SigLIP 图文同一 768d 空间。Qwen 结果必须通过正式结构校验；其 GPU 驻留另用 Ollama
`/api/ps` 的 `size_vram` 和服务日志确认。仅 `nvidia-smi` 可见设备不能证明模型使用 GPU。

`infer-provider` 的 `shadow_cuda_parity` 例子可在真实 CUDA 环境中对照同图、同 tokenizer
的 CPU 参考：从 Runtime 工作目录运行，使用启动器相同的 `LD_LIBRARY_PATH`，传入配置
和公开 JPEG。CPU 参考 manifest 只存在于临时 ArtifactStore，不放宽生产 CUDA Build。
它检查人脸坐标/置信度、SFace cosine ≥ 0.999、SigLIP 图文 cosine ≥ 0.9999。

`.gitattributes` 固定冻结契约文件为 LF；摘要校验绑定真实字节，不能通过放宽 SDK 校验
来掩盖 CRLF 差异。Windows RAW descriptor 传输仍需独立的 HANDLE binding，不属于
本次图文/人脸 HTTP 能力接入。
