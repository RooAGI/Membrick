param(
    [ValidateSet('', 'codex', 'claude', 'gemini', 'agy')]
    [string]$Agent = '',
    [string]$Project = '.',
    [string]$InstallDir = "$HOME\.local\bin",
    [switch]$ShareTelemetry
)

$ErrorActionPreference = 'Stop'
$Repo = 'RooAGI/Membrick'
$BaseUrl = "https://github.com/$Repo/releases/latest/download"
$Asset = 'membrick-windows-x86_64.exe'
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("membrick-" + [guid]::NewGuid().ToString('N'))
$BinaryTemp = Join-Path $TempDir $Asset
$ChecksumTemp = Join-Path $TempDir 'SHA256SUMS'
$InstalledBinary = Join-Path $InstallDir 'membrick.exe'
$InstallTelemetryUrl = 'https://license.rooagi.com/v1/telemetry/install'

function Send-InstallTelemetry {
    $share = [bool]$ShareTelemetry
    if (-not $share -and [Environment]::UserInteractive -and $Host.Name -eq 'ConsoleHost') {
        try {
            $answer = Read-Host 'Share pseudonymous install details with RooAGI? This sends version, OS, CPU architecture, installer type, selected agent, and a random install ID. It excludes names, email, project paths, and memory data. [y/N]'
            $share = $answer -match '^(y|yes)$'
        } catch {
            $share = $false
        }
    }
    if (-not $share) { return }

    try {
        $configRoot = if ($env:APPDATA) { Join-Path $env:APPDATA 'Membrick' } else { Join-Path $HOME '.config\membrick' }
        New-Item -ItemType Directory -Force -Path $configRoot | Out-Null
        $idPath = Join-Path $configRoot 'install-id'
        $installId = if (Test-Path -LiteralPath $idPath) { (Get-Content -Raw -LiteralPath $idPath).Trim() } else { '' }
        if ($installId -notmatch '^(?:[0-9a-fA-F]{32}|[0-9a-fA-F]{8}-(?:[0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12})$') {
            $installId = [guid]::NewGuid().ToString('N')
            [IO.File]::WriteAllText($idPath, $installId, [Text.Encoding]::ASCII)
        }
        $versionTag = (($Version -split '\s+')[-1])
        if ($versionTag -notmatch '^[A-Za-z0-9._+-]{1,32}$') { $versionTag = 'unknown' }
        $event = @{
            schema_version = 1
            event_id = [guid]::NewGuid().ToString('N')
            install_id = $installId
            installed_at_unix = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
            version = $versionTag
            os = 'windows'
            architecture = 'x86_64'
            installer = 'install_ps1'
            agent = $(if ($Agent) { $Agent } else { 'none' })
            outcome = 'success'
            failure_stage = 'none'
        } | ConvertTo-Json -Compress
        Invoke-RestMethod -Method Post -Uri $InstallTelemetryUrl -ContentType 'application/json' -Body $event -TimeoutSec 4 | Out-Null
    } catch {
        # Install telemetry is best-effort and must not change the installation result.
    }
}

New-Item -ItemType Directory -Force -Path $TempDir | Out-Null
try {
    Write-Host 'Downloading official Membrick release for Windows x86_64...'
    Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$Asset" -OutFile $BinaryTemp
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/SHA256SUMS" -OutFile $ChecksumTemp
        $ChecksumLine = Get-Content $ChecksumTemp | Where-Object { $_ -match "\s+$([regex]::Escape($Asset))$" } | Select-Object -First 1
        if (-not $ChecksumLine) { throw "No checksum found for $Asset; refusing to install." }
        $Expected = ($ChecksumLine.Trim() -split '\s+')[0].ToLowerInvariant()
    } catch {
        # Support releases published before the aggregate manifest was introduced.
        Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$Asset.sha256" -OutFile $ChecksumTemp
        $Expected = ((Get-Content -Raw $ChecksumTemp).Trim() -split '\s+')[0].ToLowerInvariant()
    }
    $Actual = (Get-FileHash -Algorithm SHA256 $BinaryTemp).Hash.ToLowerInvariant()
    if ($Expected -ne $Actual) {
        throw 'Checksum verification failed; refusing to install.'
    }

    Write-Host 'Checksum verified.'
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Force $BinaryTemp $InstalledBinary
    New-Item -ItemType File -Force -Path (Join-Path $InstallDir '.membrick-managed') | Out-Null

    $Version = & $InstalledBinary --version
    Write-Host "Installed $Version to $InstalledBinary"

    $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $PathEntries = @($UserPath -split ';' | Where-Object { $_ })
    if ($PathEntries -notcontains $InstallDir) {
        $NewUserPath = if ([string]::IsNullOrWhiteSpace($UserPath)) { $InstallDir } else { "$UserPath;$InstallDir" }
        [Environment]::SetEnvironmentVariable('Path', $NewUserPath, 'User')
        Write-Host "Added $InstallDir to your user PATH. Open a new terminal to use membrick by name."
    }
    if (($env:Path -split ';') -notcontains $InstallDir) {
        $env:Path = "$InstallDir;$env:Path"
    }

    if ($Agent) {
        Write-Host "Configuring Membrick for $Agent in $Project..."
        & $InstalledBinary install $Agent $Project
        if ($LASTEXITCODE -ne 0) { throw "Membrick $Agent configuration failed." }

        Write-Host 'Verifying the Membrick MCP runtime...'
        & $InstalledBinary verify $Agent $Project
        if ($LASTEXITCODE -ne 0) { throw 'Membrick MCP verification failed.' }

        Write-Host "Membrick is installed, configured for $Agent, and MCP verification passed."
    } else {
        Write-Host 'Membrick is installed. Re-run with -Agent codex|claude|gemini|agy to configure an agent.'
    }

    Send-InstallTelemetry
}
finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $TempDir
}
