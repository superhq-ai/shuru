use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};

use shuru_proto::{frame, WatchEvent};
use shuru_vm::Sandbox;

use crate::vm::{self, PreparedVm};

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

mod method {
    pub const EXEC: &str = "exec";
    pub const SPAWN: &str = "spawn";
    pub const KILL: &str = "kill";
    pub const INPUT: &str = "input";
    pub const WATCH: &str = "watch";
    pub const READ_FILE: &str = "read_file";
    pub const WRITE_FILE: &str = "write_file";
    pub const CHECKPOINT: &str = "checkpoint";
    pub const MKDIR: &str = "mkdir";
    pub const READ_DIR: &str = "read_dir";
    pub const STAT: &str = "stat";
    pub const REMOVE: &str = "remove";
    pub const RENAME: &str = "rename";
    pub const COPY: &str = "copy";
    pub const CHMOD: &str = "chmod";
}

// JSON-RPC 2.0 error codes
const PARSE_ERROR: i32 = -32700;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const SERVER_ERROR: i32 = -32000;

// --- JSON-RPC 2.0 types ---

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    id: serde_json::Value,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Serialize)]
struct JsonRpcNotification<P: Serialize> {
    jsonrpc: &'static str,
    method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<P>,
}

#[derive(Serialize)]
struct JsonRpcResult<T: Serialize> {
    jsonrpc: &'static str,
    id: serde_json::Value,
    result: T,
}

#[derive(Serialize)]
struct JsonRpcErrorResp {
    jsonrpc: &'static str,
    id: serde_json::Value,
    error: RpcError,
}

#[derive(Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

// --- Result payloads ---

