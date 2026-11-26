# End-to-end checks against a packaged build (unbox.exe + 7z.dll), driving the same
# plan -> worker -> verify -> commit pipeline as the window through `unbox extract`.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$PackageDir)

$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path (Join-Path $PackageDir 'unbox.exe')).Path
$work = Join-Path ([IO.Path]::GetTempPath()) ("unbox-smoke-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
$failures = [Collections.Generic.List[string]]::new()

function Invoke-Unbox([string[]]$Arguments) {
    $psi = [Diagnostics.ProcessStartInfo]::new($exe)
    foreach ($a in $Arguments) { $psi.ArgumentList.Add($a) }
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $psi.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    $p = [Diagnostics.Process]::Start($psi)
    $err = $p.StandardError.ReadToEndAsync()
    $out = $p.StandardOutput.ReadToEnd()
    if (-not $p.WaitForExit(120000)) { $p.Kill(); throw "unbox $Arguments timed out" }
    [pscustomobject]@{ Code = $p.ExitCode; Out = $out.Trim(); Err = $err.Result.Trim() }
}

function Check([string]$Name, [scriptblock]$Body) {
    try {
        & $Body
        Write-Host "[PASS] $Name" -ForegroundColor Green
    } catch {
        Write-Host "[FAIL] $Name - $($_.Exception.Message)" -ForegroundColor Red
        $failures.Add($Name)
    }
}

function Assert([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }

function Assert-NoLeftovers([string]$Dir) {
    $left = @(Get-ChildItem -LiteralPath $Dir -Force -Filter '.unzip_staging_*') + @(Get-ChildItem -LiteralPath $Dir -Force -Filter '.unbox_manifest_*')
    Assert ($left.Count -eq 0) "staging or manifest left behind: $($left.Name -join ', ')"
}

# Sample tree with nested and non-ASCII names.
$src = Join-Path $work 'src'
New-Item -ItemType Directory -Path (Join-Path $src '中文目录\深层') -Force | Out-Null
Set-Content -LiteralPath (Join-Path $src '中文目录\说明.txt') -Value 'hello unbox' -Encoding utf8
[IO.File]::WriteAllBytes((Join-Path $src '中文目录\深层\data.bin'), [byte[]](0..255 * 64))
Set-Content -LiteralPath (Join-Path $src 'root.txt') -Value 'root' -Encoding utf8

Check 'version' {
    $r = Invoke-Unbox @('--version')
    Assert ($r.Code -eq 0 -and $r.Out -match '^unbox \d+\.\d+\.\d+') "unexpected: $($r.Out) $($r.Err)"
}

$zipDir = Join-Path $work 'zip'
New-Item -ItemType Directory -Path $zipDir | Out-Null
$zip = Join-Path $zipDir 'sample.zip'
Compress-Archive -Path (Join-Path $src '*') -DestinationPath $zip

Check 'zip with unicode names' {
    $r = Invoke-Unbox @('extract', $zip)
    Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
    $out = Join-Path $zipDir 'sample'
    Assert ($r.Out -eq $out) "target was '$($r.Out)'"
    Assert ((Get-Content -LiteralPath (Join-Path $out '中文目录\说明.txt') -Raw).Trim() -eq 'hello unbox') 'content mismatch'
    $a = (Get-FileHash (Join-Path $src '中文目录\深层\data.bin')).Hash
    $b = (Get-FileHash (Join-Path $out '中文目录\深层\data.bin')).Hash
    Assert ($a -eq $b) 'binary content mismatch'
    Assert-NoLeftovers $zipDir
}

Check 'existing target gets a (2) suffix' {
    $r = Invoke-Unbox @('extract', $zip)
    Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
    Assert ($r.Out -eq (Join-Path $zipDir 'sample (2)')) "target was '$($r.Out)'"
}

Check 'single same-name root collapses' {
    $dir = Join-Path $work 'collapse'
    New-Item -ItemType Directory -Path (Join-Path $dir 'proj') -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $dir 'proj\x.txt') -Value 'x'
    $archive = Join-Path $dir 'proj.zip'
    Compress-Archive -Path (Join-Path $dir 'proj') -DestinationPath $archive
    Remove-Item -LiteralPath (Join-Path $dir 'proj') -Recurse
    $r = Invoke-Unbox @('extract', $archive)
    Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
    Assert (Test-Path -LiteralPath (Join-Path $dir 'proj\x.txt')) 'expected proj\x.txt'
    Assert (-not (Test-Path -LiteralPath (Join-Path $dir 'proj\proj'))) 'root was not collapsed'
}

Check 'tar.gz expands the inner tar' {
    $dir = Join-Path $work 'tgz'
    New-Item -ItemType Directory -Path $dir | Out-Null
    $archive = Join-Path $dir 'bundle.tar.gz'
    tar -czf $archive -C $src root.txt
    if ($LASTEXITCODE -ne 0) { throw 'tar failed' }
    $r = Invoke-Unbox @('extract', $archive)
    Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
    Assert ((Get-Content -LiteralPath (Join-Path $dir 'bundle\root.txt') -Raw).Trim() -eq 'root') 'missing root.txt'
    Assert (-not (Test-Path -LiteralPath (Join-Path $dir 'bundle\bundle.tar'))) 'inner tar was not expanded'
    Assert-NoLeftovers $dir
}

Check 'plain gzip stream keeps one layer' {
    $dir = Join-Path $work 'gz'
    New-Item -ItemType Directory -Path $dir | Out-Null
    $plain = Join-Path $dir 'notes.txt'
    Set-Content -LiteralPath $plain -Value 'stream' -Encoding ascii
    $gz = "$plain.gz"
    $in = [IO.File]::OpenRead($plain); $outStream = [IO.File]::Create($gz)
    $g = [IO.Compression.GZipStream]::new($outStream, [IO.Compression.CompressionLevel]::Optimal)
    $in.CopyTo($g); $g.Dispose(); $in.Dispose()
    Remove-Item -LiteralPath $plain
    $r = Invoke-Unbox @('extract', $gz)
    Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
    Assert (Test-Path -LiteralPath (Join-Path $dir 'notes.txt\notes.txt')) "unexpected layout: $(Get-ChildItem -Recurse $dir | Out-String)"
}

Check 'damaged archive is rejected without leftovers' {
    $dir = Join-Path $work 'bad'
    New-Item -ItemType Directory -Path $dir | Out-Null
    $bytes = [IO.File]::ReadAllBytes($zip)
    $broken = Join-Path $dir 'broken.zip'
    [IO.File]::WriteAllBytes($broken, $bytes[0..([int]($bytes.Length * 0.6))])
    $r = Invoke-Unbox @('extract', $broken)
    Assert ($r.Code -eq 1) "exit $($r.Code): $($r.Out)"
    Assert (-not (Test-Path -LiteralPath (Join-Path $dir 'broken'))) 'partial output delivered'
    Assert-NoLeftovers $dir
}

Check 'extract here spreads the content into the folder' {
    $dir = Join-Path $work 'here'
    New-Item -ItemType Directory -Path $dir | Out-Null
    $archive = Join-Path $dir 'loose.zip'
    Compress-Archive -Path (Join-Path $src '*') -DestinationPath $archive
    Set-Content -LiteralPath (Join-Path $dir 'root.txt') -Value 'already here'
    $r = Invoke-Unbox @('extract', $archive, '--here')
    Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
    Assert ($r.Out -eq $dir) "target was '$($r.Out)'"
    Assert (Test-Path -LiteralPath (Join-Path $dir '中文目录\深层\data.bin')) "unexpected layout: $(Get-ChildItem -Recurse -Name $dir | Out-String)"
    Assert ((Get-Content -LiteralPath (Join-Path $dir 'root.txt') -Raw).Trim() -eq 'already here') 'existing file was overwritten'
    Assert (Test-Path -LiteralPath (Join-Path $dir 'root (2).txt')) 'clashing file was not renamed'
    Assert (-not (Test-Path -LiteralPath (Join-Path $dir 'loose'))) 'target folder left behind'
    Assert-NoLeftovers $dir
}

Check 'extract into another folder' {
    $dir = Join-Path $work 'to'
    $dest = Join-Path $work 'to-dest'
    New-Item -ItemType Directory -Path $dir, $dest | Out-Null
    $archive = Join-Path $dir 'moved.zip'
    Copy-Item -LiteralPath $zip -Destination $archive
    $r = Invoke-Unbox @('extract', $archive, '--to', $dest)
    Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
    Assert ($r.Out -eq (Join-Path $dest 'moved')) "target was '$($r.Out)'"
    Assert (Test-Path -LiteralPath (Join-Path $dest 'moved\root.txt')) 'missing root.txt'
    Assert (Test-Path -LiteralPath $archive) 'source archive was removed'
    Assert-NoLeftovers $dir
    Assert-NoLeftovers $dest
}

# Encrypted archives need the 7-Zip command line (preinstalled on GitHub's Windows images).
$sevenZip = Join-Path $env:ProgramFiles '7-Zip\7z.exe'
function New-Encrypted([string]$Archive, [string]$Password, [string[]]$Switches) {
    & $sevenZip a -bso0 -bsp0 "-p$Password" @Switches $Archive (Join-Path $src '*') | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "7z a failed for $Archive" }
}

