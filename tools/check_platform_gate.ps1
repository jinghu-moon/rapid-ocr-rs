# 阶段 1：验证 Windows-only 平台门槛。
#
# 期望行为：在非支持 target 上编译 `src/platform_gate.rs` 时，**只**得到我们自己的
# `compile_error!`，而不是“缺少某个 Windows API”或依赖项的模糊错误。
#
# 之所以单独编译这一个文件：门槛本身没有任何依赖，因此可以独立于 ort 等
# 需要在目标平台上构建的原生依赖进行验证。`lib.rs` 通过 `#[path]` 引入的就是同一个文件。
#
# 检查三类 target：
#
# 1. 非 Windows（默认 `x86_64-linux-android`）：必须命中自定义错误；
# 2. Windows GNU（`x86_64-pc-windows-gnu`）：它同样满足 `windows + x86_64`，
#    因此是“谓词必须带 `target_env = "msvc"`”这条修复的直接反例，必须命中自定义错误；
# 3. 本机 MSVC target：必须干净通过。
#
# **不允许静默通过**：某个 target 的 std 未安装时，本脚本打印明确的
# “target not installed -> this case is NOT verified” 并以非零退出码失败，
# 而不是把“没跑”当成“通过”。
#
# 用法：pwsh -NoProfile -File tools/check_platform_gate.ps1
#       pwsh -NoProfile -File tools/check_platform_gate.ps1 -GnuTarget ''   # 显式跳过 GNU 用例（仍会标明未验证）

[CmdletBinding()]
param(
    [string]$NonWindowsTarget = 'x86_64-linux-android',
    [string]$GnuTarget = 'x86_64-pc-windows-gnu',
    [string]$GnuFallbackTarget = 'i686-pc-windows-gnu'
)

$ErrorActionPreference = 'Stop'
$Crate = Split-Path -Parent $PSScriptRoot
$Gate = Join-Path $Crate 'src\platform_gate.rs'
$Expected = 'only supports x86_64-pc-windows-msvc'

function Get-InstalledTargets {
    $lines = & rustup target list --installed 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "rustup target list --installed failed: $($lines | Out-String)"
    }
    return @($lines | ForEach-Object { $_.ToString().Trim() } | Where-Object { $_ -ne '' })
}

function Test-TargetInstalled([string]$Target, [string[]]$Installed) {
    return ($Installed -contains $Target)
}

# 断言某个 target 上“只命中我们自己的门槛错误”。
function Assert-GateFires([string]$Target) {
    Write-Host "=== 非支持 target: $Target ==="
    $rmeta = Join-Path $env:TEMP "platform_gate_$($Target -replace '[^A-Za-z0-9]', '_').rmeta"
    $output = & rustc --edition 2024 --crate-type lib --emit=metadata `
        --target $Target $Gate -o $rmeta 2>&1
    $text = ($output | Out-String)
    Write-Host $text

    if ($text -notmatch [regex]::Escape($Expected)) {
        throw "platform gate did not fire with the expected message on $Target"
    }
    # 只应出现这一条诊断：出现别的错误说明门槛之外的代码泄漏进了非支持平台。
    # rustc 末尾还会打印 "error: aborting due to N previous errors"，它不是独立诊断。
    $diagnostics = $text -split "`n" | Where-Object {
        $_ -match '^error' -and $_ -notmatch '^error: aborting due to'
    }
    Write-Host "  matched custom compile_error; diagnostics: $($diagnostics.Count)"
    if ($diagnostics.Count -ne 1) {
        throw "expected exactly one diagnostic on $Target, saw $($diagnostics.Count)"
    }
    Write-Host "  OK: $Target hits the custom platform error only"
}

$installed = Get-InstalledTargets
Write-Host "installed targets: $($installed -join ', ')"
Write-Host ''

$unverified = @()

# --- 用例 1：非 Windows ---
if (-not (Test-TargetInstalled $NonWindowsTarget $installed)) {
    Write-Host "target not installed -> this case is NOT verified: $NonWindowsTarget" -ForegroundColor Red
    Write-Host "  install it with: rustup target add $NonWindowsTarget" -ForegroundColor Red
    $unverified += $NonWindowsTarget
} else {
    Assert-GateFires $NonWindowsTarget
}

# --- 用例 2：Windows GNU（谓词必须带 target_env = "msvc" 的直接反例）---
$gnu = $null
foreach ($candidate in @($GnuTarget, $GnuFallbackTarget)) {
    if ([string]::IsNullOrWhiteSpace($candidate)) { continue }
    if (Test-TargetInstalled $candidate $installed) {
        $gnu = $candidate
        break
    }
}
if ($null -eq $gnu) {
    Write-Host "target not installed -> this case is NOT verified: $GnuTarget (fallback: $GnuFallbackTarget)" -ForegroundColor Red
    Write-Host "  install it with: rustup target add $GnuTarget" -ForegroundColor Red
    $unverified += $GnuTarget
} else {
    Assert-GateFires $gnu
}

# --- 用例 3：本机支持 target ---
Write-Host '=== 支持平台（本机）==='
$hostText = (& rustc --edition 2024 --crate-type lib --emit=metadata `
        $Gate -o (Join-Path $env:TEMP 'platform_gate_host.rmeta') 2>&1 | Out-String)
if ($hostText -match 'error') {
    Write-Host $hostText
    throw 'the platform gate must accept the supported host target'
}
Write-Host '  OK: x86_64-pc-windows-msvc compiles the gate cleanly'

Write-Host ''
if ($unverified.Count -gt 0) {
    Write-Host "FAILED: NOT verified for $($unverified.Count) target(s): $($unverified -join ', ')" -ForegroundColor Red
    Write-Host 'This script refuses to report success while a case could not be executed.' -ForegroundColor Red
    exit 3
}

Write-Host 'PASS: every case was executed and behaved as expected.' -ForegroundColor Green
exit 0
