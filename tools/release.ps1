# Cuts a release: sets the app's version, commits, tags v<version> and pushes. GitHub Actions
# then builds ember.exe and publishes EmberSetup.exe (what people download) and ember.update (the
# same exe for the in-app updater) with its signature (ember.update.minisig); installed apps show
# "Ember <version> is available" and install it when the user chooses to.
#   tools\release.ps1 -Version 0.2.0
#
# Signing a build by hand (say, to re-sign a release asset) with the offline key:
#   tools\release.ps1 -Sign path\to\ember.update -Version 0.2.0 -Key path\to\ember.key
# writes path\to\ember.update.minisig. The trusted comment names the version; the app refuses a
# signature made for another one.
#
# One-time setup of the signing key (https://jedisct1.github.io/minisign/):
#   1. minisign -G -W -p ember.pub -s ember.key
#      (-W: no password, so CI can use it unattended. Keep ember.key offline, e.g. in a password
#      manager, and never commit it.)
#   2. Put the second line of ember.pub (it starts with "RW") in PUBLIC_KEY in app/src/update.rs.
#      Until then the app finds updates but refuses to install them.
#   3. On GitHub: Settings > Secrets and variables > Actions > New repository secret, named
#      MINISIGN_SECRET_KEY, holding the whole of ember.key. The release workflow signs with it.
param(
    [Parameter(Mandatory)][string]$Version,
    [string]$Sign,
    [string]$Key
)
$ErrorActionPreference = "Stop"
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "version must look like 1.2.3" }

if ($Sign) {
    if (-not $Key) { throw "-Sign needs -Key (the minisign secret key file)" }
    if (-not (Get-Command minisign -ErrorAction SilentlyContinue)) { throw "minisign isn't on PATH" }
    minisign -S -s $Key -m $Sign -x "$Sign.minisig" -t "version:$Version file:ember.update"
    if ($LASTEXITCODE -ne 0) { throw "minisign failed ($LASTEXITCODE)" }
    Write-Host "Wrote $Sign.minisig"
    return
}

# Releases come from main as it is on GitHub, nothing else.
$branch = git rev-parse --abbrev-ref HEAD
if ($branch -ne "main") { throw "releases are cut from main (this is $branch)" }
if (git status --porcelain) { throw "commit or stash your changes first" }
git fetch origin main --quiet
if ((git rev-parse HEAD) -ne (git rev-parse origin/main)) { throw "main isn't the same as origin/main; pull or push first" }

$toml = "app/Cargo.toml"
# UTF-8 without a BOM, in Windows PowerShell 5.1 too (it has no -Encoding utf8NoBOM).
$lines = (Get-Content $toml) -replace '^version = ".*"', "version = `"$Version`""
[IO.File]::WriteAllLines((Resolve-Path $toml), $lines, [Text.UTF8Encoding]::new($false))
cargo check -p music-app --quiet   # updates Cargo.lock
git add $toml Cargo.lock
git commit -m "Release $Version"
git tag "v$Version"
git push origin HEAD "v$Version"
Write-Host "Pushed v$Version. The build is at https://github.com/Taxents/Ember/actions"
