param(
    [string]$Distribution = 'Ubuntu-24.04',
    [string]$RuntimeRoot
)
$ErrorActionPreference = 'Stop'
$launcher = Join-Path $PSScriptRoot 'start_shadow_wsl.sh'
$bridge = Join-Path $PSScriptRoot 'connect_wsl_shadow.ps1'
$linuxLauncher = & wsl.exe -d $Distribution --exec wslpath -a -u $launcher.Replace('\', '/')
if ($LASTEXITCODE -ne 0) { throw 'Cannot locate the WSL launcher.' }
# The bridge is a child of this session, and republishes the current generation
# after Runtime restarts. It never puts a credential into Discovery.
$publisher = Start-Job -ArgumentList $Distribution, $RuntimeRoot, $bridge -ScriptBlock {
    param($distro, $root, $connector)
    $previousError = $null
    while ($true) {
        try {
            $arguments = @{Distribution=$distro;RefreshDiscoveryOnly=$true}
            if ($root) { $arguments.RuntimeRoot = $root }
            & $connector @arguments | Out-Null
            $previousError = $null
        } catch {
            if ($previousError -ne $_.Exception.Message) {
                Write-Warning ('Discovery publisher: ' + $_.Exception.Message)
                $previousError = $_.Exception.Message
            }
        }
        Start-Sleep -Seconds 10
    }
}
try {
    $arguments = @('-d', $Distribution, '--exec', 'bash', $linuxLauncher.Trim())
    if ($RuntimeRoot) { $arguments += $RuntimeRoot }
    & wsl.exe @arguments
    $result = $LASTEXITCODE
} finally {
    Stop-Job $publisher
    Receive-Job $publisher
    Remove-Job $publisher
}
exit $result
