use std::collections::HashMap;
use std::io::IsTerminal;

use anyhow::{bail, Context, Result};

use shuru_vm::{MountConfig, PortMapping, Sandbox};

use crate::assets;
use crate::cli::VmArgs;
use crate::config::ShuruConfig;

fn trace_boot(label: &str) {
    use std::sync::OnceLock;
    use std::time::Instant;
    static T0: OnceLock<Instant> = OnceLock::new();
    let t0 = *T0.get_or_init(Instant::now);
    if std::env::var("SHURU_BOOT_TRACE").is_ok() {
        eprintln!(
            "shuru-trace: {:>7.1}ms {}",
            t0.elapsed().as_secs_f64() * 1000.0,
            label
        );
    }
}

pub(crate) struct PreparedVm {
    pub source_rootfs: String,
    /// Anonymous clone of the source, alive for the run and freed by the
    /// kernel whenever the process ends.
    pub work_disk: shuru_vm::WorkDisk,
    /// If restoring from a CAS checkpoint, the index path.
    pub cas_index: Option<String>,
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
    trace_boot("prepare_vm start");
    let cpus = vm.cpus.or(cfg.cpus).unwrap_or(2);
    let memory = vm.memory.or(cfg.memory).unwrap_or(2048);
    let disk_size = vm.disk_size.or(cfg.disk_size).unwrap_or(4096);
    shuru_vm::validate_disk_size(disk_size)?;
    let allow_net = vm.allow_net || cfg.allow_net.unwrap_or(false);
    let allow_host_writes = vm.allow_host_writes || cfg.allow_host_writes.unwrap_or(false);
    let verbose = vm.verbose;

