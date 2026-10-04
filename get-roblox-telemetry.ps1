$ErrorActionPreference = 'Stop'

$projectRoot = $PSScriptRoot
$scanner = Join-Path $projectRoot 'target\release\roblox-memory-scanner.exe'

if (-not (Test-Path -LiteralPath $scanner -PathType Leaf)) {
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

