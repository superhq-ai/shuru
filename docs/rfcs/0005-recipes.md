# RFC 0005: Recipes

- Status: Implemented
- Date: 2026-10-26
- Scope: shuru-cli, the superhq-ai/recipes repository
- Depends on: RFC 0004

## Summary

A recipe is a small directory, `recipe.json` plus one or more shell steps,
that describes how to turn the base image or an existing checkpoint into a
new checkpoint. Recipes live in git. The official catalog is
`superhq-ai/recipes` on GitHub, any repository can hold more, and a local
directory works for development. The CLI resolves a recipe to an exact
commit, builds it through the ordinary checkpoint flow, and records where
the checkpoint came from.

```
shuru recipe list
shuru recipe show docker
shuru checkpoint create docker --recipe docker
shuru run --recipe docker -p 8080:8080 -- docker run --rm -p 8080:8080 nginx
```

The last form builds the checkpoint on first use and reuses it afterwards.

## Motivation

RFC 0003 shipped Docker support as a shell script in the repository. The
installed CLI knows nothing about it, so a Homebrew user has to clone the
repo to get a Docker checkpoint. The same is true of every "install X into
a sandbox" recipe people write: it is a one-off script in some other
project, with no versioning, no provenance on the checkpoint it produced,
and no way to share it short of copy and paste.

The pieces already exist. `shuru checkpoint create` is the build step, RFC
0004 services are the runtime contract, and git is the distribution
channel every user already has. A recipe is the glue, kept deliberately
small.

## Non-goals

- Parameters or templating inside recipes. A recipe is fixed; variants are
  separate recipes.
- Dependencies between recipes beyond `from` naming a checkpoint.
- Signing beyond the git commit SHA. Authenticity rests on GitHub and the
  repository owner.
- A registry service. The catalog is a JSON file in a git repository.
- Prebuilt checkpoint downloads. Possible later, orthogonal to this.

## Design

### Recipe layout

```
docker/
  recipe.json
  install.sh
  pull.sh
```

`recipe.json`:

```json
{
  "name": "docker",
  "description": "Docker Engine with dockerd as a guest service",
  "min_shuru_version": "0.8.0",
  "from": null,
  "disk_size": 4096,
  "memory": 2048,
  "network": {"allow": ["deb.debian.org", "download.docker.com"]},
  "services": {
    "docker": {"exec": ["dockerd"], "ready": ["docker", "info"]}
  },
  "steps": ["install.sh", "pull.sh"]
}
```

| field | required | meaning |
|---|---|---|
| `name` | yes | checkpoint name used by `shuru run --recipe`. `[a-z0-9][a-z0-9._-]*`. |
| `description` | no | one line for `recipe list` and `recipe show`. |
| `min_shuru_version` | no | refuse to build on an older CLI. |
| `from` | no | existing checkpoint to start from instead of the base image. |
| `cpus`, `memory`, `disk_size` | no | VM size for the build. CLI flags override. |
| `network.allow` | no | hosts the build may reach. Absent means the build runs offline. An empty list means unrestricted. |
| `services` | no | RFC 0004 service files, written before the first step runs. |
| `steps` | no | scripts run in order, default `["build.sh"]`. |

Each step runs in its own boot. The first boots from `from` or the base
image, every later step boots from the checkpoint the previous step saved.
Services are written during the first step, so from the second step on
they are running. This is why the Docker recipe has two steps: the first
installs the engine and declares the service, the second pulls an image
with dockerd already up. Intermediate checkpoints are deleted when the
build succeeds and kept, with a message, when it fails.

A step script is sent to the guest as a base64 payload the same way the
RFC 0003 script was, so it can use any quoting it likes. It runs with
`sh`, from `/`, as root, and its exit status fails the build.

### References

