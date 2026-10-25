# Windows / WSL 常驻安装

此安装面向当前 Windows 用户：登录后隐藏启动 WSL2，分别托管 Runtime、Console 和
Ollama，并向原生 Infra Sentinel 发布只读 Windows 管道。关闭 Codex 或源码终端不会
结束安装实例。它不是登录前运行的 Windows SYSTEM 服务；发行版、GPU 与数据仍属于
当前用户。Windows x64 需要已有 Ubuntu/WSL systemd 和系统内置 .NET Framework 4.8。

先按 [Shadow / CUDA 准备](WINDOWS_SHADOW_WSL.md) 完成模型、库、配置和二进制验证。
配置需包含示例中的 `observer.http_credential_id = "infra-sentinel"`，以及只授予
`observer_access = "summary"`、空 Intent ACL 的 managed App。先停掉临时开发启动器，
再在 Windows PowerShell 7 执行：

```powershell
.\tools\install_windows_wsl.ps1 -ReleaseId 'my-qualified-build-1'
```

参数可指定 `-Distribution`、`-RuntimeRoot`、`-LinuxBinDirectory`、`-InstallRoot`。
WSL 发行版名和 Linux 安装路径目前只接受 ASCII 字母、数字、`_./-`；Runtime 根路径
需位于当前 Linux 用户 home，不能包含空白、引号或 `%`。安装不会编译 Linux 源码，
也不会下载模型。它复制已验证的 `infer` / `inferd` 并仅移除调试符号，在安装清单中
记录原文件与安装文件摘要。

## 日常管理

安装后的管理入口不依赖源码 checkout：

```powershell
& "$env:USERPROFILE\.infer-runtime\installed\manage.ps1" status
& "$env:USERPROFILE\.infer-runtime\installed\manage.ps1" restart
& "$env:USERPROFILE\.infer-runtime\installed\manage.ps1" stop
& "$env:USERPROFILE\.infer-runtime\installed\manage.ps1" start
& "$env:USERPROFILE\.infer-runtime\installed\manage.ps1" uninstall
```

`uninstall` 取消登录任务并禁用三个服务，保留模型、配置、凭据、历史与安装文件。
升级时用新的 release ID 重新运行安装器；新二进制在下一次服务重启时生效。
只更新 Windows 宿主或同步观测凭据时加 `-WindowsHostOnly`，保留现有 Linux 程序和服务。
Console 仍在 <http://127.0.0.1:8790/>，但 daemon 的生命周期由 systemd 管理，
重启与停止请用上面的管理入口。用户级服务均设置为异常退出后恢复。

| 内容 | 位置 |
| --- | --- |
| Windows 登录任务 | `Infer Runtime (WSL)`，当前用户 InteractiveToken，无密码 |
| Windows 宿主、管理入口、安装清单 | `%USERPROFILE%\.infer-runtime\installed` |
| Windows 宿主日志 | 安装目录 `logs\host.log`，1 MiB × 2 轮转 |
| Linux 版本化程序 | `~/.local/lib/infer-runtime/releases/<release>`，`current` 指向当前版本 |
| Linux 服务 | `infer-runtime`、`infer-runtime-console`、`infer-runtime-ollama`，均为 `systemctl --user` |
| Linux 服务日志 | `journalctl --user -u infer-runtime -u infer-runtime-console -u infer-runtime-ollama` |
| Linux 模型与持久数据 | 原 RuntimeRoot，默认 `~/.local/share/infer-runtime` |

Windows 宿主持有一个无窗口 WSL 调用，发行版关闭后重连。仅启用 systemd 不足以保证
WSL 实例常驻，参见 [Microsoft 的说明](https://learn.microsoft.com/en-us/windows/wsl/systemd)。
登录任务采用 [LogonTrigger](https://learn.microsoft.com/en-us/windows/win32/taskschd/logontrigger)，
不保管 Windows 密码，不切换为 SYSTEM 身份。任务异常退出由任务计划程序重试；
正常停止任务不触发自动重试。服务停止和任务停止应通过同一管理入口执行。

## Sentinel 自动发现

原生 Sentinel 使用的规范 Windows 根为：

```text
%LOCALAPPDATA%\Infra Protocol\Runtime\registrations\infer-runtime--local.json
```

安装宿主在真实 Windows 登录任务中发布到该路径，因此也适用于从 MSIX 应用发起安装。
为兼容现有 Shadow SDK，宿主同时维护 `%LOCALAPPDATA%\infra-protocol` 下同名注册。
两处仅在实例/源 daemon generation 改变时原子替换，不写心跳或 lease。

Windows offer 为 `infer-runtime.status@20260810.1` +
`infra.local.windows-named-pipe`。管道位于 `\\.\pipe\infra-protocol\ir-<opaque>`，
只允许当前用户与 SYSTEM，内核拒绝远程连接；读取请求前再校验连接进程 SID 为当前用户。
它保留状态协议的 512 字节请求、256 KiB 响应、单 LF 帧和 EOF、两秒期限。

桥只访问专用 App 的 `/infer/v1/observer/snapshot`：关闭代理与重定向，不复用 Shadow
或 operator 凭据，不转发任意 URL、推理或管理命令。Windows 私有凭据仅复制一次用于
安装，之后每次请求从私有文件读取；旋转该 managed App 时需重新安装同步凭据。

Windows facade 的 generation 由宿主实例和 Linux daemon generation 共同确定。
桥仅把 snapshot 的 `service.generation` 映射为此 Windows generation；指标、隐私声明、
状态和采样时间均来自真实 daemon。Linux daemon 重启后旧源 generation 的响应会被拒绝，
重新发布后 Sentinel 用自身的身份与注册复读检查接受新实例。

若 Sentinel 显示 `infer.resource.pressure_unknown`，表示当前 Linux Runtime 尚无受支持的
全机压力采样后端。该 `degraded` 状态如实透传；它不表示 CUDA 或模型执行失败。

## 验证与源码归属

```powershell
.\tools\test_windows_wsl_host.ps1
```

测试执行生产管道与 HTTP 代码，覆盖私有 ACL、请求语义与重复字段拒绝、受限上游请求、
大响应完整性及 EOF、空闲连接期限、阻塞 HTTP、源重启和稳定注册不重复写入。
安装验收还应读取真实 Sentinel Projection，验证设施出现且协议、版本和 binding 正确；
构造夹具不能替代实际产品互操作。

`apps/infer-wsl-host` 拥有 Windows 唤起/恢复与只读状态传输，`tools/service` 和安装器
拥有已安装进程的系统集成。它们不拥有模型执行、路由、计量、Console 产品逻辑或
Sentinel Projection。Windows 宿主用系统 Framework 编译成独立 `winexe`，不要求最终
用户安装 .NET SDK、Rust、Python 或 Node；WSL 服务只运行已安装的程序与现有计算库。
