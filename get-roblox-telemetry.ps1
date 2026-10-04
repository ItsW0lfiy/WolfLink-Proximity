param(
    [switch]$Continuous,
    [ValidateRange(1, 240)]
    [double]$Hz = 20
)

$ErrorActionPreference = 'Stop'

$projectRoot = $PSScriptRoot
$scanner = Join-Path $projectRoot 'target\release\roblox-memory-scanner.exe'
$buildInputs = @(
    Get-Item -LiteralPath (Join-Path $projectRoot 'Cargo.toml')
    Get-Item -LiteralPath (Join-Path $projectRoot 'Cargo.lock')
    Get-ChildItem -LiteralPath (Join-Path $projectRoot 'src') -Filter '*.rs' -File -Recurse
)
$needsBuild = -not (Test-Path -LiteralPath $scanner -PathType Leaf)

if (-not $needsBuild) {
    $scannerTimestamp = (Get-Item -LiteralPath $scanner).LastWriteTimeUtc
    $needsBuild = $null -ne ($buildInputs | Where-Object LastWriteTimeUtc -GT $scannerTimestamp | Select-Object -First 1)
}

if ($needsBuild) {
    Push-Location $projectRoot
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

if ($Continuous) {
    $invariantHz = $Hz.ToString([Globalization.CultureInfo]::InvariantCulture)
    & $scanner stream-telemetry --hz $invariantHz
    exit $LASTEXITCODE
}

$lines = @(& $scanner locate-humanoid-roots)
$exitCode = $LASTEXITCODE

if ($exitCode -ne 0) {
    $lines | Write-Output
    exit $exitCode
}

$start = -1
for ($index = 0; $index -lt $lines.Count; $index++) {
    if ($lines[$index].ToString().StartsWith('Local player:')) {
        $start = $index
        break
    }
}

if ($start -lt 0) {
    $lines | Write-Output
    throw 'Scanner succeeded but did not emit a local-player telemetry block.'
}

$lines[$start..($lines.Count - 1)] | Write-Output