| form | resolves to |
|---|---|
| `docker` | `superhq-ai/recipes`, directory `docker`, default branch |
| `owner/repo` | that repository root |
| `owner/repo/path/to/dir` | a directory in that repository |
| any of the above `@ref` | a branch, tag or commit |
| `./dir`, `/abs/dir`, `../dir` | a local directory |

A GitHub reference is resolved to a commit SHA through the GitHub API
before anything is fetched, and the files are then read at that SHA from
`raw.githubusercontent.com`. The SHA is printed, is what the checkpoint
records, and is what `--recipe docker@<sha>` pins. `GITHUB_TOKEN` or
`GH_TOKEN`, when set, is sent to api.github.com only, for private
repositories and rate limits.

A local recipe gets a content hash in place of a SHA, so editing a step
counts as a new revision.

### Trust

A recipe runs inside the VM, under the network allowlist it declares, so
it can shape the checkpoint but cannot touch the host. It still decides
what every later sandbox built from it contains. The CLI therefore prints
the source, the commit and the allowlist before every build, and for
anything that is not the official catalog or a local path it asks for
confirmation on a terminal, or requires `--yes` without one.

### Checkpoint provenance

A checkpoint built from a recipe gets a sidecar
`checkpoints/<name>.json`:

```json
{"recipe": {"name": "docker", "source": "superhq-ai/recipes/docker", "revision": "<sha>"}}
```

`shuru checkpoint list` shows it. `shuru checkpoint delete` removes it
with the image.

### `shuru run --recipe`

1. Resolve the reference and confirm trust.
2. Look for a checkpoint named after the recipe, or after `--checkpoint
   NAME` when given. The flag keeps two recipes that share a `name`, such
   as a fork and the official one, from rebuilding over each other.
   - Present with a matching revision: reuse it.
   - Present with a different revision: say so, delete it, rebuild.
   - Present without a sidecar: refuse, since it is not ours to replace.
   - Absent: build.
3. Run the command from that checkpoint with the flags the user gave.

`shuru checkpoint update NAME --recipe REF` forces a rebuild at the current
revision. It builds under `NAME.update` and swaps the files in only when
every step succeeds, so the old checkpoint survives a failed build.

The build and the run are separate phases with separate network policy.
The build uses the recipe allowlist. The run uses whatever `--allow-net`
and `--allow-host` the user passed, which by default is nothing.

### Catalog

`shuru recipe list` reads `index.json` from the default branch of
`superhq-ai/recipes`, a flat array of `{name, description}` that the
repository validates in CI against its directories. The file is cached in
the data directory for a day; `--refresh` bypasses the cache.
`shuru recipe show <ref>` prints the metadata and every step script, so
the contents can be read before they are built.

### The recipes repository

`superhq-ai/recipes` holds `index.json`, one directory per recipe, a
validation script, and a workflow that runs it. The Docker recipe moves
there from `scripts/prepare-docker-checkpoint.sh`, which is removed from
this repository. Pinned versions, the apt key hash and the image digest
stay in the recipe.

## Security

- Builds run with the recipe allowlist, never the project `shuru.json`, so
  a project config cannot leak secrets into a recipe build and a recipe
  cannot widen a project policy.
- The checkpoint records the exact commit. Two users building the same
  reference at the same SHA run the same script.
- Third-party recipes require an explicit yes. Official and local do not,
  because the official catalog is reviewed in its own repository and a
  local directory is code the user already owns.
- A recipe cannot name a service with a path, and service names and
  recipe names are validated before anything is written.

Residual risks:

- GitHub is trusted for both the SHA lookup and the file contents. A
  compromised GitHub account that owns a recipe can change what builds,
  which is the same exposure as any git dependency.
- Reuse is keyed on the recipe revision, not on the base image. A new
  Shuru release with a new rootfs does not rebuild existing recipe
  checkpoints. `min_shuru_version` catches the case where the recipe
  needs the newer kernel.

## Testing

`crates/shuru-cli/tests/recipes.rs` uses local recipes built on the fly:

