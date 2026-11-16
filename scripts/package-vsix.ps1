[CmdletBinding()]
param(
    [switch]$Offline
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$extension = Join-Path $root 'extension'
$target = Join-Path $root 'target'
$triple = 'x86_64-pc-windows-msvc'
$platform = 'win32-x64'
$bridge = 'slang-inline-bridge.exe'
$package = Get-Content -LiteralPath (Join-Path $extension 'package.json') -Raw | ConvertFrom-Json

Push-Location -LiteralPath $root
try {
    $cargoArgs = @('build', '--locked', '--release', '-p', 'slang-inline-bridge',
        '--target', $triple, '--target-dir', $target)
    if ($Offline) { $cargoArgs += '--offline' }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "Cargo failed with exit code $LASTEXITCODE." }
} finally {
    Pop-Location
}

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$outputDirectory = Join-Path $target 'vsix'
New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
$vsix = Join-Path $outputDirectory "$($package.name)-$($package.version)-$platform.vsix"
$pending = "$vsix.$([guid]::NewGuid().ToString('N')).tmp"

$id = [Security.SecurityElement]::Escape($package.name)
$version = [Security.SecurityElement]::Escape($package.version)
$publisher = [Security.SecurityElement]::Escape($package.publisher)
$displayName = [Security.SecurityElement]::Escape($package.displayName)
$description = [Security.SecurityElement]::Escape($package.description)
$engine = [Security.SecurityElement]::Escape($package.engines.vscode)

# VSIX uses an OPC archive with its metadata at the root and the extension
# under extension/. Keep the archive allowlist independent of the working tree.
$manifest = @"
<?xml version="1.0" encoding="utf-8"?>
<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011">
  <Metadata>
    <Identity Language="en-US" Id="$id" Version="$version" Publisher="$publisher" TargetPlatform="$platform" />
    <DisplayName>$displayName</DisplayName>
    <Description>$description</Description>
    <Properties>
      <Property Id="Microsoft.VisualStudio.Code.Engine" Value="$engine" />
      <Property Id="Microsoft.VisualStudio.Code.ExecutesCode" Value="true" />
    </Properties>
  </Metadata>
  <Installation>
    <InstallationTarget Id="Microsoft.VisualStudio.Code" />
  </Installation>
  <Dependencies />
  <Assets>
    <Asset Type="Microsoft.VisualStudio.Code.Manifest" Path="extension/package.json" Addressable="true" />
    <Asset Type="Microsoft.VisualStudio.Services.Content.Details" Path="extension/readme.md" Addressable="true" />
  </Assets>
</PackageManifest>
"@
$contentTypes = @'
<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="vsixmanifest" ContentType="text/xml" />
  <Default Extension="json" ContentType="application/json" />
  <Default Extension="js" ContentType="application/javascript" />
  <Default Extension="md" ContentType="text/markdown" />
  <Default Extension="exe" ContentType="application/octet-stream" />
</Types>
'@

function Write-ArchiveText($archive, [string]$name, [string]$content) {
    $stream = $archive.CreateEntry($name).Open()
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes($content)
        $stream.Write($bytes, 0, $bytes.Length)
    } finally {
        $stream.Dispose()
    }
}

try {
    $archive = [IO.Compression.ZipFile]::Open($pending, [IO.Compression.ZipArchiveMode]::Create)
    try {
        Write-ArchiveText $archive '[Content_Types].xml' $contentTypes
        Write-ArchiveText $archive 'extension.vsixmanifest' $manifest
        $files = @{
            'extension/package.json' = Join-Path $extension 'package.json'
            'extension/extension.js' = Join-Path $extension 'extension.js'
            'extension/language-features.js' = Join-Path $extension 'language-features.js'
            'extension/readme.md' = Join-Path $root 'README.md'
            "extension/bin/$bridge" = Join-Path $target "$triple/release/$bridge"
        }
        foreach ($name in ($files.Keys | Sort-Object)) {
            [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $files[$name], $name) | Out-Null
        }
    } finally {
        $archive.Dispose()
    }
    Move-Item -LiteralPath $pending -Destination $vsix -Force
    Write-Output $vsix
} finally {
    if (Test-Path -LiteralPath $pending) { Remove-Item -LiteralPath $pending -Force }
}
