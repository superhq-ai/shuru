use std::io;
#[cfg(target_os = "linux")]
use std::sync::RwLock;

use shuru_proto::hello::Identity;

// Set from the kernel command line at boot and replaced after a restore.
#[cfg(target_os = "linux")]
static CURRENT: RwLock<Identity> = RwLock::new(Identity {
    token: [0; 32],
    instance: [0; 16],
});

#[cfg(target_os = "linux")]
pub fn current() -> Identity {
    *CURRENT.read().unwrap()
}

#[cfg(target_os = "linux")]
pub fn set(identity: Identity) {
    *CURRENT.write().unwrap() = identity;
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "missing or invalid guest identity",
    )
}

fn parameter<const N: usize>(cmdline: &str, name: &str) -> io::Result<[u8; N]> {
    let mut values = cmdline.split_ascii_whitespace().filter_map(|arg| {
        let (key, value) = arg.split_once("=")?;
        (key == name).then_some(value)
    });
    let value = values.next().ok_or_else(invalid)?;
    if value.len() != N * 2
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || values.next().is_some()
    {
        return Err(invalid());
    }
    let mut bytes = [0; N];
    for (out, pair) in bytes.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        let hex = std::str::from_utf8(pair).map_err(|_| invalid())?;
        *out = u8::from_str_radix(hex, 16).map_err(|_| invalid())?;
    }
    Ok(bytes)
}

pub fn parse(cmdline: &str) -> io::Result<Identity> {
    Ok(Identity {
        token: parameter(cmdline, "shuru.token")?,
        instance: parameter(cmdline, "shuru.instance")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_identity() {
        let cmdline = format!(
            "quiet shuru.token={} shuru.instance={} root=/dev/vda",
            "ab".repeat(32),
            "CD".repeat(16)
        );
        let identity = parse(&cmdline).unwrap();
        assert_eq!(identity.token, [0xab; 32]);
        assert_eq!(identity.instance, [0xcd; 16]);
        assert!(parse(&format!("{cmdline} shuru.token={}", "ab".repeat(32))).is_err());
        assert!(parse(&cmdline.replace("ab", "zz")).is_err());
        assert!(parse(&cmdline.replace("ab", "+1")).is_err());
        assert!(parse(&cmdline.replace("CD", "C")).is_err());
        assert!(parse("").is_err());
        assert!(parse(&format!("shuru.token={}", "00".repeat(32))).is_err());
    }
}
