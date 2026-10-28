//! The host firewall behind the proxy check: whatever the network
//! processes send, by their uid, reaches public IPv4 addresses only.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

pub const NETD_UID_BASE: u32 = 200_000;
const NETD_UID_LAST: u32 = 299_999;

const NON_PUBLIC_V4: &str = "0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, \
    169.254.0.0/16, 172.16.0.0/12, 192.0.0.0/24, 192.0.2.0/24, 192.88.99.0/24, \
    192.168.0.0/16, 198.18.0.0/15, 198.51.100.0/24, 203.0.113.0/24, 224.0.0.0/3";

fn ruleset() -> String {
    let uids = format!("{NETD_UID_BASE}-{NETD_UID_LAST}");
    // Creating the table first lets the delete succeed on a fresh host;
    // nft applies the whole file as one transaction.
    format!(
        "table inet shuru {{}}
delete table inet shuru
table inet shuru {{
    chain egress {{
        type filter hook output priority filter; policy accept;
        meta skuid {uids} ip daddr {{ {NON_PUBLIC_V4} }} counter drop
        meta skuid {uids} meta nfproto ipv6 counter drop
    }}
}}
"
    )
}

/// Replaces the `inet shuru` table.
pub fn install() -> Result<()> {
    let mut nft = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running nft; is nftables installed?")?;
    nft.stdin.take().unwrap().write_all(ruleset().as_bytes())?;
    let output = nft.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "nft refused the shuru table: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ruleset_names_the_netd_uids_and_the_metadata_service() {
        let rules = ruleset();
        assert!(rules.contains("meta skuid 200000-299999 ip daddr"));
        assert!(rules.contains("169.254.0.0/16"));
        assert!(rules.contains("meta nfproto ipv6 counter drop"));
    }
}
