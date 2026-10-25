param([string]$OutputDirectory = (Join-Path (Split-Path $PSScriptRoot) '.infer-runtime\host-tests'))
$ErrorActionPreference = 'Stop'
[IO.Directory]::CreateDirectory($OutputDirectory) | Out-Null
$compiler=Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
$source=Join-Path (Split-Path $PSScriptRoot) 'apps\infer-wsl-host'
$files=@(Get-ChildItem -LiteralPath $source -Filter '*.cs' | ForEach-Object FullName)
$files+=Join-Path $source 'tests\StatusBridgeTests.cs'
$output=Join-Path $OutputDirectory 'status-bridge-tests.exe'
& $compiler /nologo /target:exe /main:InferWslHost.StatusBridgeTests /optimize+ /warn:4 /warnaserror+ /reference:System.Net.Http.dll /reference:System.Web.Extensions.dll /reference:System.Runtime.Serialization.dll "/out:$output" @files
if ($LASTEXITCODE -ne 0) { throw 'Status bridge tests did not compile.' }
& $output
if ($LASTEXITCODE -ne 0) { throw 'Status bridge checks failed.' }
