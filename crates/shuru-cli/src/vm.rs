use std::collections::HashMap;
use std::ffi::CString;
use std::io::IsTerminal;

use anyhow::{bail, Context, Result};

extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
}

pub(crate) fn clone_file(src: &str, dst: &str) -> Result<()> {
    let c_src = CString::new(src).context("invalid source path")?;
    let c_dst = CString::new(dst).context("invalid destination path")?;
    let ret = unsafe { clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        bail!("clonefile({} -> {}) failed: {}", src, dst, err);
    }
    Ok(())
}

use shuru_vm::{MountConfig, PortMapping, Sandbox};

use crate::assets;
use crate::cli::VmArgs;
use crate::config::ShuruConfig;

pub(crate) struct PreparedVm {
    pub instance_dir: String,
    pub work_rootfs: String,
    pub kernel_path: String,
    pub initrd_path: Option<String>,
    pub cpus: usize,
    pub memory: u64,
    pub disk_size: u64,
    pub proxy_config: Option<shuru_proxy::config::ProxyConfig>,
    pub verbose: bool,
    pub forwards: Vec<PortMapping>,
    pub mounts: Vec<MountConfig>,
}

pub(crate) fn prepare_vm(vm: &VmArgs, cfg: &ShuruConfig, from: Option<&str>) -> Result<PreparedVm> {
    let cpus = vm.cpus.or(cfg.cpus).unwrap_or(2);
    let memory = vm.memory.or(cfg.memory).unwrap_or(2048);
    let disk_size = vm.disk_size.or(cfg.disk_size).unwrap_or(4096);
    let allow_net = vm.allow_net || cfg.allow_net.unwrap_or(false);
    let verbose = vm.verbose;

    let proxy_config = if allow_net {
        let mut proxy = cfg.to_proxy_config();

        // Merge --secret flags: NAME=VALUE@host1,host2
        for s in &vm.secret {
            let (name, value, hosts) = parse_secret_flag(s).with_context(|| {
                format!(
                    "invalid --secret: '{}' (expected NAME=VALUE@host1,host2)",
                    s
                )
            })?;
            proxy
                .secrets
                .insert(name, shuru_proxy::config::SecretConfig { value, hosts });
        }

        // Merge --allow-domain flags
        for d in &vm.allow_host {
            proxy.network.allow.push(d.clone());
        }

        Some(proxy)
    } else {
        None
    };

    // Merge port forwards: CLI flags + config file
    let mut port_strs: Vec<&str> = vm.port.iter().map(|s| s.as_str()).collect();
    if let Some(ref cfg_ports) = cfg.ports {
        for p in cfg_ports {
            port_strs.push(p.as_str());
        }
    }
    let mut forwards = Vec::new();
    for s in &port_strs {
        let mapping =
            parse_port_mapping(s).with_context(|| format!("invalid port mapping: '{}'", s))?;
        forwards.push(mapping);
    }

    // Merge mounts: CLI flags + config file
    let mut mount_strs: Vec<&str> = vm.mount.iter().map(|s| s.as_str()).collect();
    if let Some(ref cfg_mounts) = cfg.mounts {
        for m in cfg_mounts {
            mount_strs.push(m.as_str());
        }
    }
    let mut mounts = Vec::new();
    for s in &mount_strs {
        let mc = parse_mount_spec(s).with_context(|| format!("invalid mount spec: '{}'", s))?;
        mounts.push(mc);
    }

    let data_dir = shuru_vm::default_data_dir();

    // Auto-download assets when using default paths
    if vm.kernel.is_none()
        && vm.rootfs.is_none()
        && vm.initrd.is_none()
        && !assets::assets_ready(&data_dir)
    {
        assets::download_os_image(&data_dir)?;
    }

    let kernel_path = vm
        .kernel
        .clone()
        .unwrap_or_else(|| format!("{}/Image", data_dir));
    let rootfs_path = vm
        .rootfs
        .clone()
        .unwrap_or_else(|| format!("{}/rootfs.ext4", data_dir));
    let initrd_path_str = vm
        .initrd
        .clone()
        .unwrap_or_else(|| format!("{}/initramfs.cpio.gz", data_dir));

    if !std::path::Path::new(&kernel_path).exists() {
        bail!(
            "Kernel not found at {}. Run `shuru init` to download.",
            kernel_path
        );
    }

    // Determine source for working copy: checkpoint or base rootfs
    let checkpoints_dir = format!("{}/checkpoints", data_dir);
    let source = match from {
        Some(name) => {
            let path = format!("{}/{}.ext4", checkpoints_dir, name);
            if !std::path::Path::new(&path).exists() {
                bail!("Checkpoint '{}' not found", name);
            }
            path
        }
        None => {
            if !std::path::Path::new(&rootfs_path).exists() {
                bail!(
                    "Rootfs not found at {}. Run `shuru init` to download.",
                    rootfs_path
                );
            }
            rootfs_path
        }
    };

    // Create per-instance working copy (clean any stale dir from PID reuse)
    let instance_dir = format!("{}/instances/{}", data_dir, std::process::id());
    let _ = std::fs::remove_dir_all(&instance_dir);
    std::fs::create_dir_all(&instance_dir)?;
    let work_rootfs = format!("{}/rootfs.ext4", instance_dir);
    if verbose {
        eprintln!("shuru: creating working copy...");
    }
    clone_file(&source, &work_rootfs)?;

    // Extend to requested disk size
    let f = std::fs::OpenOptions::new().write(true).open(&work_rootfs)?;
    let target = disk_size * 1024 * 1024;
    let current = f.metadata()?.len();
    if target < current {
        bail!(
            "--disk-size {}MB is smaller than the base image ({}MB)",
            disk_size,
            current / 1024 / 1024
        );
    }
    if target > current {
        f.set_len(target)?;
    }
    drop(f);

    let initrd_path = if std::path::Path::new(&initrd_path_str).exists() {
        Some(initrd_path_str)
    } else {
        eprintln!(
            "shuru: warning: initramfs not found at {}, booting without it",
            initrd_path_str
        );
        None
    };

    Ok(PreparedVm {
        instance_dir,
        work_rootfs,
        kernel_path,
        initrd_path,
        cpus,
        memory,
        disk_size,
        proxy_config,
        verbose,
        forwards,
        mounts,
    })
}

