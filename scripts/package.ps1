# Builds the portable release of the Rust edition:
#   dist/Unbox-v<version>-win-x64/ and dist/Unbox-v<version>-win-x64.zip + SHA256SUMS.txt
[CmdletBinding()]
param(
    [string]$Version,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$cargoVersion = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $Version) { $Version = $cargoVersion }
$Version = $Version.TrimStart('v')
if ($Version -ne $cargoVersion) {
    throw "Release version $Version does not match Cargo.toml version $cargoVersion"
}

$pinnedDll = 'D480E5F7ACF03183DC8CD9FE341A06D34E768B3C6606C0DB4AEC2A7D0EB5098E'
$dll = Join-Path $root 'assets\7z.dll'
if ((Get-FileHash -LiteralPath $dll -Algorithm SHA256).Hash -ne $pinnedDll) {
    throw 'assets\7z.dll does not match the pinned 7-Zip ZS release.'
}

if (-not $SkipBuild) {
    cargo build --release --locked -p unbox
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
}
$exe = Join-Path $root 'target\release\unbox.exe'
if (-not (Test-Path -LiteralPath $exe)) { throw "Missing $exe" }

$name = "Unbox-v$Version-win-x64"
$dist = Join-Path $root 'dist'
$pkg = Join-Path $dist $name
$zip = Join-Path $dist "$name.zip"
if (Test-Path $pkg) { Remove-Item -LiteralPath $pkg -Recurse -Force }
if (Test-Path $zip) { Remove-Item -LiteralPath $zip -Force }
New-Item -ItemType Directory -Path (Join-Path $pkg 'licenses\7zip-zs') -Force | Out-Null

Copy-Item -LiteralPath $exe -Destination (Join-Path $pkg 'unbox.exe')
Copy-Item -LiteralPath $dll -Destination (Join-Path $pkg '7z.dll')

# The Skia renderer links the dynamic VC++ runtime; ship it app-local (redistributable
# files from the installed Visual Studio) so unbox.exe runs without vc_redist.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$crt = Get-ChildItem -Path (Join-Path $vs 'VC\Redist\MSVC\*\x64\Microsoft.VC14*.CRT') -Directory |
    Sort-Object FullName -Descending | Select-Object -First 1
if (-not $crt) { throw 'VC++ runtime redistributable not found.' }
foreach ($runtime in 'msvcp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll') {
    Copy-Item -LiteralPath (Join-Path $crt.FullName $runtime) -Destination $pkg
}
Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination $pkg
Copy-Item -LiteralPath (Join-Path $root 'README.md') -Destination $pkg
Copy-Item -LiteralPath (Join-Path $root 'assets\licenses\7zip-zs\THIRD-PARTY.txt') -Destination (Join-Path $pkg 'licenses\7zip-zs')

Compress-Archive -Path $pkg -DestinationPath $zip -CompressionLevel Optimal
$hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
Set-Content -LiteralPath (Join-Path $dist 'SHA256SUMS.txt') -Value "$hash  $name.zip" -Encoding ascii -NoNewline

Write-Host "Package: $zip"
Write-Host "SHA-256: $hash"
if ($env:GITHUB_OUTPUT) {
    "name=$name" >> $env:GITHUB_OUTPUT
    "zip=$zip" >> $env:GITHUB_OUTPUT
    "dir=$pkg" >> $env:GITHUB_OUTPUT
    "sha256=$hash" >> $env:GITHUB_OUTPUT
    "version=$Version" >> $env:GITHUB_OUTPUT
}
