# 阶段 0：Windows x64 基线采集。
#
# 采集内容（全部写入 `tests/baseline/windows-baseline/`，随仓库提交）：
#
# - `environment.json`：机器、工具链、ORT 运行库、target 冻结信息；
# - `build-stats.json`：release 二进制体积、target 体积、crate 自身重编译耗时；
# - `bench-cpu-2000.json` / `bench-cpu-1280.json`：12 图 warm 基准（含峰值工作集）；
# - `evaluation-cpu.json`：12 图 CER / 精确匹配（golden manifest）。
#
# 峰值工作集由 `Process.PeakWorkingSet64` 在进程退出后读取，口径与
# `runtime::memory` 的 `GetProcessMemoryInfo.PeakWorkingSetSize` 一致（都是进程峰值）。
#
# 用法：pwsh -NoProfile -File tools/run_windows_baseline.ps1

param(
    [string]$Config = 'D:\100_Projects\110_Daily\SnapClip\OCR-Model\test-config-small.yaml',
    [string]$ImagesDir = 'D:\100_Projects\110_Daily\SnapClip\OCR-test-image',
    [string]$GoldenManifest = 'D:\100_Projects\110_Daily\SnapClip\OCR-test-image\golden-manifest.json',
    [int]$Rounds = 3,
    [int]$WarmupRounds = 1,
    [int]$IntraThreads = 16
)

$ErrorActionPreference = 'Stop'
$Crate = Split-Path -Parent $PSScriptRoot
$OutDir = Join-Path $Crate 'tests\baseline\windows-baseline'
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

function Measure-Process([string]$Exe, [string[]]$Arguments) {
    $process = Start-Process -FilePath $Exe -ArgumentList $Arguments -PassThru -NoNewWindow
    $process.WaitForExit()
    [pscustomobject]@{
        exit_code = $process.ExitCode
    }
}

Write-Host '=== environment ==='
$os = Get-CimInstance Win32_OperatingSystem
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$gpus = Get-CimInstance Win32_VideoController
$rustc = (& rustc --version) -join ' '
$cargo = (& cargo --version) -join ' '
$target = (& rustc -vV | Select-String '^host:').ToString().Split(':')[1].Trim()
$environment = [ordered]@{
    collected_at_utc = (Get-Date).ToUniversalTime().ToString('o')
    target           = $target
    frozen_targets   = @('x86_64-pc-windows-msvc')
    non_goals        = @('aarch64-pc-windows-msvc', 'x86_64-pc-windows-gnu', 'Wine', 'WSL', 'Linux', 'macOS')
    os               = [ordered]@{ caption = $os.Caption; version = $os.Version; build = $os.BuildNumber; arch = $os.OSArchitecture }
    cpu              = [ordered]@{ name = $cpu.Name; physical_cores = $cpu.NumberOfCores; logical_cores = $cpu.NumberOfLogicalProcessors; max_mhz = $cpu.MaxClockSpeed }
    gpus             = @($gpus | ForEach-Object { [ordered]@{ name = $_.Name; driver = $_.DriverVersion; driver_date = $_.DriverDate } })
    rustc            = $rustc
    cargo            = $cargo
    ort_crate        = '=2.0.0-rc.13'
}
$environment | ConvertTo-Json -Depth 6 | Set-Content -Path (Join-Path $OutDir 'environment.json') -Encoding UTF8

Write-Host '=== build stats ==='
$binDir = Join-Path $Crate 'target\release'
$binSizes = [ordered]@{}
foreach ($name in 'rapidocr', 'bench_warm_e2e', 'formula_eval', 'formula_bench') {
    $path = Join-Path $binDir "$name.exe"
    if (Test-Path $path) { $binSizes[$name] = (Get-Item $path).Length }
}
$targetSize = (Get-ChildItem (Join-Path $Crate 'target') -Recurse -File -ErrorAction SilentlyContinue |
    Measure-Object -Property Length -Sum).Sum
