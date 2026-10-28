import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { Sandbox } from "../../src/sandbox";

const MOCK_BIN = `bun ${resolve(import.meta.dir, "mock-shuru.ts")}`;

describe("mkdir", () => {
	let sb: Sandbox;

	beforeAll(async () => {
		sb = await Sandbox.start({ shuruBin: MOCK_BIN });
	}, 10_000);

	afterAll(async () => {
		await sb?.stop();
	}, 5_000);

	test("creates directory", async () => {
		await sb.mkdir("/tmp/test");
	});

	test("recursive defaults to true", async () => {
		await sb.mkdir("/tmp/a/b/c");
	});

	test("explicit recursive false", async () => {
		await sb.mkdir("/tmp/single", { recursive: false });
	});

	test("throws on failure", async () => {
		await expect(sb.mkdir("/fail")).rejects.toThrow("Permission denied");
	});
});

describe("readDir", () => {
	let sb: Sandbox;

	beforeAll(async () => {
		sb = await Sandbox.start({ shuruBin: MOCK_BIN });
	}, 10_000);

	afterAll(async () => {
		await sb?.stop();
	}, 5_000);

	test("returns entries with name, type, size", async () => {
		const entries = await sb.readDir("/tmp");
		expect(entries).toHaveLength(3);
		expect(entries[0]).toEqual({ name: "file.txt", type: "file", size: 42 });
		expect(entries[1]).toEqual({
			name: "subdir",
			type: "dir",
			size: 4096,
		});
		expect(entries[2]).toEqual({ name: "link", type: "symlink", size: 10 });
	});

	test("throws on nonexistent path", async () => {
		await expect(sb.readDir("/nonexistent")).rejects.toThrow(
			"No such file or directory",
		);
	});
});

describe("stat", () => {
	let sb: Sandbox;

	beforeAll(async () => {
		sb = await Sandbox.start({ shuruBin: MOCK_BIN });
	}, 10_000);

	afterAll(async () => {
		await sb?.stop();
	}, 5_000);

	test("returns metadata with camelCase fields", async () => {
		const s = await sb.stat("/tmp/file.txt");
		expect(s.size).toBe(1024);
		expect(s.mode).toBe(0o100644);
		expect(s.mtime).toBe(1700000000);
		expect(s.isDir).toBe(false);
		expect(s.isFile).toBe(true);
		expect(s.isSymlink).toBe(false);
	});

	test("throws on nonexistent path", async () => {
		await expect(sb.stat("/nonexistent")).rejects.toThrow(
			"No such file or directory",
		);
	});
});

describe("remove", () => {
	let sb: Sandbox;

	beforeAll(async () => {
		sb = await Sandbox.start({ shuruBin: MOCK_BIN });
	}, 10_000);

	afterAll(async () => {
		await sb?.stop();
	}, 5_000);

	test("removes file", async () => {
		await sb.remove("/tmp/file.txt");
	});

	test("removes directory recursively", async () => {
		await sb.remove("/tmp/dir", { recursive: true });
	});
});

describe("rename", () => {
	let sb: Sandbox;

	beforeAll(async () => {
		sb = await Sandbox.start({ shuruBin: MOCK_BIN });
	}, 10_000);

	afterAll(async () => {
		await sb?.stop();
	}, 5_000);

	test("renames file", async () => {
		await sb.rename("/tmp/old.txt", "/tmp/new.txt");
	});
});

describe("copy", () => {
	let sb: Sandbox;

	beforeAll(async () => {
		sb = await Sandbox.start({ shuruBin: MOCK_BIN });
	}, 10_000);

	afterAll(async () => {
		await sb?.stop();
	}, 5_000);

	test("copies file", async () => {
		await sb.copy("/tmp/src.txt", "/tmp/dst.txt");
	});

	test("copies directory recursively", async () => {
		await sb.copy("/tmp/srcdir", "/tmp/dstdir", { recursive: true });
	});
});

describe("chmod", () => {
	let sb: Sandbox;

	beforeAll(async () => {
		sb = await Sandbox.start({ shuruBin: MOCK_BIN });
	}, 10_000);

	afterAll(async () => {
		await sb?.stop();
	}, 5_000);

	test("changes permissions", async () => {
		await sb.chmod("/tmp/file.txt", 0o755);
	});
});

describe("exists", () => {
	let sb: Sandbox;

	beforeAll(async () => {
		sb = await Sandbox.start({ shuruBin: MOCK_BIN });
	}, 10_000);

	afterAll(async () => {
		await sb?.stop();
	}, 5_000);

	test("returns true for existing path", async () => {
		const result = await sb.exists("/tmp/file.txt");
		expect(result).toBe(true);
	});

	test("returns false for nonexistent path", async () => {
		const result = await sb.exists("/nonexistent");
		expect(result).toBe(false);
	});
});

test("trim reports support and leaves the sandbox running", async () => {
	const sb = await Sandbox.start({ shuruBin: MOCK_BIN });
	try {
		expect(await sb.trim()).toEqual({ supported: true, bytes: 1048576 });
		await sb.mkdir("/tmp/after-trim");
	} finally {
		await sb.stop();
	}
});
