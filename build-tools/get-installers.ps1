# Downloads the newest cloud-built installers from the fork's release into installers\<OS>\.
# The Windows installer is built on this PC by update-and-build, so it is left alone unless
# -IncludeWindows is given.
param(
  [switch]$IncludeWindows,
  [string]$RepoPath = "",
  [string]$OutputRoot = ""
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

if (-not $RepoPath) { $RepoPath = Split-Path -Parent $PSScriptRoot }
$RepoPath = (Resolve-Path -LiteralPath $RepoPath).Path
if (-not $OutputRoot) { $OutputRoot = Join-Path $RepoPath "installers" }

$brand = Get-Content -LiteralPath (Join-Path $PSScriptRoot "brand.json") -Raw | ConvertFrom-Json
if (-not $brand.updateRepo -or -not $brand.updateTag) {
  Write-Host "brand.json does not say where this fork publishes its builds."
  exit 1
}
$name = $brand.productName
$base = "https://github.com/$($brand.updateRepo)/releases/download/$($brand.updateTag)"

# Which folders each installer goes to.
$plan = @(
  @{ File = "$name-android-fireos.apk"; Folders = @("Android", "FireOS") },
  # The same app made smaller for one device each; either can be replaced by the one above.
  @{ File = "$name-fire-hd10-2023.apk"; Folders = @("FireOS\Fire-HD-10-2023") },
  @{ File = "$name-fire-tv-stick-4k.apk"; Folders = @("FireOS\Fire-TV-Stick-4K") },
  @{ File = "$name-ubuntu-amd64.deb"; Folders = @("Ubuntu") },
  @{ File = "$name-linux-x86_64.AppImage"; Folders = @("Ubuntu") },
  @{ File = "$name-macos-universal.dmg"; Folders = @("macOS") }
)
if ($IncludeWindows) { $plan += @{ File = "$name-windows-x64-setup.exe"; Folders = @("Windows") } }

try {
  $feed = Invoke-RestMethod -Uri "$base/latest.json" -TimeoutSec 30
  $built = [DateTimeOffset]::FromUnixTimeSeconds([long]$feed.kd.build).LocalDateTime
  Write-Host ("Newest cloud build: commit {0}, made {1:yyyy-MM-dd HH:mm}" -f $feed.kd.commit, $built)
} catch {
  Write-Host "Could not read the build details (the release may be mid-publish). Downloading what is there."
}

$failed = 0
foreach ($item in $plan) {
  $first = Join-Path (Join-Path $OutputRoot $item.Folders[0]) $item.File
  $null = New-Item -ItemType Directory -Force -Path (Split-Path -Parent $first)
  $partial = "$first.part"
  try {
    Invoke-WebRequest -Uri "$base/$($item.File)" -OutFile $partial -UseBasicParsing -TimeoutSec 600
    Move-Item -LiteralPath $partial -Destination $first -Force
    foreach ($folder in ($item.Folders | Select-Object -Skip 1)) {
      $copy = Join-Path (Join-Path $OutputRoot $folder) $item.File
      $null = New-Item -ItemType Directory -Force -Path (Split-Path -Parent $copy)
      Copy-Item -LiteralPath $first -Destination $copy -Force
    }
    Write-Host ("  {0,-34} {1,8:N1} MB  -> {2}" -f $item.File, ((Get-Item -LiteralPath $first).Length / 1MB), ($item.Folders -join ", "))
  } catch {
    $failed += 1
    if (Test-Path -LiteralPath $partial) { Remove-Item -LiteralPath $partial -Force }
    Write-Host ("  {0,-34} FAILED: {1}" -f $item.File, $_.Exception.Message)
  }
}

if ($failed) { Write-Host "$failed file(s) could not be downloaded; the older copies were kept."; exit 1 }
Write-Host "Installers are up to date in $OutputRoot"
