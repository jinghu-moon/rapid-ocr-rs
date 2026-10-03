# 阶段 5：线程模型测量矩阵。
#
# 目标：在改动线程策略**之前**先量化 ORT intra 与 Rayon 线程数对延迟/内存的影响，
# 避免“把线程数改成 CPU 核数”这种没有依据的调整。
#
# 每个组合都写成一个独立配置（det/cls/rec 三处运行时字段一起设置），
# 因此矩阵与 CLI 覆盖无关、可复现。
#
# 输出：`tests/baseline/windows-baseline/thread-matrix.json`
#
# 用法：pwsh -NoProfile -File tools/run_thread_matrix.ps1

param(
    [string]$Config = 'D:\100_Projects\110_Daily\SnapClip\OCR-Model\test-config-small.yaml',
    [string]$ImagesDir = 'D:\100_Projects\110_Daily\SnapClip\OCR-test-image',
    [int]$Rounds = 3,
    [int]$WarmupRounds = 1,
    [int]$MaxSideLen = 2000,
    # intra/rayon 组合；inter 固定为 1（ORT 的 inter 线程对本 workload 无益）。
    [array]$Combinations = @(
        @{ intra = 16; rayon = 16 },
        @{ intra = 16; rayon = 4 },
        @{ intra = 8; rayon = 4 },
        @{ intra = 8; rayon = 8 },
        @{ intra = 4; rayon = 8 }
    ),
    [switch]$PostRefactor
)

$ErrorActionPreference = 'Stop'
$Crate = Split-Path -Parent $PSScriptRoot
$OutDir = Join-Path $Crate 'tests\baseline\windows-baseline'
$WorkDir = Join-Path $Crate 'target\thread-configs'
New-Item -ItemType Directory -Force -Path $OutDir, $WorkDir | Out-Null
$Bin = Join-Path $Crate 'target\release\bench_warm_e2e.exe'

function New-ThreadConfig([int]$Intra, [int]$Rayon) {
    $text = Get-Content -Raw -LiteralPath $Config
    # 三个阶段统一设置：这是当前配置形状；阶段 5 之后改为单一 runtime 段。
    $text = [regex]::Replace($text, 'intra_threads: null', "intra_threads: $Intra")
    $text = [regex]::Replace($text, 'inter_threads: null', 'inter_threads: 1')
    $text = [regex]::Replace($text, 'auto_tune_threads: true', 'auto_tune_threads: false')
    $text = [regex]::Replace($text, 'rayon_threads: null', "rayon_threads: $Rayon")
    $path = Join-Path $WorkDir "intra$Intra-rayon$Rayon.yaml"
    Set-Content -Path $path -Value $text -Encoding UTF8
    return $path
}

$rows = @()
foreach ($combo in $Combinations) {
    $config = New-ThreadConfig $combo.intra $combo.rayon
    $out = Join-Path $WorkDir "thread-intra$($combo.intra)-rayon$($combo.rayon).json"
    & $Bin --config $config --images-dir $ImagesDir `
        --warmup-rounds $WarmupRounds --rounds $Rounds `
        --max-side-len $MaxSideLen --output $out 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "bench failed for intra=$($combo.intra) rayon=$($combo.rayon)" }
    $json = Get-Content $out -Raw -Encoding UTF8 | ConvertFrom-Json
    $row = [ordered]@{
        ort_intra              = $combo.intra
        rayon_threads          = $combo.rayon
        ort_inter              = 1
        p50_ms                 = $json.stats.ocr_total_ms.p50
        p90_ms                 = $json.stats.ocr_total_ms.p90
        mean_ms                = $json.stats.ocr_total_ms.avg
        regions_avg            = $json.stats.regions.avg
        init_ms                = $json.meta.init_ms
        peak_working_set_bytes = $json.memory.peak_working_set_bytes
    }
    $rows += $row
    Write-Host ("intra={0,2} rayon={1,2}  p50={2,8:N1} ms  p90={3,8:N1} ms  regions={4:N1}  peak={5,7:N1} MB" -f `
        $row.ort_intra, $row.rayon_threads, $row.p50_ms, $row.p90_ms, $row.regions_avg, ($row.peak_working_set_bytes / 1MB))
}

$payload = [ordered]@{
    collected_at_utc = (Get-Date).ToUniversalTime().ToString('o')
    stage            = if ($PostRefactor) { 'post-refactor' } else { 'pre-refactor' }
    conditions       = [ordered]@{
        images_dir    = $ImagesDir
        max_side_len  = $MaxSideLen
        rounds        = $Rounds
        warmup_rounds = $WarmupRounds
        logical_cpus  = [Environment]::ProcessorCount
        physical_cpus = (Get-CimInstance Win32_Processor | Select-Object -First 1).NumberOfCores
    }
    matrix           = $rows
}
$name = if ($PostRefactor) { 'thread-matrix-post.json' } else { 'thread-matrix.json' }
$payload | ConvertTo-Json -Depth 6 | Set-Content -Path (Join-Path $OutDir $name) -Encoding UTF8
Write-Host "wrote $OutDir\$name"
