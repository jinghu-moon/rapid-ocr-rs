# Provider 加速声明的证据检查器。
#
# 规则（阶段 0/2/5 冻结，终审加固）：
#   **加速必须由测量证明，而且两份报告必须可比。**
#
#   一份 bench_warm_e2e 报告只有在同时满足以下条件时才允许声称“非 CPU 加速”：
#     1. 至少一个阶段的 `selected_ep` 不是 CPU，且 `fallback_to_cpu = false`；
#     2. 该报告的 p50 相对 CPU 参考报告至少好 10%。
#   任何“声称 GPU / 实际与 CPU 相差 ±10% 以内”的报告都是**没有测量支撑的加速声明**，
#   必须判 FAIL（exit 1）。`fallback_to_cpu: true` 同样是 FAIL。
#
#   在比较之前必须先证明**可比性**；不可比一律**拒绝**（exit 2），而不是给出一个看起来
#   有意义的数字：
#     - JSON 非法、缺字段、字段类型不对 → exit 2（绝不把缺失当成 0/null 默认值）；
#     - 输入集 / max_side_len / rounds / 模型不一致 → exit 2；
#     - ORT 指纹（API 版本 + 运行库 SHA-256）不一致 → exit 2；
#     - `-Reference` 指定的报告不是全 CPU → exit 2。
#
#   为什么必须校验 ORT 指纹：阶段 0 的报告既没有版本也没有哈希，报告里写的“ORT 1.17”
#   其实是 Windows 自带同名 DLL 的**文件版本**，而真正链接进可执行文件的运行库自报
#   `1.28.0`。没有指纹就没法知道两次测量是不是同一份运行库。
#
#   `selected_ep` 的语义也要说清：它只表示“交给 ONNX Runtime 的 EP 链头部”，
#   **不是**逐节点执行证据。逐节点分配不通过该 API 暴露，所以加速结论只能来自 p50/p90。
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
#   # CPU 参考报告也可以显式指定；不给时按 selected_ep 全为 CPU 的报告自动挑选
#   pwsh -NoProfile -File tools/check_provider_claims.ps1 -Reference <cpu.json> -Reports <other.json>
#
#   # 额外落地一份机器可读的结论
#   pwsh -NoProfile -File tools/check_provider_claims.ps1 -Reports ... -Json out.json
#
# 退出码：0 = 全部 PASS（或没有加速声明）；1 = 存在 FAIL；2 = 用法/解析/可比性错误。

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
    param([string]$SelectedEp)
    if ([string]::IsNullOrWhiteSpace($SelectedEp)) { return 'None' }
    $name = ($SelectedEp -split '\{')[0].Trim()
    if ($name -eq '') { return 'None' }
    return $name
}

function Test-IsCpuProvider {
    param([string]$Kind)
    switch ($Kind) {
        'Cpu'   { return $true }
        'CPU'   { return $true }
        default { return $false }
    }
}

function Resolve-PathOrFail {
    param([string]$Path, [string]$Label)
    $resolved = $Path
    if (-not [System.IO.Path]::IsPathRooted($resolved)) {
        $resolved = Join-Path (Get-Location).ProviderPath $resolved
    }
    if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
        Fail-Usage "$Label not found: $Path" "resolved to: $resolved"
    }
    return $resolved
}

# 取 JSON 里的嵌套字段；**任何一层缺失都返回 $null**，由调用方决定是否致命。
function Get-Field {
    param($Json, [string]$DottedPath)
    $cursor = $Json
    foreach ($part in $DottedPath.Split('.')) {
        if ($null -eq $cursor) { return $null }
        $cursor = $cursor.$part
    }
    return $cursor
}

