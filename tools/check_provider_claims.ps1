# Provider 加速声明的证据检查器。
#
# 规则（阶段 0/2/5 冻结，阶段 6 复核）：
#   **加速必须由测量证明。** 一份 bench_warm_e2e 报告只有在同时满足以下两条时，
#   才允许声称“非 CPU 加速”：
#     1. 至少一个阶段的 `resolved` provider 不是 CPU，且 `fallback_to_cpu = false`；
#     2. 该报告的 p50 相对 CPU 参考报告至少好 10%。
#   任何“声称 GPU / 实际与 CPU 相差 ±10% 以内”的报告都是**没有测量支撑的加速声明**，
#   必须判 FAIL 并以非零退出码结束，而不是被当成“大概更快”。
#
#   同时：`fallback_to_cpu: true` 本身就是 FAIL —— 发生了 CPU 回退的运行不能拿
#   回退后的耗时去支撑加速结论。
#
# 用法：
#   pwsh -NoProfile -File tools/check_provider_claims.ps1 `
#       -Reports tests/baseline/windows-baseline/bench-cpu.json,
#                tests/baseline/windows-baseline/bench-direct_ml.json,
#                tests/baseline/windows-baseline/bench-cuda.json
#
#   注意：`pwsh -File` 不做 PowerShell 参数绑定，逗号不会被当成数组分隔符，所以用
#   `-File` 时必须写成**一个逗号分隔的字符串**（如上），脚本内部会自己切分。
#   需要传真正的 PowerShell 数组时请用调用运算符：
#       & .\tools\check_provider_claims.ps1 -Reports @('a.json','b.json')
#
#   # CPU 参考报告也可以显式指定；不给时按 resolved 全为 CPU 的报告自动挑选
#   pwsh -NoProfile -File tools/check_provider_claims.ps1 -Reference <cpu.json> -Reports <other.json>
#
#   # 额外落地一份机器可读的结论
#   pwsh -NoProfile -File tools/check_provider_claims.ps1 -Reports ... -Json out.json
#
# 退出码：0 = 全部 PASS（或没有加速声明）；1 = 存在 FAIL；2 = 用法/解析错误。

[CmdletBinding()]
param(
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)]
    [string[]]$Reports,

    [string]$Reference,

    [string]$Json
)

$ErrorActionPreference = 'Stop'

# `pwsh -File script.ps1 a.json,b.json` 会把整个 `a.json,b.json` 当成**一个**字符串
# 参数（`-File` 不做 PowerShell 的参数绑定，逗号不会被当作数组分隔符）。因此这里显式
# 支持逗号分隔写法，同时也接受空格分隔的多个参数。
if ($Reports) {
    $expanded = @()
    foreach ($entry in $Reports) {
        if ([string]::IsNullOrWhiteSpace($entry)) { continue }
        foreach ($piece in ($entry -split ',')) {
            $trimmed = $piece.Trim()
            if ($trimmed -ne '') { $expanded += $trimmed }
        }
    }
    $Reports = $expanded
}

# p50 落在参考值的这个区间内即视为“没有测量到的加速”。
$WithinRatio = 0.90

function Fail-Usage {
    param([string]$Message, [string]$Detail)
    Write-Host "ERROR: $Message" -ForegroundColor Red
    if ($Detail) { Write-Host $Detail -ForegroundColor Red }
    exit 2
}

# Rust Debug 名字形如 `Cuda { device_id: 0 }`；只取前面的标识符。
function Get-ProviderKind {
    param([string]$Resolved)
    if ([string]::IsNullOrWhiteSpace($Resolved)) { return 'None' }
    $name = ($Resolved -split '\{')[0].Trim()
    if ($name -eq '') { return 'None' }
    return $name
}

function Test-IsCpuProvider {
    param([string]$Kind)
    switch ($Kind) {
        'Cpu'   { return $true }
        'CPU'   { return $true }
        'None'  { return $true }
        'null'  { return $true }
        default { return $false }
    }
}

