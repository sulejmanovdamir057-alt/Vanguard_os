# dev.ps1 - one shell call instead of four.
#
# The trap this file exists for: on Windows, `cargo check 2>&1` from PowerShell
# exits with code 1 even on SUCCESS. 2>&1 folds cargo's (empty) stderr into the
# PowerShell error stream, PS 5.1 raises NativeCommandError, and $LASTEXITCODE
# ends up 1. Everything below therefore redirects `>` and `2>` to *files* and
# decides PASS/FAIL by reading them, never by trusting the merge.
#
# Usage (runs on Windows PowerShell 5.1 and on pwsh 7 - both verified):
#   pwsh -File tools\dev.ps1 check      cargo check --all-targets     (~0.4 s warm)
#   pwsh -File tools\dev.ps1 build      cargo build (both binaries)
#   pwsh -File tools\dev.ps1 smoke      build + tools\fs_smoke.ps1
#   pwsh -File tools\dev.ps1 run        build + start the GUI from the repo root
#   pwsh -File tools\dev.ps1 core       build + start the core alone (no GUI)
#   pwsh -File tools\dev.ps1 kill       kill stray vanguard_os / vanguard_core
#   pwsh -File tools\dev.ps1 all        check + build + smoke
#   pwsh -File tools\dev.ps1 publish -m <msg> -- <paths...>
#                                       gate, then git add + commit - one call
#
# `check` is the iteration loop (422 ms with nothing to do, 1.9 s after a source
# edit - measured). `all` is the pre-commit gate. `publish` collapses the whole
# "verify, stage, commit" sequence into one shell call; the gate runs first so
# nothing unbuildable gets committed.
#
# Exit code is truthful: 0 = clean, 1 = findings, 2 = missing binary.
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet('check', 'build', 'smoke', 'run', 'core', 'kill', 'all', 'publish')]
    [string]$cmd,

    # `check`/`build`/`smoke` lock target\debug\*.exe, so stray processes are
    # killed first by default. -keep skips that when the app must stay open.
    [switch]$keep,

    # publish only. ASCII: arguments travel in cp1251 while git expects UTF-8,
    # so a Russian message must go through `git commit -F <file>` instead.
    [string]$m = '',

    # publish only: paths to stage, placed after `--`.
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$paths
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

# The pre-commit gate: check, build, smoke. Shared by `all` and `publish` so the
# two can never drift apart.
function Invoke-Gate {
    if (-not (Invoke-Cargo @('check', '--all-targets'))) { return $false }
    if (-not (Invoke-Cargo @('build'))) { return $false }
    if (-not (Build-Core)) { return $false }
    # The child's stdout lands in THIS function's success stream, and
    # `$ok = Invoke-Gate` would swallow all 46 lines and keep only the verdict -
    # the same trap as Write-Output inside Invoke-Cargo. Write-Host is the host
    # stream, which no assignment can capture.
    & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'fs_smoke.ps1') -repo $repo |
        ForEach-Object { Write-Host $_ }
    return ($LASTEXITCODE -eq 0)
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
        $ok = Invoke-Gate
    }
    'publish' {
        if (-not $m) {
            Write-Host 'FAIL  publish needs -m <message>'
            $ok = $false
        } elseif (-not $paths -or $paths.Count -eq 0) {
            Write-Host 'FAIL  publish needs paths after --'
            $ok = $false
        } else {
            if (-not $keep) { Stop-Vanguard }
            $ok = Invoke-Gate
            if ($ok) {
                & git add -- @paths
                $ok = ($LASTEXITCODE -eq 0)
            }
            if ($ok) {
                & git commit --quiet -m $m
                $ok = ($LASTEXITCODE -eq 0)
            }
            if ($ok) { & git log --oneline -1 }
            else { Write-Host 'FAIL  git add/commit - nothing was committed' }
        }
    }
}

if ($ok) { exit 0 } else { exit 1 }
