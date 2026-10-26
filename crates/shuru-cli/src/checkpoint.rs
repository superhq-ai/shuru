use std::os::unix::fs::MetadataExt;

use anyhow::{bail, Result};

use shuru_vm::default_data_dir;

use crate::cli::VmArgs;
use crate::config::{load_config, ShuruConfig};
use crate::recipe;
use crate::vm;

pub(crate) fn create(
    name: String,
    vm_args: &VmArgs,
    from: Option<&str>,
    command: Vec<String>,
) -> Result<i32> {
    let cfg = load_config(vm_args.config.as_deref())?;
    create_with_config(name, vm_args, &cfg, from, command)
}

/// Create a checkpoint with an explicit config. Recipe builds pass an empty
/// one so the project shuru.json never reaches a build.
pub(crate) fn create_with_config(
    name: String,
    vm_args: &VmArgs,
    cfg: &ShuruConfig,
    from: Option<&str>,
    command: Vec<String>,
) -> Result<i32> {
    let command = if !command.is_empty() {
        command
    } else {
        vec!["/bin/sh".to_string()]
    };

    shuru_vm::validate_checkpoint_name(&name).map_err(|e| anyhow::anyhow!(e))?;

    let data_dir = shuru_vm::default_data_dir();
    let checkpoints_dir = format!("{}/checkpoints", data_dir);
    // Check both formats
    if checkpoint_exists(&checkpoints_dir, &name) {
        bail!(
            "checkpoint '{}' already exists; use `shuru checkpoint update` to replace it",
            name
        );
    }

    let prepared = vm::prepare_vm(vm_args, cfg, from)?;
    let result = vm::run_command(&prepared, &command)?;

    std::fs::create_dir_all(&checkpoints_dir)?;
    eprintln!("shuru: saving checkpoint '{}'...", name);

    if let Some(ref nbd_handle) = result.nbd_handle {
        let index_path = format!("{}/{}.idx", checkpoints_dir, name);
        nbd_handle.save_checkpoint(&index_path)?;
    } else {
        let ext4_path = format!("{}/{}.ext4", checkpoints_dir, name);
        vm::clone_file(&prepared.work_rootfs, &ext4_path)?;
    }
    eprintln!("shuru: checkpoint '{}' saved", name);

    drop(result.nbd_handle);
    let _ = std::fs::remove_dir_all(&prepared.instance_dir);
    Ok(result.exit_code)
}

/// `shuru checkpoint update NAME`: the new checkpoint is built under a
/// staging name and swapped in once the command succeeds, so a failed build
/// leaves the old checkpoint untouched. `--from NAME` layers the command on
/// top of the existing checkpoint instead of starting from the base image.
pub(crate) fn update(
    name: String,
    vm_args: &VmArgs,
    from: Option<&str>,
    command: Vec<String>,
) -> Result<i32> {
    shuru_vm::validate_checkpoint_name(&name).map_err(|e| anyhow::anyhow!(e))?;
    if !exists(&name) {
        return create(name, vm_args, from, command);
    }
    let cfg = load_config(vm_args.config.as_deref())?;
    let staging = staging_name(&name);
    if exists(&staging) {
        delete(&staging)?;
    }
    let code = create_with_config(staging.clone(), vm_args, &cfg, from, command)?;
    if code != 0 {
        let _ = delete(&staging);
        bail!(
            "command exited with status {}, checkpoint '{}' unchanged",
            code,
            name
        );
    }
    replace(&name, &staging)?;
    Ok(0)
}

/// Where an update builds before it replaces `name`.
pub(crate) fn staging_name(name: &str) -> String {
    format!("{}.update", name)
}

/// Move the files of `staging` over those of `name`, in either format,
/// including the recipe sidecar.
pub(crate) fn replace(name: &str, staging: &str) -> Result<()> {
    let checkpoints_dir = format!("{}/checkpoints", default_data_dir());
    for ext in ["idx", "ext4", "json"] {
        let target = format!("{}/{}.{}", checkpoints_dir, name, ext);
        let source = format!("{}/{}.{}", checkpoints_dir, staging, ext);
        if std::path::Path::new(&target).exists() {
            std::fs::remove_file(&target)?;
        }
        if std::path::Path::new(&source).exists() {
            std::fs::rename(&source, &target)?;
        }
    }
    eprintln!("shuru: checkpoint '{}' updated", name);
    Ok(())
}

