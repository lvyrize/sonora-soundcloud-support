# Installs Sonora with SoundCloud support on Windows, in place of a normal Sonora when there is
# one. It downloads the latest SoundCloud build's installer from GitHub and runs it silently for
# the current user, so no administrator rights are needed. Settings, accounts and playlists are
# kept. Run it again to update, or with -Uninstall to go back to the normal Sonora.
#
#   irm https://raw.githubusercontent.com/lvyrize/sonora-soundcloud-support/feat/soundcloud/scripts/install-soundcloud.ps1 | iex
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/lvyrize/sonora-soundcloud-support/feat/soundcloud/scripts/install-soundcloud.ps1))) -Uninstall

param([switch]$Uninstall)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'lvyrize/sonora-soundcloud-support'
$url = if ($Uninstall) {
    'https://github.com/sonorahq/sonora/releases/latest/download/Sonora-Setup.exe'
} else {
    "https://github.com/$repo/releases/latest/download/Sonora-SoundCloud-Setup.exe"
}

if (-not [Environment]::Is64BitOperatingSystem) {
    throw 'Sonora needs a 64-bit Windows.'
}

$setup = Join-Path ([IO.Path]::GetTempPath()) "sonora-setup-$PID.exe"
try {
    if ($Uninstall) {
        Write-Host 'Downloading the normal Sonora...'
    } else {
        Write-Host 'Downloading Sonora with SoundCloud...'
    }
    Invoke-WebRequest -Uri $url -OutFile $setup -UseBasicParsing

    Write-Host 'Installing. Sonora closes for a moment if it is running...'
    $arguments = '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/CURRENTUSER', '/CLOSEAPPLICATIONS'
    $run = Start-Process -FilePath $setup -ArgumentList $arguments -Wait -PassThru
    if ($run.ExitCode -ne 0) {
        throw "The installer stopped with code $($run.ExitCode)."
    }
} finally {
    Remove-Item -LiteralPath $setup -Force -ErrorAction SilentlyContinue
}

if ($Uninstall) {
    Write-Host 'The normal Sonora is back. Your settings and accounts were kept.'
} else {
    Write-Host 'Sonora with SoundCloud is installed. Open it from the Start menu.'
}
