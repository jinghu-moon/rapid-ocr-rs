//! 进程峰值内存采集。
//!
//! benchmark 与评测工具需要真实的内存峰值，而不是估算值。Windows 上用 PSAPI 的
//! `GetProcessMemoryInfo(...)->PeakWorkingSetSize`，无需额外依赖；这也是本 crate
//! 唯一支持的内存口径，避免出现两套数字。
//!
//! 该能力属于共享层：普通 OCR 与公式 benchmark 使用同一实现。
//!
//! 平台边界由 crate 根部的 `compile_error!` 保证，因此这里只保留 Windows 实现：
//! crate 编译不到非 Windows 平台，无需为其它平台准备分支。

/// 当前进程的峰值工作集（字节）。
///
/// 调用失败（Win32 返回 0）时返回 `None`；失败原因通过
/// [`peak_memory_failure_reason`] 提供给报告，便于区分“未采集”和“采集失败”。
pub fn peak_working_set_bytes() -> Option<u64> {
    peak_working_set_impl().ok()
}

/// 峰值内存数据的来源标识，写入报告以便审查。
///
/// 平台收窄之后该字符串是常量，但仍然通过函数暴露：报告字段需要它，
/// 而且测试要断言报告里写的就是这个口径。
pub const PEAK_MEMORY_SOURCE: &str = "windows:GetProcessMemoryInfo.PeakWorkingSetSize";

/// [`peak_memory_source`] 的常量取值。
pub fn peak_memory_source() -> &'static str {
    PEAK_MEMORY_SOURCE
}

/// 采集失败时的可定位原因；成功时返回 `None`。
///
/// `GetProcessMemoryInfo` 只返回成功/失败，没有更多信息，因此这里带上
/// `GetLastError`，让报告能说明“为什么没有峰值内存”而不是留空。
pub fn peak_memory_failure_reason() -> Option<String> {
    peak_working_set_impl()
        .err()
        .map(|code| format!("GetProcessMemoryInfo failed with Win32 error {code}"))
}

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

#[link(name = "psapi")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut core::ffi::c_void;
    fn GetProcessMemoryInfo(
        process: *mut core::ffi::c_void,
        counters: *mut ProcessMemoryCounters,
        size: u32,
    ) -> i32;
    fn GetLastError() -> u32;
}

/// 返回峰值工作集；失败时返回 `GetLastError` 的值。
fn peak_working_set_impl() -> Result<u64, u32> {
    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    // SAFETY: `counters` is a correctly sized, zero-initialized POD structure;
    // `GetCurrentProcess` returns a pseudo-handle that must not be closed.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        // SAFETY: `GetLastError` has no preconditions and is read immediately.
        return Err(unsafe { GetLastError() });
    }
    Ok(counters.peak_working_set_size as u64)
}

#[cfg(test)]
mod tests {
    use super::{
        PEAK_MEMORY_SOURCE, peak_memory_failure_reason, peak_memory_source, peak_working_set_bytes,
    };

    /// Windows 上必须拿到正值；失败时必须给出可定位的 Win32 原因，
    /// 而不是静默返回 `None`。
    #[test]
    fn peak_memory_is_always_available_on_windows() {
        match peak_working_set_bytes() {
            Some(bytes) => {
                assert!(bytes > 0, "peak working set must be positive");
                assert!(
                    peak_memory_failure_reason().is_none(),
                    "a successful sample must not report a failure reason"
                );
            }
            None => {
                let reason = peak_memory_failure_reason()
                    .expect("a failed sample must report a Win32 failure reason");
                panic!("GetProcessMemoryInfo failed: {reason}");
            }
        }
        assert_eq!(peak_memory_source(), PEAK_MEMORY_SOURCE);
    }
}
