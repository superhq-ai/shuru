//! Watch guest memory go back to the host.
//!
//! Boots a 2 GB sandbox, has the guest fill and free 1 GB, then lowers and
//! raises the memory target. After each step it prints the resident size
//! of this process, which is where guest memory lives on a Linux host, and
//! what the guest sees in /proc/meminfo. The guest reports free memory in
//! rounds two seconds apart, a sixteenth of each free list per round, so
//! a freed gigabyte drains in about ten seconds.
//!
//!     SHURU_DATA_DIR=... cargo run -p shuru-sdk --example balloon

use std::time::Duration;

use shuru_sdk::{AsyncSandbox, SandboxConfig};

const MB: u64 = 1024 * 1024;

fn host_rss_mb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok();
    out.and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

async fn guest_meminfo(sb: &AsyncSandbox) -> String {
    let out = match sb
        .exec_shell("grep -E \"MemTotal|MemFree\" /proc/meminfo")
        .await
    {
        Ok(r) => r.stdout,
        Err(e) => return format!("meminfo failed: {}", e),
    };
    out.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let kb: u64 = parts.next()?.parse().ok()?;
            Some(format!("{} {} MB", name, kb / 1024))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

async fn report(sb: &AsyncSandbox, step: &str) {
    println!(
        "{:<40} host rss {:>5} MB   guest {}",
        step,
        host_rss_mb(),
        guest_meminfo(sb).await
    );
}

async fn settle(sb: &AsyncSandbox, step: &str, seconds: u64) {
    for s in 1..=seconds {
        tokio::time::sleep(Duration::from_secs(1)).await;
        report(sb, &format!("{} +{}s", step, s)).await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = SandboxConfig {
        data_dir: std::env::var("SHURU_DATA_DIR").ok(),
        memory_mb: 2048,
        ..Default::default()
    };
    let sb = AsyncSandbox::boot(config).await?;
    report(&sb, "booted").await;

    let fill = sb
        .exec_shell(
            "mount -o remount,size=1536m /dev/shm && dd if=/dev/zero of=/dev/shm/fill bs=1M count=1024 status=none",
        )
        .await?;
    if fill.exit_code != 0 {
        anyhow::bail!("filling tmpfs failed: {}", fill.stderr.trim());
    }
    report(&sb, "guest holds 1 GB in tmpfs").await;

    sb.exec_shell("rm /dev/shm/fill").await?;
    settle(&sb, "guest freed it", 24).await;

    sb.set_memory_target(1024 * MB).await?;
    settle(&sb, "target 1 GB", 4).await;

    sb.set_memory_target(2048 * MB).await?;
    settle(&sb, "target 2 GB", 2).await;

    sb.stop().await?;
    Ok(())
}
