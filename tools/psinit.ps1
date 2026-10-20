# psinit.ps1 - the encoding/location prelude the agent dot-sources at the top of
# every shell call. The harness runs `powershell -NoProfile`, so $PROFILE is dead
# weight there and this file has to be loaded by hand:
#
#   . "$env:TEMP\opencode\psinit.ps1"     <- one-line stub, see below
#
# ASCII-only on purpose. Windows PowerShell 5.1 reads a .ps1 WITHOUT a BOM using
# the ANSI codepage (cp1251 here), so any Cyrillic in the source turns into a
# ParserError - "unrecognized token". The repo path is therefore assembled at
# runtime instead of being written out, which also keeps it correct if the
# machine's user name changes.

# --- Cyrillic ----------------------------------------------------------------
# Two separate defaults are wrong here, and they fail in different directions:
#   [Console]::OutputEncoding = cp866   -> PowerShell writes Cyrillic as cp866
#                                          bytes, the harness decodes them as
#                                          UTF-8, and the text comes out mojibake.
#   $OutputEncoding           = us-ascii -> text piped INTO a native command
#                                          (git, python, ...) has every
#                                          non-ASCII character replaced by '?'.
# Arguments still travel through cp1251, so Russian text for `git commit` still
# belongs in a file passed with -F rather than in -m.
try { [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false } catch {}
try { [Console]::InputEncoding  = New-Object System.Text.UTF8Encoding $false } catch {}
try { $OutputEncoding           = New-Object System.Text.UTF8Encoding $false } catch {}

# --- %TEMP% ------------------------------------------------------------------
# Arrives as an 8.3 short name (C:\Users\73B5~1\...) while vanguard_core
# canonicalizes paths, so any comparison against $env:TEMP fails on cosmetics
# alone. Same directory, long form, for the rest of the call - which is the point.
try {
    $userProfile = [Environment]::GetFolderPath('UserProfile')
    if ($userProfile) { $env:TEMP = Join-Path $userProfile 'AppData\Local\Temp' }
} catch {}

# --- location and helpers ----------------------------------------------------
$VG = Join-Path ([Environment]::GetFolderPath('UserProfile')) 'RustroverProjects\vanguard_os'
Set-Location $VG

# vg check|build|smoke|run|core|kill|all|publish  -- see tools\dev.ps1
# Runs under pwsh 7; the scripts themselves stay Windows PowerShell 5.1-clean.
# -m and the trailing paths are forwarded so `publish` needs no second entrypoint.
function vg {
    param(
        [Parameter(Position = 0)][string]$step = 'check',
        [string]$m = '',
        [switch]$keep,
        [Parameter(ValueFromRemainingArguments = $true)][string[]]$rest
    )
    $call = @('-NoProfile', '-File', (Join-Path $VG 'tools\dev.ps1'), $step)
    if ($m) { $call += @('-m', $m) }
    if ($keep) { $call += '-keep' }
    if ($rest) { $call += $rest }
    & pwsh @call
}

function vgkill {
    Get-Process vanguard_os, vanguard_core -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
}

# git in one word instead of three.
function gst { & git status --short }
function glg { param([int]$n = 12) & git log --oneline "-$n" }

# Run a snippet under PowerShell 7.
#
# Handing a script to pwsh.exe as `-Command '<text>'` loses every embedded
# double quote: PS 5.1 does not escape them when building the native command
# line, so pwsh receives `Write-Output (ver = )` and dies on "ver is not
# recognized". -EncodedCommand takes base64 of UTF-16LE instead, which has no
# quoting layer at all - no quote stripping, and Cyrillic survives byte-exact.
#
#   q '. "$env:TEMP\opencode\psinit.ps1"; vg build'
#
# Single-quoted at the call site, so the double quotes inside are literal text
# for 5.1 and real quotes for pwsh.
function q {
    param([Parameter(Position = 0, ValueFromPipeline = $true)][string]$code)
    process {
        $b64 = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($code))
        & pwsh -NoProfile -EncodedCommand $b64
    }
}