if (Test-Path -LiteralPath $sevenZip) {
    Check 'password guessed from a folder word (encrypted headers)' {
        $dir = Join-Path $work 'pw1\资料 s3cret 合集'
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        $archive = Join-Path $dir 'data.7z'
        New-Encrypted $archive 's3cret' @('-mhe=on')
        $r = Invoke-Unbox @('extract', $archive)
        Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
        Assert ((Get-Content -LiteralPath (Join-Path $dir 'data\root.txt') -Raw).Trim() -eq 'root') 'missing root.txt'
        Assert-NoLeftovers $dir
    }

    Check 'password guessed from the archive name (zip AES)' {
        $dir = Join-Path $work 'pw2'
        New-Item -ItemType Directory -Path $dir | Out-Null
        $archive = Join-Path $dir 'hunter2.zip'
        New-Encrypted $archive 'hunter2' @('-mem=AES256')
        $r = Invoke-Unbox @('extract', $archive)
        Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
        Assert (Test-Path -LiteralPath (Join-Path $dir 'hunter2\中文目录\深层\data.bin')) 'missing data.bin'
    }

    Check 'password guessed for split 7z volumes' {
        $dir = Join-Path $work 'pw3\vault'
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        $archive = Join-Path $dir 'parts.7z'
        New-Encrypted $archive 'vault' @('-v8k', '-mx=0')
        $r = Invoke-Unbox @('extract', "$archive.001")
        Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
        Assert (Test-Path -LiteralPath (Join-Path $dir 'parts\root.txt')) "unexpected layout: $(Get-ChildItem -Recurse $dir | Out-String)"
        Assert-NoLeftovers $dir
    }

    Check 'explicit password and wrong password' {
        $dir = Join-Path $work 'pw4'
        New-Item -ItemType Directory -Path $dir | Out-Null
        $archive = Join-Path $dir 'locked.7z'
        New-Encrypted $archive 'Not-Guessable-9' @()
        $bad = Invoke-Unbox @('extract', $archive, '--password', 'nope')
        Assert ($bad.Code -ne 0) "wrong password succeeded: $($bad.Out)"
        Assert ($bad.Err -match '密码错误') "message: $($bad.Err)"
        Assert (-not (Test-Path -LiteralPath (Join-Path $dir 'locked'))) 'output delivered with a wrong password'
        Assert-NoLeftovers $dir
        $good = Invoke-Unbox @('extract', $archive, '--password', 'Not-Guessable-9')
        Assert ($good.Code -eq 0) "exit $($good.Code): $($good.Err)"
        Assert (Test-Path -LiteralPath (Join-Path $dir 'locked\root.txt')) 'missing root.txt'
    }

    Check 'password guessed from a folder three levels up' {
        $dir = Join-Path $work 'pw5\Top Key\a\b'
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        $archive = Join-Path $dir 'deep.7z'
        New-Encrypted $archive 'Key' @('-mhe=on')
        $r = Invoke-Unbox @('extract', $archive)
        Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
        Assert (Test-Path -LiteralPath (Join-Path $dir 'deep\root.txt')) 'missing root.txt'
    }

    Check 'disguised archive inside an archive is unpacked' {
        $dir = Join-Path $work 'nested'
        New-Item -ItemType Directory -Path $dir | Out-Null
        $inner = Join-Path $dir 'payload.7z'
        & $sevenZip a -bso0 -bsp0 $inner (Join-Path $src '*') | Out-Null
        Move-Item -LiteralPath $inner -Destination (Join-Path $dir 'movie.mp4')
        $archive = Join-Path $dir 'outer.zip'
        & $sevenZip a -bso0 -bsp0 $archive (Join-Path $dir 'movie.mp4') | Out-Null
        Remove-Item -LiteralPath (Join-Path $dir 'movie.mp4')
        $r = Invoke-Unbox @('extract', $archive)
        Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
        Assert (Test-Path -LiteralPath (Join-Path $dir 'outer\root.txt')) "unexpected layout: $(Get-ChildItem -Recurse -Name $dir | Out-String)"
        # Like the source archive, the unpacked inner archive is kept unless recycling.
        Assert (Test-Path -LiteralPath (Join-Path $dir 'outer\movie.mp4')) 'inner archive was not kept'
        Assert (Test-Path -LiteralPath $archive) 'source archive was removed'
        Assert-NoLeftovers $dir

        $r = Invoke-Unbox @('extract', $archive, '--recycle')
        Assert ($r.Code -eq 0) "recycle exit $($r.Code): $($r.Err)"
        $out = Join-Path $dir 'outer (2)'
        Assert (Test-Path -LiteralPath (Join-Path $out 'root.txt')) "unexpected layout: $(Get-ChildItem -Recurse -Name $dir | Out-String)"
        Assert (-not (Test-Path -LiteralPath (Join-Path $out 'movie.mp4'))) "inner archive was not recycled: $($r.Err)"
        Assert (-not (Test-Path -LiteralPath $archive)) "source archive was not recycled: $($r.Err)"
    }

    Check 'inner archive with an unknown password is kept as it is' {
        $dir = Join-Path $work 'inner-locked'
        New-Item -ItemType Directory -Path $dir | Out-Null
        $inner = Join-Path $dir 'vault.7z'
        New-Encrypted $inner 'Not-Guessable-7' @('-mhe=on')
        $archive = Join-Path $dir 'carrier.zip'
        & $sevenZip a -bso0 -bsp0 $archive $inner | Out-Null
        Remove-Item -LiteralPath $inner
        $r = Invoke-Unbox @('extract', $archive)
        Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
        Assert (Test-Path -LiteralPath (Join-Path $dir 'carrier\vault.7z')) "unexpected layout: $(Get-ChildItem -Recurse -Name $dir | Out-String)"
        Assert-NoLeftovers $dir
    }

    Check 'lone document is not unpacked' {
        $dir = Join-Path $work 'doc'
        New-Item -ItemType Directory -Path $dir | Out-Null
        $docx = Join-Path $dir 'report.docx'
        & $sevenZip a -bso0 -bsp0 -tzip $docx (Join-Path $src 'root.txt') | Out-Null
        $archive = Join-Path $dir 'doc.zip'
        & $sevenZip a -bso0 -bsp0 $archive $docx | Out-Null
        Remove-Item -LiteralPath $docx
        $r = Invoke-Unbox @('extract', $archive)
        Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
        Assert (Test-Path -LiteralPath (Join-Path $dir 'doc\report.docx')) "unexpected layout: $(Get-ChildItem -Recurse -Name $dir | Out-String)"
    }

    function New-SplitSet([string]$Dir, [string]$Archive, [string[]]$Switches) {
        New-Item -ItemType Directory -Path $Dir | Out-Null
        & $sevenZip a -bso0 -bsp0 -mx=0 -v4k @Switches $Archive (Join-Path $src '*') | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "7z a failed for $Archive" }
        $parts = @(Get-ChildItem -LiteralPath $Dir -File | Sort-Object Name)
        Assert ($parts.Count -ge 3) "only $($parts.Count) volumes"
        $parts
    }

    Check 'split 7z volumes: any volume starts the set, recycle takes every volume' {
        $dir = Join-Path $work 'split'
        $archive = Join-Path $dir 'movie.7z'
        $parts = New-SplitSet $dir $archive @()
        $r = Invoke-Unbox @('extract', "$archive.002")
        Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
        Assert ($r.Out -eq (Join-Path $dir 'movie')) "target was '$($r.Out)'"
        $a = (Get-FileHash (Join-Path $src '中文目录\深层\data.bin')).Hash
        $b = (Get-FileHash (Join-Path $dir 'movie\中文目录\深层\data.bin')).Hash
        Assert ($a -eq $b) 'binary content mismatch'
        Assert (@(Get-ChildItem -LiteralPath $dir -Filter 'movie.7z.*').Count -eq $parts.Count) 'volumes were removed'
        Assert-NoLeftovers $dir

        $r = Invoke-Unbox @('extract', "$archive.001", '--recycle')
        Assert ($r.Code -eq 0) "recycle exit $($r.Code): $($r.Err)"
        Assert (Test-Path -LiteralPath (Join-Path $dir 'movie (2)\root.txt')) "unexpected layout: $(Get-ChildItem -Recurse -Name $dir | Out-String)"
        $left = @(Get-ChildItem -LiteralPath $dir -Filter 'movie.7z.*')
        Assert ($left.Count -eq 0) "volumes not recycled: $($left.Name -join ', ') $($r.Err)"
    }

    Check 'missing split volumes are named' {
        $dir = Join-Path $work 'split-gap'
        $archive = Join-Path $dir 'gap.7z'
        $parts = New-SplitSet $dir $archive @()
        Remove-Item -LiteralPath "$archive.002"
        $r = Invoke-Unbox @('extract', "$archive.001")
        Assert ($r.Code -eq 1) "exit $($r.Code): $($r.Out)"
        Assert ($r.Err -match 'gap\.7z\.002') "message: $($r.Err)"
        Assert (-not (Test-Path -LiteralPath (Join-Path $dir 'gap'))) 'output delivered'
        Assert-NoLeftovers $dir

        $dir = Join-Path $work 'split-tail'
        $archive = Join-Path $dir 'tail.7z'
        $parts = New-SplitSet $dir $archive @()
        $last = $parts[-1]
        Remove-Item -LiteralPath $last.FullName
        $r = Invoke-Unbox @('extract', "$archive.001")
        Assert ($r.Code -eq 1) "exit $($r.Code): $($r.Out)"
        Assert ($r.Err -match [regex]::Escape($last.Name)) "message: $($r.Err)"
        Assert-NoLeftovers $dir
    }

    Check 'split zip volumes' {
        $dir = Join-Path $work 'split-zip'
        $archive = Join-Path $dir 'bundle.zip'
        New-SplitSet $dir $archive @('-tzip') | Out-Null
        $r = Invoke-Unbox @('extract', "$archive.001")
        Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
        Assert ((Get-Content -LiteralPath (Join-Path $dir 'bundle\中文目录\说明.txt') -Raw).Trim() -eq 'hello unbox') "unexpected layout: $(Get-ChildItem -Recurse -Name $dir | Out-String)"
    }

    Check 'plain split file is joined' {
        $dir = Join-Path $work 'split-plain'
        New-Item -ItemType Directory -Path $dir | Out-Null
        $bytes = [IO.File]::ReadAllBytes((Join-Path $src '中文目录\深层\data.bin'))
        $n = 0
        for ($i = 0; $i -lt $bytes.Length; $i += 6000) {
            $n++
            $end = [Math]::Min($i + 6000, $bytes.Length) - 1
            [IO.File]::WriteAllBytes((Join-Path $dir ('blob.bin.{0:000}' -f $n)), [byte[]]$bytes[$i..$end])
        }
        $r = Invoke-Unbox @('extract', (Join-Path $dir 'blob.bin.001'))
        Assert ($r.Code -eq 0) "exit $($r.Code): $($r.Err)"
        $joined = @(Get-ChildItem -LiteralPath $r.Out -Recurse -File -Filter 'blob.bin')
        Assert ($joined.Count -eq 1) "unexpected layout: $(Get-ChildItem -Recurse -Name $dir | Out-String)"
        Assert ((Get-FileHash $joined[0].FullName).Hash -eq (Get-FileHash (Join-Path $src '中文目录\深层\data.bin')).Hash) 'joined content mismatch'
    }
} else {
    Write-Warning "7-Zip not found at $sevenZip; password checks skipped."
}

