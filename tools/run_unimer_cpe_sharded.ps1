# UniMER CPE 分片并行评测驱动。
#
# CPE 的平均标签长度为 658 字符（p95 1375、最大 5744），是 SPE/SCE/HWE 的 4-15 倍。
# 模型图内 `Loop` 每个 token 一步、解码器自注意力随序列长度二次增长，因此单进程
# 吞吐存在硬上限（实测约 0.25 img/s，与线程数关系不大：4 线程 5995 ms/图，
# 14 线程 3876 ms/图）。5,921 张图串行需要 6 小时以上。
#
# 结论：靠**分片并行**提高总吞吐，而不是靠单进程调参。
# `formula_eval --shard INDEX/COUNT` 的 manifest 始终覆盖完整集合：
#
# - 每个分片都写同一个 `manifest_sha256`，可以用 `--expect-manifest` 校验
#   样本集合与之前的完整集合完全一致；
# - `--merge-shards` 按 manifest 顺序重排记录，并用**同一套 Rust 汇总实现**
#   重新计算指标，因此合并结果与串行运行逐项等价
#   （已用 `--limit 8` + 4 分片对照串行验证：exact/normalized/CER/失败计数完全一致）。
#
# **时序口径**：分片并行下测得的吞吐/P50/P95 只能作为下界；权威性能数据来自串行、
# 无争用的 `formula_bench`（`bench-cpu.json`）。精度指标与并发无关。
#
# 用法：pwsh -NoProfile -File tools/run_unimer_cpe_sharded.ps1

$ErrorActionPreference = 'Stop'

$Crate = 'D:\100_Projects\110_Daily\SnapClip\crates\rapid-ocr-rs'
$Root = 'D:\100_Projects\110_Daily\SnapClip'
$Model = Join-Path $Root 'OCR-Model\Formula-Recognition-Models\onnx\pp_formulanet_plus_m.onnx'
$TestSet = Join-Path $Root 'Formula-TestSet'
$Eval = Join-Path $Crate 'target\release\formula_eval.exe'
$Out = Join-Path $Crate 'target\formula-eval'
$Manifest = Join-Path $Out 'manifest-unimer-cpe.json'
$Log = Join-Path $Out 'run-unimer-cpe-sharded.log'

# 5 个分片 × 3 线程 ≈ 15 线程，接近本机 14 物理核而不严重超订。
$ShardCount = 5
$Threads = 3

New-Item -ItemType Directory -Force -Path $Out | Out-Null

function Write-Log([string]$Message) {
    $line = "[{0}] {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $Message
    Write-Host $line
    Add-Content -Path $Log -Value $line
}

Write-Log "START unimer cpe sharded ($ShardCount shards x $Threads threads, batch 8)"

$processes = @{}
$shardFiles = @()
for ($i = 0; $i -lt $ShardCount; $i++) {
    $shardFile = Join-Path $Out "unimer-cpe.shard$i.json"
    $shardFiles += $shardFile
    $arguments = @(
        '--model', $Model,
        '--dataset-root', $TestSet,
        '--dataset', 'unimer',
        '--subset', 'cpe',
        '--batch-size', '8',
        '--threads', $Threads,
        '--progress-every', '500',
        '--shard', "$i/$ShardCount",
        '--expect-manifest', $Manifest,
        '--output', $shardFile
    )
    $processes[$i] = Start-Process -FilePath $Eval -ArgumentList $arguments -PassThru `
        -RedirectStandardOutput (Join-Path $Out "unimer-cpe.shard$i.stdout.log") `
        -RedirectStandardError (Join-Path $Out "unimer-cpe.shard$i.stderr.log") -NoNewWindow
    Write-Log "launched unimer-cpe shard $i/$ShardCount (pid $($processes[$i].Id))"
}

$failed = @()
for ($i = 0; $i -lt $ShardCount; $i++) {
    $process = $processes[$i]
    $process.WaitForExit()
    if ($process.ExitCode -ne 0) {
        $failed += "shard$i (exit $($process.ExitCode))"
        Write-Log "FAILED unimer-cpe shard $i (exit $($process.ExitCode))"
    }
    else {
        Write-Log "DONE  unimer-cpe shard $i"
    }
}

if ($failed.Count -gt 0) {
    Write-Log ("cpe sharded finished with failures: " + ($failed -join ', '))
    exit 1
}

Write-Log 'MERGING unimer-cpe shards'
$mergeArguments = @('--merge-shards') + $shardFiles + @('--output', (Join-Path $Out 'unimer-cpe.json'))
$mergeOutput = & $Eval @mergeArguments 2>&1
$mergeOutput | ForEach-Object { Add-Content -Path $Log -Value $_ }
if ($LASTEXITCODE -ne 0) {
    Write-Log "FAILED merge (exit $LASTEXITCODE)"
    exit 1
}
Write-Log 'cpe sharded ALL DONE'