function Read-Report {
    param([string]$Path, [string]$Label)
    $resolvedPath = $Path
    if (-not [System.IO.Path]::IsPathRooted($resolvedPath)) {
        $resolvedPath = Join-Path (Get-Location).ProviderPath $resolvedPath
    }
    if (-not (Test-Path -LiteralPath $resolvedPath -PathType Leaf)) {
        Fail-Usage "report not found: $Path" "resolved to: $resolvedPath"
    }
    $text = [System.IO.File]::ReadAllText($resolvedPath, [System.Text.Encoding]::UTF8)
    # `$null =` 是必须的：`ConvertFrom-Json` 会把对象写进管道，若不吞掉，函数返回的
    # 就是 (JSON 对象, 结果对象) 的数组，后续属性访问全部变成 $null。
    $json = $null
    try {
        $json = $text | ConvertFrom-Json
    } catch {
        Fail-Usage "report is not valid JSON: $Path" $_.Exception.Message
    }

    foreach ($field in @('meta.provider_resolution', 'stats.ocr_total_ms.p50')) {
        $cursor = $json
        foreach ($part in $field.Split('.')) {
            if ($null -eq $cursor) { break }
            $cursor = $cursor.$part
        }
        if ($null -eq $cursor) {
            Fail-Usage "report $Path is missing required field '$field'" `
                'refusing to treat a missing measurement as zero'
        }
    }

    $p50 = [double]$json.stats.ocr_total_ms.p50
    if ($p50 -le 0) {
        Fail-Usage "report $Path has a non-positive p50 ($p50)" `
            'cannot compute a ratio against a zero or negative baseline'
    }

    $claims = @()
    $fallbackProviders = @()
    $resolution = $json.meta.provider_resolution
    foreach ($stage in @('detector', 'classifier', 'recognizer')) {
        $entry = $resolution.$stage
        if ($null -eq $entry) { continue }
        $kind = Get-ProviderKind ([string]$entry.resolved)
        $fallback = [bool]$entry.fallback_to_cpu
        if ($fallback) { $fallbackProviders += "$stage=$kind" }
        if (-not (Test-IsCpuProvider $kind)) {
            $claims += [pscustomobject]@{ Stage = $stage; Kind = $kind }
        }
    }

    $result = [pscustomobject]@{
        Label             = $Label
        Path              = $resolvedPath
        P50               = $p50
        P90               = [double]$json.stats.ocr_total_ms.p90
        P50Source         = 'stats.ocr_total_ms.p50'
        Claims            = $claims
        FallbackProviders = $fallbackProviders
        PeakWorkingSet    = [double]$json.memory.peak_working_set_bytes
        MaxSideLen        = $json.meta.benchmark.max_side_len
        ImageCount        = [int]$json.meta.image_count
        Rounds            = [int]$json.meta.rounds
    }
    # 不要用 `return $result`：函数体内任何未吞掉的管道输出都会和它一起被返回。
    $result
}

if (-not $Reports -or $Reports.Count -eq 0) {
    Fail-Usage 'no reports given' `
        'usage: pwsh -File tools/check_provider_claims.ps1 -Reports <cpu.json>,<other.json>,...'
}

# 收集并去重（同一个报告被重复传入时不应重复计算）。
$parsed = @()
$seen = @{}
foreach ($path in $Reports) {
    $resolved = $path
    if (-not [System.IO.Path]::IsPathRooted($resolved)) {
        $resolved = Join-Path (Get-Location).ProviderPath $resolved
    }
    $key = $resolved.ToLowerInvariant()
    if ($seen.ContainsKey($key)) { continue }
    $seen[$key] = $true
    $parsed += Read-Report -Path $path -Label ([System.IO.Path]::GetFileName($path))
}

# 选参考报告：显式给的优先；否则要求“恰好一份 resolved 全为 CPU 且无 fallback”。
#
# 变量命名注意：PowerShell 变量名**大小写不敏感**，所以 `$Reference`（参数）和
# `$reference`（结果对象）是同一个变量。参数是 `[string]`，把结果对象写回同一个名字
# 会被隐式转换成字符串（`@{Label=...}` 的字面量），随后所有 `.Label` / `.P50` 访问都
# 变成 $null。因此这里先把参数搬到不同名的局部变量，结果对象另用一个不冲突的名字。
$cpuReferencePath = $Reference
$referenceReport = $null

if ($cpuReferencePath) {
    $refResolved = $cpuReferencePath
    if (-not [System.IO.Path]::IsPathRooted($refResolved)) {
        $refResolved = Join-Path (Get-Location).ProviderPath $refResolved
    }
    $referenceReport = $parsed |
        Where-Object { $_.Path -eq $refResolved } |
        Select-Object -First 1
    if ($null -eq $referenceReport) {
        $referenceReport = Read-Report -Path $cpuReferencePath `
            -Label ([System.IO.Path]::GetFileName($cpuReferencePath))
        $parsed += $referenceReport
    }
} else {
    $candidates = @($parsed | Where-Object {
        $_.Claims.Count -eq 0 -and $_.FallbackProviders.Count -eq 0
    })
    if ($candidates.Count -ne 1) {
        Fail-Usage "cannot auto-select a CPU reference report (found $($candidates.Count) candidates)" `
            'pass -Reference <cpu-report.json> explicitly'
    }
    $referenceReport = $candidates[0]
}

$refLabel = [string]$referenceReport.Label
$refPath = [string]$referenceReport.Path
$refP50 = [double]$referenceReport.P50
$refP90 = [double]$referenceReport.P90

if ($refP50 -le 0) {
    Fail-Usage "CPU reference report '$refLabel' has a non-positive p50 ($refP50)"
}

Write-Host ''
Write-Host 'Provider claim check' -ForegroundColor Cyan
Write-Host "Rule: a non-CPU ``resolved`` provider with fallback_to_cpu=false must show p50 at least $(100 - [int]($WithinRatio * 100))% better than the CPU reference; fallback_to_cpu=true is always a FAIL."
Write-Host 'Metric: stats.ocr_total_ms.p50 (same p50 field the committed baseline reports use).'
Write-Host ''
Write-Host ('Reference (CPU): {0}  p50={1:N2} ms  p90={2:N2} ms' -f $refLabel, $refP50, $refP90)
Write-Host ''

$rows = @()
$failures = @()
$order = @($referenceReport) + @($parsed | Where-Object { $_.Path -ne $refPath })

foreach ($report in $order) {
    $isReference = ($report.Path -eq $refPath)
    if ($isReference) {
        $verdict = 'REFERENCE'
        $delta = ''
        $speedup = ''
        $claimed = '(CPU baseline)'
        $fallbackText = 'n/a'
    } else {
        $claimed = if ($report.Claims.Count -eq 0) {
            'CPU only'
        } else {
            ($report.Claims | ForEach-Object { $_.Kind } | Select-Object -Unique) -join '+'
        }
        $fallbackText = if ($report.FallbackProviders.Count -eq 0) { 'no' } else { 'YES' }
        $ratio = $report.P50 / $refP50
        $delta = ('{0:N1}%' -f (($ratio - 1.0) * 100.0))
        $speedup = ('{0:N2}x' -f (1.0 / $ratio))

        if ($report.FallbackProviders.Count -gt 0) {
            $verdict = 'FAIL'
            $failures += [pscustomobject]@{
                Report    = $report.Label
                Reason    = "CPU fallback occurred during the run ($($report.FallbackProviders -join ', '))"
                Providers = $claimed
            }
        } elseif ($report.Claims.Count -gt 0 -and $ratio -ge $WithinRatio) {
            $verdict = 'FAIL'
            $failures += [pscustomobject]@{
                Report    = $report.Label
                Reason    = ("claims {0} but p50 ({1:N2} ms) is within +/-{2}% of the CPU reference ({3:N2} ms): acceleration claim without measured evidence" -f `
                    $claimed, $report.P50, [int]((1.0 - $WithinRatio) * 100.0), $refP50)
                Providers = $claimed
                WithinTolerance = $true
            }
        } elseif ($report.Claims.Count -gt 0) {
            $verdict = 'PASS'
        } else {
            $verdict = 'CPU'
        }
    }

    $rows += [pscustomobject]@{
        Report   = $report.Label
        Claimed  = $claimed
        Fallback = $fallbackText
        P50ms    = ('{0:N2}' -f $report.P50)
        DeltaPct = $delta
        Speedup  = $speedup
        Verdict  = $verdict
    }
}

