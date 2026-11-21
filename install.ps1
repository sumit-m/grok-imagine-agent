<#
Builds imagine-agent, installs it next to imagine.exe, and registers it as a
Buzz custom harness. Re-run after `just reset` (which wipes custom_harnesses).

  .\install.ps1                                              # dev Buzz (xyz.block.buzz.app.dev)
  .\install.ps1 -ImagineSource ..\grok-imagine-cli\dist
  .\install.ps1 -App prod                                    # installed Buzz (xyz.block.buzz.app)

Run it from your own terminal: an app that redirects its %APPDATA% writes
would leave the harness file where Buzz never looks.
#>
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\imagine'),
    [string]$ImagineSource,
    [ValidateSet('dev', 'prod')]
    [string]$App = 'dev'
)
$ErrorActionPreference = 'Stop'

cargo build --release --manifest-path (Join-Path $PSScriptRoot 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

New-Item -ItemType Directory -Force $InstallDir | Out-Null
$installed = Join-Path $InstallDir 'imagine-agent.exe'
foreach ($old in Get-ChildItem $InstallDir -Filter 'imagine-agent.*.old') {
    try { Remove-Item $old.FullName -Force -ErrorAction Stop } catch {}
}
$built = Join-Path $PSScriptRoot 'target\release\imagine-agent.exe'
try {
    Copy-Item $built $installed -Force -ErrorAction Stop
} catch {
    # A running agent locks the exe, but Windows still allows renaming it.
    Move-Item $installed (Join-Path $InstallDir "imagine-agent.$(Get-Date -Format yyyyMMddHHmmss).old")
    Copy-Item $built $installed
}

if ($ImagineSource) {
    foreach ($file in 'imagine.exe', 'config.ini', 'presets.ini', 'models.ini') {
        $source = Join-Path $ImagineSource $file
        if (Test-Path $source) { Copy-Item $source $InstallDir -Force }
    }
}
if (-not (Test-Path (Join-Path $InstallDir 'imagine.exe'))) {
    Write-Warning "imagine.exe is not in $InstallDir; pass -ImagineSource or copy it there with config.ini, presets.ini and models.ini."
}

$appId = if ($App -eq 'dev') { 'xyz.block.buzz.app.dev' } else { 'xyz.block.buzz.app' }
$harnessDir = Join-Path $env:APPDATA "$appId\custom_harnesses"
New-Item -ItemType Directory -Force $harnessDir | Out-Null
# Buzz's message-box panel. Values travel by control id; imagine-agent maps
# them to imagine flags. `--refresh-panel` adds the Image / Video toggle and
# every menu from models.ini, and the Preset menu from presets.ini, here and
# each time Buzz reads the panel.
$controls = @(
    [ordered]@{
        id = 'repo'; kind = 'repoFolder'; label = 'Save to repository'; icon = 'git-branch'; defaultFolder = 'assets/'; options = @()
    }
)

$harnessFile = Join-Path $harnessDir 'imagine.json'
$definition = [ordered]@{
    id          = 'imagine'
    label       = 'Imagine'
    command     = $installed
    args        = @()
    # imagine-agent also rebuilds the panel here when it starts.
    env         = @{ IMAGINE_HARNESS_FILE = $harnessFile }
    installHint = 'Run install.ps1 from the grok-imagine-agent repository.'
    composer    = [ordered]@{
        modes    = @([ordered]@{ id = 'image'; label = 'Image' })
        controls = $controls
        refresh  = @('--refresh-panel')
    }
}
$json = $definition | ConvertTo-Json -Depth 10
[System.IO.File]::WriteAllText($harnessFile, $json, (New-Object System.Text.UTF8Encoding $false))

$env:IMAGINE_HARNESS_FILE = $harnessFile
& $installed --refresh-panel
if ($LASTEXITCODE -ne 0) {
    Write-Warning 'The panel could not be read from models.ini and presets.ini.'
}

Write-Host "Installed $installed"
Write-Host "Registered $harnessFile"
