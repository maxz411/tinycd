# Install the latest tinycd release binary:
#
#   powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/maxz411/tinycd/main/install.ps1 | iex"
#
# Environment:
#   TINYCD_INSTALL_DIR  install directory (default: %LOCALAPPDATA%\tinycd\bin)
#   TINYCD_INSTALL_URL  download base (default: the latest GitHub release)

$ErrorActionPreference = "Stop"

$Repo = "maxz411/tinycd"
$BaseUrl = if ($env:TINYCD_INSTALL_URL) { $env:TINYCD_INSTALL_URL }
           else { "https://github.com/$Repo/releases/latest/download" }
$InstallDir = if ($env:TINYCD_INSTALL_DIR) { $env:TINYCD_INSTALL_DIR }
              else { Join-Path $env:LOCALAPPDATA "tinycd\bin" }

# Windows on ARM runs the x64 binary through emulation.
$Archive = "tinycd-x86_64-pc-windows-msvc.zip"

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    Write-Host "downloading $Archive"
    Invoke-WebRequest "$BaseUrl/$Archive" -OutFile (Join-Path $Tmp $Archive)
    Invoke-WebRequest "$BaseUrl/$Archive.sha256" -OutFile (Join-Path $Tmp "$Archive.sha256")

    $Expected = ((Get-Content (Join-Path $Tmp "$Archive.sha256")) -split '\s+')[0]
    $Actual = (Get-FileHash (Join-Path $Tmp $Archive) -Algorithm SHA256).Hash.ToLower()
    if ($Expected -ne $Actual) { throw "checksum mismatch for $Archive" }

    Expand-Archive (Join-Path $Tmp $Archive) -DestinationPath $Tmp
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Move-Item -Force (Join-Path $Tmp "tinycd.exe") (Join-Path $InstallDir "tinycd.exe")
} finally {
    Remove-Item -Recurse -Force $Tmp
}

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ';') -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable("Path", "$InstallDir;$UserPath", "User")
    $env:Path = "$InstallDir;$env:Path"
    Write-Host "added $InstallDir to your user PATH (restart other shells to pick it up)"
}
Write-Host "installed $(& (Join-Path $InstallDir "tinycd.exe") --version) to $InstallDir"