function Require-Field {
    param($Json, [string]$DottedPath, [string]$Path)
    $value = Get-Field $Json $DottedPath
    if ($null -eq $value) {
        Fail-Usage "report $Path is missing required field '$DottedPath'" `
            'refusing to treat a missing measurement as a zero/default value'
    }
    return $value
}

function Require-Number {
    param($Json, [string]$DottedPath, [string]$Path)
    $value = Require-Field $Json $DottedPath $Path
    if ($value -isnot [ValueType]) {
        Fail-Usage "report $Path field '$DottedPath' must be a number, got '$($value.GetType().Name)'"
    }
    return [double]$value
}

function Require-Integer {
    param($Json, [string]$DottedPath, [string]$Path)
    $value = Require-Number $Json $DottedPath $Path
    if ($value -ne [Math]::Floor($value)) {
        Fail-Usage "report $Path field '$DottedPath' must be an integer, got $value"
    }
    return [int]$value
}

function Require-String {
    param($Json, [string]$DottedPath, [string]$Path)
    $value = Require-Field $Json $DottedPath $Path
    if ($value -is [string] -or $value -is [ValueType]) { return [string]$value }
    Fail-Usage "report $Path field '$DottedPath' must be a scalar, got '$($value.GetType().Name)'"
}

# ONNX Runtime 指纹：API 版本 + 运行库文件身份。两者都必须是实测值。
function Read-OrtFingerprint {
    param($Json, [string]$Path)
    $fingerprint = Require-Field $Json 'meta.ort_runtime' $Path
    if ($null -eq $fingerprint -or $fingerprint -is [string]) {
        Fail-Usage "report $Path field 'meta.ort_runtime' must be the ORT fingerprint object" `
            'the fingerprint was introduced after the phase-0 reports; re-collect the baseline'
    }
    $apiVersion = Require-String $fingerprint 'api_version' $Path
    if ([string]::IsNullOrWhiteSpace($apiVersion)) {
        Fail-Usage "report $Path has a blank 'meta.ort_runtime.api_version'" `
            'an empty fingerprint cannot be compared'
    }
    $modulePath = Require-String $fingerprint 'runtime_module.path' $Path
    $moduleSize = Require-Integer $fingerprint 'runtime_module.size_bytes' $Path
    $moduleSha = Require-String $fingerprint 'runtime_module.sha256' $Path
    if ($moduleSize -le 0) {
        Fail-Usage "report $Path has a non-positive ORT runtime size ($moduleSize)"
    }
    if ($moduleSha -notmatch '^[0-9a-fA-F]{64}$') {
        Fail-Usage "report $Path has a malformed ORT runtime SHA-256: '$moduleSha'" `
            'expected 64 hexadecimal characters'
    }
    return [pscustomobject]@{
        ApiVersion = $apiVersion
        ModulePath = $modulePath
        ModuleSize = $moduleSize
        ModuleSha  = $moduleSha.ToLowerInvariant()
        Key        = "$apiVersion|$($moduleSha.ToLowerInvariant())"
    }
}

