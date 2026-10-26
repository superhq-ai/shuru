use std::os::unix::fs::MetadataExt;

use anyhow::{bail, Result};

use shuru_vm::default_data_dir;

use crate::cli::VmArgs;
use crate::config::load_config;
use crate::vm;

pub(crate) fn create(
    name: String,
    vm_args: &VmArgs,
    from: Option<&str>,
    command: Vec<String>,
) -> Result<i32> {
    let cfg = load_config(vm_args.config.as_deref())?;

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
        bail!("checkpoint '{}' already exists, delete it first", name);
    }

    let prepared = vm::prepare_vm(vm_args, &cfg, from)?;
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

    println!("{:<20} {:>10} {}", "NAME", "SIZE", "CREATED");
    for (name, size, mtime, is_cas) in &checkpoints {
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
        println!("{:<20} {:>10} {}", name, size_str, age);
    }

    Ok(())
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

    eprintln!("shuru: checkpoint '{}' deleted", name);
    Ok(())
}

/// Check if a checkpoint exists in either format.
fn checkpoint_exists(checkpoints_dir: &str, name: &str) -> bool {
    std::path::Path::new(&format!("{}/{}.idx", checkpoints_dir, name)).exists()
        || std::path::Path::new(&format!("{}/{}.ext4", checkpoints_dir, name)).exists()
}
