# Merges the author's latest code into the current branch, tests it, and builds a new Windows installer.
param(
  [switch]$Force,
  [switch]$Push,
  [switch]$SkipTests,
  [switch]$SkipBuild,
  [string]$RepoPath = "",
  [string]$UpstreamBranch = "main",
  [string]$OutputDir = ""
)

$ErrorActionPreference = "Continue"

if (-not $RepoPath) {
  $RepoPath = git -C $PSScriptRoot rev-parse --show-toplevel 2>$null
  if (-not $RepoPath) { $RepoPath = Split-Path -Parent $PSScriptRoot }
}
$RepoPath = (Resolve-Path -LiteralPath $RepoPath).Path
if (-not $OutputDir) { $OutputDir = Join-Path $RepoPath "installers\Windows" }
$Log = Join-Path $PSScriptRoot "update-and-build.log"
$started = Get-Date
$merged = $false
$pre = ""

$toolDirs = @(
  (Join-Path $env:ProgramFiles "nodejs"),
  (Join-Path $env:LOCALAPPDATA "Programs\nodejs"),
  (Join-Path $env:USERPROFILE ".cargo\bin")
)
foreach ($dir in $toolDirs) {
  if ((Test-Path -LiteralPath $dir) -and ($env:Path -notlike "*$dir*")) { $env:Path = "$dir;$env:Path" }
}

function Say([string]$Text) {
  $line = "[{0}] {1}" -f (Get-Date -Format "HH:mm:ss"), $Text
  Write-Host $line
  Add-Content -LiteralPath $Log -Value $line -Encoding UTF8
}

function Run([string]$Exe, [string[]]$Arguments) {
  & $Exe @Arguments 2>&1 | ForEach-Object {
    $text = "$_"
    Add-Content -LiteralPath $Log -Value $text -Encoding UTF8
    Write-Host $text
  }
  return $LASTEXITCODE
}

function Stop-With([int]$Code, [string]$Text) {
  Say $Text
  Say "=== stopped (code $Code) ==="
  exit $Code
}

function Undo-Merge {
  if (-not $merged) { return }
  git -C $RepoPath merge-base --is-ancestor $pre HEAD
  if ($LASTEXITCODE -eq 0) {
    $null = Run "git" @("-C", $RepoPath, "reset", "--hard", $pre)
    Say "Your branch was put back exactly where it was before this run."
  }
}

Set-Location -LiteralPath $RepoPath
Say "=== update-and-build started in $RepoPath ==="

$needed = @("git")
if (-not ($SkipTests -and $SkipBuild)) { $needed += @("node", "corepack") }
if (-not $SkipBuild) { $needed += "cargo" }
foreach ($tool in $needed) {
  if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { Stop-With 1 "Cannot continue: '$tool' was not found on this PC." }
}

$branch = git -C $RepoPath rev-parse --abbrev-ref HEAD
if ($LASTEXITCODE -ne 0) { Stop-With 1 "Cannot continue: $RepoPath is not a Git repository." }
if ($branch -eq "HEAD") { Stop-With 1 "Cannot continue: no branch is checked out." }
if (-not (git -C $RepoPath remote | Where-Object { $_ -eq "upstream" })) { Stop-With 1 "Cannot continue: there is no 'upstream' remote pointing at the author's repository." }
$gitDir = git -C $RepoPath rev-parse --absolute-git-dir
if (Test-Path -LiteralPath (Join-Path $gitDir "MERGE_HEAD")) { Stop-With 1 "Cannot continue: a merge is already in progress. Finish or abort it first." }
$dirty = @(git -C $RepoPath status --porcelain --untracked-files=no)
if ($dirty.Count -gt 0) { Stop-With 1 "Cannot continue: $($dirty.Count) file(s) have uncommitted changes. Commit or undo them first, so nothing of yours can be lost." }

Say "Branch: $branch. Fetching the author's latest code..."
if ((Run "git" @("-C", $RepoPath, "fetch", "upstream", "--prune")) -ne 0) { Stop-With 1 "Could not reach the author's repository. Check the internet connection and try again." }

$target = "upstream/$UpstreamBranch"
$pre = git -C $RepoPath rev-parse HEAD
$new = [int](git -C $RepoPath rev-list --count "HEAD..$target")

if ($new -eq 0 -and -not $Force) {
  Say "Already up to date: the author has published nothing new. No build needed."
  Say "=== finished ==="
  exit 0
}

