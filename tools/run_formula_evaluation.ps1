# 阶段 9/10 主评测驱动脚本。
#
# 顺序执行（不并行），避免 CPU 争用扭曲吞吐数据；每一步的 stdout/stderr 追加到
# `target/formula-eval/run.log`。使用已构建的 release 二进制，避免与其它 cargo
# 调用争抢构建锁。

$ErrorActionPreference = 'Stop'

$Crate = 'D:\100_Projects\110_Daily\SnapClip\crates\rapid-ocr-rs'
$Root = 'D:\100_Projects\110_Daily\SnapClip'
$Model = Join-Path $Root 'OCR-Model\Formula-Recognition-Models\onnx\pp_formulanet_plus_m.onnx'
$TestSet = Join-Path $Root 'Formula-TestSet'
$Eval = Join-Path $Crate 'target\release\formula_eval.exe'
$Bench = Join-Path $Crate 'target\release\formula_bench.exe'
$Out = Join-Path $Crate 'target\formula-eval'
$Log = Join-Path $Out 'run.log'

New-Item -ItemType Directory -Force -Path $Out | Out-Null

function Write-Log([string]$Message) {
    $line = "[{0}] {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $Message
    Write-Host $line
    Add-Content -Path $Log -Value $line
}

function Invoke-Eval {
    param([string]$Name, [string[]]$Arguments)
    Write-Log "START $Name"
    $started = Get-Date
    & $Eval @Arguments 2>&1 | ForEach-Object { Add-Content -Path $Log -Value $_ }
    if ($LASTEXITCODE -ne 0) {
        Write-Log "FAILED $Name (exit $LASTEXITCODE)"
        throw "$Name failed with exit code $LASTEXITCODE"
    }
    $elapsed = (Get-Date) - $started
    Write-Log ("DONE  {0} in {1:n1} min" -f $Name, $elapsed.TotalMinutes)
}

$common = @('--model', $Model, '--dataset-root', $TestSet)

# --- 1. im2latex 100 张固定 smoke -------------------------------------------------
Invoke-Eval 'im2latex-100' ($common + @(
    '--dataset', 'im2latex', '--split', 'test', '--limit', '100', '--batch-size', '8',
    '--manifest-output', (Join-Path $Out 'manifest-im2latex-100.json'),
    '--output', (Join-Path $Out 'im2latex-100.json')
))

# --- 2. PaddleX 示例集 501 张 val（smoke gold set） --------------------------------
Invoke-Eval 'val-501' ($common + @(
    '--dataset', 'latexocr', '--split', 'validate', '--batch-size', '8',
    '--manifest-output', (Join-Path $Out 'manifest-val-501.json'),
    '--output', (Join-Path $Out 'val-501.json')
))

# --- 3. Python/RapidDoc 参考（同一 manifest，同一批样本） -------------------------
Write-Log 'START python-reference-val-501'
$pyStart = Get-Date
python (Join-Path $Crate 'tools\formula_reference.py') `
    --model $Model --dataset-root $TestSet --dataset latexocr `
    --manifest (Join-Path $Out 'manifest-val-501.json') --batch-size 8 `
    --output (Join-Path $Out 'python-val-501.json') 2>&1 |
    ForEach-Object { Add-Content -Path $Log -Value $_ }
if ($LASTEXITCODE -ne 0) { throw "python reference failed with exit code $LASTEXITCODE" }
Write-Log ("DONE  python-reference-val-501 in {0:n1} min" -f ((Get-Date) - $pyStart).TotalMinutes)

# --- 4. Rust/Python 三方对比 ------------------------------------------------------
Invoke-Eval 'val-501-compared' ($common + @(
    '--dataset', 'latexocr', '--split', 'validate', '--batch-size', '8',
    '--expect-manifest', (Join-Path $Out 'manifest-val-501.json'),
    '--python-reference', (Join-Path $Out 'python-val-501.json'),
    '--output', (Join-Path $Out 'val-501-compared.json')
))

# --- 5. im2latex 完整测试集（10,355 条，含 71 条空标签） ---------------------------
Invoke-Eval 'im2latex-full' ($common + @(
    '--dataset', 'im2latex', '--split', 'test', '--batch-size', '8',
    '--manifest-output', (Join-Path $Out 'manifest-im2latex-full.json'),
    '--output', (Join-Path $Out 'im2latex-full.json')
))

# --- 6. UniMER 分组结果（HWE 单独报告，不与印刷体混合） ---------------------------
foreach ($subset in @('spe', 'cpe', 'sce', 'hwe')) {
    Invoke-Eval "unimer-$subset" ($common + @(
        '--dataset', 'unimer', '--subset', $subset, '--batch-size', '8',
        '--manifest-output', (Join-Path $Out "manifest-unimer-$subset.json"),
        '--output', (Join-Path $Out "unimer-$subset.json")
    ))
}

# --- 7. 性能与 provider（阶段 10） ------------------------------------------------
Write-Log 'START formula-bench-cpu'
$benchImages = Get-ChildItem -Path (Join-Path $TestSet 'ocr_rec_latexocr_dataset_example\images') -Filter 'val_*.png' |
    Select-Object -First 8 -ExpandProperty FullName
if ($benchImages.Count -eq 0) { throw 'no val_*.png found for formula_bench' }
& $Bench --model $Model `
    --image $benchImages `
    --rounds 5 --warmup 1 --batch-sizes 1,2,4,8 --provider cpu `
    --output (Join-Path $Out 'bench-cpu.json') 2>&1 |
    ForEach-Object { Add-Content -Path $Log -Value $_ }
if ($LASTEXITCODE -ne 0) { Write-Log "WARN formula-bench-cpu exited $LASTEXITCODE" }
else { Write-Log 'DONE  formula-bench-cpu' }

Write-Log 'ALL DONE'