pub(crate) fn build_sandbox(
    prepared: &PreparedVm,
    console: bool,
    network_fd: Option<i32>,
) -> Result<Sandbox> {
    let mut builder = Sandbox::builder()
        .kernel(&prepared.kernel_path)
        .rootfs(&prepared.work_rootfs)
        .cpus(prepared.cpus)
        .memory_mb(prepared.memory)
        .console(console)
        .verbose(prepared.verbose);

    if let Some(fd) = network_fd {
        builder = builder.network_fd(fd);
    }

    if let Some(initrd) = &prepared.initrd_path {
        builder = builder.initrd(initrd);
    }

    for m in &prepared.mounts {
        builder = builder.mount(m.clone());
    }

    builder.build()
}

pub(crate) fn run_command(prepared: &PreparedVm, command: &[String]) -> Result<i32> {
    if prepared.verbose {
        eprintln!("shuru: kernel={}", prepared.kernel_path);
        eprintln!("shuru: rootfs={} (work copy)", prepared.work_rootfs);
    }
    eprintln!(
        "shuru: booting VM ({}cpus, {}MB RAM, {}MB disk)...",
        prepared.cpus, prepared.memory, prepared.disk_size
    );

    // Set up proxy networking if --allow-net
    let (vm_fd, proxy_handle) = if let Some(ref proxy_config) = prepared.proxy_config {
        let (vm_fd, host_fd) = shuru_proxy::create_socketpair()?;
        let handle = shuru_proxy::start(host_fd, proxy_config.clone())?;

        if prepared.verbose {
            eprintln!("shuru: proxy started");
        }

        (Some(vm_fd), Some(handle))
    } else {
        (None, None)
    };

    let sandbox = build_sandbox(prepared, false, vm_fd)?;
    if prepared.verbose {
        eprintln!("shuru: VM created and validated successfully");
    }

    sandbox.start()?;
    if prepared.verbose {
        eprintln!("shuru: VM started, waiting for guest...");
    }

    let _fwd = if !prepared.forwards.is_empty() {
        Some(sandbox.start_port_forwarding(&prepared.forwards)?)
    } else {
        None
    };

    // Inject CA cert and secret placeholders when MITM is needed
    let mut env = HashMap::new();
    if let Some(ref handle) = proxy_handle {
        if !handle.placeholders.is_empty() {
            sandbox.write_file(
                "/usr/local/share/ca-certificates/shuru-proxy.crt",
                &handle.ca_cert_pem,
            )?;
            sandbox.exec(
                &["update-ca-certificates", "--fresh"],
                &mut std::io::sink(),
                &mut std::io::sink(),
            )?;
            if prepared.verbose {
                eprintln!("shuru: proxy CA certificate injected");
            }
            for (name, placeholder) in &handle.placeholders {
                env.insert(name.clone(), placeholder.clone());
            }
        }
    }

    let exit_code = if std::io::stdin().is_terminal() {
        sandbox.shell(command, &env)?
    } else {
        sandbox.exec_with_env(
            command,
            &env,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        )?
    };

    drop(proxy_handle);
    let _ = sandbox.stop();
    Ok(exit_code)
}

