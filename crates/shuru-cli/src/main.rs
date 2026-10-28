mod assets;
mod checkpoint;
mod cli;
mod config;
mod http;
mod recipe;
mod stdio;
mod vm;

use std::process;

use anyhow::Result;
use clap::Parser;

use shuru_vm::{default_data_dir, VmState};

use cli::{CheckpointCommands, Cli, Commands, RecipeCommands};
use config::load_config;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            vm,
            from,
            recipe,
            yes,
            checkpoint,
            console,
            stdio,
            command,
        } => {
            let cfg = load_config(vm.config.as_deref())?;

            // A recipe is built (once) before the run, with its own policy.
            let from = match recipe {
                Some(ref reference) => Some(recipe::ensure_checkpoint(
                    reference,
                    &vm,
                    yes,
                    checkpoint.as_deref(),
                )?),
                None => from,
            };

            // Command resolution: CLI args > config > default /bin/sh
            let command = if !command.is_empty() {
                command
            } else if let Some(cfg_cmd) = cfg.command.clone() {
                cfg_cmd
            } else {
                vec!["/bin/sh".to_string()]
            };

            let prepared = vm::prepare_vm(&vm, &cfg, from.as_deref())?;

            let result = if stdio {
                stdio::run_stdio(&prepared)
            } else if console {
                run_console(&prepared)
            } else {
                vm::run_command(&prepared, &command, None).map(|r| r.exit_code)
            };

            process::exit(result?);
        }
        Commands::Init { force } => {
            let data_dir = default_data_dir();
            if force {
                let _ = std::fs::remove_file(format!("{}/VERSION", data_dir));
            }
            if assets::assets_ready(&data_dir) {
                eprintln!(
                    "shuru: OS image already up to date ({})",
                    assets::CURRENT_VERSION
                );
            } else {
                assets::download_os_image(&data_dir)?;
            }
        }
        Commands::Upgrade => {
            let data_dir = default_data_dir();
            assets::upgrade(&data_dir)?;
        }
        Commands::Checkpoint { action } => match action {
            CheckpointCommands::Create {
                name,
                vm,
                from,
                recipe,
                yes,
                command,
            } => {
                if let Some(ref reference) = recipe {
                    if !command.is_empty() {
                        anyhow::bail!("--recipe builds from the recipe steps; drop the command");
                    }
                    recipe::create_checkpoint(&name, reference, &vm, yes)?;
                    return Ok(());
                }
                let exit_code = checkpoint::create(name, &vm, from.as_deref(), command)?;
                process::exit(exit_code);
            }
            CheckpointCommands::Update {
                name,
                vm,
                from,
                recipe,
                yes,
                command,
            } => {
                if let Some(ref reference) = recipe {
                    if !command.is_empty() {
                        anyhow::bail!("--recipe builds from the recipe steps; drop the command");
                    }
                    recipe::update_checkpoint(&name, reference, &vm, yes)?;
                    return Ok(());
                }
                let exit_code = checkpoint::update(name, &vm, from.as_deref(), command)?;
                process::exit(exit_code);
            }
            CheckpointCommands::List => checkpoint::list()?,
            CheckpointCommands::Delete { name } => checkpoint::delete(&name)?,
            CheckpointCommands::Push { name: _ } => {
                anyhow::bail!("checkpoint push is not yet implemented")
            }
            CheckpointCommands::Pull { name: _ } => {
                anyhow::bail!("checkpoint pull is not yet implemented")
            }
        },
        Commands::Recipe { action } => match action {
            RecipeCommands::List { refresh } => recipe::list(refresh)?,
            RecipeCommands::Show { recipe: reference } => recipe::show(&reference)?,
        },
    }

    Ok(())
}

/// Run the VM in raw serial console mode (for debugging).
fn run_console(prepared: &vm::PreparedVm) -> Result<i32> {
    eprintln!("shuru: kernel={}", prepared.kernel_path);
    eprintln!(
        "shuru: rootfs={} (anonymous clone of {})",
        prepared.work_disk.attach_path(),
        prepared.source_rootfs
    );
    eprintln!(
        "shuru: booting VM ({}cpus, {}MB RAM, {}MB disk)...",
        prepared.cpus, prepared.memory, prepared.disk_size
    );

    let sandbox = vm::build_sandbox(prepared, true, None, None)?;
    eprintln!("shuru: VM created and validated successfully");

    let state_rx = sandbox.state_channel();

    eprintln!("shuru: starting VM...");
    sandbox.start()?;
    eprintln!("shuru: VM started");

    eprintln!("shuru: running in console mode (Ctrl+C to stop)");
    let mut exit_code = 0;
    loop {
        match state_rx.recv() {
            Ok(VmState::Stopped) => {
                eprintln!("shuru: VM stopped");
                break;
            }
            Ok(VmState::Error) => {
                eprintln!("shuru: VM encountered an error");
                exit_code = 1;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    Ok(exit_code)
}
