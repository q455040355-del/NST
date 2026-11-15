/// 数值格式化：和 C# 版面板保持一致的口径。
///
/// 网速：B/s < 1K 用字节，< 1M 用 K/s（一位小数），< 1G 用 M/s（两位小数），否则 G/s。
pub fn format_speed(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second < 0.0 {
        return "0.0 K/s".to_string();
    }

    let value = bytes_per_second;
    if value >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} G/s", value / (1024.0 * 1024.0 * 1024.0))
    } else if value >= 1024.0 * 1024.0 {
        format!("{:.2} M/s", value / (1024.0 * 1024.0))
    } else if value >= 1024.0 {
        format!("{:.1} K/s", value / 1024.0)
    } else if value >= 1.0 {
        format!("{value:.0} B/s")
    } else {
        "0.0 K/s".to_string()
    }
}

/// 温度：NaN 用 `--`，否则取整加 °C。
pub fn format_temperature(celsius: f64) -> String {
    if !celsius.is_finite() {
        return "--".to_string();
    }

    format!("{:.0}°C", celsius)
}

/// 内存：字节转 GB，保留一位小数。
pub fn format_bytes_gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_units() {
        assert_eq!(format_speed(0.0), "0.0 K/s");
        assert_eq!(format_speed(600.0), "600 B/s");
        assert_eq!(format_speed(1024.0), "1.0 K/s");
        assert_eq!(format_speed(17_600.0), "17.2 K/s");
        assert_eq!(format_speed(140_000.0), "136.7 K/s");
        assert_eq!(format_speed(1_500_000.0), "1.43 M/s");
        assert_eq!(format_speed(2.0 * 1024.0 * 1024.0 * 1024.0), "2.00 G/s");
    }

    #[test]
    fn temperature_units() {
        assert_eq!(format_temperature(f64::NAN), "--");
        assert_eq!(format_temperature(56.4), "56°C");
    }

    #[test]
    fn memory_units() {
        assert_eq!(format_bytes_gb(16 * 1024 * 1024 * 1024), "16.0 GB");
    }
}
