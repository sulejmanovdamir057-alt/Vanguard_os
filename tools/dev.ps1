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
#   pwsh -File tools\dev.ps1 clean      force both cleanup tiers right now
#   pwsh -File tools\dev.ps1 dist       release build -> dist\vanguard_os.zip
#   pwsh -File tools\dev.ps1 publish -m <msg> -- <paths...>
#                                       gate, then git add + commit - one call
#
# Cleanup is AUTOMATIC (see the two-tier block below): every run drops stale
# vanguard_tmp_/vanguard_app_/vanguard_script_ files (7 days) and scratch
# logs (1 day), and target\ cargo-cleans itself past $gcLimitGb (4 GB) - at
# most once a day, only on build-touching commands, never during `check`.
#
# `check` is the iteration loop (422 ms with nothing to do, 1.9 s after a source
# edit - measured). `all` is the pre-commit gate. `publish` collapses the whole
# "verify, stage, commit" sequence into one shell call; the gate runs first so
# nothing unbuildable gets committed. `dist` makes the ONE-FILE release: static
# CRT (no VC++ redist), staged exe + README, `--core` verified, then zipped.
#
# Exit code is truthful: 0 = clean, 1 = findings, 2 = missing binary.
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet('check', 'build', 'smoke', 'run', 'core', 'kill', 'all', 'clean', 'dist', 'publish')]
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

# ---- automatic cleanup -------------------------------------------------
# Runs on EVERY invocation, so nothing has to be remembered. Two tiers:
#
#   1. artifacts: glob only, ~10 ms, no measurement, age-guarded (7 days for
#      generated files, 1 day for scratch logs) so anything freshly made or
#      still wanted by hand survives.
#   2. target\: only on commands that rebuild anyway, at most once per
#      $gcHours, and only when over $gcLimitGb. Measuring costs ~370 ms over
#      ~5k files, which would double the `check` loop (0.42 s) - so `check`
#      never runs it at all.
#
# When tier 2 fires, the NEXT build pays a full rebuild (~60 s): that is the
# price of never running `vg clean` by hand. Overrides: VANGUARD_GC_HOURS,
# VANGUARD_GC_LIMIT_GB.
#
# The globs are narrow on purpose. vanguard_gen_*.py is the user's app
# registry (/apps, /run) and vanguard_notes/settings/dialogues are user data
# - a loose vanguard_* would eat all three.
$gcArtifacts = @('vanguard_tmp_*', 'vanguard_app_*', 'vanguard_script_*')
$gcHours = 24
$gcLimitGb = 4.0
if ($env:VANGUARD_GC_HOURS) { $gcHours = [double]$env:VANGUARD_GC_HOURS }
if ($env:VANGUARD_GC_LIMIT_GB) { $gcLimitGb = [double]$env:VANGUARD_GC_LIMIT_GB }
$gcMarker = Join-Path $tmp 'vg_gc.marker'

function Invoke-GcArtifacts {
    param([switch]$force)
    $n = 0
    $freed = 0L
    $artCut = (Get-Date).AddDays(-7)
    foreach ($pat in $gcArtifacts) {
        foreach ($f in @(Get-ChildItem -LiteralPath $repo -File -Filter $pat -ErrorAction SilentlyContinue)) {
            if (-not $force -and $f.LastWriteTime -ge $artCut) { continue }
            $n++
            $freed += $f.Length
            Remove-Item -LiteralPath $f.FullName -Force -ErrorAction SilentlyContinue
        }
    }
    # src\*.rs.bak left by the old optimize_project.py. Never a recursive walk
    # of the repo: that would descend into target\ and cost hundreds of ms.
    foreach ($d in 'src', 'tools') {
        $p = Join-Path $repo $d
        if (-not (Test-Path -LiteralPath $p)) { continue }
        foreach ($f in @(Get-ChildItem -LiteralPath $p -Recurse -File -Include '*.rs.bak' -ErrorAction SilentlyContinue)) {
            if (-not $force -and $f.LastWriteTime -ge $artCut) { continue }
            $n++
            $freed += $f.Length
            Remove-Item -LiteralPath $f.FullName -Force -ErrorAction SilentlyContinue
        }
    }
    # own cargo/clippy scratch logs, regenerated on the next run anyway
    $logCut = (Get-Date).AddDays(-1)
    foreach ($f in @(Get-ChildItem -LiteralPath $tmp -File -Filter '*.txt' -ErrorAction SilentlyContinue)) {
        if (-not $force -and $f.LastWriteTime -ge $logCut) { continue }
        $n++
        $freed += $f.Length
        Remove-Item -LiteralPath $f.FullName -Force -ErrorAction SilentlyContinue
    }
    if ($n -gt 0) {
        Write-Host ('GC    removed ' + $n + ' stale files (' + [math]::Round($freed / 1MB, 2) + ' MB)')
    }
}

