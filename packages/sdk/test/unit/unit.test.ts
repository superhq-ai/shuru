import { describe, expect, test } from "bun:test";
import { buildArgs } from "../../src/sandbox";

describe("buildArgs", () => {
	test("minimal options", () => {
		const args = buildArgs("shuru", {});
		expect(args).toEqual(["shuru", "run", "--stdio"]);
	});

	test("custom binary path", () => {
		const args = buildArgs("/usr/local/bin/shuru", {});
		expect(args).toEqual(["/usr/local/bin/shuru", "run", "--stdio"]);
	});

	test("multi-word binary", () => {
		const args = buildArgs("bun mock-shuru.ts", {});
		expect(args).toEqual(["bun", "mock-shuru.ts", "run", "--stdio"]);
	});

	test("from checkpoint", () => {
		const args = buildArgs("shuru", { from: "my-checkpoint" });
		expect(args).toEqual([
			"shuru",
			"run",
			"--stdio",
			"--from",
			"my-checkpoint",
		]);
	});

	test("cpus and memory", () => {
		const args = buildArgs("shuru", { cpus: 4, memory: 4096 });
		expect(args).toEqual([
			"shuru",
			"run",
			"--stdio",
			"--cpus",
			"4",
			"--memory",
			"4096",
		]);
	});

	test("disk size", () => {
		const args = buildArgs("shuru", { diskSize: 8192 });
		expect(args).toEqual(["shuru", "run", "--stdio", "--disk-size", "8192"]);
	});

	test("allow net", () => {
		const args = buildArgs("shuru", { allowNet: true });
		expect(args).toEqual(["shuru", "run", "--stdio", "--allow-net"]);
	});

	test("allowNet false is omitted", () => {
		const args = buildArgs("shuru", { allowNet: false });
		expect(args).toEqual(["shuru", "run", "--stdio"]);
	});

	test("port forwards", () => {
		const args = buildArgs("shuru", { ports: ["8080:80", "3000:3000"] });
		expect(args).toEqual([
			"shuru",
			"run",
			"--stdio",
			"-p",
			"8080:80",
			"-p",
			"3000:3000",
		]);
	});

	test("mounts", () => {
		const args = buildArgs("shuru", {
			mounts: { "./src": "/workspace", "./data": "/data" },
		});
		expect(args).toEqual([
			"shuru",
			"run",
			"--stdio",
			"--mount",
			"./src:/workspace",
			"--mount",
			"./data:/data",
		]);
	});

	test("secrets", () => {
		const args = buildArgs("shuru", {
			allowNet: true,
			secrets: {
				API_KEY: { value: "sk-test", hosts: ["api.openai.com"] },
			},
		});
		expect(args).toEqual([
			"shuru",
			"run",
			"--stdio",
			"--allow-net",
			"--secret",
			"API_KEY=sk-test@api.openai.com",
		]);
	});

	test("secrets with multiple hosts", () => {
		const args = buildArgs("shuru", {
			secrets: {
				TOKEN: { value: "tok@test", hosts: ["a.com", "b.com"] },
			},
		});
		expect(args).toEqual([
			"shuru",
			"run",
			"--stdio",
			"--secret",
			"TOKEN=tok@test@a.com,b.com",
		]);
	});

	test("network allow hosts", () => {
		const args = buildArgs("shuru", {
			network: { allow: ["api.openai.com", "*.npmjs.org"] },
		});
		expect(args).toEqual([
			"shuru",
			"run",
			"--stdio",
			"--allow-host",
			"api.openai.com",
			"--allow-host",
			"*.npmjs.org",
		]);
	});

	test("all options combined", () => {
		const args = buildArgs("shuru", {
			from: "base",
			cpus: 2,
			memory: 2048,
			diskSize: 4096,
			allowNet: true,
			ports: ["8080:80"],
			mounts: { "./src": "/workspace" },
		});
		expect(args).toEqual([
			"shuru",
			"run",
			"--stdio",
			"--from",
			"base",
			"--cpus",
			"2",
			"--memory",
			"2048",
			"--disk-size",
			"4096",
			"--allow-net",
			"-p",
			"8080:80",
			"--mount",
			"./src:/workspace",
		]);
	});
});
