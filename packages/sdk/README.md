# @superhq/shuru

TypeScript SDK for [shuru](https://github.com/superhq-ai/shuru) — programmatic access to ephemeral Linux microVMs on macOS.

## Install

```sh
bun add @superhq/shuru
```

## Usage

```ts
import { Sandbox } from "@superhq/shuru";

const sb = await Sandbox.start();

// Buffered exec — run a command and get the full result
const result = await sb.exec("echo hello");
console.log(result.stdout); // "hello\n"

// Streaming spawn — real-time stdout/stderr
const proc = await sb.spawn("npm run dev");
proc.on("stdout", (data) => process.stdout.write(data));
proc.on("stderr", (data) => process.stderr.write(data));
proc.on("exit", (code) => console.log("exited:", code));

// File watching — guest-side inotify events
await sb.watch("/workspace", (event) => {
  console.log(event.event, event.path); // "modify" "/workspace/src/main.ts"
});

// File I/O
await sb.writeFile("/tmp/app.ts", "console.log('hi')");
const data = await sb.readFile("/tmp/app.ts"); // Uint8Array

// Filesystem operations
await sb.mkdir("/workspace/src");
const entries = await sb.readDir("/workspace"); // { name, type, size }[]
const info = await sb.stat("/tmp/app.ts");      // { size, mode, mtime, isDir, ... }
await sb.copy("/tmp/app.ts", "/tmp/backup.ts");
await sb.rename("/tmp/backup.ts", "/tmp/old.ts");
await sb.chmod("/tmp/app.ts", 0o755);
await sb.remove("/tmp/old.ts");
if (await sb.exists("/tmp/app.ts")) { /* ... */ }

// Checkpoint — save disk state and stop
await sb.checkpoint("my-env");
```

### Start from a checkpoint

```ts
const sb = await Sandbox.start({ from: "my-env" });
```

### Options

```ts
const sb = await Sandbox.start({
  from: "my-env",
  cpus: 4,
  memory: 4096,
  diskSize: 8192,
  allowNet: true,
  ports: ["8080:80"],
  mounts: { "./src": "/workspace" },
  secrets: {
    API_KEY: { value: "sk-your-openai-key", hosts: ["api.openai.com"] },
  },
  network: { allow: ["api.openai.com", "registry.npmjs.org"] },
});
```

| Option | Type | Description |
|--------|------|-------------|
| `from` | `string` | Checkpoint name to start from |
| `cpus` | `number` | Number of vCPUs |
| `memory` | `number` | Memory in MB |
| `diskSize` | `number` | Disk size in MB |
| `allowNet` | `boolean` | Enable network access |
| `ports` | `string[]` | Port forwards (`"host:guest"`) |
| `mounts` | `Record<string, string>` | Directory mounts (`{ hostPath: guestPath }`) |
| `secrets` | `Record<string, SecretConfig>` | Secrets to inject via proxy |
| `network` | `NetworkConfig` | Network access policy |
| `shuruBin` | `string` | Path to shuru binary (default: `"shuru"`) |

## API

### `Sandbox.start(opts?): Promise<Sandbox>`

Boot a new microVM. Returns when the VM is ready.

### `sandbox.exec(command): Promise<ExecResult>`

Run a shell command in the VM. Returns `{ stdout, stderr, exitCode }`. Stdout and stderr are buffered — the promise resolves when the command finishes.

### `sandbox.spawn(command, opts?): Promise<SandboxProcess>`

Spawn a long-running command in the VM. Returns a `SandboxProcess` handle immediately, streaming output in real-time.

```ts
const proc = await sb.spawn("npm run dev", { cwd: "/workspace" });

proc.on("stdout", (data: Buffer) => { /* real-time chunks */ });
proc.on("stderr", (data: Buffer) => { /* real-time chunks */ });
proc.on("exit", (code: number) => { /* process exited */ });

proc.write("input to stdin\n"); // write to stdin
await proc.kill();               // send SIGTERM
const exitCode = await proc.exited; // await completion
console.log(proc.pid);             // process ID
```

**`SpawnOptions`:**
| Option | Type | Description |
|--------|------|-------------|
| `cwd` | `string` | Working directory for the command |
| `env` | `Record<string, string>` | Environment variables |

### `sandbox.watch(path, handler, opts?): Promise<void>`

Watch a directory for file changes inside the guest VM. Uses guest-side inotify, so it detects writes to tmpfs overlays that host-side watchers cannot see.

```ts
await sb.watch("/workspace", (event) => {
  console.log(event.event, event.path);
  // event.event: "create" | "modify" | "delete" | "rename"
  // event.path: full path of the changed file
});
```

**`WatchOptions`:**
| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `recursive` | `boolean` | `true` | Watch subdirectories recursively |

### `sandbox.readFile(path): Promise<Uint8Array>`

Read a file from the VM. Returns raw bytes. Use `new TextDecoder().decode(data)` for text files.

### `sandbox.writeFile(path, content: Uint8Array | string): Promise<void>`

Write a file to the VM. Accepts raw bytes or a string.

### `sandbox.mkdir(path, opts?): Promise<void>`

Create a directory. Creates parent directories by default.

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `recursive` | `boolean` | `true` | Create parent directories |

### `sandbox.readDir(path): Promise<DirEntry[]>`

List the contents of a directory. Each entry has `name`, `type` (`"file"`, `"dir"`, or `"symlink"`), and `size` in bytes.

### `sandbox.stat(path): Promise<StatResult>`

Get file metadata. Returns `{ size, mode, mtime, isDir, isFile, isSymlink }`. `mtime` is seconds since the Unix epoch. `mode` includes the file type bits (e.g. `0o100644` for a regular file with 644 permissions).

### `sandbox.remove(path, opts?): Promise<void>`

Delete a file or empty directory. To remove a non-empty directory, pass `{ recursive: true }`.

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `recursive` | `boolean` | `false` | Remove directories and their contents |

### `sandbox.rename(oldPath, newPath): Promise<void>`

Move or rename a file or directory within the guest filesystem. Atomic on the same filesystem.

### `sandbox.copy(src, dst, opts?): Promise<void>`

Copy a file. To copy a directory tree, pass `{ recursive: true }`.

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `recursive` | `boolean` | `false` | Copy directories recursively |

### `sandbox.chmod(path, mode): Promise<void>`

Change file permissions. `mode` is a numeric permission value (e.g. `0o755`).

### `sandbox.exists(path): Promise<boolean>`

Check if a path exists. Returns `true` if it does, `false` otherwise.

### `sandbox.checkpoint(name): Promise<void>`

Save the VM's disk state and stop the VM. To continue working, call `Sandbox.start({ from: name })`.

### `sandbox.stop(): Promise<void>`

Stop the VM without saving. All changes are discarded.

### Secrets

Secrets keep API keys on the host. The guest receives a random placeholder token; the proxy substitutes the real value only on HTTPS requests to the specified hosts.

```ts
const sb = await Sandbox.start({
  allowNet: true,
  secrets: {
    API_KEY: { value: "sk-your-openai-key", hosts: ["api.openai.com"] },
  },
});
// Inside the VM, $API_KEY is a placeholder token.
// Requests to api.openai.com get the real key injected by the proxy.
```

### Network policy

Restrict which domains the guest can reach:

```ts
const sb = await Sandbox.start({
  allowNet: true,
  network: { allow: ["api.openai.com", "*.npmjs.org"] },
});
```

Omit `network.allow` to allow all domains.

## Concurrency

Multiple `spawn()` calls run concurrently in the same VM. Each gets a unique pid and independent stdout/stderr streams. You can mix `spawn()`, `exec()`, and `watch()` freely:

```ts
// Start a dev server, run tests, and watch for changes — all at once
const server = await sb.spawn("npm run dev", { cwd: "/workspace" });
const watcher = sb.watch("/workspace", (e) => console.log(e));
const tests = await sb.exec("npm test");
```

## Requirements

- macOS 14+ on Apple Silicon
- [shuru CLI](https://github.com/superhq-ai/shuru) installed
- Bun runtime
