param(
    [Parameter(Mandatory=$true)][ValidateSet('status','start','stop','restart','uninstall')][string]$Action,
    [string]$InstallRoot = (Join-Path $env:USERPROFILE '.infer-runtime\installed')
)
$ErrorActionPreference='Stop'
$manifest=Get-Content -Raw -LiteralPath (Join-Path $InstallRoot 'install-manifest.json') | ConvertFrom-Json
$units=@('infer-runtime-console.service','infer-runtime.service','infer-runtime-ollama.service')
function Invoke-Service([string]$Operation) {
    & wsl.exe --distribution $manifest.Distribution --exec systemctl --user $Operation @units
    if ($LASTEXITCODE -ne 0) { throw 'The WSL service operation failed.' }
}
switch ($Action) {
    'status' {
        Get-ScheduledTask -TaskName $manifest.TaskName | Select-Object TaskName,State
        & wsl.exe --distribution $manifest.Distribution --exec systemctl --user --no-pager status @units
    }
    'start' { Start-ScheduledTask -TaskName $manifest.TaskName; Invoke-Service 'start' }
    'stop' { Stop-ScheduledTask -TaskName $manifest.TaskName; Invoke-Service 'stop' }
    'restart' { Invoke-Service 'restart'; Start-ScheduledTask -TaskName $manifest.TaskName }
    'uninstall' {
        Stop-ScheduledTask -TaskName $manifest.TaskName
        Unregister-ScheduledTask -TaskName $manifest.TaskName -Confirm:$false
        & wsl.exe --distribution $manifest.Distribution --exec systemctl --user disable --now @units
        if ($LASTEXITCODE -ne 0) { throw 'Could not disable the installed WSL services.' }
        Write-Output 'Autostart and services disabled. Models, configuration, credentials, history, and installed files retained.'
    }
}
