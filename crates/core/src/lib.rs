//! 任务栏网速插件的核心逻辑：设置、网卡统计、内存、格式化。
//!
//! 这一层不依赖任何 UI，便于单独测试与复用（将来 CLI / 其它前端都能用）。

pub mod ai;
pub mod format;
pub mod history;
pub mod memory;
pub mod net;
pub mod settings;
pub mod temperature;

pub use ai::{format_speed as format_ai_speed, format_tokens, AiEngine, AiSnapshot};
pub use format::{format_bytes_gb, format_speed, format_temperature};
pub use memory::MemorySnapshot;
pub use net::{NetSample, NetSampler};
pub use settings::Settings;
pub use temperature::{append_watts, format_temperature as format_celsius, TemperatureMonitor, TemperatureSnapshot};