Check 'register / status / unregister' {
    $r = Invoke-Unbox @('register', '--no-settings')
    Assert ($r.Code -eq 0) "register exit $($r.Code): $($r.Err)"
    $s = Invoke-Unbox @('status')
    Assert ($s.Out -match '完整登记') "status: $($s.Out)"
    $icon = (Get-ItemProperty 'HKCU:\Software\Classes\Unbox.7z\DefaultIcon').'(default)'
    Assert ($icon -match 'unbox\.exe",-\d+$') "icon: $icon"
    $menu = 'HKCU:\Software\Classes\SystemFileAssociations\.7z\shell\Unbox.ExtractHere'
    $verb = (Get-ItemProperty "$menu\command").'(default)'
    Assert ($verb -match 'unbox\.exe" --here "%1"$') "context menu: $verb"
    $u = Invoke-Unbox @('unregister')
    Assert ($u.Code -eq 0) "unregister exit $($u.Code): $($u.Err)"
    Assert (-not (Test-Path 'HKCU:\Software\Classes\Unbox.7z')) 'ProgID left behind'
    Assert (-not (Test-Path $menu)) 'context menu left behind'
}

Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
if ($failures.Count -gt 0) {
    throw "$($failures.Count) smoke test(s) failed: $($failures -join ', ')"
}
Write-Host 'All smoke tests passed.' -ForegroundColor Green
