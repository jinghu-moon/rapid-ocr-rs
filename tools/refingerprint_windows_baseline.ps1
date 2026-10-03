# 终审：用**带 ORT 指纹**的报告替换 windows 基线里的 provider 报告。
#
# 为什么要重采：阶段 0/2 的 `bench-cpu.json` / `bench-direct_ml.json` / `bench-cuda.json`
# 既没有 `meta.ort_runtime`，也用的是旧的 `resolved` 字段名，因此：
#
# - 无法证明三份报告跑在同一份 ONNX Runtime 上（阶段 0 的记录还把它误标成 1.17）；
# - 无法被加固后的 `tools/check_provider_claims.ps1` 接受（它要求指纹与 `selected_ep`）。
#
# 本脚本按 provider 顺序重建 `bench_warm_e2e`（ort 的 provider feature 是**编译期**开关，
# 必须在同一个 target 目录里顺序重建），再跑同一组 12 图、同一组参数，写入：
#
# - `bench-cpu.json` / `bench-direct_ml.json` / `bench-cuda.json`（就地更新，字段升级）
# - `bench-cpu-ortfp.json`（与 `bench-cpu.json` 同一次构建、同一次运行产生的副本）
# - `evaluation-cpu-ortfp.json`（12 图质量评估 + ORT 指纹）
#
# **不覆盖** `tests/baseline/windows-baseline/evaluation-cpu.json`：那份是阶段 0 的证据，
# 保留它才能看出“早期文件早于指纹化”。
#
# 用法：pwsh -NoProfile -File tools/refingerprint_windows_baseline.ps1

param(
    [string]$Crate = (Split-Path -Parent $PSScriptRoot),
    [string]$Config,
    [string]$ImagesDir,
    [string]$GoldenManifest,
    [int]$Rounds = 3,
    [int]$WarmupRounds = 1,
    [int]$MaxSideLen = 2000,
    [int]$IntraThreads = 16
)

$ErrorActionPreference = 'Stop'
# 默认从 crate 位置推导仓库根（`<root>/crates/rapid-ocr-rs`），避免把开发机绝对路径写进工具。
$RepoRoot = Split-Path -Parent (Split-Path -Parent $Crate)
if (-not $Config) { $Config = Join-Path $RepoRoot 'OCR-Model\test-config-small.yaml' }
if (-not $ImagesDir) { $ImagesDir = Join-Path $RepoRoot 'OCR-test-image' }
if (-not $GoldenManifest) { $GoldenManifest = Join-Path $ImagesDir 'golden-manifest.json' }
$OutDir = Join-Path $Crate 'tests\baseline\windows-baseline'
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$WorkDir = Join-Path $Crate 'target\ortfp-configs'
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

foreach ($required in @($Config, $GoldenManifest)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { throw "required input not found: $required" }
}
if (-not (Test-Path -LiteralPath $ImagesDir -PathType Container)) { throw "images dir not found: $ImagesDir" }

# provider 变体的配置：把 `provider_preference: cpu` 换成目标 provider。
# 结构化变体在 serde_yaml 里是带标签的映射：`!direct_ml { device_id: 0 }`。
function New-ProviderConfig([string]$Provider) {
    $text = Get-Content -Raw -LiteralPath $Config
    $replacement = switch ($Provider) {
        'cpu' { 'provider_preference: cpu' }
        default { "provider_preference: !$Provider`n      device_id: 0" }
    }
    $text = $text -replace 'provider_preference: cpu', $replacement
    $path = Join-Path $WorkDir "$Provider.yaml"
    Set-Content -Path $path -Value $text -Encoding UTF8
    return $path
}

function Invoke-Bench([string]$Bin, [string]$ConfigPath, [string]$OutPath) {
    $stdout = Join-Path $WorkDir 'stdout.txt'
    $stderr = Join-Path $WorkDir 'stderr.txt'
    $process = Start-Process -FilePath $Bin -ArgumentList @(
        '--config', $ConfigPath, '--images-dir', $ImagesDir,
        '--warmup-rounds', "$WarmupRounds", '--rounds', "$Rounds",
        '--max-side-len', "$MaxSideLen", '--intra-threads', "$IntraThreads",
        '--output', $OutPath
    ) -PassThru -NoNewWindow -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $process.WaitForExit()
    return [pscustomobject]@{
        exit_code = $process.ExitCode
        stderr    = (Get-Content $stderr -Raw -ErrorAction SilentlyContinue)
    }
}

