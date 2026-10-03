# 阶段 1：验证 Windows-only 平台门槛。
#
# 期望行为：在非 Windows target 上编译 `src/platform_gate.rs` 时，**只**得到我们自己的
# `compile_error!`，而不是“缺少某个 Windows API”或依赖项的模糊错误。
#
# 之所以单独编译这一个文件：门槛本身没有任何依赖，因此可以独立于 ort/turbojpeg 等
# 需要在目标平台上构建的原生依赖进行验证。`lib.rs` 通过 `#[path]` 引入的就是同一个文件。
#
# 用法：pwsh -NoProfile -File tools/check_platform_gate.ps1

param(
    [string]$NonWindowsTarget = 'x86_64-linux-android'
)

$ErrorActionPreference = 'Stop'
$Crate = Split-Path -Parent $PSScriptRoot
$Gate = Join-Path $Crate 'src\platform_gate.rs'
$Expected = 'only supports x86_64-pc-windows-msvc'

Write-Host "=== 非 Windows target: $NonWindowsTarget ==="
$output = & rustc --edition 2024 --crate-type lib --emit=metadata `
    --target $NonWindowsTarget $Gate -o (Join-Path $env:TEMP 'platform_gate.rmeta') 2>&1
$text = ($output | Out-String)

if ($text -notmatch [regex]::Escape($Expected)) {
    Write-Host $text
    throw "platform gate did not fire with the expected message on $NonWindowsTarget"
}
# 只应出现这一条诊断：出现别的错误说明门槛之外的代码泄漏进了非支持平台。
# rustc 末尾还会打印 "error: aborting due to N previous errors"，它不是独立诊断。
$diagnostics = $text -split "`n" | Where-Object {
    $_ -match '^error' -and $_ -notmatch '^error: aborting due to'
}
Write-Host "  matched custom compile_error; diagnostics: $($diagnostics.Count)"
if ($diagnostics.Count -ne 1) {
    Write-Host $text
    throw "expected exactly one diagnostic, saw $($diagnostics.Count)"
}
Write-Host '  OK: non-Windows target hits the custom platform error only'

Write-Host '=== 支持平台（本机）==='
$hostText = (& rustc --edition 2024 --crate-type lib --emit=metadata `
    $Gate -o (Join-Path $env:TEMP 'platform_gate_host.rmeta') 2>&1 | Out-String)
if ($hostText -match 'error') {
    Write-Host $hostText
    throw 'the platform gate must accept the supported host target'
}
Write-Host '  OK: x86_64-pc-windows-msvc compiles the gate cleanly'
