# 阶段 0：CPU / DirectML / CUDA 可用性与 fallback 语义基线。
#
# 对每个 provider 分别：
#
# 1. 用对应 feature 重新构建 `bench_warm_e2e`（ort 的 provider feature 是编译期开关，
#    必须在同一个 target 目录里顺序重建）；
# 2. 用该 provider 的配置跑 12 图 warm 基准，记录解析到的 provider 与是否 fallback；
# 3. 再用 `fail_if_provider_unavailable: true` 跑一次严格模式，记录真实错误文本。
#
# 结果写入 `tests/baseline/windows-baseline/provider-matrix.json`。
# 不可用 provider 必须记录真实错误，**不得**把 CPU fallback 当加速成功。

param(
    [string]$Config = 'D:\100_Projects\110_Daily\SnapClip\OCR-Model\test-config-small.yaml',
    [string]$ImagesDir = 'D:\100_Projects\110_Daily\SnapClip\OCR-test-image',
    [int]$Rounds = 3,
    [int]$WarmupRounds = 1,
    [int]$MaxSideLen = 2000,
    [int]$IntraThreads = 16
)

$ErrorActionPreference = 'Stop'
$Crate = Split-Path -Parent $PSScriptRoot
$OutDir = Join-Path $Crate 'tests\baseline\windows-baseline'
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$WorkDir = Join-Path $Crate 'target\provider-configs'
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

function New-ProviderConfig([string]$Provider, [bool]$Strict) {
    $text = Get-Content -Raw -LiteralPath $Config
    # 结构化 provider 变体在 serde_yaml 里是带标签的映射：`!direct_ml { device_id: 0 }`。
    # 阶段 2 会把它改成扁平的 `provider` + `device_id`，届时这里同步更新。
    switch ($Provider) {
        'cpu' { $replacement = 'provider_preference: cpu' }
        default { $replacement = "provider_preference: !$Provider`n      device_id: 0" }
    }
    $text = $text -replace 'provider_preference: cpu', $replacement
    if ($Strict) {
        $text = $text -replace 'fail_if_provider_unavailable: false', 'fail_if_provider_unavailable: true'
    }
    $path = Join-Path $WorkDir "$Provider$(if ($Strict) { '-strict' } else { '' }).yaml"
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

$results = @()
foreach ($variant in $variants) {
    Write-Host "=== provider: $($variant.provider) ==="
    $buildArgs = @('build', '--release', '--bin', 'bench_warm_e2e') + $variant.features
    & cargo @buildArgs 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "build failed for provider $($variant.provider)" }
    $bin = Join-Path $Crate 'target\release\bench_warm_e2e.exe'

    $fallbackOut = Join-Path $OutDir "bench-$($variant.provider).json"
    $fallback = Invoke-Bench $bin (New-ProviderConfig $variant.provider $false) $fallbackOut
    $strict = Invoke-Bench $bin (New-ProviderConfig $variant.provider $true) (Join-Path $WorkDir "$($variant.provider)-strict.json")

    $entry = [ordered]@{ provider = $variant.provider }
    if ($fallback.exit_code -eq 0 -and (Test-Path $fallbackOut)) {
        $json = Get-Content $fallbackOut -Raw -Encoding UTF8 | ConvertFrom-Json
        $entry['fallback_run'] = [ordered]@{
            exit_code              = 0
            provider_resolution    = $json.meta.provider_resolution
            init_ms                = $json.meta.init_ms
            ocr_total_p50_ms       = $json.stats.ocr_total_ms.p50
            ocr_total_p90_ms       = $json.stats.ocr_total_ms.p90
            regions_avg            = $json.stats.regions.avg
            peak_working_set_bytes = $json.memory.peak_working_set_bytes
        }
        $resolution = $json.meta.provider_resolution.recognizer
        # 字段名是 `selected_ep`（“交给 ORT 的 EP 链头部”，不是逐节点执行证据）；
        # 旧的 `resolved` 已删除，读它会打印空值。
        Write-Host ("  fallback run: selected_ep={0} fallback={1} p50={2:N1}ms" -f `
            $resolution.selected_ep, $resolution.fallback_to_cpu, $json.stats.ocr_total_ms.p50)
    }
    else {
        $entry['fallback_run'] = [ordered]@{ exit_code = $fallback.exit_code; stderr = $fallback.stderr }
        Write-Host "  fallback run failed (exit $($fallback.exit_code)): $($fallback.stderr)"
    }
    $entry['strict_run'] = [ordered]@{ exit_code = $strict.exit_code; stderr = $strict.stderr }
    Write-Host "  strict run: exit=$($strict.exit_code) stderr=$($strict.stderr)"
    $results += $entry
}

# 恢复默认 feature 的二进制，避免把带 GPU feature 的产物留在 target 里误导后续基线。
& cargo build --release --bins 2>&1 | Out-Null

[ordered]@{
    collected_at_utc = (Get-Date).ToUniversalTime().ToString('o')
    conditions       = [ordered]@{
        max_side_len  = $MaxSideLen
        intra_threads = $IntraThreads
        rounds        = $Rounds
        warmup_rounds = $WarmupRounds
        images_dir    = $ImagesDir
    }
    providers        = $results
} | ConvertTo-Json -Depth 8 | Set-Content -Path (Join-Path $OutDir 'provider-matrix.json') -Encoding UTF8
Write-Host "wrote $OutDir\provider-matrix.json"
