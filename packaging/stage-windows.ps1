# SPDX-FileCopyrightText: 2026 Matt Curfman
# SPDX-License-Identifier: Apache-2.0

param([Parameter(Mandatory=$true)][string]$Target,
      [Parameter(Mandatory=$true)][string]$Destination)
$ErrorActionPreference = 'Stop'
if (!(Test-Path README.md)) { throw 'README.md is required release input' }
Remove-Item -Recurse -Force $Destination -ErrorAction SilentlyContinue
New-Item -ItemType Directory "$Destination/bin", "$Destination/completions" | Out-Null
Copy-Item "target/$Target/release/vps.exe" "$Destination/bin/vps.exe"
Copy-Item README.md, vps.example.toml, target/THIRD_PARTY_LICENSES.txt $Destination
if (Test-Path LICENSE) { Copy-Item LICENSE $Destination }
& "$Destination/bin/vps.exe" completion powershell | Set-Content -Encoding utf8 "$Destination/completions/_vps.ps1"
