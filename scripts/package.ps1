# Builds the Windows packages into artifacts/installer:
#   glotzkiste-<version>-setup.exe, glotzkiste-<version>-windows-x64.zip, SHA256SUMS.txt
# Build only: never installs Glotzkiste, never runs the setup, never changes PATH.
#
#   scripts\package.ps1 [-MakeNsis <makensis.exe>] [-BinDir <folder with glotzkiste.exe>]
#
# glotzkiste.exe carries libmpv inside (build.rs, GK_LIBMPV); this script
# fetches the pinned libmpv and sets GK_LIBMPV for the build when it builds.
# NSIS 3.13 (zlib licence, a build tool, not shipped): scripts/fetch-nsis.ps1
# downloads the pinned portable ZIP. THIRD-PARTY-NOTICES.txt comes from
# cargo-about when it is installed (CI); otherwise a local test notice is written.
[CmdletBinding()]
param(
    [string]$MakeNsis = $(if ($env:GK_MAKENSIS) { $env:GK_MAKENSIS } else { 'E:/Development/_cache/tools/nsis-3.13/makensis.exe' }),
    [string]$BinDir,
    [string]$LibMpvCache = $(if ($env:RUNNER_TEMP) { Join-Path $env:RUNNER_TEMP 'libmpv' } else { 'E:/Development/_cache/tools/libmpv' })
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$toml = Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw
if ($toml -notmatch '(?m)^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"') { throw 'No workspace version in Cargo.toml' }
$version = $Matches[1]
if ($version -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*)?$') { throw "Unexpected version: $version" }
$numeric = "$($Matches[1]).$($Matches[2]).$($Matches[3]).0"
if (!(Test-Path -LiteralPath $MakeNsis -PathType Leaf)) {
    throw "NSIS compiler not found at '$MakeNsis'. Run scripts/fetch-nsis.ps1 -Destination <folder> and pass -MakeNsis."
}
Write-Host "glotzkiste $version from commit $(& git -C $root rev-parse --short HEAD)"

function Invoke-Bounded([string]$File, [string[]]$Arguments, [int]$Seconds) {
    $info = [Diagnostics.ProcessStartInfo]::new($File)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.WorkingDirectory = $root
    foreach ($a in $Arguments) { $info.ArgumentList.Add($a) }
    $p = [Diagnostics.Process]::Start($info)
    Write-Host "Started PID $($p.Id): $(Split-Path -Leaf $File) (timeout $Seconds s)"
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    try {
        if (!$p.WaitForExit($Seconds * 1000)) {
            $p.Kill($true) # only this process and its own children
            $p.WaitForExit()
            throw "$File timed out after $Seconds s"
        }
        $o = $out.GetAwaiter().GetResult(); if ($o) { Write-Host $o }
        $e = $err.GetAwaiter().GetResult(); if ($e) { Write-Host $e }
        if ($p.ExitCode) { throw "$File failed ($($p.ExitCode))" }
        return $o
    } finally { $p.Dispose() }
}

$cargo = (Get-Command cargo -ErrorAction Stop).Source
if (!$BinDir) {
    # The pinned libmpv goes inside the exe; the variable lives only in this process.
    if (!$env:GK_LIBMPV) { $env:GK_LIBMPV = & (Join-Path $PSScriptRoot 'fetch-libmpv.ps1') -Destination $LibMpvCache }
    Invoke-Bounded $cargo @('build', '--release', '--locked', '-j', '8', '-p', 'glotzkiste') 3600 | Out-Null
    $meta = & $cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
    $BinDir = Join-Path $meta.target_directory 'release'
}
$exe = Join-Path $BinDir 'glotzkiste.exe'
if (!(Test-Path -LiteralPath $exe -PathType Leaf)) { throw "Missing glotzkiste.exe in $BinDir" }
# A package without libmpv inside would not play anything.
if ((Get-Item -LiteralPath $exe).Length -lt 20MB) { throw "$exe is too small to contain libmpv: was GK_LIBMPV set for the build?" }

$out = Join-Path $root 'artifacts/installer'
New-Item -ItemType Directory -Force -Path $out | Out-Null
$stage = Join-Path $out ('stage-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stage | Out-Null
Copy-Item -LiteralPath $exe -Destination $stage
Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination (Join-Path $stage 'LICENSE.txt')
Copy-Item -LiteralPath (Join-Path $root 'licenses/libmpv-NOTICE.txt') -Destination $stage
Copy-Item -LiteralPath (Join-Path $root 'licenses/LGPL-2.1.txt') -Destination $stage
$notices = Join-Path $stage 'THIRD-PARTY-NOTICES.txt'
if (Get-Command cargo-about -ErrorAction SilentlyContinue) {
    Invoke-Bounded $cargo @('about', 'generate', '-o', $notices, (Join-Path $root 'about.hbs')) 600 | Out-Null
} else {
    Write-Warning 'cargo-about is not installed: writing a local test notice. Release builds generate the real one in CI.'
    "Third-party notices are generated in CI release builds.`r`nThis is a local test package of Glotzkiste $version." | Set-Content -LiteralPath $notices -Encoding utf8
}

$setup = Join-Path $out "glotzkiste-$version-setup.exe"
Invoke-Bounded $MakeNsis @('-V3', "-DVERSION=$version", "-DVI_VERSION=$numeric", "-DSRC=$stage", "-DOUTFILE=$setup", (Join-Path $root 'installer/glotzkiste.nsi')) 300 | Out-Null
$zip = Join-Path $out "glotzkiste-$version-windows-x64.zip"
if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
Remove-Item -LiteralPath $stage -Recurse -Force

$lines = foreach ($f in @($setup, $zip)) {
    $h = (Get-FileHash -LiteralPath $f -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Host ('{0:N0} bytes  {1}' -f (Get-Item -LiteralPath $f).Length, $f)
    "$h  $(Split-Path -Leaf $f)"
}
# sha256sum format with LF endings, the contract with the in-app updater.
[IO.File]::WriteAllText((Join-Path $out 'SHA256SUMS.txt'), (($lines -join "`n") + "`n"))
Write-Host "Wrote $(Join-Path $out 'SHA256SUMS.txt')"