$variants = @(
    [pscustomobject]@{ provider = 'cpu'; features = @() },
    [pscustomobject]@{ provider = 'direct_ml'; features = @('--features', 'directml-provider') },
    [pscustomobject]@{ provider = 'cuda'; features = @('--features', 'cuda-provider') }
)

$rows = @()
foreach ($variant in $variants) {
    Write-Host "=== provider: $($variant.provider) ==="
    $buildArgs = @('build', '--release', '--bin', 'bench_warm_e2e') + $variant.features
    & cargo @buildArgs 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "build failed for provider $($variant.provider)" }
    $bin = Join-Path $Crate 'target\release\bench_warm_e2e.exe'

    $outPath = Join-Path $OutDir "bench-$($variant.provider).json"
    $run = Invoke-Bench $bin (New-ProviderConfig $variant.provider) $outPath
    if ($run.exit_code -ne 0) { throw "bench failed for provider $($variant.provider): $($run.stderr)" }

    $json = Get-Content $outPath -Raw -Encoding UTF8 | ConvertFrom-Json
    $selected = $json.meta.provider_resolution.recognizer.selected_ep
    $rows += [pscustomobject]@{
        provider      = $variant.provider
        selected_ep   = $selected
        fallback      = $json.meta.provider_resolution.recognizer.fallback_to_cpu
        p50_ms        = $json.stats.ocr_total_ms.p50
        p90_ms        = $json.stats.ocr_total_ms.p90
        regions_avg   = $json.stats.regions.avg
        conserved     = $json.timing_ledger.conservation.conserved
        ledger_resid  = $json.timing_ledger.conservation.residual_ms
        ort_api       = $json.meta.ort_runtime.api_version
        ort_sha256    = $json.meta.ort_runtime.runtime_module.sha256
    }
    Write-Host ("  selected_ep={0} fallback={1} p50={2:N1}ms p90={3:N1}ms ort={4}" -f `
            $selected, $json.meta.provider_resolution.recognizer.fallback_to_cpu,
        $json.stats.ocr_total_ms.p50, $json.stats.ocr_total_ms.p90, $json.meta.ort_runtime.api_version)

    # CPU 变体顺便产出一份带同样指纹的副本，供 `bench-cpu-ortfp.json` 使用。
    if ($variant.provider -eq 'cpu') {
        Copy-Item -LiteralPath $outPath -Destination (Join-Path $OutDir 'bench-cpu-ortfp.json') -Force
        Write-Host '  wrote bench-cpu-ortfp.json (same run as bench-cpu.json)'
    }
}

# 恢复默认 feature 的二进制，避免把带 GPU feature 的产物留在 target 里误导后续基线。
& cargo build --release --bins 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'failed to rebuild the default-feature binaries' }

Write-Host '=== 12-image evaluation (with ORT fingerprint) ==='
$evalOut = Join-Path $OutDir 'evaluation-cpu-ortfp.json'
$rapidocr = Join-Path $Crate 'target\release\rapidocr.exe'
$evalProcess = Start-Process -FilePath $rapidocr -ArgumentList @(
    'evaluate', '--manifest', $GoldenManifest, '--config', $Config, '--output', $evalOut
) -PassThru -NoNewWindow -RedirectStandardOutput (Join-Path $WorkDir 'eval-stdout.txt') `
    -RedirectStandardError (Join-Path $WorkDir 'eval-stderr.txt')
$evalProcess.WaitForExit()
if ($evalProcess.ExitCode -ne 0) {
    throw "rapidocr evaluate failed (exit $($evalProcess.ExitCode)); see $WorkDir\eval-stderr.txt"
}
$evaluation = Get-Content $evalOut -Raw -Encoding UTF8 | ConvertFrom-Json
Write-Host ("  mean CER={0} exact={1} cases={2} ort={3}" -f `
        $evaluation.mean_cer, $evaluation.exact_match_rate, $evaluation.cases.Count, $evaluation.ort_runtime.api_version)

Write-Host ''
Write-Host '=== summary ==='
$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
$cpuRow = $rows | Where-Object { $_.provider -eq 'cpu' }
Write-Host ("hard gates: mean_cer={0} regions_avg={1} (cpu p50={2:N1} ms)" -f `
        $evaluation.mean_cer, $cpuRow.regions_avg, $cpuRow.p50_ms)
Write-Host "wrote $OutDir"