function Read-Report {
    param([string]$Path, [string]$Label)
    $resolvedPath = Resolve-PathOrFail $Path 'report'
    $text = [System.IO.File]::ReadAllText($resolvedPath, [System.Text.Encoding]::UTF8)
    # `$null =` 是必须的：`ConvertFrom-Json` 会把对象写进管道，若不吞掉，函数返回的
    # 就是 (JSON 对象, 结果对象) 的数组，后续属性访问全部变成 $null。
    $json = $null
    try {
        $json = $text | ConvertFrom-Json
    } catch {
        Fail-Usage "report is not valid JSON: $Path" $_.Exception.Message
    }
    if ($null -eq $json -or $json -is [string] -or $json -is [array]) {
        Fail-Usage "report is not a JSON object: $Path" 'expected a single top-level object'
    }

    # --- 必需字段（一次读全，缺任何一个都 exit 2）---
    $p50 = Require-Number $json 'stats.ocr_total_ms.p50' $Path
    $p90 = Require-Number $json 'stats.ocr_total_ms.p90' $Path
    if ($p50 -le 0) {
        Fail-Usage "report $Path has a non-positive p50 ($p50)" `
            'cannot compute a ratio against a zero or negative baseline'
    }
    if ($p90 -le 0) {
        Fail-Usage "report $Path has a non-positive p90 ($p90)"
    }
    $imageCount = Require-Integer $json 'meta.image_count' $Path
    $rounds = Require-Integer $json 'meta.rounds' $Path
    $maxSideLen = Require-Integer $json 'meta.benchmark.max_side_len' $Path
    $imagesDir = Require-String $json 'meta.images_dir' $Path
    $modelId = Require-String $json 'meta.provider_resolution.model_id' $Path

    # --- 三个阶段的解析结果：检测器与识别器必须在，分类器按是否启用出现 ---
    $resolution = Require-Field $json 'meta.provider_resolution' $Path
    $stages = [ordered]@{}
    foreach ($stage in @('detector', 'recognizer')) {
        $entry = Get-Field $resolution $stage
        if ($null -eq $entry) {
            Fail-Usage "report $Path is missing 'meta.provider_resolution.$stage'" `
                'every enabled stage must report its selected execution provider'
        }
        $stages[$stage] = $entry
    }
    $classifier = Get-Field $resolution 'classifier'
    if ($null -ne $classifier) { $stages['classifier'] = $classifier }

    $fingerprint = Read-OrtFingerprint $json $Path
    $threads = Require-Field $json 'meta.thread_plan' $Path
    if ($null -eq $threads) {
        Fail-Usage "report $Path is missing 'meta.thread_plan'" `
            'an unresolvable thread plan makes the run non-reproducible'
    }

    $claims = @()
    $fallbackProviders = @()
    foreach ($stage in $stages.Keys) {
        $entry = $stages[$stage]
        $selectedRaw = Get-Field $entry 'selected_ep'
        if ($null -eq $selectedRaw) {
            Fail-Usage "report $Path is missing 'meta.provider_resolution.$stage.selected_ep'" `
                'the field was renamed from `resolved`; re-collect the report'
        }
        $fallbackRaw = Get-Field $entry 'fallback_to_cpu'
        if ($null -eq $fallbackRaw) {
            Fail-Usage "report $Path is missing 'meta.provider_resolution.$stage.fallback_to_cpu'"
        }
        $kind = Get-ProviderKind ([string]$selectedRaw)
        $fallback = [bool]$fallbackRaw
        if ($fallback) { $fallbackProviders += "$stage=$kind" }
        if (-not (Test-IsCpuProvider $kind)) {
            $claims += [pscustomobject]@{ Stage = $stage; Kind = $kind }
        }
    }

    $result = [pscustomobject]@{
        Label             = $Label
        Path              = $resolvedPath
        P50               = $p50
        P90               = $p90
        P50Source         = 'stats.ocr_total_ms.p50'
        Claims            = $claims
        FallbackProviders = $fallbackProviders
        PeakWorkingSet    = Require-Number $json 'memory.peak_working_set_bytes' $Path
        MaxSideLen        = $maxSideLen
        ImageCount        = $imageCount
        Rounds            = $rounds
        ImagesDir         = $imagesDir
        ModelId           = $modelId
        Fingerprint       = $fingerprint
        StageCount        = $stages.Count
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
    $resolved = Resolve-PathOrFail $path 'report'
    $key = $resolved.ToLowerInvariant()
    if ($seen.ContainsKey($key)) { continue }
    $seen[$key] = $true
    $parsed += Read-Report -Path $path -Label ([System.IO.Path]::GetFileName($path))
}

# 选参考报告：显式给的优先；否则要求“恰好一份 selected_ep 全为 CPU 且无 fallback”。
#
# 变量命名注意：PowerShell 变量名**大小写不敏感**，所以 `$Reference`（参数）和
# `$reference`（结果对象）是同一个变量。参数是 `[string]`，把结果对象写回同一个名字
# 会被隐式转换成字符串。因此这里先把参数搬到不同名的局部变量。
$cpuReferencePath = $Reference
$referenceReport = $null