    let proxy_config = if allow_net {
        let mut proxy = cfg.to_proxy_config()?;

        if let Some(resolver) = vm.dns_resolver {
            proxy.dns_resolver = resolver;
        }

        // Merge --secret flags: NAME=ENV_VAR@host1,host2
        for s in &vm.secret {
            let (name, from, hosts) = parse_secret_flag(s).with_context(|| {
                format!("invalid --secret: '{}' (expected NAME=ENV@host1,host2)", s)
            })?;
            proxy.secrets.insert(
                name,
                shuru_proxy::config::SecretConfig::from_env(from, hosts),
            );
        }

        // Merge --allow-domain flags
        for d in &vm.allow_host {
            proxy.network.allow.push(d.clone());
        }

        // Merge --expose-host flags
        for s in &vm.expose_host {
            let mapping = crate::config::parse_expose_host(s)
                .with_context(|| format!("invalid --expose-host: '{}'", s))?;
            proxy.expose_host.push(mapping);
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

    if !mounts.is_empty() {
        validate_mounts(&mounts, allow_host_writes)?;
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
        .unwrap_or_else(|| format!("{}/initramfs.cpio", data_dir));

    if !std::path::Path::new(&kernel_path).exists() {
        bail!(
            "Kernel not found at {}. Run `shuru init` to download.",
            kernel_path
        );
    }

    // Determine source for working copy: checkpoint or base rootfs
    let checkpoints_dir = format!("{}/checkpoints", data_dir);
    let mut cas_index: Option<String> = None;
    let source = match from {
        Some(name) => {
            shuru_vm::validate_checkpoint_name(name).map_err(|e| anyhow::anyhow!(e))?;
            // Check .idx (CAS) first, then .ext4 (legacy)
            let idx_path = format!("{}/{}.idx", checkpoints_dir, name);
            let ext4_path = format!("{}/{}.ext4", checkpoints_dir, name);
            if std::path::Path::new(&idx_path).exists() {
                cas_index = Some(idx_path.clone());
                idx_path
            } else if std::path::Path::new(&ext4_path).exists() {
                ext4_path
            } else {
                bail!("Checkpoint '{}' not found", name);
            }
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

    trace_boot("prepare: config + assets checked");
    // A CAS checkpoint is served over NBD, so its disk is only a placeholder.
    if from.is_none() && vm.rootfs.is_none() {
        if let Err(e) = shuru_vm::WorkDisk::prepare_base(&source) {
            eprintln!("shuru: note: {:#}", e);
        }
    }
    let work_disk = if cas_index.is_none() {
        if verbose {
            eprintln!("shuru: creating working copy...");
        }
        shuru_vm::WorkDisk::clone_from(&source, &data_dir, disk_size)?
    } else {
        shuru_vm::WorkDisk::empty(&data_dir)?
    };
    trace_boot("prepare: rootfs cloned");

    let initrd_path = if std::path::Path::new(&initrd_path_str).exists() {
        Some(initrd_path_str)
    } else {
        bail!("required initramfs not found at {}", initrd_path_str);
    };

    trace_boot("prepare_vm done (disk cloned + extended)");
    Ok(PreparedVm {
        source_rootfs: source,
        work_disk,
        cas_index,
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
    nbd_uri: Option<&str>,
) -> Result<Sandbox> {
    let mut builder = Sandbox::builder()
        .kernel(&prepared.kernel_path)
        .rootfs(prepared.work_disk.attach_path())
        .cpus(prepared.cpus)
        .memory_mb(prepared.memory)
        .console(console)
        .verbose(prepared.verbose);

    if let Some(fd) = network_fd {
        builder = builder.network_fd(fd);
    }

    if let Some(uri) = nbd_uri {
        builder = builder.nbd_uri(uri);
    }

    if let Some(initrd) = &prepared.initrd_path {
        builder = builder.initrd(initrd);
    }

    for m in &prepared.mounts {
        builder = builder.mount(m.clone());
    }

    builder.build()
}

/// Start the CAS NBD server for a prepared VM. Direct disk mode is the
/// default; CAS is opt-in via SHURU_STORAGE=cas. Booting from a CAS
/// checkpoint always uses NBD since the chunk store is the only place its
/// content exists.
pub(crate) fn uses_cas(prepared: &PreparedVm) -> bool {
    prepared.cas_index.is_some()
        || matches!(std::env::var("SHURU_STORAGE").as_deref(), Ok("cas" | "nbd"))
}

pub(crate) fn start_nbd(prepared: &PreparedVm) -> Result<Option<shuru_store::NbdHandle>> {
    if !uses_cas(prepared) {
        return Ok(None);
    }
    let data_dir = shuru_vm::default_data_dir();
    let cas_dir = format!("{}/cas", data_dir);
    std::fs::create_dir_all(&cas_dir)?;
    let socket_path = format!(
        "{}/nbd-{}-{}.sock",
        cas_dir,
        std::process::id(),
        prepared.work_disk.as_raw_fd()
    );
    let index_path = if let Some(ref idx) = prepared.cas_index {
        idx.clone()
    } else {
        let source_hash = blake3::hash(prepared.source_rootfs.as_bytes()).to_hex();
        format!("{}/cas/indexes/{}.idx", data_dir, &source_hash[..16])
    };
    let target_size = prepared.disk_size * 1024 * 1024;
    Ok(Some(shuru_store::start_cas_nbd_server(
        &prepared.source_rootfs,
        &cas_dir,
        &index_path,
        &socket_path,
        target_size,
    )?))
}

pub(crate) struct RunResult {
    pub exit_code: i32,
    pub nbd_handle: Option<shuru_store::NbdHandle>,
}

pub(crate) fn run_command(
    prepared: &PreparedVm,
    command: &[String],
    checkpoint: Option<&str>,
) -> Result<RunResult> {
    if prepared.verbose {
        eprintln!("shuru: kernel={}", prepared.kernel_path);
        eprintln!(
            "shuru: rootfs={} (anonymous clone of {})",
            prepared.work_disk.attach_path(),
            prepared.source_rootfs
        );
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

    let nbd_handle = start_nbd(prepared)?;
    let nbd_uri = nbd_handle.as_ref().map(|h| h.uri());
    trace_boot("proxy + storage ready");

    let sandbox = build_sandbox(prepared, false, vm_fd, nbd_uri.as_deref())?;
    if prepared.verbose {
        eprintln!("shuru: VM created and validated successfully");
    }
    trace_boot("sandbox built + validated");

    sandbox.start()?;
    if prepared.verbose {
        eprintln!("shuru: VM started, waiting for guest...");
    }
    trace_boot("vm running");
    if std::env::var("SHURU_BOOT_TRACE").is_ok() {
        let _ = sandbox.wait_ready();
        trace_boot("guest vsock ready");
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
    trace_boot("command done");

    if let Some(dest) = checkpoint {
        let report = sandbox.save_disk(dest, shuru_vm::AfterSave::PowerOff, |path| {
            if let Some(ref handle) = nbd_handle {
                handle.save_checkpoint(path)
            } else {
                prepared.work_disk.save_to(path)
            }
        })?;
        report_services(&report, prepared.verbose);
    } else {
        match sandbox.shutdown() {
            Ok(report) => report_services(&report, prepared.verbose),
            Err(e) => eprintln!("shuru: guest shutdown failed, powering off: {}", e),
        }
        trace_boot("guest shut down");
        sandbox.kill()?;
    }
    drop(proxy_handle);
    Ok(RunResult {
        exit_code,
        nbd_handle,
    })
}

/// Pure string validation — no filesystem access. Separated from `parse_mount_spec`
/// so unit tests can exercise mode/path logic without touching the filesystem.
fn parse_mount_parts(host: &str, guest: &str, mode: Option<&str>) -> Result<MountConfig> {
    if !guest.starts_with('/') {
        bail!("guest path must be absolute (start with /): '{}'", guest);
    }
    let read_only = match mode {
        None | Some("ro") => true,
        Some("rw") => false,
        Some(other) => bail!("invalid mount mode '{}': expected 'ro' or 'rw'", other),
    };
    Ok(MountConfig {
        host_path: host.to_string(),
        guest_path: guest.to_string(),
        read_only,
    })
}

fn parse_mount_spec(s: &str) -> Result<MountConfig> {
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.len() < 2 {
        bail!("expected HOST:GUEST[:ro|rw] format (e.g. ./src:/workspace:rw)");
    }

    let host_path = std::fs::canonicalize(parts[0])
        .with_context(|| format!("host path does not exist: '{}'", parts[0]))?
        .to_string_lossy()
        .to_string();

    let guest = parts[1];
    let mode = parts.get(2).copied();

    parse_mount_parts(&host_path, guest, mode)
}

fn report_services(report: &shuru_proto::ShutdownResponse, verbose: bool) {
    if verbose && !report.stopped.is_empty() {
        eprintln!("shuru: stopped services: {}", report.stopped.join(", "));
    }
    if !report.killed.is_empty() {
        eprintln!(
            "shuru: services ignored SIGTERM and were killed: {}",
            report.killed.join(", ")
        );
    }
}

fn validate_mounts(mounts: &[MountConfig], allow_host_writes: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to determine current working directory")?;
    let cwd =
        std::fs::canonicalize(&cwd).context("failed to canonicalize current working directory")?;
    validate_mounts_with_cwd(mounts, allow_host_writes, &cwd)
}

fn validate_mounts_with_cwd(
    mounts: &[MountConfig],
    allow_host_writes: bool,
    cwd: &std::path::Path,
) -> Result<()> {
    if cwd == std::path::Path::new("/") {
        bail!(
            "cannot use mounts when the current working directory is '/'. \
             Change to a project directory first."
        );
    }

    for mc in mounts {
        let host = std::path::Path::new(&mc.host_path);

        if host == std::path::Path::new("/") {
            bail!("mounting '/' as a host path is not allowed. Mount a specific subdirectory instead.");
        }

        if !host.starts_with(cwd) {
            bail!(
                "mount host path '{}' is outside the current working directory '{}'. \
                 Only paths within CWD can be mounted.",
                mc.host_path,
                cwd.display()
            );
        }

        if !mc.read_only && !allow_host_writes {
            bail!(
                "read-write mount '{}:{}:rw' requires --allow-host-writes flag \
                 (or \"allow_host_writes\": true in config).",
                mc.host_path,
                mc.guest_path
            );
        }
    }

    Ok(())
}

/// Parse `NAME=ENV_VAR@host1,host2` into (name, from, hosts).
fn parse_secret_flag(s: &str) -> Result<(String, String, Vec<String>)> {
    let (name, rest) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("missing '=' separator"))?;
    let (from, hosts_str) = rest
        .split_once('@')
        .ok_or_else(|| anyhow::anyhow!("missing '@' separator for hosts"))?;
    let hosts: Vec<String> = hosts_str.split(',').map(|h| h.trim().to_string()).collect();
    if name.is_empty() || from.is_empty() || hosts.is_empty() {
        bail!("name, env var, and hosts must all be non-empty");
    }
    Ok((name.to_string(), from.to_string(), hosts))
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
    use super::*;

    #[test]
    fn mount_defaults_to_read_only() {
        let mc = parse_mount_parts("/some/host", "/workspace", None).unwrap();
        assert!(mc.read_only);
        assert_eq!(mc.guest_path, "/workspace");
    }

    #[test]
    fn mount_ro_suffix() {
        let mc = parse_mount_parts("/some/host", "/workspace", Some("ro")).unwrap();
        assert!(mc.read_only);
    }

    #[test]
    fn mount_rw_suffix() {
        let mc = parse_mount_parts("/some/host", "/workspace", Some("rw")).unwrap();
        assert!(!mc.read_only);
    }

    #[test]
    fn mount_rejects_bad_mode() {
        assert!(parse_mount_parts("/some/host", "/workspace", Some("xx")).is_err());
    }

    #[test]
    fn mount_rejects_relative_guest() {
        assert!(parse_mount_parts("/some/host", "relative/path", None).is_err());
    }

    #[test]
    fn rw_mount_rejected_without_flag() {
        let cwd = std::env::current_dir().unwrap();
        let mounts = vec![MountConfig {
            host_path: cwd.to_string_lossy().to_string(),
            guest_path: "/workspace".to_string(),
            read_only: false,
        }];
        let err = validate_mounts_with_cwd(&mounts, false, &cwd).unwrap_err();
        assert!(err.to_string().contains("--allow-host-writes"));
    }

    #[test]
    fn rw_mount_accepted_with_flag() {
        let cwd = std::env::current_dir().unwrap();
        let mounts = vec![MountConfig {
            host_path: cwd.to_string_lossy().to_string(),
            guest_path: "/workspace".to_string(),
            read_only: false,
        }];
        assert!(validate_mounts_with_cwd(&mounts, true, &cwd).is_ok());
    }

    #[test]
    fn ro_mount_accepted_without_flag() {
        let cwd = std::env::current_dir().unwrap();
        let mounts = vec![MountConfig {
            host_path: cwd.to_string_lossy().to_string(),
            guest_path: "/workspace".to_string(),
            read_only: true,
        }];
        assert!(validate_mounts_with_cwd(&mounts, false, &cwd).is_ok());
    }

    #[test]
    fn mount_outside_cwd_rejected() {
        let cwd = std::path::Path::new("/Users/testuser/project");
        let mounts = vec![MountConfig {
            host_path: "/tmp".to_string(),
            guest_path: "/workspace".to_string(),
            read_only: true,
        }];
        let err = validate_mounts_with_cwd(&mounts, false, cwd).unwrap_err();
        assert!(err
            .to_string()
            .contains("outside the current working directory"));
    }

    #[test]
    fn root_host_path_rejected() {
        let cwd = std::path::Path::new("/Users/testuser/project");
        let mounts = vec![MountConfig {
            host_path: "/".to_string(),
            guest_path: "/workspace".to_string(),
            read_only: true,
        }];
        let err = validate_mounts_with_cwd(&mounts, false, cwd).unwrap_err();
        assert!(err.to_string().contains("mounting '/'"));
    }

    #[test]
    fn empty_mounts_passes() {
        let cwd = std::env::current_dir().unwrap();
        assert!(validate_mounts_with_cwd(&[], false, &cwd).is_ok());
    }

    #[test]
    fn cwd_root_rejected() {
        let mounts = vec![MountConfig {
            host_path: "/usr".to_string(),
            guest_path: "/workspace".to_string(),
            read_only: true,
        }];
        let err = validate_mounts_with_cwd(&mounts, false, std::path::Path::new("/")).unwrap_err();
        assert!(err.to_string().contains("current working directory is '/'"));
    }
}