if ($new -gt 0) {
  Say "The author has $new new commit(s):"
  git -C $RepoPath log --oneline --no-decorate "HEAD..$target" | Select-Object -First 15 | ForEach-Object { Say "   $_" }

  $identity = git -C $RepoPath log -1 --format="%an|%ae" "$target..HEAD"
  if (-not $identity) { $identity = "local-build|local-build@localhost" }
  $name, $email = $identity -split "\|", 2

  Say "Merging into $branch..."
  $code = Run "git" @("-C", $RepoPath, "-c", "user.name=$name", "-c", "user.email=$email", "merge", "--no-edit", $target)
  if ($code -ne 0) {
    $conflicts = @(git -C $RepoPath diff --name-only --diff-filter=U)
    $null = Run "git" @("-C", $RepoPath, "merge", "--abort")
    Say "MERGE CONFLICT: the author changed the same lines as your fixes in $($conflicts.Count) file(s):"
    $conflicts | ForEach-Object { Say "   $_" }
    Stop-With 2 "The merge was cancelled and your branch is unchanged. These files need a manual merge before a new build is possible."
  }
  $merged = $true
  Say "Merged cleanly."
} else {
  Say "Nothing new from the author; rebuilding anyway because -Force was given."
}

if (-not ($SkipTests -and $SkipBuild)) {
  Say "Installing dependencies..."
  if ((Run "corepack" @("pnpm", "install", "--frozen-lockfile")) -ne 0) { Undo-Merge; Stop-With 3 "Installing dependencies failed. See $Log." }
}

if (-not $SkipTests) {
  Say "Running the test suite..."
  if ((Run "corepack" @("pnpm", "test")) -ne 0) { Undo-Merge; Stop-With 3 "The tests failed with the author's new code merged in. No installer was built." }
  Say "Tests passed."
}

if ($SkipBuild) {
  Say "Build skipped (-SkipBuild)."
  Say "=== finished ==="
  exit 0
}

$config = Get-Content -LiteralPath (Join-Path $RepoPath "src-tauri\tauri.conf.json") -Raw | ConvertFrom-Json
$before = [string]$config.build.beforeBuildCommand
if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) { $before = [regex]::Replace($before, '(?<!corepack )\bpnpm\b', 'corepack pnpm') }
# The author's updater signing key is not available to a fork, so updater artifacts are switched off.
$override = @{ build = @{ beforeBuildCommand = $before }; bundle = @{ createUpdaterArtifacts = $false } } | ConvertTo-Json -Depth 5
$overridePath = Join-Path $env:TEMP "infinitv-build-override.json"
[IO.File]::WriteAllText($overridePath, $override, (New-Object Text.UTF8Encoding($false)))

Say "Building the Windows installer (this takes several minutes)..."
$code = Run "corepack" @("pnpm", "exec", "tauri", "build", "--bundles", "nsis", "--config", $overridePath)
if ($code -ne 0) { Undo-Merge; Stop-With 4 "The build failed. No installer was produced. See $Log." }

$bundleDir = Join-Path $RepoPath "src-tauri\target\release\bundle\nsis"
$installer = Get-ChildItem -LiteralPath $bundleDir -Filter "*-setup.exe" -ErrorAction SilentlyContinue |
  Where-Object { $_.LastWriteTime -ge $started } | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $installer) { Undo-Merge; Stop-With 4 "The build reported success but no new installer was found in $bundleDir." }

$null = New-Item -ItemType Directory -Force -Path $OutputDir
$destination = Join-Path $OutputDir $installer.Name
Copy-Item -LiteralPath $installer.FullName -Destination $destination -Force
$hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash
Say "NEW INSTALLER: $destination"
Say ("   size {0:N1} MB, SHA-256 {1}" -f ($installer.Length / 1MB), $hash)

if ($Push) {
  $pushUrl = git -C $RepoPath remote get-url --push origin
  $upstreamUrl = git -C $RepoPath remote get-url upstream
  if (-not $pushUrl -or $pushUrl -eq $upstreamUrl) {
    Say "Not uploading: 'origin' does not point at your own fork."
  } elseif ((Run "git" @("-C", $RepoPath, "push", "origin", "HEAD")) -ne 0) {
    Say "The upload to your fork failed (you may need to sign in to GitHub). The installer is still ready."
  } else {
    Say "Uploaded $branch to your fork."
  }
}

Say "Close the app, then run the installer to update it."
Say "=== finished ==="
exit 0
