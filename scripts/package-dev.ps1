param(
    [string]$SdkRoot = (Join-Path $PSScriptRoot '..\dependancies\DiscordSocialSdk-1.10.19337\discord_social_sdk')
)

$ErrorActionPreference = 'Stop'
$project = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$output = Join-Path $project 'dist\WolfLink-0.1.0-dev'
$rust = Join-Path $project 'target\release'
$voice = Join-Path $project 'voice\build\Release'
$files = @(
    (Join-Path $rust 'WolfLink.exe'),
    (Join-Path $rust 'client.exe'),
    (Join-Path $rust 'server.exe'),
    (Join-Path $rust 'roblox-memory-scanner.exe'),
    (Join-Path $voice 'wolflink_voice_poc.exe'),
    (Join-Path $project 'FRIENDS-QUICKSTART.txt')
)
$runtime = Get-ChildItem -LiteralPath (Join-Path $SdkRoot 'bin\release') -File |
    Where-Object { $_.Extension -in '.dll', '.kef' }
$vcRedist = Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) 'Microsoft Visual Studio\2022\BuildTools\VC\Redist\MSVC'
$vcVersion = Get-ChildItem -LiteralPath $vcRedist -Directory |
    Where-Object { $_.Name -match '^\d+\.\d+\.\d+$' } |
    Sort-Object { [Version]$_.Name } -Descending |
    Select-Object -First 1
if (-not $vcVersion) { throw 'Visual C++ x64 redistributable files are unavailable' }
$vcRuntime = Join-Path $vcVersion.FullName 'x64\Microsoft.VC143.CRT'
$notice = Join-Path $SdkRoot 'License-Notices.txt'
$files += $runtime.FullName
$files += @('msvcp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll') |
    ForEach-Object { Join-Path $vcRuntime $_ }
$files += $notice
foreach ($file in $files) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
        throw "Missing required build component: $file"
    }
}
New-Item -ItemType Directory -Path $output -Force | Out-Null
foreach ($file in $files) { Copy-Item -LiteralPath $file -Destination $output -Force }
$hashes = Get-ChildItem -LiteralPath $output -File |
    Where-Object { $_.Name -ne 'SHA256SUMS.txt' } |
    Sort-Object Name |
    ForEach-Object { "{0}  {1}" -f (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(), $_.Name }
Set-Content -LiteralPath (Join-Path $output 'SHA256SUMS.txt') -Value $hashes -Encoding ascii
$archive = Join-Path $project 'dist\WolfLink-0.1.0-dev.zip'
Compress-Archive -Path (Join-Path $output '*') -DestinationPath $archive -Force
Write-Output "Development build: $output\WolfLink.exe"
Write-Output "Portable archive: $archive"
