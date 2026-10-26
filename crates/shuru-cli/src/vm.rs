use std::collections::HashMap;
use std::io::IsTerminal;

use anyhow::{bail, Context, Result};

#[cfg(target_os = "macos")]
extern "C" {
    fn clonefile(
        src: *const libc::c_char,
        dst: *const libc::c_char,
        flags: u32,
    ) -> libc::c_int;
}

#[cfg(target_os = "macos")]
pub(crate) fn clone_file(src: &str, dst: &str) -> Result<()> {
    use std::ffi::CString;

    let c_src = CString::new(src).context("invalid source path")?;
    let c_dst = CString::new(dst).context("invalid destination path")?;
    let ret = unsafe { clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        bail!("clonefile({} -> {}) failed: {}", src, dst, err);
    }
    Ok(())
}

/// Reflink the image when the filesystem supports it (XFS with reflink=1,
/// btrfs), so a boot or a checkpoint costs no disk and no copy time, the
/// same as clonefile on APFS. Filesystems without reflinks (ext4, tmpfs)
/// fall back to a full copy, with a note the first time so an operator
/// knows why boots are slow and disks fill up.
#[cfg(target_os = "linux")]
pub(crate) fn clone_file(src: &str, dst: &str) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    use std::sync::Once;

    static COPY_NOTE: Once = Once::new();

    let src_file = std::fs::File::open(src).with_context(|| format!("failed to open {}", src))?;
    let dst_file = std::fs::File::create(dst).with_context(|| format!("failed to create {}", dst))?;
    let ret = unsafe { libc::ioctl(dst_file.as_raw_fd(), libc::FICLONE as _, src_file.as_raw_fd()) };
    if ret == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EOPNOTSUPP) | Some(libc::ENOTTY) | Some(libc::EXDEV) | Some(libc::EINVAL) => {
            drop(dst_file);
            COPY_NOTE.call_once(|| {
                let dir = std::path::Path::new(dst).parent().map(|p| p.display().to_string()).unwrap_or_default();
                eprintln!(
                    "shuru: note: {} does not support reflinks, copying disk images instead; put the data directory on XFS or btrfs for instant clones",
                    dir
                );
            });
            std::fs::copy(src, dst).with_context(|| format!("failed to copy {} -> {}", src, dst))?;
            Ok(())
        }
        _ => bail!("reflink({} -> {}) failed: {}", src, dst, err),
    }
}

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
    pub instance_dir: String,
    pub source_rootfs: String,
    pub work_rootfs: String,
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

pub(crate) fn prepare_vm(
    vm: &VmArgs,
    cfg: &ShuruConfig,
    from: Option<&str>,
) -> Result<PreparedVm> {
    trace_boot("prepare_vm start");
    let cpus = vm.cpus.or(cfg.cpus).unwrap_or(2);
    let memory = vm.memory.or(cfg.memory).unwrap_or(2048);
    let disk_size = vm.disk_size.or(cfg.disk_size).unwrap_or(4096);
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
            let (name, from, hosts) = parse_secret_flag(s)
                .with_context(|| format!("invalid --secret: '{}' (expected NAME=ENV@host1,host2)", s))?;
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
        let mapping = parse_port_mapping(s)
            .with_context(|| format!("invalid port mapping: '{}'", s))?;
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
        let mc = parse_mount_spec(s)
            .with_context(|| format!("invalid mount spec: '{}'", s))?;
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
        .unwrap_or_else(|| format!("{}/initramfs.cpio.gz", data_dir));

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
            shuru_vm::validate_checkpoint_name(name)
                .map_err(|e| anyhow::anyhow!(e))?;
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
    // Create per-instance working copy (clean any stale dir from PID reuse)
    let instance_dir = format!("{}/instances/{}", data_dir, std::process::id());
    let _ = std::fs::remove_dir_all(&instance_dir);
    std::fs::create_dir_all(&instance_dir)?;
    let work_rootfs = format!("{}/rootfs.ext4", instance_dir);
    trace_boot("prepare: instance dir ready");

    // CAS checkpoints don't need a file copy — the NBD server reads from the chunk store.
    // We still need a rootfs file for the VM builder (kernel cmdline root=), but it can be
    // a dummy for CAS mode since I/O goes through NBD.
    if cas_index.is_none() {
        if verbose {
            eprintln!("shuru: creating working copy...");
        }
        clone_file(&source, &work_rootfs)?;
    } else {
        // Create a minimal placeholder so the VM builder doesn't fail
        std::fs::File::create(&work_rootfs)?;
    }
    trace_boot("prepare: rootfs cloned");

    // Extend to requested disk size
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&work_rootfs)?;
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

    trace_boot("prepare_vm done (disk cloned + extended)");
    Ok(PreparedVm {
        instance_dir,
        source_rootfs: source,
        work_rootfs,
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
        .rootfs(&prepared.work_rootfs)
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
pub(crate) fn start_nbd(prepared: &PreparedVm) -> Result<Option<shuru_store::NbdHandle>> {
    let storage = std::env::var("SHURU_STORAGE").unwrap_or_default();
    let want_cas = storage == "cas" || storage == "nbd";
    if prepared.cas_index.is_none() && !want_cas {
        return Ok(None);
    }
    let socket_path = format!("{}/nbd.sock", prepared.instance_dir);
    let data_dir = shuru_vm::default_data_dir();
    let cas_dir = format!("{}/cas", data_dir);
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

pub(crate) fn run_command(prepared: &PreparedVm, command: &[String]) -> Result<RunResult> {
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
        sandbox.exec_with_env(command, &env, &mut std::io::stdout(), &mut std::io::stderr())?
    };
    trace_boot("command done");

    // Graceful shutdown: guest services get SIGTERM and the disk is synced
    // before power off, so a checkpoint clone sees a consistent image.
    match sandbox.shutdown() {
        Ok(report) => {
            if prepared.verbose && !report.stopped.is_empty() {
                eprintln!("shuru: stopped services: {}", report.stopped.join(", "));
            }
            if !report.killed.is_empty() {
                eprintln!(
                    "shuru: services ignored SIGTERM and were killed: {}",
                    report.killed.join(", ")
                );
            }
        }
        Err(e) => eprintln!("shuru: guest shutdown failed, powering off: {}", e),
    }
    trace_boot("guest shut down");

    drop(proxy_handle);
    let _ = sandbox.kill();
    Ok(RunResult { exit_code, nbd_handle })
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

fn validate_mounts(mounts: &[MountConfig], allow_host_writes: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to determine current working directory")?;
    let cwd = std::fs::canonicalize(&cwd).context("failed to canonicalize current working directory")?;
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
        assert!(err.to_string().contains("outside the current working directory"));
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
