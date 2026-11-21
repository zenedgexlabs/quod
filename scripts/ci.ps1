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

Write-Host "`nlocal CI passed"