function Invoke-GcTarget {
    param([switch]$force)
    # Cargo clean needs every target\*.exe unlocked, otherwise the tree is
    # left half-deleted. Skip now, retry on the next invocation.
    if (@(Get-Process vanguard_os, vanguard_core -ErrorAction SilentlyContinue).Count -gt 0) {
        if (-not $force) { return }
        Stop-Vanguard
        Start-Sleep -Milliseconds 300
    }
    if (-not $force) {
        if (Test-Path -LiteralPath $gcMarker) {
            $age = (Get-Date).ToUniversalTime() - (Get-Item -LiteralPath $gcMarker).LastWriteTimeUtc
            if ($age.TotalHours -lt $gcHours) { return }
        }
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $size = (Get-ChildItem -LiteralPath (Join-Path $repo 'target') -Recurse -File -ErrorAction SilentlyContinue |
            Measure-Object Length -Sum).Sum
        # The marker records the CHECK, not the clean: otherwise this walk
        # would be paid on every build until target\ crossed the line.
        Set-Content -LiteralPath $gcMarker -Value (Get-Date).ToString('o')
        if ($size -lt ($gcLimitGb * 1GB)) {
            Write-Host ('GC    target\ ' + [math]::Round($size / 1GB, 2) + ' GB kept (< ' + $gcLimitGb + ' GB, ' + [int]$sw.ElapsedMilliseconds + ' ms)')
            return
        }
        Write-Host ('GC    target\ ' + [math]::Round($size / 1GB, 2) + ' GB >= ' + $gcLimitGb + ' GB, cleaning')
    }
    $out = Join-Path $tmp 'dev_out.txt'
    $err = Join-Path $tmp 'dev_err.txt'
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & cargo clean > $out 2> $err
    $ErrorActionPreference = $prevEap
    if ($LASTEXITCODE -ne 0) {
        Get-Content $out, $err -ErrorAction SilentlyContinue | ForEach-Object { Write-Host $_ }
        Write-Host 'GC    FAIL cargo clean (target\ left as is)'
        return
    }
    Set-Content -LiteralPath $gcMarker -Value (Get-Date).ToString('o')
    Write-Host 'GC    cargo clean - next build is a full rebuild (~60 s)'
}

$ok = $true