if ($cpuReferencePath) {
    $refResolved = Resolve-PathOrFail $cpuReferencePath 'reference report'
    $referenceReport = $parsed |
        Where-Object { $_.Path -eq $refResolved } |
        Select-Object -First 1
    if ($null -eq $referenceReport) {
        $referenceReport = Read-Report -Path $cpuReferencePath `
            -Label ([System.IO.Path]::GetFileName($cpuReferencePath))
        $parsed += $referenceReport
    }
    # 显式指定的参考报告必须真的是全 CPU：否则比值算的是“GPU vs GPU”，结论没有意义。
    if ($referenceReport.Claims.Count -gt 0) {
        $claimed = ($referenceReport.Claims | ForEach-Object { "$($_.Stage)=$($_.Kind)" }) -join ', '
        Fail-Usage "the -Reference report '$($referenceReport.Label)' is not all-CPU (it claims $claimed)" `
            'pass a report whose every stage selected_ep is Cpu with fallback_to_cpu=false'
    }
    if ($referenceReport.FallbackProviders.Count -gt 0) {
        Fail-Usage "the -Reference report '$($referenceReport.Label)' recorded a CPU fallback" `
            'a fallback run cannot serve as the all-CPU baseline'
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

# --- 可比性校验：任何一项不一致都必须拒绝，而不是给出一个看似正确的比值 ---
foreach ($report in $parsed) {
    if ($report.Path -eq $refPath) { continue }
    if ($report.MaxSideLen -ne $referenceReport.MaxSideLen) {
        Fail-Usage ("report '{0}' used max_side_len {1} but the reference '{2}' used {3}" -f `
            $report.Label, $report.MaxSideLen, $refLabel, $referenceReport.MaxSideLen) `
            'different input resolutions are not comparable'
    }
    if ($report.ImageCount -ne $referenceReport.ImageCount) {
        Fail-Usage ("report '{0}' measured {1} images but the reference '{2}' measured {3}" -f `
            $report.Label, $report.ImageCount, $refLabel, $referenceReport.ImageCount) `
            'different input sets are not comparable'
    }
    if ($report.Rounds -ne $referenceReport.Rounds) {
        Fail-Usage ("report '{0}' ran {1} rounds but the reference '{2}' ran {3}" -f `
            $report.Label, $report.Rounds, $refLabel, $referenceReport.Rounds) `
            'different round counts are not comparable'
    }
    if ($report.ImagesDir -ne $referenceReport.ImagesDir) {
        Fail-Usage ("report '{0}' used images_dir '{1}' but the reference '{2}' used '{3}'" -f `
            $report.Label, $report.ImagesDir, $refLabel, $referenceReport.ImagesDir) `
            'different input sets are not comparable'
    }
    if ($report.ModelId -ne $referenceReport.ModelId) {
        Fail-Usage ("report '{0}' used model '{1}' but the reference '{2}' used '{3}'" -f `
            $report.Label, $report.ModelId, $refLabel, $referenceReport.ModelId) `
            'different models are not comparable'
    }
    if ($report.Fingerprint.Key -ne $referenceReport.Fingerprint.Key) {
        Fail-Usage ("report '{0}' ran on a different ONNX Runtime than the reference '{1}'" -f `
            $report.Label, $refLabel) `
            ("reference: api={0} sha256={1}`n            report:    api={2} sha256={3}" -f `
                $referenceReport.Fingerprint.ApiVersion, $referenceReport.Fingerprint.ModuleSha, `
                $report.Fingerprint.ApiVersion, $report.Fingerprint.ModuleSha)
    }
}

Write-Host ''
Write-Host 'Provider claim check' -ForegroundColor Cyan
Write-Host "Rule: a non-CPU ``selected_ep`` with fallback_to_cpu=false must show p50 at least $(100 - [int]($WithinRatio * 100))% better than the CPU reference; fallback_to_cpu=true is always a FAIL."
Write-Host 'Metric: stats.ocr_total_ms.p50 (same p50 field the committed baseline reports use).'
Write-Host ('Comparability: same images_dir/image_count/rounds/max_side_len/model and the same ORT fingerprint (api + runtime sha256).')
Write-Host ''
Write-Host ('Reference (CPU): {0}  p50={1:N2} ms  p90={2:N2} ms' -f $refLabel, $refP50, $refP90)
Write-Host ('ORT runtime:     api={0} sha256={1}' -f `
        $referenceReport.Fingerprint.ApiVersion, $referenceReport.Fingerprint.ModuleSha)
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
        P90ms    = ('{0:N2}' -f $report.P90)
        DeltaPct = $delta
        Speedup  = $speedup
        Verdict  = $verdict
    }
}

$rows | Format-Table -AutoSize | Out-String -Width 220 | Write-Host

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
        rule = "non-CPU selected_ep with fallback_to_cpu=false must have p50 < $WithinRatio x CPU reference p50"
        metric = 'stats.ocr_total_ms.p50'
        comparability = 'same images_dir/image_count/rounds/max_side_len/model and ORT fingerprint'
        reference = [pscustomobject]@{
            report = $refLabel
            p50_ms = $refP50
            ort_api_version = $referenceReport.Fingerprint.ApiVersion
            ort_runtime_sha256 = $referenceReport.Fingerprint.ModuleSha
        }
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
