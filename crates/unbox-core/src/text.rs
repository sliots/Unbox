//! User-facing text.

pub fn format_bytes(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.1} KB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MB", b / (K * K))
    } else {
        format!("{:.1} GB", b / (K * K * K))
    }
}

/// A remaining time as people say it: coarser the further away it is, so the text
/// does not tick every second (under a minute in 5 s steps, under ten minutes in 10 s
/// steps, then whole minutes).
pub fn format_duration(seconds: f64) -> String {
    if seconds < 5.0 {
        return "几秒钟".into();
    }
    let up_to = |value: f64, step: u64| ((value / step as f64).ceil() as u64) * step;
    if seconds < 60.0 {
        let s = up_to(seconds, 5);
        return if s >= 60 { "1 分钟".into() } else { format!("{s} 秒") };
    }
    if seconds < 600.0 {
        let total = up_to(seconds, 10);
        return match total % 60 {
            0 => format!("{} 分钟", total / 60),
            s => format!("{} 分 {s} 秒", total / 60),
        };
    }
    let minutes = up_to(seconds, 60) / 60;
    if minutes < 60 {
        return format!("{minutes} 分钟");
    }
    format!("{} 小时 {} 分钟", minutes / 60, minutes % 60)
}

pub fn format_speed(mbps: f64) -> String {
    if mbps > 0.0 && mbps < 10.0 {
        format!("{mbps:.1} MB/秒")
    } else {
        format!("{mbps:.0} MB/秒")
    }
}

/// Text for a worker `error_reason`.
pub fn describe_failure(reason: &str, exit_code: i32, status: &str) -> String {
    match reason {
        "password_or_data_error" => "密码错误或数据损坏，源压缩包已保留。".into(),
        "wrong_password" => "归档中至少有一个条目的密码不匹配，源压缩包已保留。".into(),
        "crc_error" => "文件数据校验 (CRC) 错误，已放弃交付，源压缩包已保留。".into(),
        "data_error" => "条目数据损坏或归档不完整，已放弃交付，源压缩包已保留。".into(),
        "unsupported_method" => "压缩算法不受支持，已放弃交付，源压缩包已保留。".into(),
        "corrupted_archive" | "unrecognized_or_corrupted_inner_archive" | "corrupted_inner_archive" => {
            "归档已损坏或缺少后续分卷，已放弃交付，源压缩包已保留。".into()
        }
        "inner_not_tar" => "复合 TAR 归档的内层不是有效的 TAR 内容，已放弃交付，源压缩包已保留。".into(),
        "inner_payload_missing" => "复合 TAR 归档缺少可展开的内层载荷，已放弃交付，源压缩包已保留。".into(),
        "insufficient_disk_space" => "目标磁盘可用空间不足，已安全终止，源压缩包已保留。".into(),
        "prompt_timeout" => "等待输入超时，源压缩包已保留。".into(),
        "output_budget_exceeded" => "实际写出数据超出预算（疑似声明大小失真），已安全终止，源压缩包已保留。".into(),
        "extract_error" => "写入目标文件时出错，已放弃交付，源压缩包已保留。".into(),
        "manifest_write_failed" => "无法写入交付清单，已放弃交付，源压缩包已保留。".into(),
        _ => format!("解压未完整结束（状态：{status}，工作进程退出码：{exit_code}），源压缩包已安全保留。"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(format_bytes(10), "10 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(3 << 30), "3.0 GB");
        assert_eq!(format_duration(3.0), "几秒钟");
        assert_eq!(format_duration(42.2), "45 秒");
        assert_eq!(format_duration(41.0), "45 秒");
        assert_eq!(format_duration(57.0), "1 分钟");
        assert_eq!(format_duration(125.0), "2 分 10 秒");
        assert_eq!(format_duration(175.0), "3 分钟");
        assert_eq!(format_duration(1234.0), "21 分钟");
        assert_eq!(format_duration(7300.0), "2 小时 2 分钟");
        assert_eq!(format_speed(3.21), "3.2 MB/秒");
        assert_eq!(format_speed(123.4), "123 MB/秒");
    }
}