# Tier 1 on every command; tier 2 only where cargo rebuilds anyway. Both run
# ahead of the switch so `clean` below just forces them.
Invoke-GcArtifacts
if (@('build', 'smoke', 'run', 'core', 'all', 'publish') -contains $cmd) {
    if (-not $keep) { Stop-Vanguard }
    Invoke-GcTarget
}

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
    'clean' {
        Stop-Vanguard
        # Same two functions as the automatic path, only forced: no age limit,
        # no threshold, no throttle.
        Invoke-GcTarget -force
        Invoke-GcArtifacts -force
        $ok = $true
    }
    'dist' {
        if (-not $keep) { Stop-Vanguard }
        # Tier 2 (which runs ahead of the switch for the other build commands
        # only) is called here explicitly: a GC firing inside dist would wipe
        # target\ right before the one build where a full rebuild hurts most.
        Invoke-GcTarget
        # Static CRT, release-only. The deliverable has to run on a machine with
        # no VC++ Redistributable: a dynamic build imports vcruntime140.dll
        # (measured), while api-ms-win-crt-* is inbox on Windows 10+. Nothing
        # else in this script builds release, so this flag never thrashes the
        # release cache back and forth.
        $env:RUSTFLAGS = '-C target-feature=+crt-static'
        $ok = Invoke-Cargo @('build', '--release')
        Remove-Item env:RUSTFLAGS -ErrorAction SilentlyContinue

        $dist = Join-Path $repo 'dist'
        $zip = Join-Path $dist 'vanguard_os.zip'
        if ($ok) {
            if (Test-Path -LiteralPath $dist) { Remove-Item -LiteralPath $dist -Recurse -Force }
            New-Item -ItemType Directory -Path $dist | Out-Null
            Copy-Item (Join-Path $repo 'target\release\vanguard_os.exe') -Destination $dist
            Copy-Item (Join-Path $PSScriptRoot 'dist_readme.txt') -Destination (Join-Path $dist 'README.txt')
            Write-Host 'OK    staged dist\vanguard_os.exe + README.txt'
        }

        # The entire point of one file: `--core` must actually serve. Ping the
        # STAGED exe (not target\) on 127.0.0.1:7331, then stop exactly the PID
        # we started - never Stop-Vanguard here, that would take a GUI the user
        # may have open with -keep.
        if ($ok) {
            $exe = Join-Path $dist 'vanguard_os.exe'
            $p = Start-Process -FilePath $exe -ArgumentList '--core' `
                -WorkingDirectory $dist -PassThru -WindowStyle Hidden
            $pong = $false
            for ($i = 0; $i -lt 30 -and -not $pong; $i++) {
                Start-Sleep -Milliseconds 100
                try {
                    $c = New-Object Net.Sockets.TcpClient
                    $c.Connect('127.0.0.1', 7331)
                    $st = $c.GetStream()
                    $st.ReadTimeout = 1500
                    $b = [Text.Encoding]::UTF8.GetBytes('{"cmd":"ping"}' + "`n")
                    $st.Write($b, 0, $b.Length)
                    $r = New-Object byte[] 4096
                    $n = $st.Read($r, 0, $r.Length)
                    $c.Close()
                    $txt = [Text.Encoding]::UTF8.GetString($r, 0, $n).Trim()
                    if ($txt) {
                        # Same shape fs_smoke.ps1 asserts on: Response::Pong is
                        # serde-tagged, so the line is {"kind":"pong"}.
                        $o = $txt | ConvertFrom-Json
                        if ($o.kind -eq 'pong') { $pong = $true }
                    }
                } catch { }
            }
            # Verdict BEFORE the stop: killing first flips HasExited, so every
            # successful run would read as a crash - that is exactly how the
            # first dist run "failed" while the exe served pings fine.
            $alive = ($p -and -not $p.HasExited)
            if (-not $alive) {
                Write-Host 'FAIL  staged exe exited on --core (port 7331 already taken?)'
                $ok = $false
            } elseif (-not $pong) {
                Write-Host 'FAIL  staged exe did not answer ping on 127.0.0.1:7331'
                $ok = $false
            } else {
                Write-Host 'OK    staged exe serves as core (--core answered pong)'
            }
            if ($alive) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
        }

        if ($ok) {
            Compress-Archive -Path (Join-Path $dist 'vanguard_os.exe'), (Join-Path $dist 'README.txt') `
                -DestinationPath $zip -Force
            Write-Host ('OK    dist\vanguard_os.zip  ' + [math]::Round((Get-Item $zip).Length / 1MB, 2) + ' MB')
        }
        if (-not $ok) { Write-Host 'FAIL  dist' }
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
