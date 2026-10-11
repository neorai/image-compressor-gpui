use std::time::Duration;

pub fn duration(duration: Duration) -> String {
    if duration.as_secs() >= 60 {
        format!("{:.1} min", duration.as_secs_f64() / 60.0)
    } else {
        format!("{:.1} s", duration.as_secs_f64())
    }
}

pub fn signed_bytes(value: i128) -> String {
    if value < 0 {
        format!(
            "−{}",
            bytes(u64::try_from(value.unsigned_abs()).unwrap_or(u64::MAX))
        )
    } else {
        bytes(u64::try_from(value).unwrap_or(u64::MAX))
    }
}

pub fn bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * KIB;
    const GIB: f64 = 1024.0 * MIB;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

/// Removes the Windows verbatim prefix (`\\?\`) that canonical paths carry,
/// which is noise for a reader. The stored path itself is left unchanged.
pub fn display_path(path: &str) -> String {
    if let Some(share) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{share}")
    } else if let Some(local) = path.strip_prefix(r"\\?\") {
        local.to_owned()
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{bytes, display_path, signed_bytes};

    #[test]
    fn byte_formatting_handles_positive_and_negative_savings() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(signed_bytes(-1536), "−1.5 KiB");
        assert_eq!(signed_bytes(i128::MIN), format!("−{}", bytes(u64::MAX)));
    }

    #[test]
    fn verbatim_prefixes_are_hidden() {
        assert_eq!(display_path(r"\\?\C:\Photos\a.jpg"), r"C:\Photos\a.jpg");
        assert_eq!(
            display_path(r"\\?\UNC\server\share\a.jpg"),
            r"\\server\share\a.jpg"
        );
        assert_eq!(display_path("/home/a.jpg"), "/home/a.jpg");
    }
}
