# Local CI: the same checks as .github/workflows/ci.yml.
# Usage: pwsh scripts/ci.ps1   (or: powershell -File scripts/ci.ps1)
$ErrorActionPreference = 'Stop'
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
Set-Location (git rev-parse --show-toplevel)

function Invoke-Step([string]$Name, [scriptblock]$Command) {
    Write-Host "`n==> $Name"
    & $Command
    if ($LASTEXITCODE -ne 0) { Write-Host "local CI FAILED at: $Name"; exit $LASTEXITCODE }
}

Invoke-Step 'fmt'    { cargo fmt --all -- --check }
Invoke-Step 'clippy' { cargo clippy --all-targets --locked -- -D warnings }
Invoke-Step 'test'   { cargo test --locked }

$env:Path = "$env:USERPROFILE\.local\bin;$env:Path"
if (Get-Command uv -ErrorAction SilentlyContinue) {
    Invoke-Step 'python probe' { Push-Location probes/python; try { uv run --locked -q pytest -q } finally { Pop-Location } }
} else {
    Write-Host "`n(skipping python probe: uv not found)"
}

if (Get-Command npm -ErrorAction SilentlyContinue) {
    Invoke-Step 'js probe' { Push-Location probes/js; try { npm ci --silent; if ($LASTEXITCODE -ne 0) { throw 'npm ci failed' }; npm test --silent; if ($LASTEXITCODE -ne 0) { throw 'js probe tests failed' } } finally { Pop-Location } }
} else {
    Write-Host "`n(skipping js probe: npm not found)"
}

Write-Host "`nlocal CI passed"
