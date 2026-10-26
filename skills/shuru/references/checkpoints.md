# Checkpoints

Checkpoints save the full ext4 disk state after a command finishes. They use APFS copy-on-write clones, so a checkpoint only consumes disk space for blocks that differ from the base image.

## Creating a Checkpoint

```bash
shuru checkpoint create <name> [--allow-net] [--from <existing>] [-- command...]
```

This boots a VM, runs the command (or drops to `/bin/sh` if none given), and saves the disk state when the command exits. The checkpoint is saved regardless of the exit code.

If `--from` is specified, the VM boots from that checkpoint instead of the base image.

`create` refuses a name that already exists. Use `update` to replace it.

## Updating a Checkpoint

```bash
shuru checkpoint update <name> [flags] [-- command...]
```

`update` takes the same arguments as `create` and rebuilds the checkpoint in place, or creates it when missing. The new disk is built under a staging name and swapped in only when the command exits 0, so a failed update leaves the old checkpoint untouched and prints that it is unchanged.

```bash
# Rebuild from the base image
shuru checkpoint update base --allow-net -- apt-get install -y build-essential git make

# Layer the command onto the existing checkpoint
shuru checkpoint update base --from base --allow-net -- apt-get install -y jq

# Rebuild a recipe checkpoint at the current recipe revision
shuru checkpoint update docker --recipe docker
```

## Building from a Recipe

```bash
shuru checkpoint create <name> --recipe <ref>
```

Runs the recipe steps instead of a command, each step in a fresh boot from the previous one. A recipe checkpoint records its source and revision in a sidecar file, and `checkpoint list` shows where it came from. `shuru run --recipe <ref>` builds the same checkpoint on first use under the recipe name (or `--checkpoint <name>`), and rebuilds it when the recipe changes. See [recipes.md](recipes.md).

## Stacking

Checkpoints can be layered. Each layer only stores its diff from the parent:

```bash
shuru checkpoint create base --allow-net -- apt-get install -y build-essential git
shuru checkpoint create node --from base --allow-net -- apt-get install -y nodejs npm
shuru checkpoint create deps --from node --allow-net --mount .:/app -- sh -c 'cd /app && npm ci'
```

`shuru checkpoint list` shows actual disk usage per checkpoint (allocated blocks, not apparent size).

## Booting from a Checkpoint

```bash
shuru run --from <name> [flags] [-- command...]
```

The VM gets a fresh clone of the checkpoint. Changes during the run are discarded on exit.

If the checkpoint declares guest services (Docker, the browser), they start at boot and the command runs only once their readiness checks pass. They are stopped and the disk synced before the VM powers off.

## Lifecycle

- `checkpoint create`: save disk state under a new name
- `checkpoint update`: replace an existing checkpoint, keeping the old one until the new build succeeds
- `checkpoint list`: show all checkpoints with size, age and recipe source
- `checkpoint delete <name>`: permanently remove a checkpoint

## Disk Usage

Checkpoints use APFS clonefile. A fresh checkpoint from the base image might only use 10-50MB of actual disk space depending on what changed. Use `checkpoint list` to see real usage.

`/run`, `/dev/shm` and `/sys/fs/cgroup` are tmpfs inside the guest and are never part of a checkpoint.

If you are running low on disk, delete unused checkpoints with `checkpoint delete`.