#[derive(Serialize)]
struct ExecResultPayload {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

#[derive(Serialize)]
struct SpawnResultPayload {
    pid: String,
}

#[derive(Serialize)]
struct ReadFileResultPayload {
    content: String,
}

#[derive(Serialize)]
struct EmptyResult {}

// --- Notification payloads ---

#[derive(Serialize)]
struct OutputParams {
    pid: String,
    stream: String,
    data: String,
}

#[derive(Serialize)]
struct ExitParams {
    pid: String,
    code: i32,
}

#[derive(Serialize)]
struct FileChangeParams {
    path: String,
    event: String,
}

// --- Param types ---

#[derive(Deserialize)]
struct ExecParams {
    argv: Vec<String>,
}

#[derive(Deserialize)]
struct SpawnParams {
    argv: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct KillParams {
    pid: String,
}

#[derive(Deserialize)]
struct InputParams {
    pid: String,
    data: String,
}

#[derive(Deserialize)]
struct WatchParams {
    path: String,
    #[serde(default = "default_true")]
    recursive: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct ReadFileParams {
    path: String,
}

#[derive(Deserialize)]
struct WriteFileParams {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct CheckpointParams {
    name: String,
}

#[derive(Deserialize)]
struct MkdirParams {
    path: String,
    #[serde(default = "default_true")]
    recursive: bool,
}

#[derive(Deserialize)]
struct ReadDirParams {
    path: String,
}

#[derive(Deserialize)]
struct StatParams {
    path: String,
}

#[derive(Deserialize)]
struct RemoveParams {
    path: String,
    #[serde(default)]
    recursive: bool,
}

#[derive(Deserialize)]
struct RenameParams {
    old_path: String,
    new_path: String,
}

#[derive(Deserialize)]
struct CopyParams {
    src: String,
    dst: String,
    #[serde(default)]
    recursive: bool,
}

#[derive(Deserialize)]
struct ChmodParams {
    path: String,
    mode: u32,
}

#[derive(Serialize)]
struct DirEntryPayload {
    name: String,
    #[serde(rename = "type")]
    entry_type: String,
    size: u64,
}

#[derive(Serialize)]
struct ReadDirResultPayload {
    entries: Vec<DirEntryPayload>,
}

#[derive(Serialize)]
struct StatResultPayload {
    size: u64,
    mode: u32,
    mtime: u64,
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
}

// --- Events from background threads ---

enum Event {
    Output {
        pid: String,
        stream: &'static str,
        data: Vec<u8>,
    },
    Exit {
        pid: String,
        code: i32,
    },
    FileChange {
        path: String,
        event: String,
    },
}

// --- Process handle for stdin/kill ---

enum ProcessInput {
    Stdin(Vec<u8>),
    Kill,
}

struct ProcessHandle {
    input_tx: std::sync::mpsc::Sender<ProcessInput>,
}

// --- Shared writer ---

type SharedWriter = Arc<Mutex<io::Stdout>>;

fn send_json_shared(w: &SharedWriter, value: &impl Serialize) -> Result<()> {
    let line = serde_json::to_string(value)?;
    let mut out = w.lock().unwrap();
    writeln!(out, "{}", line)?;
    out.flush()?;
    Ok(())
}

fn send_result_shared<T: Serialize>(
    w: &SharedWriter,
    id: serde_json::Value,
    result: T,
) -> Result<()> {
    send_json_shared(
        w,
        &JsonRpcResult {
            jsonrpc: "2.0",
            id,
            result,
        },
    )
}

fn send_error_shared(
    w: &SharedWriter,
    id: serde_json::Value,
    code: i32,
    message: String,
) -> Result<()> {
    send_json_shared(
        w,
        &JsonRpcErrorResp {
            jsonrpc: "2.0",
            id,
            error: RpcError { code, message },
        },
    )
}

pub(crate) fn run_stdio(prepared: &PreparedVm) -> Result<i32> {
    let out: SharedWriter = Arc::new(Mutex::new(io::stdout()));

    // Set up proxy networking if --allow-net
    let (vm_fd, proxy_handle) = if let Some(ref proxy_config) = prepared.proxy_config {
        let (vm_fd, host_fd) = shuru_proxy::create_socketpair()?;
        let handle = shuru_proxy::start(host_fd, proxy_config.clone())?;
        (Some(vm_fd), Some(handle))
    } else {
        (None, None)
    };

    let sandbox = Arc::new(vm::build_sandbox(prepared, false, vm_fd)?);
    sandbox.start()?;

    // Inject CA cert and secret placeholders when MITM is needed
    let secret_env: HashMap<String, String> = if let Some(ref handle) = proxy_handle {
        if !handle.placeholders.is_empty() {
            sandbox.write_file(
                "/usr/local/share/ca-certificates/shuru-proxy.crt",
                &handle.ca_cert_pem,
            )?;
            sandbox.exec(
                &["update-ca-certificates", "--fresh"],
                &mut io::sink(),
                &mut io::sink(),
            )?;
        }
        handle.placeholders.clone()
    } else {
        HashMap::new()
    };

    let _fwd = if !prepared.forwards.is_empty() {
        Some(sandbox.start_port_forwarding(&prepared.forwards)?)
    } else {
        None
    };

    // Event channel: background threads -> main loop
    let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();

    // Send ready notification
    send_json_shared(
        &out,
        &JsonRpcNotification::<()> {
            jsonrpc: "2.0",
            method: "ready",
            params: None,
        },
    )?;

    let mut pid_counter: u64 = 0;
    let mut processes: HashMap<String, ProcessHandle> = HashMap::new();
    let mut bg_threads: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // Spawn a thread to drain events and write notifications to stdout.
    // This ensures we never block the main stdin-reading loop.
    let out_for_events = out.clone();
    let event_thread = std::thread::spawn(move || {
        for event in event_rx {
            let res = match event {
                Event::Output { pid, stream, data } => send_json_shared(
                    &out_for_events,
                    &JsonRpcNotification {
                        jsonrpc: "2.0",
                        method: "output",
                        params: Some(OutputParams {
                            pid,
                            stream: stream.to_string(),
                            data: BASE64.encode(&data),
                        }),
                    },
                ),
                Event::Exit { pid, code } => send_json_shared(
                    &out_for_events,
                    &JsonRpcNotification {
                        jsonrpc: "2.0",
                        method: "exit",
                        params: Some(ExitParams { pid, code }),
                    },
                ),
                Event::FileChange { path, event } => send_json_shared(
                    &out_for_events,
                    &JsonRpcNotification {
                        jsonrpc: "2.0",
                        method: "file_change",
                        params: Some(FileChangeParams { path, event }),
                    },
                ),
            };
            if res.is_err() {
                break;
            }
        }
    });

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Err(_) => break,
            Ok(_) => {}
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                send_error_shared(
                    &out,
                    serde_json::Value::Null,
                    PARSE_ERROR,
                    format!("parse error: {}", e),
                )?;
                continue;
            }
        };

