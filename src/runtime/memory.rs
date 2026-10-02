//! 进程峰值内存采集。
//!
//! benchmark 与评测工具需要真实的内存峰值，而不是估算值。这里使用平台自带接口：
//!
//! - Windows：`GetProcessMemoryInfo(...)->PeakWorkingSetSize`（PSAPI，无需额外依赖）；
//! - Linux：`/proc/self/status` 的 `VmHWM`；
//! - 其它平台：返回 `None`，并在报告里显式说明未采集。
//!
//! 该能力属于共享层：普通 OCR 与公式 benchmark 使用同一实现，避免出现两套口径。

/// 当前进程的峰值工作集（字节）。平台不支持时返回 `None`。
pub fn peak_working_set_bytes() -> Option<u64> {
    peak_working_set_impl()
}

/// 峰值内存数据的来源标识，写入报告以便审查。
pub fn peak_memory_source() -> &'static str {
    if cfg!(windows) {
        "windows:GetProcessMemoryInfo.PeakWorkingSetSize"
    } else if cfg!(target_os = "linux") {
        "linux:/proc/self/status:VmHWM"
    } else {
        "unsupported"
    }
}

#[cfg(windows)]
fn peak_working_set_impl() -> Option<u64> {
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
    }

    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    // SAFETY: `counters` is a correctly sized, zero-initialized POD structure;
    // `GetCurrentProcess` returns a pseudo-handle that must not be closed.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then_some(counters.peak_working_set_size as u64)
}

#[cfg(target_os = "linux")]
fn peak_working_set_impl() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kilobytes: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kilobytes * 1024);
        }
    }
    None
}

#[cfg(not(any(windows, target_os = "linux")))]
fn peak_working_set_impl() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::{peak_memory_source, peak_working_set_bytes};

    #[test]
    fn peak_memory_is_reported_or_explicitly_unsupported() {
        match peak_working_set_bytes() {
            Some(bytes) => {
                assert!(bytes > 0, "peak working set must be positive");
                assert_ne!(peak_memory_source(), "unsupported");
            }
            None => assert_eq!(peak_memory_source(), "unsupported"),
        }
    }
}
