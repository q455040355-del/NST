//! 网卡字节计数：GetIfTable2 取所有接口的累计收发字节，按时间差算速率。
//!
//! 只统计“已连接”的物理接口（跳过回环/隧道/断开），和任务管理器口径接近。

use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2,
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
/// 一张网卡（用于设置里选择）。
#[derive(Debug, Clone)]
pub struct Adapter {
    /// 系统里的连接名，例如 "WLAN" / "以太网"
    pub alias: String,
    /// 驱动描述，作为提示用
    pub description: String,
}

fn utf16_field(raw: &[u16]) -> String {
    let end = raw.iter().position(|c| *c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end]).trim().to_string()
}

/// NDIS 过滤层/虚拟交换机的端口（名字多以 `-0000` 结尾）。
///
/// 它们和底层网卡上报同一份计数，参与求和会把网速放大好几倍，
/// 在设置里列出来也全是噪音，所以统一排除。
fn is_filter_port(row: &MIB_IF_ROW2) -> bool {
    row.InterfaceAndOperStatusFlags._bitfield & 0b10 != 0
}

/// 真正有数据要走的网卡：物理网卡，或者已经在跑流量的虚拟网卡
/// （Tailscale、vEthernet 之类）。纯虚拟的口子（vSwitch、Wi-Fi Direct）不列。
fn is_usable(row: &MIB_IF_ROW2) -> bool {
    if is_filter_port(row) {
        return false;
    }

    let hardware = row.InterfaceAndOperStatusFlags._bitfield & 0b1 != 0;
    hardware || row.InOctets + row.OutOctets > 0
}

/// 列出所有「已连接」的网卡（排除回环、隧道、过滤层）。
pub fn adapters() -> Vec<Adapter> {
    let mut list = Vec::new();
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();

    if unsafe { GetIfTable2(&mut table) } != 0 || table.is_null() {
        return list;
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
            if !is_usable(row) {
                continue;
            }

            let alias = utf16_field(&row.Alias);
            if alias.is_empty() {
                continue;
            }
            list.push(Adapter {
                alias,
                description: utf16_field(&row.Description),
            });
        }

        FreeMibTable(table as *const _);
    }

    list.sort_by(|a, b| a.alias.cmp(&b.alias));
    list.dedup_by(|a, b| a.alias == b.alias);
    list
}

pub fn read_counters(interface: Option<&str>) -> Counters {
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

            if let Some(wanted) = interface {
                if utf16_field(&row.Alias) != wanted {
                    continue;
                }
            } else if is_filter_port(row) {
                // 合计模式下必须跳过过滤层端口，否则同一份流量会被算 5~6 次
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
    /// None = 所有网卡合计；Some(连接名) = 只看这一块
    interface: Option<String>,
}

fn normalize_interface(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        None
    } else {
        Some(trimmed.to_string())
    }
}

impl NetSampler {
    pub fn new(interface: &str) -> Self {
        Self {
            last: None,
            smoothed_up: 0.0,
            smoothed_down: 0.0,
            interface: normalize_interface(interface),
        }
    }

    /// 切换网卡：清掉基线，避免换卡瞬间算出一个巨大的速率
    pub fn set_interface(&mut self, interface: &str) {
        let next = normalize_interface(interface);
        if next == self.interface {
            return;
        }
        self.interface = next;
        self.last = None;
        self.smoothed_up = 0.0;
        self.smoothed_down = 0.0;
    }

    pub fn interface(&self) -> Option<&str> {
        self.interface.as_deref()
    }

    pub fn sample(&mut self) -> NetSample {
        let now = std::time::Instant::now();
        let counters = read_counters(self.interface.as_deref());

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
        Self::new("auto")
    }
}
