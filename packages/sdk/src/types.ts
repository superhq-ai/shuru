export interface SecretConfig {
	/** Literal secret value held on the host. */
	value: string;
	/** Domains where this secret may be sent (e.g. "api.openai.com"). */
	hosts: string[];
}

export interface NetworkConfig {
	/** Allowed domain patterns. Omit to allow all. */
	allow?: string[];
}

export interface StartOptions {
	from?: string;
	cpus?: number;
	memory?: number;
	diskSize?: number;
	allowNet?: boolean;
	ports?: string[];
	mounts?: Record<string, string>;
	secrets?: Record<string, SecretConfig>;
	network?: NetworkConfig;
	shuruBin?: string;
}

export interface ExecResult {
	stdout: string;
	stderr: string;
	exitCode: number;
}

export interface ExecOptions {
	/** Shell to use when command is a string. Defaults to "sh". Ignored when command is an array. */
	shell?: string;
}

export interface SpawnOptions {
	cwd?: string;
	env?: Record<string, string>;
	/** Shell to use when command is a string. Defaults to "sh". Ignored when command is an array. */
	shell?: string;
}

export interface WatchOptions {
	recursive?: boolean;
}

export interface FileChangeEvent {
	path: string;
	event: "create" | "modify" | "delete" | "rename";
}

// --- Filesystem types ---

export interface DirEntry {
	name: string;
	type: "file" | "dir" | "symlink";
	size: number;
}

export interface StatResult {
	size: number;
	mode: number;
	/** Seconds since Unix epoch */
	mtime: number;
	isDir: boolean;
	isFile: boolean;
	isSymlink: boolean;
}

export interface MkdirOptions {
	recursive?: boolean;
}

export interface RemoveOptions {
	recursive?: boolean;
}

export interface CopyOptions {
	recursive?: boolean;
}

// --- JSON-RPC 2.0 wire types (internal) ---

export interface JsonRpcResult {
	jsonrpc: "2.0";
	id: number;
	result: unknown;
}

export interface JsonRpcError {
	jsonrpc: "2.0";
	id: number;
	error: { code: number; message: string };
}

export interface JsonRpcNotification {
	jsonrpc: "2.0";
	method: string;
	params?: Record<string, unknown>;
}

export type JsonRpcResponse =
	| JsonRpcResult
	| JsonRpcError
	| JsonRpcNotification;
