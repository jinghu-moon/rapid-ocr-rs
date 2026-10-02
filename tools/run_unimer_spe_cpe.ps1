# UniMER SPE + CPE 并行评测驱动（长序列子集）。
#
# SCE/HWE 已由 `tools/run_unimer_parallel.ps1` 完成。SPE 与 CPE 的标签序列远长于
# 另外两个子集（中文/长印刷公式），单个样本的图内自回归 `Loop` 步数决定了耗时，
# 因此这两个子集的墙钟时间由它们自己主导。
#
# 实测调优依据（在相同争用条件下对比）：
# - CPE batch=8 单图 4277 ms，batch=16 单图 5264 ms → **batch 8 更优**：
#   `Loop` 会一直运行到 batch 内所有行都产生 EOS，batch 越大越容易被最长样本拖住；
# - CPE `--threads 4` 单图 5995 ms，`--threads 14` 单图 3876 ms → **线程数有效**。
#
# 因此本驱动用 2 个进程 × 8 线程（≈ 本机 14 物理核），batch=8。
#
# **时序口径**：并行争用下测得的吞吐/P50/P95 只能作为下界；权威性能数据来自串行、
# 无争用的 `formula_bench`（`bench-cpu.json`）。精度指标与并发无关。
#
# 用法：pwsh -NoProfile -File tools/run_unimer_spe_cpe.ps1

$ErrorActionPreference = 'Stop'

$Crate = 'D:\100_Projects\110_Daily\SnapClip\crates\rapid-ocr-rs'
$Root = 'D:\100_Projects\110_Daily\SnapClip'
$Model = Join-Path $Root 'OCR-Model\Formula-Recognition-Models\onnx\pp_formulanet_plus_m.onnx'
$TestSet = Join-Path $Root 'Formula-TestSet'
$Eval = Join-Path $Crate 'target\release\formula_eval.exe'
$Out = Join-Path $Crate 'target\formula-eval'

New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Log = Join-Path $Out 'run-unimer-spe-cpe.log'

function Write-Log([string]$Message) {
    $line = "[{0}] {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $Message
    Write-Host $line
    Add-Content -Path $Log -Value $line
}

Write-Log 'START unimer spe+cpe (8 threads each, batch 8)'

$subsets = @('spe', 'cpe')
$processes = @{}
foreach ($subset in $subsets) {
    $arguments = @(
        '--model', $Model,
        '--dataset-root', $TestSet,
        '--dataset', 'unimer',
        '--subset', $subset,
        '--batch-size', '8',
        '--threads', '8',
        '--progress-every', '500',
        '--manifest-output', (Join-Path $Out "manifest-unimer-$subset.json"),
        '--output', (Join-Path $Out "unimer-$subset.json")
    )
    $processes[$subset] = Start-Process -FilePath $Eval -ArgumentList $arguments -PassThru `
        -RedirectStandardOutput (Join-Path $Out "unimer-$subset.stdout.log") `
        -RedirectStandardError (Join-Path $Out "unimer-$subset.stderr.log") -NoNewWindow
    Write-Log "launched unimer-$subset (pid $($processes[$subset].Id))"
}

$failed = @()
foreach ($subset in $subsets) {
    $process = $processes[$subset]
    $process.WaitForExit()
    if ($process.ExitCode -ne 0) {
        $failed += "$subset (exit $($process.ExitCode))"
        Write-Log "FAILED unimer-$subset (exit $($process.ExitCode))"
    }
    else {
        Write-Log "DONE  unimer-$subset"
    }
}

if ($failed.Count -gt 0) {
    Write-Log ("spe/cpe finished with failures: " + ($failed -join ', '))
    exit 1
}
Write-Log 'spe+cpe ALL DONE'
