param(
    [string]$Distribution = 'Ubuntu-24.04',
    [string]$RuntimeRoot = '/home/glen/.local/share/infer-runtime',
    [string]$LinuxBinDirectory = '/home/glen/.cache/infer-runtime/target/debug',
    [string]$InstallRoot = (Join-Path $env:USERPROFILE '.infer-runtime\installed'),
    [string]$ReleaseId,
    [switch]$WindowsHostOnly
)
$ErrorActionPreference = 'Stop'
if (-not $ReleaseId) { $ReleaseId = 'local-' + (Get-Date -Format 'yyyyMMdd-HHmmss') }
if ($ReleaseId -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,95}$') { throw 'Invalid release ID.' }
$userSid = [Security.Principal.WindowsIdentity]::GetCurrent().User
$systemSid = [Security.Principal.SecurityIdentifier]::new('S-1-5-18')

function Protect-Directory([string]$Path) {
    $absolute = [IO.Path]::GetFullPath($Path)
    if ($absolute -notlike ($env:USERPROFILE + '\*') -or $absolute.Contains('..')) { throw 'Install into the current user profile.' }
    for ($parent=$absolute; $parent; $parent=[IO.Path]::GetDirectoryName($parent)) {
        if ((Test-Path -LiteralPath $parent) -and ([IO.File]::GetAttributes($parent) -band [IO.FileAttributes]::ReparsePoint)) { throw 'Install paths cannot contain reparse points.' }
    }
    [IO.Directory]::CreateDirectory($absolute) | Out-Null
    $acl=[Security.AccessControl.DirectorySecurity]::new()
    $acl.SetOwner($userSid); $acl.SetAccessRuleProtection($true,$false)
    foreach ($sid in @($userSid,$systemSid)) {
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow'))
    }
    [IO.FileSystemAclExtensions]::SetAccessControl([IO.DirectoryInfo]::new($absolute),$acl)
}

function Wsl-Text([string[]]$Arguments) {
    $value = & wsl.exe --distribution $Distribution --exec @Arguments
    if ($LASTEXITCODE -ne 0) { throw 'WSL installation operation failed.' }
    return ($value -join "`n").Trim()
}

