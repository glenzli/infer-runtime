param(
    [string]$Distribution = 'Ubuntu-24.04',
    [string]$RuntimeRoot,
    [switch]$RefreshDiscoveryOnly
)

$ErrorActionPreference = 'Stop'
$userSid = [Security.Principal.WindowsIdentity]::GetCurrent().User
$systemSid = [Security.Principal.SecurityIdentifier]::new('S-1-5-18')

function Invoke-WslText([string[]]$Arguments) {
    $value = & wsl.exe -d $Distribution --exec @Arguments
    if ($LASTEXITCODE -ne 0) { throw 'Unable to read WSL Runtime state.' }
    return ($value -join "`n").Trim()
}

function Assert-LocalPath([string]$Path) {
    $absolute = [IO.Path]::GetFullPath($Path)
    $drive = [IO.DriveInfo]::new([IO.Path]::GetPathRoot($absolute))
    if ($drive.DriveType -ne [IO.DriveType]::Fixed) {
        throw 'Runtime credentials and Discovery require a local fixed drive.'
    }
    $current = $absolute
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            if (([IO.File]::GetAttributes($current) -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw 'Runtime credential and Discovery paths cannot contain reparse points.'
            }
        }
        $parent = [IO.Path]::GetDirectoryName($current)
        if ($parent -eq $current) { break }
        $current = $parent
    }
}

function Protect-Directory([string]$Path) {
    Assert-LocalPath $Path
    [IO.Directory]::CreateDirectory($Path) | Out-Null
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetOwner($userSid)
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($sid in @($userSid, $systemSid)) {
        $rule = [Security.AccessControl.FileSystemAccessRule]::new(
            $sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
        $acl.AddAccessRule($rule)
    }
    [IO.FileSystemAclExtensions]::SetAccessControl([IO.DirectoryInfo]::new($Path), $acl)
}

function Write-PrivateFile([string]$Path, [byte[]]$Bytes) {
    Assert-LocalPath $Path
    $parent = [IO.Path]::GetDirectoryName($Path)
    Protect-Directory $parent
    $temporary = Join-Path $parent ('.' + [IO.Path]::GetFileName($Path) + '.' + [guid]::NewGuid().ToString('N') + '.tmp')
    try {
        [IO.File]::Open($temporary, 'CreateNew', 'Write', 'None').Dispose()
        $acl = [Security.AccessControl.FileSecurity]::new()
        $acl.SetOwner($userSid)
        $acl.SetAccessRuleProtection($true, $false)
        foreach ($sid in @($userSid, $systemSid)) {
            $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', 'Allow'))
        }
        [IO.FileSystemAclExtensions]::SetAccessControl([IO.FileInfo]::new($temporary), $acl)
        [IO.File]::WriteAllBytes($temporary, $Bytes)
        [IO.File]::Move($temporary, $Path, $true)
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary }
    }
}

if (-not $RuntimeRoot) {
    $linuxUserHome = Invoke-WslText @('printenv', 'HOME')
    $RuntimeRoot = "$linuxUserHome/.local/share/infer-runtime"
}
$linuxUid = Invoke-WslText @('id', '-u')
foreach ($directory in @($RuntimeRoot, "$RuntimeRoot/discovery", "$RuntimeRoot/discovery/registrations")) {
    $mode = Invoke-WslText @('stat', '-c', '%a:%u:%F', '--', $directory)
    if ($mode -ne "700:${linuxUid}:directory") { throw 'The WSL Runtime directory is not private.' }
}
$sourceManifest = "$RuntimeRoot/discovery/registrations/infer-runtime--local.json"
$manifestMode = Invoke-WslText @('stat', '-c', '%a:%u:%F', '--', $sourceManifest)
if ($manifestMode -ne "600:${linuxUid}:regular file") {
    throw 'The WSL Discovery manifest is not a private regular file.'
}
$manifest = (Invoke-WslText @('cat', '--', $sourceManifest)) | ConvertFrom-Json
if ($manifest.schema -ne 'infra.discovery.registration' -or $manifest.schema_version -ne '20260812.1' -or
    $manifest.service.kind -ne 'infer-runtime' -or $manifest.service.instance_id -ne 'local') {
    throw 'The WSL Discovery manifest has an incompatible identity.'
}
$offers = @($manifest.offers | Where-Object {
    $_.protocol -eq 'infer-runtime.consumer-core' -and $_.binding -eq 'infer-runtime.http-loopback'
})
if ($offers.Count -ne 1) { throw 'Exactly one Consumer Core offer is required.' }
$endpoint = [Uri]::new($offers[0].endpoint)
$address = $null
if ($endpoint.Scheme -ne 'http' -or -not [Net.IPAddress]::TryParse($endpoint.DnsSafeHost, [ref]$address) -or
    -not [Net.IPAddress]::IsLoopback($address) -or $endpoint.Port -eq 0 -or $endpoint.UserInfo -or
    $endpoint.AbsolutePath -ne '/' -or $endpoint.Query -or $endpoint.Fragment) {
    throw 'The WSL Consumer offer must use numeric HTTP loopback.'
}
$health = Invoke-RestMethod -Uri ($offers[0].endpoint + '/health') -NoProxy -TimeoutSec 5
if ($health.status -ne 'ok' -or $health.service -ne 'inferd') { throw 'The WSL Runtime is not healthy.' }

$discoveryRoot = Join-Path $env:LOCALAPPDATA 'infra-protocol'
Protect-Directory $discoveryRoot
$manifest.offers = $offers
$manifestPath = Join-Path $discoveryRoot 'registrations\infer-runtime--local.json'
$encodedManifest = [Text.UTF8Encoding]::new($false).GetBytes(($manifest | ConvertTo-Json -Depth 8) + "`n")
Write-PrivateFile $manifestPath $encodedManifest

$credentialPath = Join-Path $env:LOCALAPPDATA 'Shadow\credentials\infer-runtime-shadow.token'
if (-not $RefreshDiscoveryOnly) {
    $credentialDirectoryMode = Invoke-WslText @('stat', '-c', '%a:%u:%F', '--', "$RuntimeRoot/.infer-runtime/credentials")
    if ($credentialDirectoryMode -ne "700:${linuxUid}:directory") { throw 'The WSL credential directory is not private.' }
    $sourceCredential = "$RuntimeRoot/.infer-runtime/credentials/shadow.token"
    $credentialMode = Invoke-WslText @('stat', '-c', '%a:%u:%F', '--', $sourceCredential)
    if ($credentialMode -ne "600:${linuxUid}:regular file") { throw 'The WSL Shadow credential is not private.' }
    $token = Invoke-WslText @('cat', '--', $sourceCredential)
    if ($token -notmatch '^[a-f0-9]{64}$') { throw 'The managed Shadow credential has an invalid format.' }
    $tokenBytes = [Text.UTF8Encoding]::new($false).GetBytes($token + "`n")
    try { Write-PrivateFile $credentialPath $tokenBytes }
    finally { [Array]::Clear($tokenBytes); $token = $null }
}

[pscustomobject]@{DiscoveryRoot=$discoveryRoot;Generation=$manifest.service.generation;
    Endpoint=$offers[0].endpoint;CredentialFile=$credentialPath;CredentialUpdated=(-not $RefreshDiscoveryOnly)} | ConvertTo-Json
