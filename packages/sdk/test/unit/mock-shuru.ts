/**
 * Mock shuru binary for unit testing the SDK's streaming/notification features.
 * Speaks the same JSON-RPC 2.0 protocol as the real CLI --stdio mode.
 */

const decoder = new TextDecoder();

function writeLine(obj: unknown) {
	const line = `${JSON.stringify(obj)}\n`;
	process.stdout.write(line);
}

// Send ready notification
writeLine({ jsonrpc: "2.0", method: "ready" });

const reader = Bun.stdin.stream().getReader();
let remainder = "";

async function main() {
	while (true) {
		const { done, value } = await reader.read();
		if (done) break;

		remainder += decoder.decode(value, { stream: true });

		while (true) {
			const idx = remainder.indexOf("\n");
			if (idx === -1) break;
			const line = remainder.slice(0, idx);
			remainder = remainder.slice(idx + 1);
			if (!line.trim()) continue;

			const msg = JSON.parse(line);
			await handleMessage(msg);
		}
	}
}

async function handleMessage(msg: {
	id?: number;
	method: string;
	params?: Record<string, unknown>;
}) {
	switch (msg.method) {
		case "exec": {
			const argv = msg.params?.argv as string[];
			const cmd = argv?.join(" ") ?? "";
			if (cmd.includes("echo hello")) {
				writeLine({
					jsonrpc: "2.0",
					id: msg.id,
					result: { stdout: "hello\n", stderr: "", exit_code: 0 },
				});
			} else {
				writeLine({
					jsonrpc: "2.0",
					id: msg.id,
					result: { stdout: "", stderr: "", exit_code: 0 },
				});
			}
			break;
		}

		case "spawn": {
			const argv = msg.params?.argv as string[];
			const cwd = msg.params?.cwd as string | undefined;
			const cmd = argv?.join(" ") ?? "";
			const pid = `p${msg.id}`;

			// Respond with pid immediately
			writeLine({ jsonrpc: "2.0", id: msg.id, result: { pid } });

			// Simulate streaming output based on command
			if (cmd.includes("stream-test")) {
				// Send 3 stdout chunks, 1 stderr chunk, then exit
				for (let i = 1; i <= 3; i++) {
					await Bun.sleep(10);
					writeLine({
						jsonrpc: "2.0",
						method: "output",
						params: {
							pid,
							stream: "stdout",
							data: Buffer.from(`chunk${i}\n`).toString("base64"),
						},
					});
				}
				await Bun.sleep(10);
				writeLine({
					jsonrpc: "2.0",
					method: "output",
					params: {
						pid,
						stream: "stderr",
						data: Buffer.from("warn\n").toString("base64"),
					},
				});
				await Bun.sleep(10);
				writeLine({
					jsonrpc: "2.0",
					method: "exit",
					params: { pid, code: 0 },
				});
			} else if (cmd.includes("exit-42")) {
				await Bun.sleep(10);
				writeLine({
					jsonrpc: "2.0",
					method: "exit",
					params: { pid, code: 42 },
				});
			} else if (cmd.includes("long-running")) {
				// Send output slowly — will be killed
				const interval = setInterval(() => {
					writeLine({
						jsonrpc: "2.0",
						method: "output",
						params: {
							pid,
							stream: "stdout",
							data: Buffer.from("tick\n").toString("base64"),
						},
					});
				}, 50);

				// Store interval so kill can clear it
				(globalThis as Record<string, unknown>)[`interval_${pid}`] = interval;
			} else if (cmd.includes("cwd-test")) {
				await Bun.sleep(10);
				writeLine({
					jsonrpc: "2.0",
					method: "output",
					params: {
						pid,
						stream: "stdout",
						data: Buffer.from(`cwd=${cwd ?? "none"}\n`).toString("base64"),
					},
				});
				await Bun.sleep(10);
				writeLine({
					jsonrpc: "2.0",
					method: "exit",
					params: { pid, code: 0 },
				});
			} else if (cmd.includes("stdin-echo")) {
				// Wait for input notifications — they'll be forwarded
				// The test will send input and then kill
				// We just exit after a short delay
				await Bun.sleep(200);
				writeLine({
					jsonrpc: "2.0",
					method: "exit",
					params: { pid, code: 0 },
				});
			} else {
				// Default: immediate exit
				await Bun.sleep(10);
				writeLine({
					jsonrpc: "2.0",
					method: "exit",
					params: { pid, code: 0 },
				});
			}
			break;
		}

		case "kill": {
			const pid = msg.params?.pid as string;
			const key = `interval_${pid}`;
			const interval = (globalThis as Record<string, unknown>)[key] as
				| ReturnType<typeof setInterval>
				| undefined;
			if (interval) {
				clearInterval(interval);
				delete (globalThis as Record<string, unknown>)[key];
			}
			writeLine({ jsonrpc: "2.0", id: msg.id, result: {} });
			// Send exit after kill
			await Bun.sleep(10);
			writeLine({
				jsonrpc: "2.0",
				method: "exit",
				params: { pid, code: 137 },
			});
			break;
		}

		case "input": {
			// In mock, we just ignore input data
			break;
		}

		case "watch": {
			const watchPath = msg.params?.path as string;
			writeLine({ jsonrpc: "2.0", id: msg.id, result: {} });

			// Simulate a realistic sequence of file change events
			const events = [
				{ path: `${watchPath}/src/main.ts`, event: "modify" },
				{ path: `${watchPath}/src/new.ts`, event: "create" },
				{ path: `${watchPath}/src/old.ts`, event: "delete" },
				{ path: `${watchPath}/src/renamed.ts`, event: "rename" },
			];

			for (const evt of events) {
				await Bun.sleep(15);
				writeLine({
					jsonrpc: "2.0",
					method: "file_change",
					params: evt,
				});
			}
			break;
		}

		case "read_file": {
			writeLine({
				jsonrpc: "2.0",
				id: msg.id,
				result: {
					content: Buffer.from("file content").toString("base64"),
				},
			});
			break;
		}

		case "write_file": {
			writeLine({ jsonrpc: "2.0", id: msg.id, result: {} });
			break;
		}

		case "mkdir": {
			const path = msg.params?.path as string;
			if (path === "/fail") {
				writeLine({
					jsonrpc: "2.0",
					id: msg.id,
					error: { code: -32000, message: "mkdir /fail: Permission denied" },
				});
			} else {
				writeLine({ jsonrpc: "2.0", id: msg.id, result: {} });
			}
			break;
		}

		case "read_dir": {
			const dirPath = msg.params?.path as string;
			if (dirPath === "/nonexistent") {
				writeLine({
					jsonrpc: "2.0",
					id: msg.id,
					error: { code: -32000, message: "read_dir /nonexistent: No such file or directory" },
				});
			} else {
				writeLine({
					jsonrpc: "2.0",
					id: msg.id,
					result: {
						entries: [
							{ name: "file.txt", type: "file", size: 42 },
							{ name: "subdir", type: "dir", size: 4096 },
							{ name: "link", type: "symlink", size: 10 },
						],
					},
				});
			}
			break;
		}

		case "stat": {
			const statPath = msg.params?.path as string;
			if (statPath === "/nonexistent") {
				writeLine({
					jsonrpc: "2.0",
					id: msg.id,
					error: { code: -32000, message: "stat /nonexistent: No such file or directory" },
				});
			} else {
				writeLine({
					jsonrpc: "2.0",
					id: msg.id,
					result: {
						size: 1024,
						mode: 0o100644,
						mtime: 1700000000,
						is_dir: false,
						is_file: true,
						is_symlink: false,
					},
				});
			}
			break;
		}

		case "remove": {
			writeLine({ jsonrpc: "2.0", id: msg.id, result: {} });
			break;
		}

		case "rename": {
			writeLine({ jsonrpc: "2.0", id: msg.id, result: {} });
			break;
		}

		case "copy": {
			writeLine({ jsonrpc: "2.0", id: msg.id, result: {} });
			break;
		}

		case "chmod": {
			writeLine({ jsonrpc: "2.0", id: msg.id, result: {} });
			break;
		}

		case "trim": {
			writeLine({ jsonrpc: "2.0", id: msg.id, result: { supported: true, bytes: 1048576 } });
			break;
		}
		case "checkpoint": {
			writeLine({ jsonrpc: "2.0", id: msg.id, result: {} });
			process.exit(0);
		}
	}
}

main().catch(() => process.exit(0));
