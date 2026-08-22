<#
.SYNOPSIS
  Meshcast installer for Windows (viewer + app).

.DESCRIPTION
  Downloads the latest Meshcast release, installs it to %LOCALAPPDATA%\Meshcast,
  adds it to your PATH, registers the meshcast:// link handler and creates a
  Start Menu shortcut. No admin rights needed.

  Run from PowerShell:
    irm https://raw.githubusercontent.com/mattcree/meshcast/main/scripts/install.ps1 | iex

  Or with options:
    .\install.ps1 -Version v0.5.0 -NoStartup

  Note: on Windows, Meshcast can *watch* streams. Screen capture (streaming)
  isn't supported by iroh-live on Windows yet.
#>
[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$Repo = "mattcree/meshcast",
    [switch]$NoStartup,
    [switch]$NoLaunch,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$InstallDir = Join-Path $env:LOCALAPPDATA "Meshcast"
$StartMenu = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Meshcast.lnk"
$Startup = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Startup\Meshcast.lnk"
$Asset = "meshcast-windows-x86_64.zip"

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

if ($Uninstall) {
    Write-Step "Removing Meshcast"
    Get-Process meshcast, meshcast-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $InstallDir -ErrorAction SilentlyContinue
    Remove-Item -Force $StartMenu, $Startup -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force "HKCU:\Software\Classes\meshcast" -ErrorAction SilentlyContinue
    $path = [Environment]::GetEnvironmentVariable("Path", "User")
    $new = ($path -split ";" | Where-Object { $_ -and ($_ -ne $InstallDir) }) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $new, "User")
    Write-Host "Meshcast removed. Config kept in $env:APPDATA\meshcast."
    return
}

if ([Environment]::Is64BitOperatingSystem -eq $false) { throw "Meshcast needs 64-bit Windows." }

$base = if ($Version -eq "latest") { "https://github.com/$Repo/releases/latest/download" } else { "https://github.com/$Repo/releases/download/$Version" }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("meshcast-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    Write-Step "Downloading $Asset ($Version)"
    $zip = Join-Path $tmp $Asset
    Invoke-WebRequest -Uri "$base/$Asset" -OutFile $zip -UseBasicParsing

    try {
        $sums = Join-Path $tmp "SHA256SUMS"
        Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sums -UseBasicParsing
        $expected = (Get-Content $sums | Where-Object { $_ -match " $([regex]::Escape($Asset))$" }) -split "\s+" | Select-Object -First 1
        $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
        if ($expected -and ($expected.ToLower() -ne $actual)) { throw "Checksum mismatch for $Asset" }
        Write-Step "Checksum OK"
    } catch {
        if ($_.Exception.Message -like "Checksum mismatch*") { throw }
        Write-Warning "No SHA256SUMS published for this release; skipping verification."
    }

    Write-Step "Installing to $InstallDir"
    Get-Process meshcast, meshcast-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item (Join-Path $tmp "meshcast\*") $InstallDir -Recurse -Force

    # PATH
    $path = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($path -split ";") -notcontains $InstallDir) {
        [Environment]::SetEnvironmentVariable("Path", "$path;$InstallDir", "User")
        Write-Step "Added $InstallDir to your PATH (open a new terminal to use 'meshcast')"
    }

    # meshcast:// URL protocol (per-user, no admin)
    $exe = Join-Path $InstallDir "meshcast.exe"
    $key = "HKCU:\Software\Classes\meshcast"
    New-Item -Path $key -Force | Out-Null
    Set-ItemProperty -Path $key -Name "(Default)" -Value "URL:Meshcast stream"
    Set-ItemProperty -Path $key -Name "URL Protocol" -Value ""
    New-Item -Path "$key\shell\open\command" -Force | Out-Null
    Set-ItemProperty -Path "$key\shell\open\command" -Name "(Default)" -Value "`"$exe`" watch `"%1`""
    Write-Step "Registered meshcast:// links"

    # Shortcuts
    $shell = New-Object -ComObject WScript.Shell
    $app = Join-Path $InstallDir "meshcast-app.exe"
    foreach ($lnk in @($StartMenu) + $(if (-not $NoStartup) { @($Startup) } else { @() })) {
        $s = $shell.CreateShortcut($lnk)
        $s.TargetPath = $app
        $s.WorkingDirectory = $InstallDir
        $s.Description = "Meshcast — P2P screen streaming for Discord"
        $s.Save()
    }
    if (-not $NoStartup) { Write-Step "Meshcast will start at login (remove the Startup shortcut to disable)" }

    Write-Host ""
    Write-Host "Meshcast installed." -ForegroundColor Green
    Write-Host "  Next: in Discord type /link, then paste the code into the Meshcast window."
    if (-not $NoLaunch) { Start-Process $app }
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