        match req.method.as_str() {
            method::EXEC => {
                let params: ExecParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                handle_exec(&sandbox, req.id, &params.argv, &secret_env, &out)?;
            }

            method::SPAWN => {
                let params: SpawnParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };

                pid_counter += 1;
                let pid = format!("p{}", pid_counter);

                // Channel for sending stdin/kill to the process
                let (input_tx, input_rx) = std::sync::mpsc::channel::<ProcessInput>();
                processes.insert(pid.clone(), ProcessHandle { input_tx });

                let sb = sandbox.clone();
                let tx = event_tx.clone();
                let pid_clone = pid.clone();
                let mut spawn_env = secret_env.clone();
                spawn_env.extend(params.env.into_iter());

                bg_threads.push(std::thread::spawn(move || {
                    let argv: Vec<&str> = params.argv.iter().map(|s| s.as_str()).collect();
                    let stream = match sb.open_exec(&argv, &spawn_env, params.cwd.as_deref()) {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("shuru: spawn failed: {}", e);
                            let _ = tx.send(Event::Exit {
                                pid: pid_clone,
                                code: 1,
                            });
                            return;
                        }
                    };

                    let mut vsock_reader = match stream.try_clone() {
                        Ok(r) => r,
                        Err(_) => return,
                    };
                    let mut vsock_writer = stream;

                    // Thread: forward stdin/kill from SDK to vsock.
                    // Returns vsock_writer so it isn't dropped before the read
                    // loop finishes — on Apple vsock, dropping the original fd
                    // closes the entire connection even if a dup'd reader exists.
                    let input_thread = std::thread::spawn(move || -> std::net::TcpStream {
                        for msg in input_rx {
                            match msg {
                                ProcessInput::Stdin(data) => {
                                    if frame::write_frame(&mut vsock_writer, frame::STDIN, &data)
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                ProcessInput::Kill => {
                                    let _ =
                                        frame::write_frame(&mut vsock_writer, frame::KILL, &[]);
                                    break;
                                }
                            }
                        }
                        vsock_writer
                    });

                    // Read vsock frames -> send events
                    let pid_for_read = pid_clone.clone();
                    loop {
                        match frame::read_frame(&mut vsock_reader) {
                            Ok(Some((frame::STDOUT, data))) => {
                                let _ = tx.send(Event::Output {
                                    pid: pid_for_read.clone(),
                                    stream: "stdout",
                                    data,
                                });
                            }
                            Ok(Some((frame::STDERR, data))) => {
                                let _ = tx.send(Event::Output {
                                    pid: pid_for_read.clone(),
                                    stream: "stderr",
                                    data,
                                });
                            }
                            Ok(Some((frame::EXIT, data))) => {
                                let code = frame::parse_exit_code(&data).unwrap_or(0);
                                let _ = tx.send(Event::Exit {
                                    pid: pid_for_read.clone(),
                                    code,
                                });
                                break;
                            }
                            Ok(Some((frame::ERROR, data))) => {
                                let _ = tx.send(Event::Output {
                                    pid: pid_for_read.clone(),
                                    stream: "stderr",
                                    data,
                                });
                                let _ = tx.send(Event::Exit {
                                    pid: pid_for_read.clone(),
                                    code: 1,
                                });
                                break;
                            }
                            _ => {
                                let _ = tx.send(Event::Exit {
                                    pid: pid_for_read.clone(),
                                    code: 1,
                                });
                                break;
                            }
                        }
                    }

                    let _ = input_thread.join();
                }));

                send_result_shared(&out, req.id, SpawnResultPayload { pid })?;
            }

            method::KILL => {
                let params: KillParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                if let Some(handle) = processes.get(&params.pid) {
                    let _ = handle.input_tx.send(ProcessInput::Kill);
                }
                send_result_shared(&out, req.id, EmptyResult {})?;
            }