# crate 自身重编译耗时（依赖已构建）：provider 与发布基线使用同一口径。
& cargo clean -p rapid-ocr-rs --release 2>&1 | Out-Null
$sw = [System.Diagnostics.Stopwatch]::StartNew()
& cargo build --release --bins 2>&1 | Out-Null
$sw.Stop()
$buildStats = [ordered]@{
    collected_at_utc       = (Get-Date).ToUniversalTime().ToString('o')
    profile                = 'release'
    crate_rebuild_ms       = $sw.ElapsedMilliseconds
    crate_rebuild_scope    = 'cargo clean -p rapid-ocr-rs --release; cargo build --release --bins'
    binary_bytes           = $binSizes
    target_dir_bytes       = $targetSize
}
$buildStats | ConvertTo-Json -Depth 4 | Set-Content -Path (Join-Path $OutDir 'build-stats.json') -Encoding UTF8

Write-Host '=== 12-image bench (max_side_len 2000 / 1280) ==='
$benchRows = @()
foreach ($side in 2000, 1280) {
    $out = Join-Path $OutDir "bench-cpu-$side.json"
    $args = @(
        '--config', $Config, '--images-dir', $ImagesDir,
        '--warmup-rounds', "$WarmupRounds", '--rounds', "$Rounds",
        '--max-side-len', "$side", '--intra-threads', "$IntraThreads",
        '--output', $out
    )
    $app = Measure-Process (Join-Path $binDir 'bench_warm_e2e.exe') $args
    if ($app.exit_code -ne 0) { throw "bench_warm_e2e failed for max_side_len=$side (exit $($app.exit_code))" }
    $json = Get-Content $out -Raw -Encoding UTF8 | ConvertFrom-Json
    $benchRows += [pscustomobject]@{
        max_side_len           = $side
        init_ms                = $json.meta.init_ms
        ocr_total_p50_ms       = $json.stats.ocr_total_ms.p50
        ocr_total_p90_ms       = $json.stats.ocr_total_ms.p90
        ocr_total_avg_ms       = $json.stats.ocr_total_ms.avg
        wall_p50_ms            = $json.stats.wall_ms.p50
        regions_avg            = $json.stats.regions.avg
        peak_working_set_bytes = $json.memory.peak_working_set_bytes
    }
    Write-Host ("  max_side_len={0}: init={1:N1}ms p50={2:N1}ms p90={3:N1}ms regions={4:N1} peak={5:N1}MB" -f `
        $side, $json.meta.init_ms, $json.stats.ocr_total_ms.p50, $json.stats.ocr_total_ms.p90,
        $json.stats.regions.avg, ($json.memory.peak_working_set_bytes / 1MB))
}

Write-Host '=== 12-image evaluation (CER) ==='
$evalOut = Join-Path $OutDir 'evaluation-cpu.json'
$evalApp = Measure-Process (Join-Path $binDir 'rapidocr.exe') @(
    'evaluate', '--manifest', $GoldenManifest, '--config', $Config, '--output', $evalOut
)
if ($evalApp.exit_code -ne 0) { throw "rapidocr evaluate failed (exit $($evalApp.exit_code))" }
$evaluation = Get-Content $evalOut -Raw -Encoding UTF8 | ConvertFrom-Json
Write-Host ("  mean CER={0:N4} exact={1:N4} peak={2:N1}MB" -f `
    $evaluation.mean_cer, $evaluation.exact_match_rate, ($evaluation.peak_working_set_bytes / 1MB))

$summary = [ordered]@{
    environment   = $environment
    build_stats   = $buildStats
    bench         = $benchRows
    evaluation    = [ordered]@{
        mean_cer               = $evaluation.mean_cer
        exact_match_rate       = $evaluation.exact_match_rate
        cases                  = $evaluation.cases.Count
        peak_working_set_bytes = $evaluation.peak_working_set_bytes
        memory_source          = $evaluation.memory_source
        manifest               = $GoldenManifest
    }
}
$summary | ConvertTo-Json -Depth 8 | Set-Content -Path (Join-Path $OutDir 'summary.json') -Encoding UTF8
Write-Host "wrote $OutDir"
