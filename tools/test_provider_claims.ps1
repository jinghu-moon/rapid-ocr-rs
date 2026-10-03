# 证据检查器（`check_provider_claims.ps1`）的可执行测试。
#
# 目的：把“拒绝”行为也变成可复现的证据，而不是只在文档里声称。
# 本脚本会**生成**合成报告（不写入仓库），逐条运行真实检查器，断言退出码：
#
# | 用例 | 期望 |
# | --- | --- |
# | 合法 CPU 参考（单独） | 0 |
# | 合法 CPU + DirectML（~2×） | 0 |
# | 合法 CPU + CUDA（p50 相同） | 1（FAIL：无测量支撑的加速声明） |
# | JSON 非法 | 2 |
# | 缺字段（meta.thread_plan） | 2 |
# | max_side_len 不一致 | 2 |
# | ORT 指纹不一致 | 2 |
# | `-Reference` 不是全 CPU | 2 |
# | 缺 selected_ep（旧 `resolved` 字段名） | 2 |
#
# 用法：pwsh -NoProfile -File tools/test_provider_claims.ps1
# 退出码：0 = 全部符合预期；1 = 有用例不符；2 = 环境/用法错误。

[CmdletBinding()]
param(
    [string]$WorkDir
)

$ErrorActionPreference = 'Stop'
$Crate = Split-Path -Parent $PSScriptRoot
$Checker = Join-Path $PSScriptRoot 'check_provider_claims.ps1'
if (-not (Test-Path $Checker)) { throw "checker not found: $Checker" }
if (-not $WorkDir) {
    $WorkDir = Join-Path ([System.IO.Path]::GetTempPath()) ("rapid-ocr-provider-claims-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
}
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
Write-Host "work dir: $WorkDir"

$Script:Failures = @()
$Script:Passed = 0

# 合成一份与 `bench_warm_e2e` 真实输出同形的报告。
function New-Report {
    param(
        [string]$Path,
        [string]$SelectedEp = 'Cpu',
        [double]$P50 = 1000.0,
        [double]$P90 = 1200.0,
        [int]$ImageCount = 12,
        [int]$Rounds = 3,
        [int]$MaxSideLen = 2000,
        [string]$ImagesDir = 'D:\images\set-a',
        [string]$ModelId = 'PP-OCRv6-small-ch',
        [string]$OrtApiVersion = '1.28.0',
        [string]$OrtSha = ('a' * 64),
        [bool]$FallbackToCpu = $false,
        [switch]$OmitThreadPlan,
        [switch]$UseLegacyResolvedField
    )
    $stageResolution = [ordered]@{
        requested       = $SelectedEp
        fallback_to_cpu = $FallbackToCpu
    }
    if ($UseLegacyResolvedField) {
        $stageResolution['resolved'] = $SelectedEp
    } else {
        $stageResolution['selected_ep'] = $SelectedEp
    }
    $meta = [ordered]@{
        images_dir   = $ImagesDir
        image_count  = $ImageCount
        rounds       = $Rounds
        warmup_rounds = 1
        init_ms      = 130.0
        benchmark    = [ordered]@{
            build_profile = 'release'
            max_side_len = $MaxSideLen
            timing_scope = [ordered]@{
                wall_ms      = 'file_read_plus_ocr'
                ocr_total_ms = 'ocr_pipeline_only'
            }
        }
        provider_resolution = [ordered]@{
            model_id   = $ModelId
            detector   = $stageResolution
            classifier = $null
            recognizer = $stageResolution
        }
        ort_runtime = [ordered]@{
            api_version    = $OrtApiVersion
            runtime_source = 'static_link'
            runtime_module = [ordered]@{
                path       = 'C:\cache\ort\onnxruntime.lib'
                size_bytes = 341152186
                sha256     = $OrtSha
                reason     = $null
            }
            link         = $null
            link_reason  = $null
            provider_dlls = @()
            provider_reason = $null
        }
        ort_runtime_version = $OrtApiVersion
    }
    if (-not $OmitThreadPlan) {
        $meta['thread_plan'] = [ordered]@{
            source = 'explicit'; budget = 14; ort_intra = 16; ort_inter = 1; rayon = 4; sessions = 2
        }
    }
    $report = [ordered]@{
        meta  = $meta
        stats = [ordered]@{
            wall_ms = [ordered]@{ count = 36; avg = $P50; p50 = $P50; p90 = $P90; min = 700.0; max = 1500.0 }
            ocr_total_ms = [ordered]@{ count = 36; avg = $P50; p50 = $P50; p90 = $P90; min = 700.0; max = 1500.0 }
            regions = [ordered]@{ count = 36; avg = 34.83; p50 = 37.0; p90 = 61.0; min = 13.0; max = 61.0 }
        }
        memory = [ordered]@{ peak_working_set_bytes = 1300000000; source = 'windows:GetProcessMemoryInfo.PeakWorkingSetSize' }
    }
    $report | ConvertTo-Json -Depth 10 | Set-Content -Path $Path -Encoding UTF8
    return $Path
}

# 运行检查器并断言退出码；同时要求 stdout+stderr 里出现给定关键字。
function Assert-Check {
    param(
        [string]$Name,
        [string[]]$Arguments,
        [int]$ExpectedExit,
        [string]$ExpectMessage
    )
    $output = & pwsh -NoProfile -File $Checker @Arguments 2>&1 | Out-String
    $code = $LASTEXITCODE
    $ok = ($code -eq $ExpectedExit)
    if ($ok -and $ExpectMessage) {
        $ok = $output -match [regex]::Escape($ExpectMessage)
    }
    if ($ok) {
        $Script:Passed++
        Write-Host ("  PASS  {0,-46} exit={1}" -f $Name, $code) -ForegroundColor Green
    } else {
        $Script:Failures += [pscustomobject]@{
            Name     = $Name
            Expected = $ExpectedExit
            Actual   = $code
            Message  = $ExpectMessage
            Output   = $output
        }
        Write-Host ("  FAIL  {0,-46} exit={1} (expected {2})" -f $Name, $code, $ExpectedExit) -ForegroundColor Red
    }
}

# --- 合法基线 ---
$cpu = New-Report -Path (Join-Path $WorkDir 'cpu.json') -SelectedEp 'Cpu' -P50 1000.0
$directml = New-Report -Path (Join-Path $WorkDir 'directml.json') -SelectedEp 'DirectMl { device_id: 0 }' -P50 500.0
$cuda = New-Report -Path (Join-Path $WorkDir 'cuda.json') -SelectedEp 'Cuda { device_id: 0 }' -P50 1000.0

Write-Host ''
Write-Host '=== 1. 合法行为（不应被加固破坏）===' -ForegroundColor Cyan
Assert-Check -Name 'CPU only' -Arguments @('-Reports', $cpu) -ExpectedExit 0 -ExpectMessage 'PASS'
Assert-Check -Name 'CPU + DirectML (~2x)' -Arguments @('-Reports', "$cpu,$directml") -ExpectedExit 0 -ExpectMessage 'PASS'
Assert-Check -Name 'CPU + CUDA (same p50) -> FAIL' -Arguments @('-Reports', "$cpu,$cuda") -ExpectedExit 1 -ExpectMessage 'acceleration claim without measured evidence'
Assert-Check -Name 'explicit -Reference (CPU)' -Arguments @('-Reference', $cpu, '-Reports', "$directml") -ExpectedExit 0 -ExpectMessage 'PASS'

Write-Host ''
Write-Host '=== 2. 拒绝：必须 exit 2 ===' -ForegroundColor Cyan

$badJson = Join-Path $WorkDir 'malformed.json'
'{ "meta": { "image_count": 12, ' | Set-Content -Path $badJson -Encoding UTF8
Assert-Check -Name 'malformed JSON' -Arguments @('-Reports', $badJson) -ExpectedExit 2 -ExpectMessage 'not valid JSON'

$noThreads = New-Report -Path (Join-Path $WorkDir 'no-thread-plan.json') -OmitThreadPlan
Assert-Check -Name 'missing meta.thread_plan' -Arguments @('-Reports', $noThreads) -ExpectedExit 2 -ExpectMessage "missing required field 'meta.thread_plan'"

$legacy = New-Report -Path (Join-Path $WorkDir 'legacy-resolved.json') -UseLegacyResolvedField
Assert-Check -Name 'legacy resolved field (no selected_ep)' -Arguments @('-Reports', $legacy) -ExpectedExit 2 -ExpectMessage 'selected_ep'

$sideMismatch = New-Report -Path (Join-Path $WorkDir 'side-mismatch.json') -SelectedEp 'DirectMl { device_id: 0 }' -P50 500.0 -MaxSideLen 1280
Assert-Check -Name 'max_side_len mismatch' -Arguments @('-Reports', "$cpu,$sideMismatch") -ExpectedExit 2 -ExpectMessage 'max_side_len'

$imageMismatch = New-Report -Path (Join-Path $WorkDir 'image-count-mismatch.json') -SelectedEp 'DirectMl { device_id: 0 }' -P50 500.0 -ImageCount 13
Assert-Check -Name 'image_count mismatch' -Arguments @('-Reports', "$cpu,$imageMismatch") -ExpectedExit 2 -ExpectMessage 'images but the reference'

$roundMismatch = New-Report -Path (Join-Path $WorkDir 'rounds-mismatch.json') -SelectedEp 'DirectMl { device_id: 0 }' -P50 500.0 -Rounds 5
Assert-Check -Name 'rounds mismatch' -Arguments @('-Reports', "$cpu,$roundMismatch") -ExpectedExit 2 -ExpectMessage 'rounds but the reference'

$dirMismatch = New-Report -Path (Join-Path $WorkDir 'images-dir-mismatch.json') -SelectedEp 'DirectMl { device_id: 0 }' -P50 500.0 -ImagesDir 'D:\images\set-b'
Assert-Check -Name 'images_dir mismatch' -Arguments @('-Reports', "$cpu,$dirMismatch") -ExpectedExit 2 -ExpectMessage 'images_dir'

$modelMismatch = New-Report -Path (Join-Path $WorkDir 'model-mismatch.json') -SelectedEp 'DirectMl { device_id: 0 }' -P50 500.0 -ModelId 'PP-OCRv6-medium-ch'
Assert-Check -Name 'model mismatch' -Arguments @('-Reports', "$cpu,$modelMismatch") -ExpectedExit 2 -ExpectMessage 'used model'

$ortMismatch = New-Report -Path (Join-Path $WorkDir 'ort-mismatch.json') -SelectedEp 'DirectMl { device_id: 0 }' -P50 500.0 -OrtSha ('b' * 64)
Assert-Check -Name 'different ORT fingerprint' -Arguments @('-Reports', "$cpu,$ortMismatch") -ExpectedExit 2 -ExpectMessage 'different ONNX Runtime'

$nonCpuReference = New-Report -Path (Join-Path $WorkDir 'non-cpu-reference.json') -SelectedEp 'Cuda { device_id: 0 }' -P50 900.0
Assert-Check -Name 'non-CPU -Reference' -Arguments @('-Reference', $nonCpuReference, '-Reports', $cpu) -ExpectedExit 2 -ExpectMessage 'is not all-CPU'

Write-Host ''
if ($Script:Failures.Count -gt 0) {
    foreach ($failure in $Script:Failures) {
        Write-Host ("UNEXPECTED: {0}" -f $failure.Name) -ForegroundColor Red
        Write-Host ("  expected exit {0}, got {1}; expected message containing '{2}'" -f `
                $failure.Expected, $failure.Actual, $failure.Message) -ForegroundColor Red
        Write-Host ($failure.Output -split "`n" | Select-Object -First 12 | Out-String) -ForegroundColor DarkGray
    }
    Write-Host ("FAILED: {0} case(s) did not behave as expected ({1} passed)." -f $Script:Failures.Count, $Script:Passed) -ForegroundColor Red
    exit 1
}

Write-Host ("PASS: {0} case(s) behaved as expected." -f $Script:Passed) -ForegroundColor Green
exit 0