            method::INPUT => {
                // Fire-and-forget notification (may have no id)
                let params: InputParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if let Some(handle) = processes.get(&params.pid) {
                    if let Ok(data) = BASE64.decode(&params.data) {
                        let _ = handle.input_tx.send(ProcessInput::Stdin(data));
                    }
                }
            }

            method::WATCH => {
                let params: WatchParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };

                let sb = sandbox.clone();
                let tx = event_tx.clone();
                let path = params.path.clone();
                let recursive = params.recursive;

                bg_threads.push(std::thread::spawn(move || {
                    let stream = match sb.open_watch(&path, recursive) {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("shuru: watch failed: {}", e);
                            return;
                        }
                    };
                    let mut reader = stream;
                    loop {
                        match frame::read_frame(&mut reader) {
                            Ok(Some((frame::WATCH_EVENT, data))) => {
                                if let Ok(evt) = serde_json::from_slice::<WatchEvent>(&data) {
                                    let _ = tx.send(Event::FileChange {
                                        path: evt.path,
                                        event: evt.event,
                                    });
                                }
                            }
                            _ => break,
                        }
                    }
                }));

                send_result_shared(&out, req.id, EmptyResult {})?;
            }

            method::READ_FILE => {
                let params: ReadFileParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                handle_read_file(&sandbox, req.id, &params.path, &out)?;
            }

            method::WRITE_FILE => {
                let params: WriteFileParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                handle_write_file(&sandbox, req.id, &params.path, &params.content, &out)?;
            }

            method::CHECKPOINT => {
                let params: CheckpointParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                handle_checkpoint(&sandbox, prepared, req.id, &params.name, &out)?;
                let _ = sandbox.stop();
                drop(event_tx);
                let _ = event_thread.join();
                return Ok(0);
            }

            method::MKDIR => {
                let params: MkdirParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                match sandbox.mkdir(&params.path, params.recursive) {
                    Ok(()) => send_result_shared(&out, req.id, EmptyResult {})?,
                    Err(e) => send_error_shared(&out, req.id, SERVER_ERROR, format!("{}", e))?,
                }
            }

            method::READ_DIR => {
                let params: ReadDirParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                match sandbox.read_dir(&params.path) {
                    Ok(resp) => {
                        let entries: Vec<DirEntryPayload> = resp
                            .entries
                            .into_iter()
                            .map(|e| DirEntryPayload {
                                name: e.name,
                                entry_type: e.entry_type,
                                size: e.size,
                            })
                            .collect();
                        send_result_shared(&out, req.id, ReadDirResultPayload { entries })?;
                    }
                    Err(e) => send_error_shared(&out, req.id, SERVER_ERROR, format!("{}", e))?,
                }
            }

            method::STAT => {
                let params: StatParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                match sandbox.stat(&params.path) {
                    Ok(resp) => {
                        send_result_shared(
                            &out,
                            req.id,
                            StatResultPayload {
                                size: resp.size,
                                mode: resp.mode,
                                mtime: resp.mtime,
                                is_dir: resp.is_dir,
                                is_file: resp.is_file,
                                is_symlink: resp.is_symlink,
                            },
                        )?;
                    }
                    Err(e) => send_error_shared(&out, req.id, SERVER_ERROR, format!("{}", e))?,
                }
            }

            method::REMOVE => {
                let params: RemoveParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                match sandbox.remove(&params.path, params.recursive) {
                    Ok(()) => send_result_shared(&out, req.id, EmptyResult {})?,
                    Err(e) => send_error_shared(&out, req.id, SERVER_ERROR, format!("{}", e))?,
                }
            }

            method::RENAME => {
                let params: RenameParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                match sandbox.rename(&params.old_path, &params.new_path) {
                    Ok(()) => send_result_shared(&out, req.id, EmptyResult {})?,
                    Err(e) => send_error_shared(&out, req.id, SERVER_ERROR, format!("{}", e))?,
                }
            }

