//! 内存占用：GlobalMemoryStatusEx。

use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

#[derive(Debug, Clone, Copy, Default)]
pub struct MemorySnapshot {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

impl MemorySnapshot {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }

    pub fn used_ratio(&self) -> f32 {
        if self.total_bytes == 0 {
            return 0.0;
        }

        (self.used_bytes() as f64 / self.total_bytes as f64) as f32
    }
}

pub fn read_memory() -> MemorySnapshot {
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;

    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 {
        return MemorySnapshot::default();
    }

    MemorySnapshot {
        total_bytes: status.ullTotalPhys,
        available_bytes: status.ullAvailPhys,
    }
}
