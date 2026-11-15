//! 网卡字节计数：GetIfTable2 取所有接口的累计收发字节，按时间差算速率。
//!
//! 只统计“已连接”的物理接口（跳过回环/隧道/断开），和任务管理器口径接近。

use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, MIB_IF_TABLE2,
};
// MIB_IF_ROW2.OperStatus == IfOperStatusUp
const IF_OPER_STATUS_UP: i32 = 1;
// IF_TYPE_*（windows-sys 没导出这两个，按 SDK 定义写死）
const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_TYPE_TUNNEL: u32 = 131;

#[derive(Debug, Clone, Copy, Default)]
pub struct Counters {
    pub in_octets: u64,
    pub out_octets: u64,
}

/// 累计计数器快照。
pub fn read_counters() -> Counters {
    let mut counters = Counters::default();
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();

    let status = unsafe { GetIfTable2(&mut table) };
    if status != 0 || table.is_null() {
        return counters;
    }

    unsafe {
        let count = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), count);
        for row in rows {
            if row.Type == IF_TYPE_SOFTWARE_LOOPBACK || row.Type == IF_TYPE_TUNNEL {
                continue;
            }

            if row.OperStatus != IF_OPER_STATUS_UP {
                continue;
            }

            counters.in_octets += row.InOctets;
            counters.out_octets += row.OutOctets;
        }

        FreeMibTable(table as *const _);
    }

    counters
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NetSample {
    pub up_bytes_per_sec: f64,
    pub down_bytes_per_sec: f64,
}

/// 按固定间隔采样并平滑（指数滑动平均，避免数字乱跳）。
pub struct NetSampler {
    last: Option<(Counters, std::time::Instant)>,
    smoothed_up: f64,
    smoothed_down: f64,
}

impl NetSampler {
    pub fn new() -> Self {
        Self {
            last: None,
            smoothed_up: 0.0,
            smoothed_down: 0.0,
        }
    }

    pub fn sample(&mut self) -> NetSample {
        let now = std::time::Instant::now();
        let counters = read_counters();

        let Some((previous, previous_at)) = self.last else {
            self.last = Some((counters, now));
            return NetSample::default();
        };

        let elapsed = now.duration_since(previous_at).as_secs_f64();
        if elapsed < 0.05 {
            return NetSample {
                up_bytes_per_sec: self.smoothed_up,
                down_bytes_per_sec: self.smoothed_down,
            };
        }

        // 计数器可能回绕/重置（休眠唤醒、换网卡），负值直接当 0
        let up_delta = counters.out_octets.saturating_sub(previous.out_octets) as f64;
        let down_delta = counters.in_octets.saturating_sub(previous.in_octets) as f64;

        let up = up_delta / elapsed;
        let down = down_delta / elapsed;

        // 0.6 新值 / 0.4 旧值：既有响应又不会抖
        self.smoothed_up = self.smoothed_up * 0.4 + up * 0.6;
        self.smoothed_down = self.smoothed_down * 0.4 + down * 0.6;

        self.last = Some((counters, now));

        NetSample {
            up_bytes_per_sec: if self.smoothed_up < 1.0 { 0.0 } else { self.smoothed_up },
            down_bytes_per_sec: if self.smoothed_down < 1.0 { 0.0 } else { self.smoothed_down },
        }
    }
}

impl Default for NetSampler {
    fn default() -> Self {
        Self::new()
    }
}
