# Recipes and Guest Services

A recipe is a directory with a `recipe.json` and shell steps. Shuru builds it into a checkpoint by running each step in a fresh boot from the previous step, then records the recipe source and revision next to the checkpoint.

## Commands

```bash
shuru recipe list                        # official catalog (github.com/superhq-ai/recipes)
shuru recipe show <ref>                  # metadata, services and every step, without booting
shuru run --recipe <ref> [-- command]    # build on first use, reuse after, rebuild when the recipe changes
shuru run --recipe <ref> --checkpoint <name> -- ...   # choose the checkpoint name
shuru checkpoint create <name> --recipe <ref>         # build explicitly
shuru checkpoint update <name> --recipe <ref>         # force a rebuild at the current revision
```

`--recipe` replaces the command on `checkpoint create` and `update`; passing both is an error.

## References

| form | resolves to |
|------|-------------|
| `docker` | the official catalog, directory `docker`, default branch |
| `owner/repo` | that repository root |
| `owner/repo/path/to/dir` | a directory in that repository |
| any of the above `@ref` | a branch, tag or commit |
| `./dir`, `../dir`, `/abs/dir`, `~/dir` | a local directory |

A bare name never means a local directory, even when one of that name exists in the working directory. Write `./name` for the local one.

GitHub references resolve to a commit SHA through the GitHub API. Local recipes get a content hash. The resolved revision is printed during the build and stored in `<checkpoint>.json` next to the checkpoint, which is what `run --recipe` compares to decide between reuse and rebuild.

## Trust

- Official catalog and local paths build without prompting.
- Third-party GitHub recipes print their source and ask for confirmation on a TTY. In scripts and agents pass `--yes`.
- `run --recipe` refuses to replace a checkpoint of the same name that was not built from a recipe. Delete it or pick another name with `--checkpoint`.

## recipe.json

```json
{
  "name": "docker",
  "description": "Docker Engine 29.9 with dockerd as a guest service and nginx-unprivileged pre-pulled",
  "min_shuru_version": "0.8.0",
  "disk_size": 4096,
  "memory": 2048,
  "network": {
    "allow": ["deb.debian.org", "security.debian.org", "download.docker.com", "*.docker.io"]
  },
  "services": {
    "docker": {
      "exec": ["dockerd"],
      "ready": ["docker", "info"],
      "ready_timeout_secs": 60,
      "stop_timeout_secs": 30
    }
  },
  "steps": ["install.sh", "pull.sh"]
}
```

| field | required | meaning |
|-------|----------|---------|
| `name` | yes | default checkpoint name. `[a-z0-9][a-z0-9._-]*` |
| `description` | no | one line for `recipe list` and `recipe show` |
| `min_shuru_version` | no | refuse to build on an older CLI |
| `from` | no | existing checkpoint to start from instead of the base image |
| `cpus`, `memory`, `disk_size` | no | VM size for the build; CLI flags override |
| `network.allow` | no | hosts the build may reach. Absent means the build runs offline. An empty list means unrestricted |
| `services` | no | guest service files, written before the first step runs |
| `steps` | no | scripts run in order, default `["build.sh"]` |

Builds use the recipe allowlist, never the project `shuru.json`. Every step runs as root in `/bin/sh` from a fresh boot of the previous step, so services declared in `recipe.json` are already running from step 2 onwards.

## Guest services

Any image can declare long-running processes in `/etc/shuru/services/<name>.json`. The guest init starts them at boot, waits for the readiness check before running the command, and sends SIGTERM before the VM powers off. There is no systemd in the guest; this is the mechanism for daemons.

```json
{
  "exec": ["dockerd"],
  "ready": ["docker", "info"],
  "ready_timeout_secs": 60,
  "stop_timeout_secs": 30,
  "env": {"HOME": "/root"},
  "cwd": "/"
}
```

| field | meaning |
|-------|---------|
| `exec` | argv of the daemon, never passed to a shell |
| `ready` | argv polled until it exits 0; the command waits for it |
| `ready_timeout_secs` | give up and fail the run if `ready` never passes |
| `stop_timeout_secs` | wait this long after SIGTERM before SIGKILL |
| `env` | extra environment for the daemon |
| `cwd` | working directory for the daemon |

A recipe writes these files for you through its `services` block. A hand-built checkpoint can write them directly during `checkpoint create`.

## Official recipes

### docker

Docker Engine (pinned docker-ce and containerd) with `dockerd` as a guest service and `nginxinc/nginx-unprivileged:1.31.7-alpine-slim` pre-pulled, so `docker run` of that image works offline. Containers reach the internet only through the Shuru proxy with `--allow-net`, host ports only through `-p`. Keep Docker at its defaults inside the VM.

```bash
shuru run --recipe docker -p 8080:8080 -- \
  docker run --rm -p 8080:8080 nginxinc/nginx-unprivileged:1.31.7-alpine-slim
```

### browser

Chromium from Debian, run headless as the unprivileged `browser` user with the DevTools protocol on port 9222 inside the VM. The service is ready when `/json/version` answers.

```bash
shuru run --recipe browser -p 9222:9222 -- sleep infinity
curl -s http://127.0.0.1:9222/json/version
```

Point Playwright, Puppeteer or browser-use at `http://127.0.0.1:9222`. The DevTools HTTP endpoint ignores HTTP/1.0 requests, so use an HTTP/1.1 client. Add `--allow-net` for real websites and `--allow-host` to restrict them. Use `--memory 4096` for heavy pages.

## Writing a recipe

```
my-recipe/
  recipe.json
  install.sh
  check.sh
```

Pin everything a step installs (package versions, signing keys, image digests) so two people building the recipe get the same checkpoint. Test locally with `shuru run --recipe ./my-recipe -- ...`, and share it from any GitHub repository as `owner/repo/my-recipe@tag`.
