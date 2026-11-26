<#
.SYNOPSIS
    Cleans Rust build artifacts in target/ and reports freed disk space on drive C:.
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Format-Bytes {
    param([Parameter(Mandatory = $true)][double]$Bytes)
    if ($Bytes -ge 1GB) {
        return "{0:N2} GB" -f ($Bytes / 1GB)
    }
    elseif ($Bytes -ge 1MB) {
        return "{0:N2} MB" -f ($Bytes / 1MB)
    }
    elseif ($Bytes -ge 1KB) {
        return "{0:N2} KB" -f ($Bytes / 1KB)
    }
    return "{0:N0} bytes" -f $Bytes
}

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot

try {
    $targetPath = Join-Path $repoRoot 'target'
    if (Test-Path -LiteralPath $targetPath) {
        $targetBytes = (Get-ChildItem -LiteralPath $targetPath -Recurse -Force -File -ErrorAction SilentlyContinue |
            Measure-Object -Property Length -Sum).Sum
        if ($null -eq $targetBytes) { $targetBytes = 0 }
        Write-Host ("Current target/ size: {0}" -f (Format-Bytes -Bytes $targetBytes))
    }
    else {
        Write-Host "target/ directory not found (0 bytes)."
    }

    $freeBefore = [System.IO.DriveInfo]::new('C').AvailableFreeSpace
    Write-Host ("Drive C: free space before clean: {0}" -f (Format-Bytes -Bytes $freeBefore))

    Write-Host "Running 'cargo clean'..."
    cargo clean
    if ($LASTEXITCODE -ne 0) {
        throw "cargo clean failed with exit code $LASTEXITCODE"
    }

    $freeAfter = [System.IO.DriveInfo]::new('C').AvailableFreeSpace
    $freed = [Math]::Max([int64]0, [int64]($freeAfter - $freeBefore))
    Write-Host ("Drive C: free space after clean:  {0}" -f (Format-Bytes -Bytes $freeAfter))
    Write-Host ("Freed disk space on drive C::     {0}" -f (Format-Bytes -Bytes $freed))
}
finally {
    Pop-Location
}
