import type { ShuruProcess } from "./process";

const STDIN_CHUNK = 256 * 1024;
const STDIN_WINDOW = 1024 * 1024;

type EventMap = {
	stdout: (data: Buffer) => void;
	stderr: (data: Buffer) => void;
	exit: (code: number) => void;
};

export class SandboxProcess {
	readonly pid: string;
	readonly exited: Promise<number>;
	private proc: ShuruProcess;
	private listeners: {
		stdout: ((data: Buffer) => void)[];
		stderr: ((data: Buffer) => void)[];
		exit: ((code: number) => void)[];
	} = { stdout: [], stderr: [], exit: [] };
	private stdinQueue: {
		chunk: Buffer;
		resolve: () => void;
		reject: (err: Error) => void;
	}[] = [];
	private stdinInFlight = 0;

	constructor(proc: ShuruProcess, pid: string) {
		this.proc = proc;
		this.pid = pid;

		this.exited = new Promise<number>((resolve) => {
			proc.processHandlers.set(pid, {
				onStdout: (data) => {
					for (const h of this.listeners.stdout) h(data);
				},
				onStderr: (data) => {
					for (const h of this.listeners.stderr) h(data);
				},
				onExit: (code) => {
					for (const h of this.listeners.exit) h(code);
					resolve(code);
				},
			});
		});
	}

	on<K extends keyof EventMap>(event: K, handler: EventMap[K]): this {
		this.listeners[event].push(handler);
		return this;
	}

	/**
	 * Write to the process stdin. Resolves once the data reaches the VM.
	 * Writes stay in order; past STDIN_WINDOW in flight they wait here, in
	 * this process, instead of piling up in the sandbox host.
	 */
	write(data: Buffer | string): Promise<void> {
		const buf = Buffer.from(data);
		const parts: Promise<void>[] = [];
		for (let at = 0; at < buf.length; at += STDIN_CHUNK) {
			const chunk = buf.subarray(at, at + STDIN_CHUNK);
			parts.push(
				new Promise<void>((resolve, reject) => {
					this.stdinQueue.push({ chunk, resolve, reject });
				}),
			);
		}
		this.pumpStdin();
		const done = Promise.all(parts).then(() => undefined);
		// A caller that does not await should not get an unhandled rejection.
		done.catch(() => {});
		return done;
	}

	private pumpStdin(): void {
		while (this.stdinQueue.length > 0) {
			const next = this.stdinQueue[0];
			if (
				this.stdinInFlight > 0 &&
				this.stdinInFlight + next.chunk.length > STDIN_WINDOW
			) {
				return;
			}
			this.stdinQueue.shift();
			this.stdinInFlight += next.chunk.length;
			this.proc
				.send("input", {
					pid: this.pid,
					data: next.chunk.toString("base64"),
				})
				.then(() => next.resolve(), next.reject)
				.finally(() => {
					this.stdinInFlight -= next.chunk.length;
					this.pumpStdin();
				});
		}
	}

	async kill(): Promise<void> {
		await this.proc.send("kill", { pid: this.pid });
	}
}