Protect-Directory $InstallRoot
if($WindowsHostOnly){
    $previous=Get-Content -Raw -LiteralPath (Join-Path $InstallRoot 'install-manifest.json') | ConvertFrom-Json
    $Distribution=$previous.Distribution
    $RuntimeRoot=$previous.RuntimeRoot
}
$release = Join-Path $InstallRoot "releases\$ReleaseId"
if (Test-Path -LiteralPath $release) { throw 'Choose a new release ID for this installation.' }
Protect-Directory $release
Protect-Directory (Join-Path $InstallRoot 'credentials')
Protect-Directory (Join-Path $InstallRoot 'logs')
$compiler = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
if (-not (Test-Path -LiteralPath $compiler)) { throw '.NET Framework 4.8 is required.' }
$source = Join-Path (Split-Path $PSScriptRoot) 'apps\infer-wsl-host'
$sources = @(Get-ChildItem -LiteralPath $source -Filter '*.cs' | ForEach-Object FullName)
& $compiler /nologo /target:winexe /platform:x64 /optimize+ /warn:4 /warnaserror+ /reference:System.Net.Http.dll /reference:System.Web.Extensions.dll /reference:System.Runtime.Serialization.dll "/out:$release\infer-wsl-host.exe" @sources
if ($LASTEXITCODE -ne 0) { throw 'Windows host build failed.' }
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'manage_windows_wsl.ps1') -Destination (Join-Path $InstallRoot 'manage.ps1')
if(-not $WindowsHostOnly){
    $linuxInstaller = Wsl-Text @('wslpath','-a','-u',(Join-Path $PSScriptRoot 'install_wsl_service.sh').Replace('\','/'))
    Wsl-Text @('bash',$linuxInstaller,$RuntimeRoot,$LinuxBinDirectory,$ReleaseId) | Write-Output
}
$linuxInstall = (Wsl-Text @('printenv','HOME')) + '/.local/lib/infer-runtime'
$credential = Join-Path $InstallRoot 'credentials\infra-sentinel.token'
$ready=$false
for($attempt=0; $attempt -lt 60; $attempt++) {
    & wsl.exe --distribution $Distribution --exec test -f "$RuntimeRoot/.infer-runtime/credentials/infra-sentinel.token"
    if($LASTEXITCODE -eq 0){$ready=$true;break}
    Start-Sleep -Milliseconds 300
}
if(-not $ready){throw 'The daemon did not create its managed summary-only credential.'}
$linuxUid=Wsl-Text @('id','-u')
$credentialDirectoryMode=Wsl-Text @('stat','-c','%a:%u:%F','--',"$RuntimeRoot/.infer-runtime/credentials")
$credentialMode=Wsl-Text @('stat','-c','%a:%u:%F','--',"$RuntimeRoot/.infer-runtime/credentials/infra-sentinel.token")
if($credentialDirectoryMode -ne "700:${linuxUid}:directory" -or $credentialMode -ne "600:${linuxUid}:regular file") {
    throw 'The WSL observer credential must be a private, current-user regular file.'
}
$token = Wsl-Text @('cat','--',"$RuntimeRoot/.infer-runtime/credentials/infra-sentinel.token")
try {
    if ($token -notmatch '^[a-f0-9]{64}$') { throw 'A managed summary-only infra-sentinel App must be configured first.' }
    [IO.File]::WriteAllText($credential,$token+"`n",[Text.UTF8Encoding]::new($false))
} finally { $token=$null }
$settings = [ordered]@{
    Distribution=$Distribution; LinuxInstall=$linuxInstall; CredentialFile=$credential;
    DiscoveryRoot=(Join-Path $env:LOCALAPPDATA 'Infra Protocol\Runtime');
    LegacyDiscoveryRoot=(Join-Path $env:LOCALAPPDATA 'infra-protocol');
    LogDirectory=(Join-Path $InstallRoot 'logs'); Endpoint='http://127.0.0.1:8787'
}
$config = Join-Path $InstallRoot 'host.json'
[IO.File]::WriteAllText($config,($settings | ConvertTo-Json),[Text.UTF8Encoding]::new($false))
$taskName='Infer Runtime (WSL)'
$action=New-ScheduledTaskAction -Execute (Join-Path $release 'infer-wsl-host.exe') -Argument ('"'+$config+'"') -WorkingDirectory $InstallRoot
$trigger=New-ScheduledTaskTrigger -AtLogOn -User $userSid.Value
$trigger.Delay='PT10S'
$principal=New-ScheduledTaskPrincipal -UserId $userSid.Value -LogonType Interactive -RunLevel Limited
$taskSettings=New-ScheduledTaskSettingsSet -Hidden -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
$existing=Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
if ($existing) {
    Stop-ScheduledTask -InputObject $existing
    for($attempt=0; $attempt -lt 50; $attempt++) {
        if((Get-ScheduledTask -TaskName $taskName).State -ne 'Running'){break}
        Start-Sleep -Milliseconds 100
    }
    # The previous process must release its per-user singleton before launch.
    $oldHosts=@(Get-Process -Name infer-wsl-host -ErrorAction SilentlyContinue | Where-Object { $_.Path -like ($InstallRoot+'\releases\*') })
    foreach($oldHost in $oldHosts){$oldHost.WaitForExit(5000) | Out-Null}
}
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $taskSettings -Description 'Current-user WSL CUDA Runtime and read-only Infra Protocol Windows status bridge.' -Force | Out-Null
$manifest=[ordered]@{Release=$ReleaseId;Distribution=$Distribution;RuntimeRoot=$RuntimeRoot;LinuxInstall=$linuxInstall;TaskName=$taskName;
    LinuxRelease=(Wsl-Text @('readlink','-f',"$linuxInstall/current"));
    HostExecutable=(Join-Path $release 'infer-wsl-host.exe');HostSha256=(Get-FileHash -LiteralPath (Join-Path $release 'infer-wsl-host.exe') -Algorithm SHA256).Hash.ToLowerInvariant()}
[IO.File]::WriteAllText((Join-Path $InstallRoot 'install-manifest.json'),($manifest | ConvertTo-Json),[Text.UTF8Encoding]::new($false))
Start-ScheduledTask -TaskName $taskName
$manifest | ConvertTo-Json
