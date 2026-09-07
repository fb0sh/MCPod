//! Small shared utilities.

/// Short random hex string (temp file names, bash output logs). Prefers
/// /dev/urandom and falls back to a time+counter mix.
pub fn random_hex(byte_len: usize) -> String {
    let byte_len = byte_len.min(8);
    let mut buf = vec![0u8; byte_len];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if file.read_exact(&mut buf).is_ok() {
            return buf.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = nanos
        ^ COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:0width$x}", n, width = byte_len * 2)
}

#[cfg(test)]
mod tests {
    use super::random_hex;

    #[test]
    fn produces_distinct_hex_strings() {
        let a = random_hex(4);
        let b = random_hex(4);
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
