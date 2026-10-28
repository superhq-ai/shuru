import type { Subprocess } from "bun";
import type { FileChangeEvent, JsonRpcResponse, JsonRpcResult } from "./types";

interface PendingRequest {
	resolve: (value: JsonRpcResult) => void;
	reject: (reason: Error) => void;
}

export interface ProcessEventHandlers {
	onStdout: (data: Buffer) => void;
	onStderr: (data: Buffer) => void;
	onExit: (code: number) => void;
}

export class ShuruProcess {
	private proc: Subprocess<"pipe", "pipe", "inherit"> | null = null;
	private pending = new Map<number, PendingRequest>();
	private idCounter = 0;
	private onReady: (() => void) | null = null;
	private onReadyError: ((err: Error) => void) | null = null;

	/** Handlers for spawned process output/exit, keyed by pid. */
	readonly processHandlers = new Map<string, ProcessEventHandlers>();

	/** Handler for file change events from the guest watcher. */
	fileChangeHandler: ((event: FileChangeEvent) => void) | null = null;

	async start(args: string[]): Promise<void> {
		this.proc = Bun.spawn(args, {
			stdin: "pipe",
			stdout: "pipe",
			stderr: "inherit",
		});

		this.readLoop();

		await new Promise<void>((resolve, reject) => {
			const timeout = setTimeout(() => {
				reject(new Error("shuru: timed out waiting for ready signal (30s)"));
			}, 30_000);

			this.onReady = () => {
				clearTimeout(timeout);
				resolve();
			};
			this.onReadyError = (err: Error) => {
				clearTimeout(timeout);
				reject(err);
			};
		});
	}

	send(
		method: string,
		params: Record<string, unknown>,
	): Promise<JsonRpcResult> {
		if (!this.proc) throw new Error("shuru process not started");

		const id = ++this.idCounter;
		const line = `${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`;
		const proc = this.proc;

		return new Promise<JsonRpcResult>((resolve, reject) => {
			this.pending.set(id, { resolve, reject });
			proc.stdin.write(line);
			proc.stdin.flush();
		});
	}

	/** Send a fire-and-forget notification (no id, no response expected). */
	sendNotification(method: string, params: Record<string, unknown>): void {
		if (!this.proc) throw new Error("shuru process not started");
		const line = `${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`;
		this.proc.stdin.write(line);
		this.proc.stdin.flush();
	}

	async stop(): Promise<void> {
		if (!this.proc) return;

		try {
			this.proc.stdin.end();
		} catch {
			// stdin may already be closed
		}

		const exited = this.proc.exited;
		const timeout = new Promise<never>((_, reject) =>
			setTimeout(
				() => reject(new Error("shuru: shutdown timed out (5s)")),
				5_000,
			),
		);

		try {
			await Promise.race([exited, timeout]);
		} catch {
			this.proc.kill();
			await this.proc.exited;
		}

		for (const [, req] of this.pending) {
			req.reject(new Error("shuru process stopped"));
		}
		this.pending.clear();
		this.proc = null;
	}

	private async readLoop(): Promise<void> {
		if (!this.proc?.stdout) return;

		const reader = this.proc.stdout.getReader();
		const decoder = new TextDecoder();
		// A file read arrives as one line of tens of megabytes, so only the
		// newest chunk is searched for the newline.
		let parts: string[] = [];

		try {
			while (true) {
				const { done, value } = await reader.read();
				if (done) break;

				const text = decoder.decode(value, { stream: true });
				let start = 0;
				let newlineIdx = text.indexOf("\n");
				while (newlineIdx !== -1) {
					parts.push(text.slice(start, newlineIdx));
					const line = parts.join("");
					parts = [];
					start = newlineIdx + 1;
					newlineIdx = text.indexOf("\n", start);

					if (!line) continue;

					try {
						const msg = JSON.parse(line) as JsonRpcResponse;
						this.dispatch(msg);
					} catch {
						// malformed line
					}
				}
				if (start < text.length) parts.push(text.slice(start));
			}
		} catch {
			// stream closed
		}

		if (this.onReadyError) {
			this.onReadyError(new Error("shuru process exited unexpectedly"));
			this.onReady = null;
			this.onReadyError = null;
		}
		for (const [, req] of this.pending) {
			req.reject(new Error("shuru process exited unexpectedly"));
		}
		this.pending.clear();
	}

	private dispatch(msg: JsonRpcResponse): void {
		// Handle notifications (no id)
		if ("method" in msg && !("id" in msg)) {
			const params = msg.params as Record<string, unknown> | undefined;

			switch (msg.method) {
				case "ready": {
					if (this.onReady) {
						this.onReady();
						this.onReady = null;
						this.onReadyError = null;
					}
					return;
				}
				case "output": {
					if (!params) return;
					const pid = params.pid as string;
					const h = this.processHandlers.get(pid);
					if (!h) return;
					const buf = Buffer.from(params.data as string, "base64");
					if (params.stream === "stdout") h.onStdout(buf);
					else if (params.stream === "stderr") h.onStderr(buf);
					return;
				}
				case "exit": {
					if (!params) return;
					const pid = params.pid as string;
					const h = this.processHandlers.get(pid);
					if (h) {
						h.onExit(params.code as number);
						this.processHandlers.delete(pid);
					}
					return;
				}
				case "file_change": {
					if (!params) return;
					this.fileChangeHandler?.({
						path: params.path as string,
						event: params.event as FileChangeEvent["event"],
					});
					return;
				}
			}
			return;
		}

		if (!("id" in msg) || msg.id == null) return;
		const id = msg.id as number;

		const req = this.pending.get(id);
		if (!req) return;
		this.pending.delete(id);

		if ("error" in msg) {
			req.reject(new Error(msg.error.message));
		} else {
			req.resolve(msg as JsonRpcResult);
		}
	}
}
