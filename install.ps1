# Requires -Version 5.1
<#
.SYNOPSIS
    Install or uninstall spool download manager on Windows for the current user.
.DESCRIPTION
    Installs spool without administrative privileges to $env:LOCALAPPDATA\Programs\spool,
    creates Start Menu shortcuts, registers browser extension native messaging hosts,
    and associates magnet links and .torrent files.
.PARAMETER Uninstall
    Uninstalls spool, removes shortcuts, registry keys, and host manifests.
#>
[CmdletBinding()]
param(
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

$App = "spool"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\$App"
$BinPath = Join-Path $InstallDir "$App.exe"
$DataDir = Join-Path $env:LOCALAPPDATA "com.saikarthik.spool"
$ExtDir = Join-Path $DataDir "extension"
$StartMenuDir = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs"
$ShortcutPath = Join-Path $StartMenuDir "$App.lnk"
$HostManifestDir = Join-Path $InstallDir "native-messaging-hosts"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$ReleaseExe = Join-Path $ScriptDir "src-tauri\target\release\$App.exe"
$ExtensionSource = Join-Path $ScriptDir "extension"
$IconSource = Join-Path $ScriptDir "src-tauri\icons\icon.ico"

function Write-Info($msg) {
    Write-Host "spool: $msg" -ForegroundColor Cyan
}

function Write-Success($msg) {
    Write-Host "spool: $msg" -ForegroundColor Green
}

function Write-Warn($msg) {
    Write-Host "spool: $msg" -ForegroundColor Yellow
}

function Remove-RegistryKeySafely($Path) {
    if (Test-Path $Path) {
        Remove-Item -Path $Path -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($Uninstall) {
    Write-Info "Uninstalling $App..."

    # Stop running processes
    Get-Process -Name $App -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1

    # Remove Start Menu shortcut
    if (Test-Path $ShortcutPath) {
        Remove-Item -Path $ShortcutPath -Force -ErrorAction SilentlyContinue
        Write-Info "Removed Start Menu shortcut."
    }

    # Remove Native Messaging registry keys
    $RegTargets = @(
        "HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.saikarthik.spool",
        "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\com.saikarthik.spool",
        "HKCU:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.saikarthik.spool",
        "HKCU:\Software\Vivaldi\NativeMessagingHosts\com.saikarthik.spool",
        "HKCU:\Software\Opera Software\Opera Stable\NativeMessagingHosts\com.saikarthik.spool",
        "HKCU:\Software\Mozilla\NativeMessagingHosts\com.saikarthik.spool"
    )
    foreach ($key in $RegTargets) {
        Remove-RegistryKeySafely $key
    }
    Write-Info "Removed browser native messaging registrations."

    # Remove magnet and torrent protocol associations
    Remove-RegistryKeySafely "HKCU:\Software\Classes\magnet\shell\open\command"
    Remove-RegistryKeySafely "HKCU:\Software\Classes\.torrent"

    # Remove binary and extension directory
    if (Test-Path $InstallDir) {
        Remove-Item -Path $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path $ExtDir) {
        Remove-Item -Path $ExtDir -Recurse -Force -ErrorAction SilentlyContinue
    }

    # Clean user PATH
    $UserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($UserPath -like "*$InstallDir*") {
        $NewPath = ($UserPath -split ";" | Where-Object { $_ -and $_ -ne $InstallDir }) -join ";"
        [Environment]::SetEnvironmentVariable("PATH", $NewPath, "User")
        Write-Info "Removed $InstallDir from user PATH."
    }

    Write-Success "Uninstalled $App successfully."
    Write-Host "Downloads, settings, and queue files were left intact at $DataDir" -ForegroundColor Gray
    exit 0
}

# --- INSTALLATION ---

if (-not (Test-Path $ReleaseExe)) {
    Write-Warn "Release binary not found at $ReleaseExe"
    Write-Info "Building release binary via 'npm run tauri build -- --no-bundle'..."
    Push-Location $ScriptDir
    try {
        & npm run tauri build -- --no-bundle
    } finally {
        Pop-Location
    }
    if (-not (Test-Path $ReleaseExe)) {
        Write-Error "Build failed; release binary $ReleaseExe is still missing."
        exit 1
    }
}

# Stop any running instances before overwriting
Get-Process -Name $App -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 1

# Create installation directory
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
Copy-Item -Path $ReleaseExe -Destination $BinPath -Force
Write-Info "Installed $BinPath"

# Copy browser extension to canonical directory
if (Test-Path $ExtensionSource) {
    New-Item -ItemType Directory -Path $ExtDir -Force | Out-Null
    Copy-Item -Path "$ExtensionSource\*" -Destination $ExtDir -Recurse -Force
    Write-Info "Installed browser extension to $ExtDir"
}

# Create Start Menu shortcut
try {
    $WshShell = New-Object -ComObject WScript.Shell
    $Shortcut = $WshShell.CreateShortcut($ShortcutPath)
    $Shortcut.TargetPath = $BinPath
    $Shortcut.WorkingDirectory = $InstallDir
    $Shortcut.Description = "Personal segmented download manager"
    $Shortcut.IconLocation = "$BinPath,0"
    $Shortcut.Save()
    Write-Info "Created Start Menu shortcut: $ShortcutPath"
} catch {
    Write-Warn "Could not create Start Menu shortcut: $_"
}

# Ensure native messaging host manifests are created
New-Item -ItemType Directory -Path $HostManifestDir -Force | Out-Null
$ChromeManifestPath = Join-Path $HostManifestDir "com.saikarthik.spool.json"
$FirefoxManifestPath = Join-Path $HostManifestDir "com.saikarthik.spool-firefox.json"

$ChromeJson = @"
{
  "name": "com.saikarthik.spool",
  "description": "spool download manager",
  "path": "$($BinPath.Replace('\', '\\'))",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://mlddhjdhcladccjcgffcmeonbjhkhhlo/"
  ]
}
"@

$FirefoxJson = @"
{
  "name": "com.saikarthik.spool",
  "description": "spool download manager",
  "path": "$($BinPath.Replace('\', '\\'))",
  "type": "stdio",
  "allowed_extensions": [
    "spool@saikarthik.com"
  ]
}
"@

Set-Content -Path $ChromeManifestPath -Value $ChromeJson -Encoding UTF8
Set-Content -Path $FirefoxManifestPath -Value $FirefoxJson -Encoding UTF8

# Register registry keys for all supported browsers
$ChromeRegTargets = @(
    "HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.saikarthik.spool",
    "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\com.saikarthik.spool",
    "HKCU:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.saikarthik.spool",
    "HKCU:\Software\Vivaldi\NativeMessagingHosts\com.saikarthik.spool",
    "HKCU:\Software\Opera Software\Opera Stable\NativeMessagingHosts\com.saikarthik.spool"
)
foreach ($reg in $ChromeRegTargets) {
    try {
        New-Item -Path $reg -Force | Out-Null
        Set-ItemProperty -Path $reg -Name "(default)" -Value $ChromeManifestPath -Force
    } catch {
        Write-Warn "Could not register native messaging for $reg: $_"
    }
}

try {
    $FfReg = "HKCU:\Software\Mozilla\NativeMessagingHosts\com.saikarthik.spool"
    New-Item -Path $FfReg -Force | Out-Null
    Set-ItemProperty -Path $FfReg -Name "(default)" -Value $FirefoxManifestPath -Force
} catch {
    Write-Warn "Could not register Firefox native messaging: $_"
}
Write-Info "Registered browser native messaging hosts."

# Register magnet: protocol handler and .torrent file association
try {
    $MagnetKey = "HKCU:\Software\Classes\magnet"
    New-Item -Path "$MagnetKey\shell\open\command" -Force | Out-Null
    Set-ItemProperty -Path $MagnetKey -Name "(default)" -Value "URL:Magnet Protocol" -Force
    Set-ItemProperty -Path $MagnetKey -Name "URL Protocol" -Value "" -Force
    Set-ItemProperty -Path "$MagnetKey\shell\open\command" -Name "(default)" -Value "`"$BinPath`" `"%1`"" -Force

    $TorrentKey = "HKCU:\Software\Classes\.torrent"
    New-Item -Path $TorrentKey -Force | Out-Null
    Set-ItemProperty -Path $TorrentKey -Name "(default)" -Value "spool.torrent" -Force
    New-Item -Path "HKCU:\Software\Classes\spool.torrent\shell\open\command" -Force | Out-Null
    Set-ItemProperty -Path "HKCU:\Software\Classes\spool.torrent\shell\open\command" -Name "(default)" -Value "`"$BinPath`" `"%1`"" -Force
    Write-Info "Registered magnet: and .torrent associations."
} catch {
    Write-Warn "Could not set file/protocol associations: $_"
}

# Add to user PATH if not present
$UserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if ($UserPath -notlike "*$InstallDir*") {
    $NewPath = if ($UserPath) { "$UserPath;$InstallDir" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable("PATH", $NewPath, "User")
    $env:PATH = "$env:PATH;$InstallDir"
    Write-Info "Added $InstallDir to user PATH."
}

Write-Host ""
Write-Success "Done! spool is installed."
Write-Host "You can launch it from the Start Menu, or by typing: spool" -ForegroundColor White
