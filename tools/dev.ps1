# dev.ps1 - one shell call instead of four.
#
# The trap this file exists for: on Windows, `cargo check 2>&1` from PowerShell
# exits with code 1 even on SUCCESS. 2>&1 folds cargo's (empty) stderr into the
# PowerShell error stream, PS 5.1 raises NativeCommandError, and $LASTEXITCODE
# ends up 1. Everything below therefore redirects `>` and `2>` to *files* and
# decides PASS/FAIL by reading them, never by trusting the merge.
#
# Usage (runs on Windows PowerShell 5.1 and on pwsh 7 - both verified):
#   pwsh -File tools\dev.ps1 check      cargo check --all-targets
#   pwsh -File tools\dev.ps1 build      cargo build (both binaries)
#   pwsh -File tools\dev.ps1 smoke      build + tools\fs_smoke.ps1
#   pwsh -File tools\dev.ps1 run        build + start the GUI from the repo root
#   pwsh -File tools\dev.ps1 core       build + start the core alone (no GUI)
#   pwsh -File tools\dev.ps1 kill       kill stray vanguard_os / vanguard_core
#   pwsh -File tools\dev.ps1 all        check + build + smoke
#
# Exit code is truthful: 0 = clean, 1 = findings, 2 = missing binary.
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet('check', 'build', 'smoke', 'run', 'core', 'kill', 'all')]
    [string]$cmd,

    # `check`/`build`/`smoke` lock target\debug\*.exe, so stray processes are
    # killed first by default. -keep skips that when the app must stay open.
    [switch]$keep
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

$tmp = Join-Path $env:TEMP 'opencode'
if (-not (Test-Path $tmp)) { New-Item -ItemType Directory -Path $tmp | Out-Null }

function Stop-Vanguard {
    Get-Process vanguard_os, vanguard_core -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
}

# Runs cargo with stdout/stderr in files, prints any error/warning line and a
# timing, and returns $true only for a clean run. Judgement is by file content.
function Invoke-Cargo {
    param([string[]]$cargoArgs)
    $out = Join-Path $tmp 'dev_out.txt'
    $err = Join-Path $tmp 'dev_err.txt'
    $sw = [Diagnostics.Stopwatch]::StartNew()
    # cargo writes `Finished ...` to *stderr*. Under $ErrorActionPreference='Stop'
    # a `2>` redirect turns that into a terminating NativeCommandError and the
    # script dies on a SUCCESSFUL build - so the preference is relaxed here and
    # only here, and the verdict below comes from the files, not from the stream.
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & cargo @cargoArgs > $out 2> $err
    $ErrorActionPreference = $prevEap
    $sw.Stop()
    $bad = @(Select-String -Path $out, $err -Pattern '^(error|warning)' -ErrorAction SilentlyContinue)
    foreach ($b in $bad) { Write-Host ('    ' + $b.Line) }
    $label = 'cargo ' + ($cargoArgs -join ' ')
    if ($bad.Count -gt 0) {
        Write-Host ('FAIL  ' + $label + '  (' + $bad.Count + ' findings, ' + [int]$sw.ElapsedMilliseconds + ' ms)')
        return $false
    }
    Write-Host ('OK    ' + $label + '  (' + [int]$sw.ElapsedMilliseconds + ' ms)')
    return $true
}

function Build-Core {
    $exe = Join-Path $repo 'target\debug\vanguard_core.exe'
    if (-not (Test-Path -LiteralPath $exe)) {
        Write-Host ('MISSING ' + $exe)
        return $false
    }
    return $true
}

$ok = $true

switch ($cmd) {
    'kill' {
        Stop-Vanguard
        Write-Host 'OK    stray vanguard_* processes killed'
    }
    'check' {
        if (-not $keep) { Stop-Vanguard }
        $ok = Invoke-Cargo @('check', '--all-targets')
    }
    'build' {
        if (-not $keep) { Stop-Vanguard }
        $ok = Invoke-Cargo @('build')
    }
    'smoke' {
        if (-not $keep) { Stop-Vanguard }
        $ok = Invoke-Cargo @('build')
        if ($ok -and (Build-Core)) {
            & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'fs_smoke.ps1') -repo $repo
            $ok = ($LASTEXITCODE -eq 0)
        } elseif ($ok) {
            $ok = $false
        }
    }
    'run' {
        if (-not $keep) { Stop-Vanguard }
        $ok = Invoke-Cargo @('build')
        if ($ok) {
            Start-Process -FilePath (Join-Path $repo 'target\debug\vanguard_os.exe') `
                -WorkingDirectory $repo
            Write-Host ('OK    started target\debug\vanguard_os.exe from ' + $repo)
        }
    }
    'core' {
        if (-not $keep) { Stop-Vanguard }
        $ok = Invoke-Cargo @('build')
        if ($ok -and (Build-Core)) {
            Start-Process -FilePath (Join-Path $repo 'target\debug\vanguard_core.exe') `
                -WorkingDirectory $repo -WindowStyle Hidden
            Write-Host ('OK    started vanguard_core.exe on 127.0.0.1:7331 from ' + $repo)
        } elseif ($ok) {
            $ok = $false
        }
    }
    'all' {
        if (-not $keep) { Stop-Vanguard }
        $ok = Invoke-Cargo @('check', '--all-targets')
        if ($ok) { $ok = Invoke-Cargo @('build') }
        if ($ok -and (Build-Core)) {
            & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'fs_smoke.ps1') -repo $repo
            $ok = ($LASTEXITCODE -eq 0)
        } elseif ($ok) {
            $ok = $false
        }
    }
}

if ($ok) { exit 0 } else { exit 1 }
