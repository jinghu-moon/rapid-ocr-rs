# Launch headless Chrome and assert the page's upload flow end to end (M5 regression check).
#
#   pwsh -NoProfile -File tools\run-page-flow-cdp.ps1 -Base http://127.0.0.1:8760
#
# It is the only check that catches the M5 bug class: the pre-fix page accepted the 202, never
# polled `GET /api/jobs/{id}`, and left `.stage.scanning` on forever with an empty region list.
param(
  [string]$Base = 'http://127.0.0.1:8760',
  [int]$CdpPort = 9347,
  [string]$Image = '',
  [string]$OutDir = ''
)
$ErrorActionPreference = 'Stop'
if (-not $OutDir) { $OutDir = Join-Path $PSScriptRoot '..\target\flow-gate\cdp-flow' }
if (-not $Image) { $Image = 'D:\100_Projects\110_Daily\SnapClip\OCR-test-image\01基础多位置文本.png' }
$chrome = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
if (-not (Test-Path $chrome)) { throw "chrome not found: $chrome" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# A fresh profile per run: a leftover headless Chrome keeps the previous one locked.
$userData = Join-Path $OutDir ("chrome-profile-" + (Get-Date -Format 'HHmmss'))
New-Item -ItemType Directory -Force -Path $userData | Out-Null
$args = @(
  '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
  '--hide-scrollbars', '--disable-extensions',
  "--remote-debugging-port=$CdpPort", "--user-data-dir=$userData",
  'about:blank'
)
$proc = Start-Process -FilePath $chrome -ArgumentList $args -PassThru
$code = 1
try {
  Start-Sleep -Milliseconds 1800
  node (Join-Path $PSScriptRoot 'check-page-flow-cdp.mjs') --base $Base --cdp $CdpPort --image $Image --out $OutDir
  $code = $LASTEXITCODE
} finally {
  if ($proc -and -not $proc.HasExited) { $proc.Kill() }
  Get-Process chrome -ErrorAction SilentlyContinue |
      Where-Object { $_.Path -eq $chrome } | ForEach-Object {
          try { if ($_.CommandLine -like "*$userData*") { $_.Kill() } } catch {}
      }
  Start-Sleep -Milliseconds 400
}
exit $code