- `recipe show` prints metadata and steps without booting anything
- a two step recipe with a service: step one output, step two output,
  and proof the service was running during step two and during the run
- `run --recipe` builds once, reuses on the next run, and rebuilds after
  the step script changes, with the sidecar reflecting each revision
- `checkpoint create NAME --recipe` under a chosen name
- with network, the real Docker recipe from a checkout of
  `superhq-ai/recipes` builds and runs a container as uid 101

## Rollout

The Docker checkpoint for the RFC 0003 test suite is now built with
`shuru checkpoint create docker --recipe docker` against the official
catalog, or from a checkout while a recipe is being changed. The README and
the website show that form first and `shuru run --recipe docker` as the
one-liner.

## Appendix A: Evidence

Captured on 2026-10-26 against real VMs on an M-series Mac, kernel 6.12.114,
isolated data dir, codesigned debug CLI 0.8.0, with the official catalog at
superhq-ai/recipes commit 2a5c726. Full `--nocapture` output is in
`docs/rfcs/0005-recipes-evidence.log`.

Recipes suite, six tests in one run (two parse-only, three on throwaway
local recipes, one that builds the docker recipe for real):

```
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 130.08s
```

Lines that show each behaviour, from that log:

```
shuru: building recipe shuru-test-recipe-4199 from <tmp>/.tmpEZbp9s@48ff0ed (2 steps)
shuru: step 1/2: first.sh
shuru: step 2/2: second.sh
step1
service-file-present
step2
step2-saw-service
RUN_SAW_SERVICE
shuru: using checkpoint shuru-test-recipe-4199 (<tmp>/.tmpEZbp9s@48ff0ed)
shuru: recipe changed (<tmp>/.tmpEZbp9s@48ff0ed -> <tmp>/.tmpEZbp9s@ae89e3d), rebuilding checkpoint shuru-test-recipe-4199

Error: --recipe builds from the recipe steps; drop the command
Error: checkpoint shuru-test-recipe-named-4199 already exists, delete it first

shuru: intermediate checkpoint shuru-test-recipe-fail-4199-step1 kept for debugging; delete it with `shuru checkpoint delete shuru-test-recipe-fail-4199-step1`
Error: recipe step bad.sh exited with status 7

shuru: step 2/2: pull.sh
Digest: sha256:a8cb0f33d51fa107c96fd2bd3a71aaf564ec131e9d2bdf8700944f0ed8b170d7
29.9.2
101
```

The official path, resolved from GitHub. The data dir held a `docker`
checkpoint built from a local checkout, so the first `shuru run --recipe
docker` saw a different source and rebuilt; the second reused it:

```
$ shuru recipe list --refresh
NAME    DESCRIPTION
docker  Docker Engine 29.9 with dockerd as a guest service and nginx-unprivileged pre-pulled

$ shuru run --recipe docker -- sh -c "docker info --format '{{.ServerVersion}}'; docker run --rm nginxinc/nginx-unprivileged:1.31.7-alpine-slim id -u"
shuru: recipe changed (<recipes-checkout>/docker@dcbbf2c -> superhq-ai/recipes/docker@2a5c726), rebuilding checkpoint docker
shuru: step 1/2: install.sh
shuru: step 2/2: pull.sh
shuru: checkpoint 'docker' saved
29.9.2
101
real    2m3.632s

$ shuru run --recipe docker -- docker image ls --digests
shuru: using checkpoint docker (superhq-ai/recipes/docker@2a5c726)
real    0m3.910s

$ shuru checkpoint list
NAME                       SIZE CREATED      RECIPE
docker                   1.3 GB just now     superhq-ai/recipes/docker@2a5c726
```

The RFC 0003 Docker suite was rerun against a checkpoint built by the
recipe and passed nine of nine in 78.79s; its log is
`docs/rfcs/0003-docker-on-shuru-evidence.log`.
