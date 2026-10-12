# Downloads the newest cloud-built installers from the fork's release into installers\<OS>\,
# each named with its version and build time; older ones are never removed.
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

# Every build is saved under its own name (app version, then the day and time the build was
# made), so a new download never replaces an older installer.
try {
  $feed = Invoke-RestMethod -Uri "$base/latest.json" -TimeoutSec 30
  $built = [DateTimeOffset]::FromUnixTimeSeconds([long]$feed.kd.build).LocalDateTime
  $label = "v{0}-{1:yyyyMMdd-HHmm}" -f $feed.version, $built
} catch {
  Write-Host "Could not read the build details (the release may be mid-publish). Nothing was downloaded; try again in a minute."
  exit 1
}
Write-Host ("Newest cloud build: {0} (commit {1})" -f $label, $feed.kd.commit)

$failed = 0
foreach ($item in $plan) {
  $extension = [IO.Path]::GetExtension($item.File)
  $saveAs = "{0}-{1}{2}" -f [IO.Path]::GetFileNameWithoutExtension($item.File), $label, $extension
  $first = Join-Path (Join-Path $OutputRoot $item.Folders[0]) $saveAs
  $null = New-Item -ItemType Directory -Force -Path (Split-Path -Parent $first)
  $partial = "$first.part"
  try {
    $had = Test-Path -LiteralPath $first
    if (-not $had) {
      Invoke-WebRequest -Uri "$base/$($item.File)" -OutFile $partial -UseBasicParsing -TimeoutSec 600
      Move-Item -LiteralPath $partial -Destination $first
    }
    foreach ($folder in ($item.Folders | Select-Object -Skip 1)) {
      $copy = Join-Path (Join-Path $OutputRoot $folder) $saveAs
      $null = New-Item -ItemType Directory -Force -Path (Split-Path -Parent $copy)
      if (-not (Test-Path -LiteralPath $copy)) { Copy-Item -LiteralPath $first -Destination $copy }
    }
    $note = if ($had) { "already here" } else { "downloaded" }
    Write-Host ("  {0,-58} {1,8:N1} MB  {2,-12} -> {3}" -f $saveAs, ((Get-Item -LiteralPath $first).Length / 1MB), $note, ($item.Folders -join ", "))
  } catch {
    $failed += 1
    if (Test-Path -LiteralPath $partial) { Remove-Item -LiteralPath $partial -Force }
    Write-Host ("  {0,-58} FAILED: {1}" -f $saveAs, $_.Exception.Message)
  }
}

if ($failed) { Write-Host "$failed file(s) could not be downloaded; nothing already on disk was changed."; exit 1 }
Write-Host "Installers are up to date in $OutputRoot (older builds are kept beside the new ones)"