fn parse_mount_spec(s: &str) -> Result<MountConfig> {
    let parts: Vec<&str> = s.splitn(2, ':').collect();
    if parts.len() < 2 {
        bail!("expected HOST:GUEST format (e.g. ./src:/workspace)");
    }

    let host_path = std::fs::canonicalize(parts[0])
        .with_context(|| format!("host path does not exist: '{}'", parts[0]))?
        .to_string_lossy()
        .to_string();

    let guest_path = parts[1].to_string();
    if !guest_path.starts_with('/') {
        bail!(
            "guest path must be absolute (start with /): '{}'",
            guest_path
        );
    }

    Ok(MountConfig {
        host_path,
        guest_path,
    })
}

/// Parse `NAME=VALUE@host1,host2` into (name, value, hosts).
fn parse_secret_flag(s: &str) -> Result<(String, String, Vec<String>)> {
    let (name, rest) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("missing '=' separator"))?;
    let (value, hosts_str) = rest
        .rsplit_once('@')
        .ok_or_else(|| anyhow::anyhow!("missing '@' separator for hosts"))?;
    let hosts: Vec<String> = hosts_str
        .split(',')
        .map(|h| h.trim())
        .filter(|h| !h.is_empty())
        .map(|h| h.to_string())
        .collect();
    if name.is_empty() || value.is_empty() || hosts.is_empty() {
        bail!("name, value, and hosts must all be non-empty");
    }
    Ok((name.to_string(), value.to_string(), hosts))
}

fn parse_port_mapping(s: &str) -> Result<PortMapping> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        bail!("expected HOST:GUEST format (e.g. 8080:80)");
    }
    let host_port: u16 = parts[0]
        .parse()
        .with_context(|| format!("invalid host port: '{}'", parts[0]))?;
    let guest_port: u16 = parts[1]
        .parse()
        .with_context(|| format!("invalid guest port: '{}'", parts[1]))?;
    Ok(PortMapping {
        host_port,
        guest_port,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_secret_flag;

    #[test]
    fn parses_literal_secret_flag() {
        let (name, value, hosts) =
            parse_secret_flag("API_KEY=sk-test@api.openai.com").expect("flag should parse");

        assert_eq!(name, "API_KEY");
        assert_eq!(value, "sk-test");
        assert_eq!(hosts, vec!["api.openai.com"]);
    }

    #[test]
    fn parses_secret_flag_using_last_at_as_host_separator() {
        let (name, value, hosts) =
            parse_secret_flag("AUTH_TOKEN=tok@segment@api.openai.com,api.anthropic.com")
                .expect("flag should parse");

        assert_eq!(name, "AUTH_TOKEN");
        assert_eq!(value, "tok@segment");
        assert_eq!(hosts, vec!["api.openai.com", "api.anthropic.com"]);
    }

    #[test]
    fn rejects_secret_flag_without_hosts() {
        let err =
            parse_secret_flag("API_KEY=sk-test@").expect_err("flag without hosts should fail");

        assert!(err
            .to_string()
            .contains("name, value, and hosts must all be non-empty"));
    }
}