            method::COPY => {
                let params: CopyParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                match sandbox.copy(&params.src, &params.dst, params.recursive) {
                    Ok(()) => send_result_shared(&out, req.id, EmptyResult {})?,
                    Err(e) => send_error_shared(&out, req.id, SERVER_ERROR, format!("{}", e))?,
                }
            }

            method::CHMOD => {
                let params: ChmodParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error_shared(&out, req.id, INVALID_PARAMS, format!("invalid params: {}", e))?;
                        continue;
                    }
                };
                match sandbox.chmod(&params.path, params.mode) {
                    Ok(()) => send_result_shared(&out, req.id, EmptyResult {})?,
                    Err(e) => send_error_shared(&out, req.id, SERVER_ERROR, format!("{}", e))?,
                }
            }

            _ => {
                send_error_shared(
                    &out,
                    req.id,
                    METHOD_NOT_FOUND,
                    format!("method not found: {}", req.method),
                )?;
            }
        }
    }

    // Stop the VM first — this closes vsock connections, unblocking
    // any background threads stuck on read_frame().
    let _ = sandbox.stop();

    // Wait briefly for background threads to notice and exit
    for thread in bg_threads {
        let _ = thread.join();
    }

    drop(event_tx);
    let _ = event_thread.join();
    Ok(0)
}

fn handle_exec(
    sandbox: &Sandbox,
    id: serde_json::Value,
    argv: &[String],
    env: &HashMap<String, String>,
    out: &SharedWriter,
) -> Result<()> {
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    let exit_code = match sandbox.exec_with_env(argv, env, &mut stdout_buf, &mut stderr_buf) {
        Ok(code) => code,
        Err(e) => {
            return send_error_shared(out, id, SERVER_ERROR, format!("exec failed: {}", e));
        }
    };

    send_result_shared(
        out,
        id,
        ExecResultPayload {
            stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
            stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
            exit_code,
        },
    )
}

fn handle_read_file(
    sandbox: &Sandbox,
    id: serde_json::Value,
    path: &str,
    out: &SharedWriter,
) -> Result<()> {
    match sandbox.read_file(path) {
        Ok(data) => send_result_shared(
            out,
            id,
            ReadFileResultPayload {
                content: BASE64.encode(&data),
            },
        ),
        Err(e) => send_error_shared(out, id, SERVER_ERROR, format!("{}", e)),
    }
}

fn handle_write_file(
    sandbox: &Sandbox,
    id: serde_json::Value,
    path: &str,
    content: &str,
    out: &SharedWriter,
) -> Result<()> {
    let data = match BASE64.decode(content) {
        Ok(d) => d,
        Err(e) => {
            return send_error_shared(out, id, INVALID_PARAMS, format!("invalid base64: {}", e));
        }
    };

    match sandbox.write_file(path, &data) {
        Ok(()) => send_result_shared(out, id, EmptyResult {}),
        Err(e) => send_error_shared(out, id, SERVER_ERROR, format!("{}", e)),
    }
}

fn handle_checkpoint(
    sandbox: &Sandbox,
    prepared: &PreparedVm,
    id: serde_json::Value,
    name: &str,
    out: &SharedWriter,
) -> Result<()> {
    let mut discard_out = Vec::new();
    let mut discard_err = Vec::new();
    if let Err(e) = sandbox.exec(&["sync"], &mut discard_out, &mut discard_err) {
        return send_error_shared(out, id, SERVER_ERROR, format!("checkpoint sync failed: {}", e));
    }

    let data_dir = shuru_vm::default_data_dir();
    let checkpoints_dir = format!("{}/checkpoints", data_dir);
    let checkpoint_path = format!("{}/{}.ext4", checkpoints_dir, name);

    if let Err(e) = std::fs::create_dir_all(&checkpoints_dir) {
        return send_error_shared(
            out,
            id,
            SERVER_ERROR,
            format!("failed to create checkpoints dir: {}", e),
        );
    }

    if let Err(e) = vm::clone_file(&prepared.work_rootfs, &checkpoint_path) {
        return send_error_shared(out, id, SERVER_ERROR, format!("checkpoint clone failed: {}", e));
    }

    send_result_shared(out, id, EmptyResult {})
}
