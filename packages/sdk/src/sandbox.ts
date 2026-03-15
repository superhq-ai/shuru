import { ShuruProcess } from "./process";
import { SandboxProcess } from "./process-handle";
import type {
	CopyOptions,
	DirEntry,
	ExecOptions,
	ExecResult,
	FileChangeEvent,
	MkdirOptions,
	RemoveOptions,
	SpawnOptions,
	StartOptions,
	StatResult,
	WatchOptions,
} from "./types";

const Method = {
	EXEC: "exec",
	SPAWN: "spawn",
	READ_FILE: "read_file",
	WRITE_FILE: "write_file",
	CHECKPOINT: "checkpoint",
	WATCH: "watch",
	MKDIR: "mkdir",
	READ_DIR: "read_dir",
	STAT: "stat",
	REMOVE: "remove",
	RENAME: "rename",
	COPY: "copy",
	CHMOD: "chmod",
} as const;

export class Sandbox {
	private proc: ShuruProcess;
	private stopped = false;

	private constructor(proc: ShuruProcess) {
		this.proc = proc;
	}

	static async start(opts: StartOptions = {}): Promise<Sandbox> {
		const bin = opts.shuruBin ?? "shuru";
		const args = buildArgs(bin, opts);

		const proc = new ShuruProcess();
		await proc.start(args);

		return new Sandbox(proc);
	}

	async exec(
		command: string | string[],
		opts?: ExecOptions,
	): Promise<ExecResult> {
		const argv =
			typeof command === "string"
				? [opts?.shell ?? "sh", "-c", command]
				: command;
		const resp = await this.proc.send(Method.EXEC, { argv });
		const r = resp.result as {
			stdout: string;
			stderr: string;
			exit_code: number;
		};
		return {
			stdout: r.stdout,
			stderr: r.stderr,
			exitCode: r.exit_code,
		};
	}

	async spawn(
		command: string | string[],
		opts?: SpawnOptions,
	): Promise<SandboxProcess> {
		const argv =
			typeof command === "string"
				? [opts?.shell ?? "sh", "-c", command]
				: command;
		const resp = await this.proc.send(Method.SPAWN, {
			argv,
			cwd: opts?.cwd,
			env: opts?.env,
		});
		const { pid } = resp.result as { pid: string };
		return new SandboxProcess(this.proc, pid);
	}

	async watch(
		path: string,
		handler: (event: FileChangeEvent) => void,
		opts?: WatchOptions,
	): Promise<void> {
		this.proc.fileChangeHandler = handler;
		await this.proc.send(Method.WATCH, {
			path,
			recursive: opts?.recursive ?? true,
		});
	}

	async readFile(path: string): Promise<Uint8Array> {
		const resp = await this.proc.send(Method.READ_FILE, { path });
		const r = resp.result as { content: string };
		return new Uint8Array(Buffer.from(r.content, "base64"));
	}

	async writeFile(path: string, content: Uint8Array | string): Promise<void> {
		const b64 = Buffer.from(content).toString("base64");
		await this.proc.send(Method.WRITE_FILE, { path, content: b64 });
	}

	async mkdir(path: string, opts?: MkdirOptions): Promise<void> {
		await this.proc.send(Method.MKDIR, {
			path,
			recursive: opts?.recursive ?? true,
		});
	}

	async readDir(path: string): Promise<DirEntry[]> {
		const resp = await this.proc.send(Method.READ_DIR, { path });
		const r = resp.result as { entries: DirEntry[] };
		return r.entries;
	}

	async stat(path: string): Promise<StatResult> {
		const resp = await this.proc.send(Method.STAT, { path });
		const r = resp.result as {
			size: number;
			mode: number;
			mtime: number;
			is_dir: boolean;
			is_file: boolean;
			is_symlink: boolean;
		};
		return {
			size: r.size,
			mode: r.mode,
			mtime: r.mtime,
			isDir: r.is_dir,
			isFile: r.is_file,
			isSymlink: r.is_symlink,
		};
	}

	async remove(path: string, opts?: RemoveOptions): Promise<void> {
		await this.proc.send(Method.REMOVE, {
			path,
			recursive: opts?.recursive ?? false,
		});
	}

	async rename(oldPath: string, newPath: string): Promise<void> {
		await this.proc.send(Method.RENAME, {
			old_path: oldPath,
			new_path: newPath,
		});
	}

	async copy(src: string, dst: string, opts?: CopyOptions): Promise<void> {
		await this.proc.send(Method.COPY, {
			src,
			dst,
			recursive: opts?.recursive ?? false,
		});
	}

	async chmod(path: string, mode: number): Promise<void> {
		await this.proc.send(Method.CHMOD, { path, mode });
	}

	async exists(path: string): Promise<boolean> {
		try {
			await this.stat(path);
			return true;
		} catch {
			return false;
		}
	}

	async checkpoint(name: string): Promise<void> {
		await this.proc.send(Method.CHECKPOINT, { name });
		this.stopped = true;
		await this.proc.stop();
	}

	async stop(): Promise<void> {
		if (this.stopped) return;
		this.stopped = true;
		await this.proc.stop();
	}
}

/** @internal exported for testing */
export function buildArgs(bin: string, opts: StartOptions): string[] {
	const args = [...bin.split(/\s+/), "run", "--stdio"];

	if (opts.from) args.push("--from", opts.from);
	if (opts.cpus) args.push("--cpus", String(opts.cpus));
	if (opts.memory) args.push("--memory", String(opts.memory));
	if (opts.diskSize) args.push("--disk-size", String(opts.diskSize));
	if (opts.allowNet) args.push("--allow-net");

	if (opts.secrets) {
		for (const [name, secret] of Object.entries(opts.secrets)) {
			args.push("--secret", `${name}=${secret.value}@${secret.hosts.join(",")}`);
		}
	}

	if (opts.network?.allow) {
		for (const host of opts.network.allow) {
			args.push("--allow-host", host);
		}
	}

	if (opts.ports) {
		for (const p of opts.ports) {
			args.push("-p", p);
		}
	}

	if (opts.mounts) {
		for (const [host, guest] of Object.entries(opts.mounts)) {
			args.push("--mount", `${host}:${guest}`);
		}
	}

	return args;
}