$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host

$withinToleranceFail = $false
foreach ($failure in $failures) {
    Write-Host ("FAIL: {0} - {1}" -f $failure.Report, $failure.Reason) -ForegroundColor Red
    if ($failure.PSObject.Properties.Name -contains 'WithinTolerance') { $withinToleranceFail = $true }
}

if ($withinToleranceFail) {
    Write-Host ''
    Write-Host 'NOTE: a CUDA FAIL whose p50 equals the CPU p50 is the documented, expected verdict on this' -ForegroundColor Yellow
    Write-Host '      machine (RTX 4070 Ti SUPER, ONNX Runtime 1.28.0): the CUDA execution provider for this' -ForegroundColor Yellow
    Write-Host '      model performs no better than CPU here, so the acceleration claim is unsupported. This is a' -ForegroundColor Yellow
    Write-Host '      real finding about the provider, NOT a bug in this tool. Do not "fix" it by relaxing the rule;' -ForegroundColor Yellow
    Write-Host '      either produce a measurement that shows real acceleration or drop the claim.' -ForegroundColor Yellow
}

$status = if ($failures.Count -eq 0) { 'PASS' } else { 'FAIL' }

if ($Json) {
    $payload = [pscustomobject]@{
        rule = "non-CPU resolved provider with fallback_to_cpu=false must have p50 < $WithinRatio x CPU reference p50"
        metric = 'stats.ocr_total_ms.p50'
        reference = [pscustomobject]@{ report = $refLabel; p50_ms = $refP50 }
        rows = $rows
        failures = $failures
        status = $status
    }
    $jsonPath = $Json
    if (-not [System.IO.Path]::IsPathRooted($jsonPath)) {
        $jsonPath = Join-Path (Get-Location).ProviderPath $jsonPath
    }
    $payload | ConvertTo-Json -Depth 6 | Set-Content -Path $jsonPath -Encoding UTF8
    Write-Host "wrote $jsonPath"
}

Write-Host ''
if ($status -eq 'PASS') {
    Write-Host 'PASS: every acceleration claim is supported by a measured p50 improvement.' -ForegroundColor Green
    exit 0
}

Write-Host ("FAIL: {0} report(s) make an acceleration claim that measurement does not support." -f $failures.Count) -ForegroundColor Red
exit 1
