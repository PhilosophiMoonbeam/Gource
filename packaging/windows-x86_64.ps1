# SPDX-FileCopyrightText: 2026 Gource contributors
# SPDX-License-Identifier: GPL-3.0-or-later

[CmdletBinding()]
param(
    [Parameter()]
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]*$')]
    [string] $Target = 'x86_64-pc-windows-msvc',
    [Parameter()]
    [string] $OutputDirectory = 'dist'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$Helper = Join-Path $ProjectRoot 'packaging\archive.py'
$Python = Get-Command py -ErrorAction SilentlyContinue
$PythonUsesLauncher = $null -ne $Python
if (-not $PythonUsesLauncher) {
    $Python = Get-Command python -ErrorAction Stop
}

function Invoke-Helper {
    param(
        [Parameter(Mandatory = $false, Position = 0)]
        [string[]] $Arguments = @()
    )

    if ($script:PythonUsesLauncher) {
        & $script:Python.Source -3 $script:Helper @Arguments
    }
    else {
        & $script:Python.Source $script:Helper @Arguments
    }
    if ($LASTEXITCODE -ne 0) {
        throw "packaging helper failed with exit code $LASTEXITCODE"
    }
}

Push-Location $ProjectRoot
$tempRoot = $null
try {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargo is required'
    }
    if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
        throw "rustup is required to install target $Target"
    }

    & rustup target add $Target
    if ($LASTEXITCODE -ne 0) {
        throw "rustup could not install target $Target"
    }
    & cargo build --locked --release --package gource-app --target $Target
    if ($LASTEXITCODE -ne 0) {
        throw 'cargo build failed'
    }

    $Version = (Invoke-Helper @('--print-version')).Trim()
    if ([string]::IsNullOrWhiteSpace($Version)) {
        throw 'workspace version is empty'
    }

    if ([IO.Path]::IsPathRooted($OutputDirectory)) {
        $OutputRoot = $OutputDirectory
    }
    else {
        $OutputRoot = Join-Path $ProjectRoot $OutputDirectory
    }
    New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null

    $tempRoot = Join-Path ([IO.Path]::GetTempPath()) ('gource-package-' + [Guid]::NewGuid().ToString('N'))
    $PackageRoot = Join-Path $tempRoot "gource-$Version-$Target"
    New-Item -ItemType Directory -Force -Path @(
        (Join-Path $PackageRoot 'bin'),
        (Join-Path $PackageRoot 'assets\fonts'),
        (Join-Path $PackageRoot 'share\man\man1'),
        (Join-Path $PackageRoot 'examples\fixtures')
    ) | Out-Null

    $Binary = Join-Path $ProjectRoot "target\$Target\release\gource-app.exe"
    $Fixture = Join-Path $ProjectRoot 'tests\fixtures\single-event.log'
    $RequiredInputs = @(
        $Binary,
        (Join-Path $ProjectRoot 'COPYING'),
        (Join-Path $ProjectRoot 'THIRD_PARTY_NOTICES'),
        (Join-Path $ProjectRoot 'README.md'),
        (Join-Path $ProjectRoot 'data\gource.style'),
        (Join-Path $ProjectRoot 'data\fonts\README'),
        (Join-Path $ProjectRoot 'data\gource.1'),
        $Fixture
    )
    foreach ($Required in $RequiredInputs) {
        if (-not (Test-Path -LiteralPath $Required -PathType Leaf)) {
            throw "required input is missing: $Required"
        }
    }

    Copy-Item -LiteralPath $Binary -Destination (Join-Path $PackageRoot 'bin\gource-app.exe')
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'COPYING') -Destination (Join-Path $PackageRoot 'COPYING')
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'THIRD_PARTY_NOTICES') -Destination (Join-Path $PackageRoot 'THIRD_PARTY_NOTICES')
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'README.md') -Destination (Join-Path $PackageRoot 'README.md')
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'data\gource.style') -Destination (Join-Path $PackageRoot 'assets\gource.style')
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'data\fonts\README') -Destination (Join-Path $PackageRoot 'assets\fonts\README')
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'data\gource.1') -Destination (Join-Path $PackageRoot 'share\man\man1\gource.1')
    Copy-Item -LiteralPath $Fixture -Destination (Join-Path $PackageRoot 'examples\fixtures\single-event.log')

    $HelpOutput = Join-Path $tempRoot 'help.txt'
    & (Join-Path $PackageRoot 'bin\gource-app.exe') --help *> $HelpOutput
    if ($LASTEXITCODE -ne 0) {
        throw '--help smoke failed'
    }
    if ((Get-Item -LiteralPath $HelpOutput).Length -eq 0) {
        throw '--help smoke produced no output'
    }

    $DiagnoseOutput = Join-Path $tempRoot 'diagnose.json'
    $DiagnoseError = Join-Path $tempRoot 'diagnose.stderr'
    & (Join-Path $PackageRoot 'bin\gource-app.exe') diagnose --input (Join-Path $PackageRoot 'examples\fixtures\single-event.log') --threads 1 1> $DiagnoseOutput 2> $DiagnoseError
    if ($LASTEXITCODE -ne 0) {
        throw "diagnose smoke failed: $((Get-Content -LiteralPath $DiagnoseError -Raw).Trim())"
    }
    Invoke-Helper @('--verify-diagnose', $DiagnoseOutput)

    $ArchiveName = "gource-$Version-$Target.zip"
    $ArchivePath = Join-Path $OutputRoot $ArchiveName
    Invoke-Helper @('--archive', '--format', 'zip', '--source', $PackageRoot, '--output', $ArchivePath)
    Invoke-Helper @('--verify-archive', $ArchivePath, '--format', 'zip')
    $Checksum = (Invoke-Helper @('--sha256', $ArchivePath)).Trim()
    if ($Checksum -notmatch '^[0-9a-f]{64}$') {
        throw 'checksum helper returned an invalid SHA-256 digest'
    }
    $ChecksumPath = Join-Path $OutputRoot "$ArchiveName.sha256"
    $Utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    [IO.File]::WriteAllText($ChecksumPath, "$Checksum  $ArchiveName`n", $Utf8NoBom)
    Write-Output "packaging: wrote $ArchivePath and $ChecksumPath"
}
finally {
    if ($null -ne $tempRoot -and (Test-Path -LiteralPath $tempRoot)) {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force
    }
    Pop-Location
}