pub(crate) fn list() -> Result<()> {
    let data_dir = default_data_dir();
    let checkpoints_dir = format!("{}/checkpoints", data_dir);

    let entries = match std::fs::read_dir(&checkpoints_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("No checkpoints found.");
            return Ok(());
        }
        Err(e) => bail!("Failed to read checkpoints directory: {}", e),
    };

    let mut checkpoints: Vec<(String, u64, std::time::SystemTime, bool)> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        let is_cas = ext == Some("idx");
        if !is_cas && ext != Some("ext4") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let meta = entry.metadata()?;
        let disk_usage = if is_cas {
            // Count non-ZERO chunks × 64KB for actual referenced data size
            shuru_store::ChunkIndex::load(path.to_str().unwrap_or("")).map(|idx| {
                let non_zero = (0..idx.num_chunks())
                    .filter(|&i| idx.get_hash(i).map(|h| h != "ZERO").unwrap_or(false))
                    .count();
                (non_zero as u64) * 64 * 1024
            })?
        } else {
            meta.blocks() * 512
        };
        checkpoints.push((name, disk_usage, meta.modified()?, is_cas));
    }

    if checkpoints.is_empty() {
        eprintln!("No checkpoints found.");
        return Ok(());
    }

    checkpoints.sort_by_key(|(_, _, t, _)| *t);

    println!("NAME                       SIZE CREATED      RECIPE");
    for (name, size, mtime, is_cas) in &checkpoints {
        let origin = recipe::read_meta(name)
            .map(|m| {
                format!(
                    "{}@{}",
                    m.recipe.source,
                    &m.recipe.revision[..m.recipe.revision.len().min(7)]
                )
            })
            .unwrap_or_default();
        let size_str = if *is_cas {
            if *size >= 1024 * 1024 {
                format!("{} MB (cas)", size / (1024 * 1024))
            } else {
                format!("{} KB (cas)", size / 1024)
            }
        } else if *size >= 1024 * 1024 * 1024 {
            format!("{:.1} GB", *size as f64 / (1024.0 * 1024.0 * 1024.0))
        } else {
            format!("{} MB", size / (1024 * 1024))
        };
        let elapsed = mtime
            .elapsed()
            .unwrap_or(std::time::Duration::ZERO)
            .as_secs();
        let age = if elapsed < 60 {
            "just now".to_string()
        } else if elapsed < 3600 {
            format!("{}m ago", elapsed / 60)
        } else if elapsed < 86400 {
            format!("{}h ago", elapsed / 3600)
        } else {
            format!("{}d ago", elapsed / 86400)
        };
        println!("{:<20} {:>10} {:<12} {}", name, size_str, age, origin);
    }

    Ok(())
}

/// Whether a checkpoint exists in either format.
pub(crate) fn exists(name: &str) -> bool {
    let checkpoints_dir = format!("{}/checkpoints", default_data_dir());
    checkpoint_exists(&checkpoints_dir, name)
}

pub(crate) fn delete(name: &str) -> Result<()> {
    shuru_vm::validate_checkpoint_name(name).map_err(|e| anyhow::anyhow!(e))?;

    let data_dir = default_data_dir();
    let checkpoints_dir = format!("{}/checkpoints", data_dir);

    // Try .idx (CAS) first, then .ext4 (legacy)
    let idx_path = format!("{}/{}.idx", checkpoints_dir, name);
    let ext4_path = format!("{}/{}.ext4", checkpoints_dir, name);

    if std::path::Path::new(&idx_path).exists() {
        std::fs::remove_file(&idx_path)?;
    } else if std::path::Path::new(&ext4_path).exists() {
        std::fs::remove_file(&ext4_path)?;
    } else {
        bail!("Checkpoint '{}' not found", name);
    }
    recipe::remove_meta(name);

    eprintln!("shuru: checkpoint '{}' deleted", name);
    Ok(())
}

/// Check if a checkpoint exists in either format.
fn checkpoint_exists(checkpoints_dir: &str, name: &str) -> bool {
    std::path::Path::new(&format!("{}/{}.idx", checkpoints_dir, name)).exists()
        || std::path::Path::new(&format!("{}/{}.ext4", checkpoints_dir, name)).exists()
}
