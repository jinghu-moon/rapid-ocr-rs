# UniMER 四个子集并行评测驱动（**可选加速路径**）。
#
# 默认记录在案的结果来自串行驱动 `tools/run_formula_evaluation.ps1`：串行运行时
# 时序数据无争用、可直接互相比较。本脚本只在需要压缩墙钟时间时使用。
#
# 每个子集单独运行、单独输出报告与 manifest（HWE 不与印刷体混合汇总）。
# 4 个子集并行、每个进程 `--threads 4`（合计约 16 线程 ≈ 本机 14 物理核）。
#
# **时序口径说明**：并行运行时的吞吐/P50/P95 是在 4 路争用下测得的，只能作为
# 下界，不能与串行结果直接比较；权威性能数据来自串行、无争用的
# `formula_bench`（`bench-cpu.json`）。**精度指标（exact/normalized/CER/
# 失败分类）与并发无关**，仍然是完整数据集的精确结果。
#
# 用法：pwsh -NoProfile -File tools/run_unimer_parallel.ps1

$ErrorActionPreference = 'Stop'

$Crate = 'D:\100_Projects\110_Daily\SnapClip\crates\rapid-ocr-rs'
$Root = 'D:\100_Projects\110_Daily\SnapClip'
$Model = Join-Path $Root 'OCR-Model\Formula-Recognition-Models\onnx\pp_formulanet_plus_m.onnx'
$TestSet = Join-Path $Root 'Formula-TestSet'
$Eval = Join-Path $Crate 'target\release\formula_eval.exe'
$Out = Join-Path $Crate 'target\formula-eval'

New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Log = Join-Path $Out 'run-unimer-parallel.log'

function Write-Log([string]$Message) {
    $line = "[{0}] {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $Message
    Write-Host $line
    Add-Content -Path $Log -Value $line
}

Write-Log 'START unimer parallel (spe/cpe/sce/hwe, 4 threads each)'

$subsets = @('spe', 'cpe', 'sce', 'hwe')
$processes = @{}
foreach ($subset in $subsets) {
    $stdout = Join-Path $Out "unimer-$subset.stdout.log"
    $stderr = Join-Path $Out "unimer-$subset.stderr.log"
    $arguments = @(
        '--model', $Model,
        '--dataset-root', $TestSet,
        '--dataset', 'unimer',
        '--subset', $subset,
        '--batch-size', '8',
        '--threads', '4',
        '--progress-every', '500',
        '--manifest-output', (Join-Path $Out "manifest-unimer-$subset.json"),
        '--output', (Join-Path $Out "unimer-$subset.json")
    )
    $processes[$subset] = Start-Process -FilePath $Eval -ArgumentList $arguments -PassThru `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr -NoNewWindow
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
    Write-Log ("unimer parallel finished with failures: " + ($failed -join ', '))
    exit 1
}
Write-Log 'unimer parallel ALL DONE'
