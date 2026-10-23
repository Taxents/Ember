# Cuts a release: sets the app's version, commits, tags v<version> and pushes. GitHub Actions
# then builds ember.exe and publishes it, and installed apps update themselves.
#   tools\release.ps1 -Version 0.2.0
param([Parameter(Mandatory)][string]$Version)
$ErrorActionPreference = "Stop"
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "version must look like 1.2.3" }
if (git status --porcelain) { throw "commit or stash your changes first" }
$toml = "app/Cargo.toml"
(Get-Content $toml) -replace '^version = ".*"', "version = `"$Version`"" | Set-Content $toml -Encoding utf8NoBOM
cargo check -p music-app --quiet   # updates Cargo.lock
git add $toml Cargo.lock
git commit -m "Release $Version"
git tag "v$Version"
git push origin HEAD "v$Version"
Write-Host "Pushed v$Version. The build is at https://github.com/Taxents/Ember/actions"
