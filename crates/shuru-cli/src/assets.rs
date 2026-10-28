use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use serde::Deserialize;
use tar::Archive;

const GITHUB_REPO: &str = "superhq-ai/shuru";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Check if OS image assets exist and match the expected version.
pub fn assets_ready(data_dir: &str) -> bool {
    let kernel = format!("{}/Image", data_dir);
    let rootfs = format!("{}/rootfs.ext4", data_dir);
    let initramfs = format!("{}/initramfs.cpio", data_dir);

    if !Path::new(&kernel).exists()
        || !Path::new(&rootfs).exists()
        || !Path::new(&initramfs).exists()
    {
        return false;
    }

    let version_file = format!("{}/VERSION", data_dir);
    match fs::read_to_string(&version_file) {
        Ok(v) => v.trim() == CURRENT_VERSION,
        Err(_) => false,
    }
}

/// Download and extract OS image assets from GitHub Releases.
///
/// Streams directly: HTTP → gzip decompress → tar extract → disk.
/// No temp files needed.
pub fn download_os_image(data_dir: &str) -> Result<()> {
    download_os_image_version(data_dir, CURRENT_VERSION)
}

fn download_os_image_version(data_dir: &str, version: &str) -> Result<()> {
    let tag = format!("v{}", version);
    let tarball_name = format!("shuru-os-{}-aarch64.tar.gz", tag);
    let url = format!(
        "https://github.com/{}/releases/download/{}/{}",
        GITHUB_REPO, tag, tarball_name
    );

    fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create data directory: {}", data_dir))?;

    eprintln!("shuru: downloading OS image ({})...", tag);
    eprintln!("shuru: {}", url);

    let response = crate::http::agent()
        .get(&url)
        .call()
        .with_context(|| format!("download failed — is version {} released?", tag))?;

    let total_bytes = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let reader = ProgressReader::new(response.into_body().into_reader(), total_bytes);
    let decoder = GzDecoder::new(reader);
    let mut archive = Archive::new(decoder);

    archive
        .unpack(data_dir)
        .context("failed to extract OS image")?;

    eprintln!(); // newline after progress

    shuru_vm::WorkDisk::prepare_base(&format!("{}/rootfs.ext4", data_dir))?;

    // Write VERSION file
    let version_file = format!("{}/VERSION", data_dir);
    fs::write(&version_file, format!("{}\n", version)).context("failed to write VERSION file")?;

    eprintln!("shuru: OS image ready ({})", version);
    Ok(())
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

fn cli_tarball_name(version: &str) -> Result<String> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("linux", "aarch64") => "linux-aarch64",
        (os, arch) => bail!("automatic upgrade is not supported on {}-{}", os, arch),
    };

    Ok(format!("shuru-v{}-{}.tar.gz", version, platform))
}

/// Check for a newer release and upgrade the CLI binary + OS image.
pub fn upgrade(data_dir: &str) -> Result<()> {
    if let Ok(exe) = std::env::current_exe() {
        if exe.to_string_lossy().contains("/Cellar/") {
            bail!("This copy was installed via Homebrew. Please run `brew upgrade shuru` instead");
        }
    }
    // Working disks lived here before 0.9.2 and could outlive a crashed run.
    let _ = std::fs::remove_dir_all(format!("{}/instances", data_dir));

    eprintln!("shuru: checking for updates...");

    let api_url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        GITHUB_REPO
    );

    let response = crate::http::agent()
        .get(&api_url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "shuru")
        .call()
        .context("failed to check for updates")?;

    let release: GithubRelease = response
        .into_body()
        .read_json()
        .context("failed to parse release info")?;

    let latest = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);

    if latest == CURRENT_VERSION {
        eprintln!("shuru: already on latest version ({})", CURRENT_VERSION);
        return Ok(());
    }

    eprintln!("shuru: upgrading {} -> {}", CURRENT_VERSION, latest);

    // Update CLI binary
    let cli_tarball = cli_tarball_name(latest)?;
    let cli_url = format!(
        "https://github.com/{}/releases/download/v{}/{}",
        GITHUB_REPO, latest, cli_tarball
    );

    let current_exe = std::env::current_exe().context("failed to determine current binary path")?;

    eprintln!("shuru: downloading CLI ({})...", latest);
    eprintln!("shuru: {}", cli_url);

    let response = crate::http::agent()
        .get(&cli_url)
        .call()
        .with_context(|| format!("failed to download CLI v{}", latest))?;

    let total_bytes = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let reader = ProgressReader::new(response.into_body().into_reader(), total_bytes);
    let decoder = GzDecoder::new(reader);
    let mut archive = Archive::new(decoder);

    // Extract to a temp file next to the current binary
    let tmp_path = current_exe.with_extension("new");
    for entry in archive.entries().context("failed to read CLI archive")? {
        let mut entry = entry.context("failed to read archive entry")?;
        if entry.path()?.to_str() == Some("shuru") {
            let mut out = fs::File::create(&tmp_path).context("failed to create temp binary")?;
            io::copy(&mut entry, &mut out)?;
            break;
        }
    }

    eprintln!();

    if !tmp_path.exists() {
        bail!("'shuru' binary not found in CLI archive");
    }

    // Set executable permission
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755))?;
    }

    // Atomic-ish replace: rename current -> .old, rename new -> current, remove .old
    let old_path = current_exe.with_extension("old");
    let _ = fs::remove_file(&old_path);
    fs::rename(&current_exe, &old_path)
        .context("failed to move current binary (try with sudo?)")?;
    if let Err(e) = fs::rename(&tmp_path, &current_exe) {
        // Rollback
        let _ = fs::rename(&old_path, &current_exe);
        return Err(e).context("failed to install new binary");
    }
    let _ = fs::remove_file(&old_path);

    eprintln!("shuru: CLI updated to {}", latest);

    // Update OS image
    download_os_image_version(data_dir, latest)?;

    eprintln!("shuru: upgrade complete ({})", latest);
    Ok(())
}

/// Wraps a reader to print download progress to stderr.
struct ProgressReader<R> {
    inner: R,
    bytes_read: u64,
    total_bytes: Option<u64>,
    last_printed_mb: u64,
}

impl<R> ProgressReader<R> {
    fn new(inner: R, total_bytes: Option<u64>) -> Self {
        Self {
            inner,
            bytes_read: 0,
            total_bytes,
            last_printed_mb: u64::MAX, // force first print
        }
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes_read += n as u64;

        let current_mb = self.bytes_read / (1024 * 1024);
        if current_mb != self.last_printed_mb {
            self.last_printed_mb = current_mb;
            let mut stderr = io::stderr().lock();
            if let Some(total) = self.total_bytes {
                let total_mb = total / (1024 * 1024);
                let _ = write!(
                    stderr,
                    "\rshuru: downloaded {} / {} MB",
                    current_mb, total_mb
                );
            } else {
                let _ = write!(stderr, "\rshuru: downloaded {} MB", current_mb);
            }
            let _ = stderr.flush();
        }

        Ok(n)
    }
}
