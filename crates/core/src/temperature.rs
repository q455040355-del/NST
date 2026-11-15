//! 温度与功耗：CPU 走 WMI 热区，GPU 走 `nvidia-smi --loop`。
//!
//! 两个采集都在后台线程，UI 只读缓存值；超过 15 秒没更新就当无效（显示 `--`）。

use std::io::{BufRead, BufReader};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JobObjectExtendedLimitInformation,
};

const STALE_AFTER: Duration = Duration::from_secs(15);

/// 子进程闸门（Job Object，`KILL_ON_JOB_CLOSE`）。
///
/// nvidia-smi 是常驻子进程；以前退出时 `WM_DESTROY` 直接 `process::exit`，
/// 后台线程根本走不到 `child.kill()`，于是每次退出都留一个 nvidia-smi
/// 在后台每秒唤醒查 GPU（实测攒了 27 个）。
///
/// 挂进 Job 之后，本进程的句柄表无论怎么没的（正常退出 / 强杀 / 崩溃），
/// 系统都会关闭 Job 句柄并连带杀掉里面所有子进程。
struct ChildJob(*mut std::ffi::c_void);

impl ChildJob {
    fn new() -> Option<Self> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(job);
                return None;
            }

            Some(Self(job))
        }
    }

    fn adopt(&self, child: &std::process::Child) -> bool {
        unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as *mut std::ffi::c_void) != 0 }
    }
}

impl Drop for ChildJob {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TemperatureSnapshot {
    pub cpu_celsius: f64,
    pub gpu_celsius: f64,
    pub gpu_watts: f64,
}

#[derive(Clone, Copy)]
struct Reading {
    celsius: f64,
    watts: f64,
    at: Option<Instant>,
}

pub struct TemperatureMonitor {
    cpu: Arc<Mutex<Reading>>,
    gpu: Arc<Mutex<Reading>>,
    stop: Arc<AtomicBool>,
}

impl TemperatureMonitor {
    pub fn start() -> Self {
        let cpu = Arc::new(Mutex::new(Reading {
            celsius: f64::NAN,
            watts: f64::NAN,
            at: None,
        }));
        let gpu = Arc::new(Mutex::new(Reading {
            celsius: f64::NAN,
            watts: f64::NAN,
            at: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));

        spawn_cpu_reader(Arc::clone(&cpu), Arc::clone(&stop));
        spawn_gpu_reader(Arc::clone(&gpu), Arc::clone(&stop));

        Self { cpu, gpu, stop }
    }

    pub fn read(&self) -> TemperatureSnapshot {
        let cpu = self.cpu.lock().map(|value| *value).unwrap_or(Reading {
            celsius: f64::NAN,
            watts: f64::NAN,
            at: None,
        });
        let gpu = self.gpu.lock().map(|value| *value).unwrap_or(Reading {
            celsius: f64::NAN,
            watts: f64::NAN,
            at: None,
        });

        let fresh = |reading: &Reading| {
            reading
                .at
                .map(|at| at.elapsed() < STALE_AFTER)
                .unwrap_or(false)
        };

        TemperatureSnapshot {
            cpu_celsius: if fresh(&cpu) { cpu.celsius } else { f64::NAN },
            gpu_celsius: if fresh(&gpu) { gpu.celsius } else { f64::NAN },
            gpu_watts: if fresh(&gpu) { gpu.watts } else { f64::NAN },
        }
    }
}

impl Drop for TemperatureMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn spawn_cpu_reader(slot: Arc<Mutex<Reading>>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        // wmi 0.18 自己处理 COM 初始化
        let Ok(connection) = wmi::WMIConnection::with_namespace_path("ROOT\\CIMV2") else {
            return;
        };

        while !stop.load(Ordering::Relaxed) {
            if let Some(celsius) = query_cpu(&connection) {
                if let Ok(mut reading) = slot.lock() {
                    reading.celsius = celsius;
                    reading.at = Some(Instant::now());
                }
            }

            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

fn query_cpu(connection: &wmi::WMIConnection) -> Option<f64> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ThermalZone {
        #[serde(default)]
        high_precision_temperature: Option<u32>,
        #[serde(default)]
        temperature: Option<u32>,
    }

    let rows: Vec<ThermalZone> = connection
        .raw_query(
            "SELECT HighPrecisionTemperature, Temperature \
             FROM Win32_PerfFormattedData_Counters_ThermalZoneInformation",
        )
        .ok()?;

    let mut max = f64::NAN;
    for row in rows {
        // ACPI 热区单位是 0.1K
        let value = row
            .high_precision_temperature
            .or(row.temperature)
            .map(|raw| raw as f64 / 10.0 - 273.15);

        if let Some(value) = value {
            if value > 0.0 && value < 130.0 && (max.is_nan() || value > max) {
                max = value;
            }
        }
    }

    if max.is_nan() {
        None
    } else {
        Some(max)
    }
}

fn spawn_gpu_reader(slot: Arc<Mutex<Reading>>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        // 先建 Job：子进程一 spawn 就挂进去，退出时系统连带清理
        let job = ChildJob::new();

        // nvidia-smi 自己每秒输出一行，比反复起进程便宜
        let mut child = Command::new("nvidia-smi.exe")
            .args([
                "--query-gpu=temperature.gpu,power.draw",
                "--format=csv,noheader,nounits",
                "--loop=1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn();

        if child.is_err() {
            // 没有 nvidia-smi：退回 PATH 再试一次
            child = Command::new("nvidia-smi")
                .args([
                    "--query-gpu=temperature.gpu,power.draw",
                    "--format=csv,noheader,nounits",
                    "--loop=1",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(0x0800_0000)
                .spawn();
        }

        let Ok(mut child) = child else {
            return;
        };

        if let Some(job) = job.as_ref() {
            job.adopt(&child);
        }

        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return;
        };

        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let Ok(line) = line else {
                continue;
            };

            let mut parts = line.split(',');
            let celsius = parts
                .next()
                .and_then(|value| value.trim().parse::<f64>().ok())
                .unwrap_or(f64::NAN);
            let watts = parts
                .next()
                .and_then(|value| value.trim().parse::<f64>().ok())
                .unwrap_or(f64::NAN);

            if celsius.is_nan() && watts.is_nan() {
                continue;
            }

            if let Ok(mut reading) = slot.lock() {
                reading.celsius = celsius;
                reading.watts = watts;
                reading.at = Some(Instant::now());
            }
        }

        let _ = child.kill();
        let _ = child.wait();
        // job 在这里 drop → 关句柄，子进程（如果还活着）被系统一并杀掉
        drop(job);
    });
}

/// 温度文本：NaN → `--`。
pub fn format_temperature(celsius: f64) -> String {
    if !celsius.is_finite() {
        return "--".to_string();
    }

    format!("{celsius:.0}°C")
}

/// 温度行后面拼功耗：`56°C 45W`。
pub fn append_watts(text: String, watts: f64) -> String {
    if !watts.is_finite() || watts <= 0.0 || watts >= 1000.0 {
        return text;
    }

    if text == "--" {
        format!("{watts:.0}W")
    } else {
        format!("{text} {watts:.0}W")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_units() {
        assert_eq!(format_temperature(f64::NAN), "--");
        assert_eq!(format_temperature(56.4), "56°C");
    }

    #[test]
    fn watts_suffix() {
        assert_eq!(append_watts("56°C".to_string(), 45.2), "56°C 45W");
        assert_eq!(append_watts("--".to_string(), 45.2), "45W");
        assert_eq!(append_watts("56°C".to_string(), f64::NAN), "56°C");
    }
}
