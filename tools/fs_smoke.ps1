# fs_smoke.ps1 - ASCII-only on purpose: PS 5.1 chokes on Cyrillic in .ps1 files
# in this environment, and the repo path contains Cyrillic. Lives in <repo>\tools.
#
# Drives the vanguard_core Fs* protocol over raw TCP: no GUI, no shell.
# Verifies the "all drives" rewrite: virtual root, absolute paths, mtime,
# the .. / relative-path rejections and the drive-root delete/rename guards.
param(
    # Default = the repo root: this script lives in <repo>\tools.
    [string]$repo = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = 'Stop'
$fail = 0
$pass = 0

function Check([string]$name, [bool]$cond, [string]$detail) {
    if ($cond) {
        $script:pass++
        Write-Output ("PASS  " + $name)
    } else {
        $script:fail++
        Write-Output ("FAIL  " + $name + "  -> " + $detail)
    }
}

# ---- start a fresh core (short paths: repo dir contains Cyrillic) ----------
$coreExe = Join-Path $repo 'target\debug\vanguard_core.exe'
if (-not (Test-Path -LiteralPath $coreExe)) {
    Write-Output "MISSING core binary: $coreExe"
    exit 2
}
$fso = New-Object -ComObject Scripting.FileSystemObject
$shortExe = $fso.GetFile($coreExe).ShortPath
$shortRepo = $fso.GetFolder($repo).ShortPath
$proc = Start-Process -FilePath $shortExe -WorkingDirectory $shortRepo -WindowStyle Hidden -PassThru

$ready = $false
for ($i = 0; $i -lt 50; $i++) {
    Start-Sleep -Milliseconds 100
    try {
        $t = New-Object Net.Sockets.TcpClient
        $t.Connect('127.0.0.1', 7331)
        $t.Close()
        $ready = $true
        break
    } catch { }
}
if (-not $ready) {
    Write-Output "core did not open port 7331"
    exit 3
}

$enc = New-Object System.Text.UTF8Encoding($false)
$tcp = New-Object Net.Sockets.TcpClient
$tcp.Connect('127.0.0.1', 7331)
$stream = $tcp.GetStream()
$reader = New-Object System.IO.StreamReader($stream, $enc)

function Invoke-Req($obj) {
    $json = $obj | ConvertTo-Json -Compress
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($json + "`n")
    $script:stream.Write($bytes, 0, $bytes.Length)
    $script:stream.Flush()
    while ($true) {
        $line = $script:reader.ReadLine()
        if ($null -eq $line) { return $null }
        $line = $line.Trim()
        if ($line -eq '') { continue }
        $r = $line | ConvertFrom-Json
        if ($r.kind -eq 'event') { continue }
        return $r
    }
}

# ---- 1. heartbeat ----------------------------------------------------------
$pong = Invoke-Req @{ cmd = 'ping' }
Check 'ping' ($pong -and $pong.kind -eq 'pong') "got: $($pong | ConvertTo-Json -Compress)"

# ---- 2. virtual root = the drives -----------------------------------------
$root = Invoke-Req @{ cmd = 'fs_list'; path = '' }
Check 'virtual root answers' ($root -and $root.kind -eq 'ok') "got: $($root | ConvertTo-Json -Compress)"
$rootObj = $null
if ($root -and $root.kind -eq 'ok') { $rootObj = $root.text | ConvertFrom-Json }
Check 'virtual root path is empty' ($rootObj -and $rootObj.path -eq '') "path='$($rootObj.path)'"
# NB: every `.Count` below goes through @(). PS 5.1 reports $null for the
# Count of a *single* object (and 0 for none), so `(...).Count -eq 1` on a
# one-element match silently evaluates to $null -eq 1 = false.
Check 'virtual root lists C drive' ($rootObj -and @($rootObj.entries | Where-Object { $_.name -eq 'C:\' }).Count -eq 1) `
    "entries: $(($rootObj.entries | ForEach-Object { $_.name }) -join ',')"
Check 'virtual root entry is a dir' ($rootObj -and @($rootObj.entries).Count -gt 0 -and @($rootObj.entries)[0].is_dir) 'not a dir'
Check 'virtual root has no plain names' ($rootObj -and @($rootObj.entries | Where-Object { $_.name -notmatch '^[A-Z]:\\$' }).Count -eq 0) `
    "names: $(($rootObj.entries | ForEach-Object { $_.name }) -join ',')"

# ---- 3. a real directory: path echo + mtime --------------------------------
$win = Invoke-Req @{ cmd = 'fs_list'; path = 'C:\' }
$winObj = $null
if ($win -and $win.kind -eq 'ok') { $winObj = $win.text | ConvertFrom-Json }
Check 'C:\ lists' ($win.kind -eq 'ok') "got: $($win | ConvertTo-Json -Compress)"
Check 'C:\ echoes pretty absolute path' ($winObj -and $winObj.path -eq 'C:\') "path='$($winObj.path)'"
Check 'C:\ has entries' ($winObj -and @($winObj.entries).Count -gt 0) 'empty'
Check 'C:\ entries carry mtime' ($winObj -and @($winObj.entries | Where-Object { $_.PSObject.Properties.Name -notcontains 'mtime' }).Count -eq 0) 'no mtime field'
Check 'C:\ dirs come first' ($winObj -and @($winObj.entries)[0].is_dir) 'first is not a dir'
Check 'C:\ mtime populated' ($winObj -and @($winObj.entries | Where-Object { $_.mtime -gt 0 }).Count -gt 0) 'all mtimes are 0'

# ---- 4. rejections ---------------------------------------------------------
$r = Invoke-Req @{ cmd = 'fs_list'; path = '..\evil' }
Check 'reject ..' ($r -and $r.kind -eq 'err') "got: $($r | ConvertTo-Json -Compress)"
$r = Invoke-Req @{ cmd = 'fs_list'; path = 'C:\Windows\..\..' }
Check 'reject .. after canonical part' ($r -and $r.kind -eq 'err') "got: $($r | ConvertTo-Json -Compress)"
$r = Invoke-Req @{ cmd = 'fs_list'; path = 'relative\path' }
Check 'reject relative path' ($r -and $r.kind -eq 'err') "got: $($r | ConvertTo-Json -Compress)"
$r = Invoke-Req @{ cmd = 'fs_read'; path = 'src\app.rs' }
Check 'reject relative read' ($r -and $r.kind -eq 'err') "got: $($r | ConvertTo-Json -Compress)"
$r = Invoke-Req @{ cmd = 'fs_delete'; path = '' }
Check 'reject blank path for mutation' ($r -and $r.kind -eq 'err') "got: $($r | ConvertTo-Json -Compress)"

# ---- 5. drive-root guards --------------------------------------------------
$r = Invoke-Req @{ cmd = 'fs_delete'; path = 'C:\' }
Check 'refuse deleting drive root' ($r -and $r.kind -eq 'err') "got: $($r | ConvertTo-Json -Compress)"
$r = Invoke-Req @{ cmd = 'fs_rename'; from = 'C:\'; to = 'C:\zzz_probe' }
Check 'refuse renaming drive root' ($r -and $r.kind -eq 'err') "got: $($r | ConvertTo-Json -Compress)"
Check 'C:\ still exists after delete probe' (Test-Path -LiteralPath 'C:\') 'gone!'
Check 'C:\zzz_probe was not created' (-not (Test-Path -LiteralPath 'C:\zzz_probe')) 'created!'

# ---- 6. full CRUD round trip in a scratch dir ------------------------------
# NB: in this shell $env:TEMP is an 8.3 short name (C:\Users\73B5~1\...), while
# the core echoes back the canonical long path. Every path assertion below
# compares against what the core echoes, so take the canonical form FROM the
# core instead of from the environment — otherwise the comparison fails on
# cosmetic differences and says nothing about the code.
$scratchShort = Join-Path $env:TEMP 'opencode_fs_smoke'
if (Test-Path -LiteralPath $scratchShort) { Remove-Item -LiteralPath $scratchShort -Recurse -Force }
$mk = Invoke-Req @{ cmd = 'fs_mkdir'; path = $scratchShort }
Check 'fs_mkdir creates' ($mk -and $mk.kind -eq 'ok') "got: $($mk | ConvertTo-Json -Compress)"
$scratch = $scratchShort
if ($mk -and $mk.kind -eq 'ok') { $scratch = $mk.text | ConvertFrom-Json }
Check 'fs_mkdir echoes an absolute path' ([IO.Path]::IsPathRooted($scratch)) "path='$scratch'"
Check 'scratch path is the expanded long form' ($scratch -and $scratch -notmatch '~') "path='$scratch'"
$mk2 = Invoke-Req @{ cmd = 'fs_mkdir'; path = $scratch }
Check 'fs_mkdir again is idempotent' ($mk2 -and $mk2.kind -eq 'ok') 'mkdir failed twice'

$f1 = Join-Path $scratch 'a.txt'
Check 'fs_create makes a file' ((Invoke-Req @{ cmd = 'fs_create'; path = $f1 }).kind -eq 'ok') 'create failed'
Check 'fs_create refuses occupied name' ((Invoke-Req @{ cmd = 'fs_create'; path = $f1 }).kind -eq 'err') 'overwrote!'
Check 'occupant kept its content' ((Get-Content -LiteralPath $f1 -Raw) -eq $null -or (Get-Content -LiteralPath $f1 -Raw) -eq '') 'has content?'

Check 'fs_write fills it' ((Invoke-Req @{ cmd = 'fs_write'; path = $f1; content = 'hello vanguard' }).kind -eq 'ok') 'write failed'
$read = Invoke-Req @{ cmd = 'fs_read'; path = $f1 }
$readObj = $null
if ($read -and $read.kind -eq 'ok') { $readObj = $read.text | ConvertFrom-Json }
Check 'fs_read round-trips content' ($readObj -and $readObj.content -eq 'hello vanguard') "got '$($readObj.content)'"
Check 'fs_read echoes pretty path' ($readObj -and $readObj.path -eq $f1) "path='$($readObj.path)'"

$f2 = Join-Path $scratch 'b.txt'
Check 'fs_rename moves it' ((Invoke-Req @{ cmd = 'fs_rename'; from = $f1; to = $f2 }).kind -eq 'ok') 'rename failed'
Check 'source is gone' (-not (Test-Path -LiteralPath $f1)) 'still there'
$f3 = Join-Path $scratch 'c.txt'
# Occupied target needs the victim to exist first — create it, then try to
# move b.txt on top of c.txt. std::fs::rename would replace it silently.
Check 'fs_create makes the second file' ((Invoke-Req @{ cmd = 'fs_create'; path = $f3 }).kind -eq 'ok') 'create failed'
Check 'fs_rename refuses occupied target' ((Invoke-Req @{ cmd = 'fs_rename'; from = $f2; to = $f3 }).kind -eq 'err') 'replaced target'
Check 'victim still readable' ((Invoke-Req @{ cmd = 'fs_read'; path = $f3 }).kind -eq 'ok') 'victim damaged'
Check 'src==dst is a no-op, not an error' ((Invoke-Req @{ cmd = 'fs_rename'; from = $f2; to = $f2 }).kind -eq 'ok') 'rejected same name'

$listing = Invoke-Req @{ cmd = 'fs_list'; path = $scratch }
$lo = $null
if ($listing -and $listing.kind -eq 'ok') { $lo = $listing.text | ConvertFrom-Json }
Check 'scratch lists exactly two entries' ($lo -and @($lo.entries).Count -eq 2) "count=$(@($lo.entries).Count)"
$moved = $null
if ($lo) { $moved = $lo.entries | Where-Object { $_.name -eq 'b.txt' } }
Check 'renamed file is listed' ($null -ne $moved) "names=$($lo.entries.name -join ',')"
Check 'file size is carried' ($moved -and $moved.size -eq 14) "size=$($moved.size)"
Check 'file mtime is carried' ($moved -and $moved.mtime -gt 0) "mtime=$($moved.mtime)"

$r = Invoke-Req @{ cmd = 'fs_read'; path = $scratch }
Check 'refuse reading a directory' ($r -and $r.kind -eq 'err') "got: $($r | ConvertTo-Json -Compress)"
$r = Invoke-Req @{ cmd = 'fs_list'; path = $f2 }
Check 'refuse listing a file' ($r -and $r.kind -eq 'err') "got: $($r | ConvertTo-Json -Compress)"
$r = Invoke-Req @{ cmd = 'fs_read'; path = (Join-Path $scratch 'nope.txt') }
Check 'refuse missing file' ($r -and $r.kind -eq 'err') "got: $($r | ConvertTo-Json -Compress)"

Check 'fs_delete removes the dir' ((Invoke-Req @{ cmd = 'fs_delete'; path = $scratch }).kind -eq 'ok') 'delete failed'
Check 'scratch is really gone' (-not (Test-Path -LiteralPath $scratch)) 'still exists'

# ---- teardown --------------------------------------------------------------
$tcp.Close()
Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue

Write-Output ("----  PASS=" + $pass + "  FAIL=" + $fail)
if ($fail -gt 0) { exit 1 }
exit 0
