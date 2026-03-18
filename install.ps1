$ErrorActionPreference = "Stop"

$Repo = "querymt/querymt"
$Channel = if ($env:QMT_CHANNEL -eq "nightly") { "nightly" } else { "latest" }
$InstallDir = if ($env:QMT_INSTALL_DIR) { $env:QMT_INSTALL_DIR } else { Join-Path $env:USERPROFILE ".local\bin" }

$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
switch ($arch) {
    "X64" { $Target = "x86_64-pc-windows-msvc" }
    "Arm64" { $Target = "aarch64-pc-windows-msvc" }
    default { throw "Unsupported Windows architecture: $arch" }
}

function Get-ReleaseApiUrl {
    if ($Channel -eq "nightly") {
        return "https://api.github.com/repos/$Repo/releases/tags/nightly"
    }
    return "https://api.github.com/repos/$Repo/releases/latest"
}

function Get-AssetUrl([string]$Binary) {
    $release = Invoke-RestMethod -Uri (Get-ReleaseApiUrl)
    if ($Channel -eq "nightly") {
        $regex = "^$Binary-nightly-.*-$Target\.zip$"
    } else {
        $regex = "^$Binary-.*-$Target\.zip$"
    }

    $asset = $release.assets | Where-Object { $_.name -match $regex } | Select-Object -First 1
    if (-not $asset) {
        throw "Could not find asset for $Binary ($Target, $Channel)"
    }

    return $asset.browser_download_url
}

function Install-Binary([string]$Binary) {
    $tmpRoot = Join-Path $env:TEMP ("qmt-install-" + [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $tmpRoot | Out-Null

    try {
        $zipPath = Join-Path $tmpRoot "$Binary.zip"
        $extractDir = Join-Path $tmpRoot "extract"

        $url = Get-AssetUrl -Binary $Binary
        Write-Host "Downloading $Binary ($Channel, $Target)..."
        Invoke-WebRequest -Uri $url -OutFile $zipPath

        Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force

        $exe = Get-ChildItem -Path $extractDir -Recurse -Filter "$Binary.exe" | Select-Object -First 1
        if (-not $exe) {
            throw "Failed to locate $Binary.exe in extracted archive"
        }

        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        Copy-Item $exe.FullName -Destination (Join-Path $InstallDir "$Binary.exe") -Force
    }
    finally {
        Remove-Item -Path $tmpRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Install-Binary -Binary "qmt"
Install-Binary -Binary "coder_agent"

Write-Host "Installed to: $InstallDir"

$pathParts = [Environment]::GetEnvironmentVariable("Path", "User") -split ";"
if ($pathParts -notcontains $InstallDir) {
    $newPath = if ([string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable("Path", "User"))) {
        $InstallDir
    } else {
        [Environment]::GetEnvironmentVariable("Path", "User") + ";" + $InstallDir
    }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    Write-Host "Added $InstallDir to user PATH. Restart your shell to pick it up."
}

& (Join-Path $InstallDir "qmt.exe") --version
& (Join-Path $InstallDir "coder_agent.exe") --version
