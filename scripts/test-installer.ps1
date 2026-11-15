# Installer lifecycle test. CI ONLY: it installs Glotzkiste for the current
# user, writes HKCU and %LOCALAPPDATA%, and uninstalls again. It refuses to run
# outside GitHub Actions so it can never touch a real profile.
#   scripts/test-installer.ps1 -Setup <glotzkiste-x.y.z-setup.exe> -Version <x.y.z>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Setup,
    [Parameter(Mandatory)][string]$Version
)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') { throw 'test-installer.ps1 runs only on disposable CI runners.' }

$dir = Join-Path $env:LOCALAPPDATA 'Programs\Glotzkiste'
$exe = Join-Path $dir 'glotzkiste.exe'
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Glotzkiste'
$settings = Join-Path $env:APPDATA 'Glotzkiste'

function Run([string]$File, [string[]]$Arguments, [int]$Seconds = 120) {
    $p = Start-Process -FilePath $File -ArgumentList $Arguments -PassThru
    if (!$p.WaitForExit($Seconds * 1000)) { $p.Kill(); throw "$File $Arguments timed out" }
    return $p.ExitCode
}

# Runs the installed exe with one option; returns (exit code, stdout).
function Query([string]$Option) {
    $out = Join-Path $env:RUNNER_TEMP "glotzkiste$Option.txt"
    $p = Start-Process -FilePath $exe -ArgumentList $Option -RedirectStandardOutput $out -PassThru -NoNewWindow
    if (!$p.WaitForExit(60000)) { $p.Kill(); throw "glotzkiste $Option timed out" }
    return $p.ExitCode, (Get-Content -LiteralPath $out -Raw).Trim()
}

function Value([string]$Path, [string]$Name) {
    $item = Get-ItemProperty -LiteralPath $Path -ErrorAction SilentlyContinue
    if ($null -eq $item) { return $null }
    return $item.$Name
}

function Assert-Installed {
    if (!(Test-Path -LiteralPath $exe)) { throw "glotzkiste.exe missing in $dir" }
    foreach ($f in 'libmpv-2.dll', 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt', 'libmpv-NOTICE.txt', 'LGPL-2.1.txt', 'uninstall.exe') {
        if (!(Test-Path -LiteralPath (Join-Path $dir $f))) { throw "$f missing" }
    }
    if ((Value $key 'DisplayVersion') -ne $Version) { throw "DisplayVersion is '$(Value $key 'DisplayVersion')', expected $Version" }
    if (!(Test-Path -LiteralPath (Join-Path ([Environment]::GetFolderPath('Programs')) 'Glotzkiste.lnk'))) { throw 'Start menu shortcut missing' }
    # Release builds are GUI-subsystem; redirected stdout still carries the output.
    $code, $line = Query '--version'
    if ($code -ne 0 -or $line -ne "glotzkiste $Version") { throw "glotzkiste --version printed '$line' ($code)" }
    # The libmpv next to it loads and has the client API Glotzkiste needs.
    $code, $line = Query '--mpv-info'
    if ($code -ne 0) { throw "glotzkiste --mpv-info failed ($code): $line" }
    Write-Host "   $line"
}

# A saved layout that install, update and uninstall must keep.
New-Item -ItemType Directory -Force -Path $settings | Out-Null
'{"tabs":[],"active":0,"latency":"Ultra"}' | Set-Content -LiteralPath (Join-Path $settings 'layout.json')

Write-Host '1. Silent install'
if ((Run $Setup @('/S'))) { throw 'Setup failed' }
Assert-Installed

Write-Host '2. Silent reinstall over the existing copy (what an update does)'
if ((Run $Setup @('/S'))) { throw 'Reinstall failed' }
Assert-Installed

Write-Host '3. A locked glotzkiste.exe (as while it runs) makes a silent install give up after 30 s'
$lock = [IO.File]::Open($exe, 'Open', 'Read', 'Read')
try {
    $started = Get-Date
    $code = Run $Setup @('/S') 90
    $waited = ((Get-Date) - $started).TotalSeconds
} finally { $lock.Dispose() }
if ($code -eq 0) { throw 'Setup replaced a locked glotzkiste.exe' }
if ($waited -lt 25) { throw "Setup gave up after $([int]$waited) s instead of waiting about 30 s" }
Write-Host ("   setup refused with exit code {0} after {1:N0} s" -f $code, $waited)
Assert-Installed

Write-Host '4. Silent uninstall'
if ((Run (Join-Path $dir 'uninstall.exe') @('/S'))) { throw 'Uninstall failed' }
# The NSIS uninstaller copies itself to TEMP and finishes there.
$deadline = (Get-Date).AddSeconds(30)
while ((Test-Path -LiteralPath $exe) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250 }
if (Test-Path -LiteralPath $exe) { throw 'glotzkiste.exe still present after uninstall' }
if (Test-Path -LiteralPath $key) { throw 'Installed apps entry still present' }
if (Test-Path -LiteralPath (Join-Path ([Environment]::GetFolderPath('Programs')) 'Glotzkiste.lnk')) { throw 'Start menu shortcut still present' }
if (!(Test-Path -LiteralPath (Join-Path $settings 'layout.json'))) { throw 'Uninstall removed the saved layout' }
Write-Host 'Installer lifecycle passed.'
