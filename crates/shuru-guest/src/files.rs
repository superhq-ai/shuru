//! File frames: writes, listings, copies, modes, owners and detailed
//! watches. Errors go back as ERROR frames that end in their errno, as
//! "(os error N)".

use std::ffi::CString;
use std::fs::{self, Metadata};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;

use shuru_proto::{
    frame, watch_kind, ChmodRequest, ChownRequest, CopyRequest, DirEntry, FsOkResponse, Owner,
    ReadDirRequest, ReadDirResponse, WatchDetail, WatchRequest, WriteFileRequest,
    WriteFileResponse,
};

/// A streamed listing sends a frame once its entries pass this many bytes,
/// well under the frame limit.
const LISTING_BYTES: usize = 768 * 1024;
/// What a listing in one frame can take, its wrapper aside.
const WHOLE_LISTING_BYTES: usize = frame::MAX_FRAME as usize - 64;
/// Watch events the guest holds for a host that reads slowly.
const WATCH_BACKLOG: usize = 4096;

/// An error as the host reads it, ending in its errno.
fn describe(e: &io::Error) -> String {
    if e.raw_os_error().is_some() {
        return e.to_string();
    }
    let errno = match e.kind() {
        io::ErrorKind::NotFound => libc::ENOENT,
        io::ErrorKind::AlreadyExists => libc::EEXIST,
        io::ErrorKind::PermissionDenied => libc::EACCES,
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => libc::EINVAL,
        _ => libc::EIO,
    };
    format!("{e} (os error {errno})")
}

fn send_error(writer: &mut impl Write, what: String, e: &io::Error) {
    let msg = format!("{what}: {}", describe(e));
    let _ = frame::write_frame(writer, frame::ERROR, msg.as_bytes());
}

fn send_ok(writer: &mut impl Write, resp: FsOkResponse) {
    let _ = frame::send_json(writer, frame::FS_OK_RESP, &resp);
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

pub fn write(req: &WriteFileRequest, reader: &mut impl Read, writer: &mut impl Write) {
    let mut file = open_for_write(req);

    // Every chunk is read even after a failure, so the host gets the
    // error rather than a closed connection.
    let mut received = 0u64;
    let mut short = None;
    while received < req.len {
        let chunk = match frame::read_frame(reader) {
            Ok(Some((frame::WRITE_FILE_DATA, chunk))) if !chunk.is_empty() => chunk,
            _ => {
                short = Some("expected WRITE_FILE_DATA frame".to_string());
                break;
            }
        };
        received += chunk.len() as u64;
        if received > req.len {
            short = Some(format!(
                "length mismatch: expected {}, got {}",
                req.len, received
            ));
            break;
        }
        if let Ok(f) = &mut file {
            if let Err(e) = f.write_all(&chunk) {
                file = Err(e);
            }
        }
    }
    let result = match short {
        Some(error) => Err(error),
        None => file
            .and_then(|f| finish_write(req, f))
            .map_err(|e| format!("write_file {}: {}", req.path, describe(&e))),
    };
    let resp = match result {
        Ok((size, mode)) => WriteFileResponse {
            ok: true,
            size: Some(size),
            mode: Some(mode),
            ..Default::default()
        },
        Err(error) => WriteFileResponse {
            ok: false,
            error: Some(error),
            ..Default::default()
        },
    };
    let _ = frame::send_json(writer, frame::WRITE_FILE_RESP, &resp);
}

fn open_for_write(req: &WriteFileRequest) -> io::Result<fs::File> {
    let path = Path::new(&req.path);
    if req.mkdir != Some(false) {
        // A failure here shows as the open failing, with a truer errno.
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    if let Some(mode) = req.mode {
        options.mode(mode & 0o7777);
    }
    options.open(path)
}

fn finish_write(req: &WriteFileRequest, file: fs::File) -> io::Result<(u64, u32)> {
    if let Some(mode) = req.mode {
        file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))?;
    }
    let meta = file.metadata()?;
    Ok((meta.len(), meta.mode() & 0o7777))
}

pub fn read_dir(req: &ReadDirRequest, writer: &mut impl Write) {
    let limit = if req.stream {
        LISTING_BYTES
    } else {
        WHOLE_LISTING_BYTES
    };
    if let Err(e) = list(req, writer, limit) {
        send_error(writer, format!("read_dir {}", req.path), &e);
    }
}

fn list(req: &ReadDirRequest, writer: &mut impl Write, frame_bytes: usize) -> io::Result<()> {
    let root = Path::new(&req.path);
    let pattern = match &req.pattern {
        Some(pattern) => {
            let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
            builder
                .add_line(None, pattern)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
            Some(
                builder
                    .build()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?,
            )
        }
        None => None,
    };
    let mut listing = Listing {
        entries: Vec::new(),
        bytes: 0,
        frame_bytes,
        stream: req.stream,
    };
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if dir == root => return Err(e),
            Err(_) => continue,
        };
        // Entries go out as they are read, in directory order; one gone
        // before its metadata is read is left out. Links are not followed.
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let shown = pattern
                .as_ref()
                .is_none_or(|pattern| pattern.matched(rel, meta.is_dir()).is_ignore());
            if shown {
                listing.push(dir_entry(rel.to_string_lossy().into_owned(), &meta), writer)?;
            }
            if req.recursive && meta.is_dir() {
                dirs.push(path);
            }
        }
    }
    listing.finish(writer)
}

fn dir_entry(name: String, meta: &Metadata) -> DirEntry {
    let kind = if meta.file_type().is_symlink() {
        "symlink"
    } else if meta.is_dir() {
        "dir"
    } else {
        "file"
    };
    DirEntry {
        name,
        entry_type: kind.to_string(),
        size: meta.len(),
        mode: Some(meta.mode()),
        mtime: Some(meta.mtime().max(0) as u64),
    }
}

struct Listing {
    entries: Vec<DirEntry>,
    bytes: usize,
    frame_bytes: usize,
    stream: bool,
}

impl Listing {
    fn push(&mut self, entry: DirEntry, writer: &mut impl Write) -> io::Result<()> {
        let size = serde_json::to_vec(&entry).map_or(0, |json| json.len()) + 1;
        if self.bytes + size > self.frame_bytes && !self.entries.is_empty() {
            if !self.stream {
                return Err(io::Error::from_raw_os_error(libc::E2BIG));
            }
            let resp = ReadDirResponse {
                entries: std::mem::take(&mut self.entries),
                more: true,
            };
            frame::send_json(writer, frame::READ_DIR_RESP, &resp)?;
            self.bytes = 0;
        }
        self.bytes += size;
        self.entries.push(entry);
        Ok(())
    }

    fn finish(self, writer: &mut impl Write) -> io::Result<()> {
        let resp = ReadDirResponse {
            entries: self.entries,
            more: false,
        };
        frame::send_json(writer, frame::READ_DIR_RESP, &resp)
    }
}

#[derive(Default, Debug, PartialEq)]
struct Copied {
    count: u64,
    bytes: u64,
}

pub fn copy(req: &CopyRequest, writer: &mut impl Write) {
    let src = Path::new(&req.src);
    let dst = Path::new(&req.dst);
    match copy_paths(src, dst, req.recursive, req.preserve_attrs) {
        Ok(copied) => send_ok(
            writer,
            FsOkResponse {
                ok: true,
                count: Some(copied.count),
                total_bytes: Some(copied.bytes),
                ..Default::default()
            },
        ),
        Err(e) => send_error(writer, format!("copy {} -> {}", req.src, req.dst), &e),
    }
}

fn copy_paths(src: &Path, dst: &Path, recursive: bool, preserve: bool) -> io::Result<Copied> {
    let meta = fs::metadata(src)?;
    let mut copied = Copied::default();
    if !meta.is_dir() {
        copy_one(src, dst, &meta, preserve, &mut copied)?;
    } else if recursive {
        copy_tree(src, dst, preserve, &mut copied)?;
    } else {
        return Err(io::Error::from_raw_os_error(libc::EISDIR));
    }
    Ok(copied)
}

/// Iterative directory copy. Links are copied as links, and a destination
/// inside the source is not copied into itself.
/// Inspired by https://github.com/mdunsmuir/copy_dir (MIT).
fn copy_tree(src: &Path, dst: &Path, preserve: bool, copied: &mut Copied) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    let dst_meta = fs::metadata(dst)?;
    let dst_id = (dst_meta.dev(), dst_meta.ino());

    // Directory modes and times are set last, once nothing more is
    // written into them.
    let mut made = Vec::new();
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((s, d)) = stack.pop() {
        let meta = fs::metadata(&s)?;
        fs::create_dir_all(&d)?;
        copied.count += 1;
        for entry in fs::read_dir(&s)? {
            let entry = entry?;
            let from = entry.path();
            let to = d.join(entry.file_name());
            let child = entry.metadata()?;
            if child.is_dir() {
                if (child.dev(), child.ino()) != dst_id {
                    stack.push((from, to));
                }
            } else {
                copy_one(&from, &to, &child, preserve, copied)?;
            }
        }
        made.push((d, meta));
    }
    for (dir, meta) in made.iter().rev() {
        fs::set_permissions(dir, meta.permissions())?;
        if preserve {
            keep_attrs(dir, meta)?;
        }
    }
    Ok(())
}

fn copy_one(
    from: &Path,
    to: &Path,
    meta: &Metadata,
    preserve: bool,
    copied: &mut Copied,
) -> io::Result<()> {
    let kind = meta.file_type();
    if kind.is_symlink() {
        let target = fs::read_link(from)?;
        if let Err(e) = std::os::unix::fs::symlink(&target, to) {
            if e.kind() != io::ErrorKind::AlreadyExists {
                return Err(e);
            }
            fs::remove_file(to)?;
            std::os::unix::fs::symlink(&target, to)?;
        }
    } else if kind.is_file() {
        copied.bytes += fs::copy(from, to)?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a file, directory or link", from.display()),
        ));
    }
    if preserve {
        keep_attrs(to, meta)?;
    }
    copied.count += 1;
    Ok(())
}

/// Gives `path` the owner, mode and times in `meta`. A link keeps its own
/// and its target is left alone.
fn keep_attrs(path: &Path, meta: &Metadata) -> io::Result<()> {
    let c = c_path(path)?;
    if unsafe { libc::lchown(c.as_ptr(), meta.uid(), meta.gid()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // A chown clears the setuid and setgid bits, so the mode goes after.
    if !meta.file_type().is_symlink() {
        fs::set_permissions(path, meta.permissions())?;
    }
    let times = [
        libc::timespec {
            tv_sec: meta.atime() as _,
            tv_nsec: meta.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: meta.mtime() as _,
            tv_nsec: meta.mtime_nsec() as _,
        },
    ];
    let set = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if set != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Calls `f` on `root`, then with `recursive` on everything below it,
/// telling it which paths are links found on the way, which it must not
/// follow. Counts the paths `f` changed.
fn walk(
    root: &Path,
    recursive: bool,
    mut f: impl FnMut(&Path, bool) -> io::Result<bool>,
) -> io::Result<u64> {
    let mut count = u64::from(f(root, false)?);
    if !recursive || !fs::metadata(root)?.is_dir() {
        return Ok(count);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            count += u64::from(f(&path, kind.is_symlink())?);
            if kind.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(count)
}

pub fn chmod(req: &ChmodRequest, writer: &mut impl Write) {
    let mode = fs::Permissions::from_mode(req.mode & 0o7777);
    let result = walk(Path::new(&req.path), req.recursive, |path, link| {
        if link {
            return Ok(false);
        }
        fs::set_permissions(path, mode.clone())?;
        Ok(true)
    });
    match result {
        Ok(count) => send_ok(
            writer,
            FsOkResponse {
                ok: true,
                count: Some(count),
                ..Default::default()
            },
        ),
        Err(e) => send_error(writer, format!("chmod {}", req.path), &e),
    }
}

pub fn chown(req: &ChownRequest, writer: &mut impl Write) {
    match chown_paths(req) {
        Ok((count, uid, gid)) => send_ok(
            writer,
            FsOkResponse {
                ok: true,
                count: Some(count),
                uid,
                gid,
                ..Default::default()
            },
        ),
        Err(e) => send_error(writer, format!("chown {}", req.path), &e),
    }
}

fn chown_paths(req: &ChownRequest) -> io::Result<(u64, Option<u32>, Option<u32>)> {
    let uid = req
        .uid
        .as_ref()
        .map(|owner| resolve(owner, "/etc/passwd", "user"))
        .transpose()?;
    let gid = req
        .gid
        .as_ref()
        .map(|owner| resolve(owner, "/etc/group", "group"))
        .transpose()?;
    if uid.is_none() && gid.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no user or group given",
        ));
    }
    let count = walk(Path::new(&req.path), req.recursive, |path, link| {
        let c = c_path(path)?;
        let (uid, gid) = (uid.unwrap_or(u32::MAX), gid.unwrap_or(u32::MAX));
        let done = if link {
            unsafe { libc::lchown(c.as_ptr(), uid, gid) }
        } else {
            unsafe { libc::chown(c.as_ptr(), uid, gid) }
        };
        if done != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(true)
    })?;
    Ok((count, uid, gid))
}

fn resolve(owner: &Owner, db: &str, what: &str) -> io::Result<u32> {
    let name = match owner {
        Owner::Id(id) => return Ok(*id),
        Owner::Name(name) => name,
    };
    if let Ok(id) = name.parse() {
        return Ok(id);
    }
    let entries = fs::read_to_string(db).unwrap_or_default();
    lookup(&entries, name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("no {what} {name}")))
}

/// The id of `name` in a passwd or group file.
fn lookup(entries: &str, name: &str) -> Option<u32> {
    entries.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next() != Some(name) {
            return None;
        }
        fields.nth(1)?.parse().ok()
    })
}

/// Whether the host has closed the connection. The vsock device reports a
/// host close as the end of what the guest reads, so that counts too.
pub fn hung_up(stream: &std::net::TcpStream) -> bool {
    let mut fds = [libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLRDHUP,
        revents: 0,
    }];
    unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) };
    fds[0].revents & (libc::POLLHUP | libc::POLLRDHUP | libc::POLLERR) != 0
}

/// Tells the host that events were lost and its view is out of date.
fn send_dropped(writer: &mut impl Write, path: &str) {
    let msg = format!(
        "watch {path}: events were dropped, list again to catch up (os error {})",
        libc::ENOBUFS
    );
    let _ = frame::write_frame(writer, frame::ERROR, msg.as_bytes());
}

fn notify_io(e: notify::Error) -> io::Error {
    match e.kind {
        notify::ErrorKind::Io(e) => e,
        notify::ErrorKind::PathNotFound => io::Error::from_raw_os_error(libc::ENOENT),
        notify::ErrorKind::MaxFilesWatch => io::Error::from_raw_os_error(libc::ENOSPC),
        other => io::Error::other(format!("{other:?}")),
    }
}

/// The detailed watch of WatchRequest::detail.
pub fn watch(req: &WatchRequest, mut writer: std::net::TcpStream) {
    use notify::{RecursiveMode, Watcher};

    let what = format!("watch {}", req.path);
    let path = Path::new(&req.path);
    if let Err(e) = fs::metadata(path) {
        return send_error(&mut writer, what, &e);
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(WATCH_BACKLOG);
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let lost = dropped.clone();
    let mut watcher = match notify::recommended_watcher(move |res| {
        if let Err(std::sync::mpsc::TrySendError::Full(_)) = tx.try_send(res) {
            lost.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }) {
        Ok(watcher) => watcher,
        Err(e) => return send_error(&mut writer, what, &notify_io(e)),
    };
    let mode = if req.recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    if let Err(e) = watcher.watch(path, mode) {
        return send_error(&mut writer, what, &notify_io(e));
    }
    let ready = FsOkResponse {
        ok: true,
        ..Default::default()
    };
    if frame::send_json(&mut writer, frame::FS_OK_RESP, &ready).is_err() {
        return;
    }
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(Ok(event)) if event.need_rescan() => send_dropped(&mut writer, &req.path),
            Ok(Ok(event)) => {
                for detail in details(&event) {
                    if frame::send_json(&mut writer, frame::WATCH_DETAIL, &detail).is_err() {
                        return;
                    }
                }
            }
            Ok(Err(e)) => send_error(&mut writer, what.clone(), &notify_io(e)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if dropped.swap(false, std::sync::atomic::Ordering::Relaxed) {
            send_dropped(&mut writer, &req.path);
        }
        if hung_up(&writer) {
            return;
        }
    }
}

fn details(event: &notify::Event) -> Vec<WatchDetail> {
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};
    let kind = match event.kind {
        EventKind::Access(_) => return Vec::new(),
        // A rename already arrived as its From and To halves.
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => return Vec::new(),
        EventKind::Create(_) => watch_kind::CREATE,
        EventKind::Remove(_) => watch_kind::DELETE,
        EventKind::Modify(ModifyKind::Name(_)) => watch_kind::RENAME,
        EventKind::Modify(ModifyKind::Metadata(_)) => watch_kind::CHMOD,
        _ => watch_kind::MODIFY,
    };
    let folder = matches!(
        event.kind,
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder)
    );
    event
        .paths
        .iter()
        .filter_map(|path| {
            let meta = fs::symlink_metadata(path).ok();
            Some(WatchDetail {
                kind,
                path: path.to_str()?.to_string(),
                size: meta.as_ref().map(|m| m.len()),
                is_dir: meta.map_or(folder, |m| m.is_dir()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("shuru-files-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }

        fn text(&self, rel: &str) -> String {
            self.path(rel).to_str().unwrap().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn frames(mut bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(frame) = frame::read_frame(&mut bytes).unwrap() {
            out.push(frame);
        }
        out
    }

    fn write_request(req: &WriteFileRequest, data: &[u8]) -> WriteFileResponse {
        let mut input = Vec::new();
        for chunk in data.chunks(4) {
            frame::write_frame(&mut input, frame::WRITE_FILE_DATA, chunk).unwrap();
        }
        let mut output = Vec::new();
        write(req, &mut input.as_slice(), &mut output);
        let frames = frames(&output);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, frame::WRITE_FILE_RESP);
        serde_json::from_slice(&frames[0].1).unwrap()
    }

    #[test]
    fn errors_end_in_an_errno() {
        let e = io::Error::from_raw_os_error(libc::ENOENT);
        assert!(describe(&e).ends_with("(os error 2)"));
        let e = io::Error::new(io::ErrorKind::InvalidInput, "no user bob");
        assert_eq!(describe(&e), "no user bob (os error 22)");
        let e = io::Error::other("odd");
        assert_eq!(describe(&e), "odd (os error 5)");
    }

    #[test]
    fn a_write_sets_the_mode_and_reports_the_file() {
        let s = Scratch::new("write");
        let req = WriteFileRequest {
            path: s.text("a/b/run.sh"),
            len: 10,
            mode: Some(0o750),
            ..Default::default()
        };
        let resp = write_request(&req, b"echo hello");
        assert!(resp.ok, "{:?}", resp.error);
        assert_eq!((resp.size, resp.mode), (Some(10), Some(0o750)));
        let meta = fs::metadata(s.path("a/b/run.sh")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o750);

        // A mode applies to a file already there, too.
        let req = WriteFileRequest {
            mode: Some(0o600),
            len: 2,
            ..req
        };
        let resp = write_request(&req, b"hi");
        assert_eq!((resp.size, resp.mode), (Some(2), Some(0o600)));
    }

    #[test]
    fn a_write_without_mkdir_needs_the_parent() {
        let s = Scratch::new("nomkdir");
        let req = WriteFileRequest {
            path: s.text("missing/file"),
            len: 1,
            mkdir: Some(false),
            ..Default::default()
        };
        let resp = write_request(&req, b"x");
        assert!(!resp.ok);
        assert!(resp.error.unwrap().ends_with("(os error 2)"));
        assert!(!s.path("missing").exists());
    }

    #[test]
    fn a_short_write_is_an_error() {
        let s = Scratch::new("short");
        let req = WriteFileRequest {
            path: s.text("f"),
            len: 10,
            ..Default::default()
        };
        let resp = write_request(&req, b"abc");
        assert!(!resp.ok);
        assert_eq!(
            resp.error.as_deref(),
            Some("expected WRITE_FILE_DATA frame")
        );
    }

    /// The names a listing sends, sorted, and the frames it took.
    fn listing(req: &ReadDirRequest, frame_bytes: usize) -> (Vec<String>, usize) {
        let mut output = Vec::new();
        list(req, &mut output, frame_bytes).unwrap();
        let frames = frames(&output);
        let mut names = Vec::new();
        for (i, (kind, payload)) in frames.iter().enumerate() {
            assert_eq!(*kind, frame::READ_DIR_RESP);
            let resp: ReadDirResponse = serde_json::from_slice(payload).unwrap();
            assert_eq!(resp.more, i + 1 < frames.len());
            names.extend(resp.entries.into_iter().map(|e| e.name));
        }
        names.sort();
        (names, frames.len())
    }

    fn tree(s: &Scratch) {
        fs::create_dir_all(s.path("src/deep")).unwrap();
        fs::write(s.path("src/main.rs"), "fn main() {}").unwrap();
        fs::write(s.path("src/deep/lib.rs"), "").unwrap();
        fs::write(s.path("README.md"), "hi").unwrap();
        std::os::unix::fs::symlink(s.path("src"), s.path("link")).unwrap();
    }

    #[test]
    fn listings_flat_recursive_and_by_pattern() {
        let s = Scratch::new("list");
        tree(&s);
        let req = ReadDirRequest {
            path: s.text(""),
            ..Default::default()
        };
        assert_eq!(listing(&req, LISTING_BYTES).0, ["README.md", "link", "src"]);

        let req = ReadDirRequest {
            recursive: true,
            ..req
        };
        assert_eq!(
            listing(&req, LISTING_BYTES).0,
            [
                "README.md",
                "link",
                "src",
                "src/deep",
                "src/deep/lib.rs",
                "src/main.rs"
            ]
        );

        let req = ReadDirRequest {
            pattern: Some("*.rs".into()),
            ..req
        };
        assert_eq!(
            listing(&req, LISTING_BYTES).0,
            ["src/deep/lib.rs", "src/main.rs"]
        );
    }

    #[test]
    fn entries_carry_type_mode_and_time() {
        let s = Scratch::new("entries");
        tree(&s);
        let mut output = Vec::new();
        let req = ReadDirRequest {
            path: s.text(""),
            ..Default::default()
        };
        list(&req, &mut output, LISTING_BYTES).unwrap();
        let mut resp: ReadDirResponse = serde_json::from_slice(&frames(&output)[0].1).unwrap();
        resp.entries.sort_by(|a, b| a.name.cmp(&b.name));
        let types: Vec<_> = resp.entries.iter().map(|e| e.entry_type.as_str()).collect();
        assert_eq!(types, ["file", "symlink", "dir"]);
        assert_eq!(resp.entries[2].mode.unwrap() & libc::S_IFMT, libc::S_IFDIR);
        assert!(resp.entries[0].mtime.unwrap() > 0);
    }

    #[test]
    fn a_stream_splits_a_long_listing() {
        let s = Scratch::new("stream");
        for i in 0..50 {
            fs::write(s.path(&format!("file-{i:02}")), "").unwrap();
        }
        let req = ReadDirRequest {
            path: s.text(""),
            stream: true,
            ..Default::default()
        };
        let (names, frames) = listing(&req, 1024);
        assert_eq!(names.len(), 50);
        assert_eq!(names[0], "file-00");
        assert!(frames > 2, "{frames}");

        let mut output = Vec::new();
        let req = ReadDirRequest {
            stream: false,
            ..req
        };
        let e = list(&req, &mut output, 1024).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::E2BIG));
        assert!(output.is_empty());
    }

    #[test]
    fn a_missing_directory_is_enoent() {
        let req = ReadDirRequest {
            path: "/nonexistent/shuru".into(),
            ..Default::default()
        };
        let mut output = Vec::new();
        read_dir(&req, &mut output);
        let frames = frames(&output);
        assert_eq!(frames[0].0, frame::ERROR);
        assert!(String::from_utf8_lossy(&frames[0].1).ends_with("(os error 2)"));
    }

    #[test]
    fn a_tree_copy_counts_and_keeps_links() {
        let s = Scratch::new("copy");
        tree(&s);
        fs::set_permissions(s.path("src/main.rs"), fs::Permissions::from_mode(0o741)).unwrap();
        let copied = copy_paths(&s.path("src"), &s.path("out"), true, false).unwrap();
        // src, src/deep, two files
        assert_eq!(
            copied,
            Copied {
                count: 4,
                bytes: 12
            }
        );
        let meta = fs::metadata(s.path("out/main.rs")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o741);

        let copied = copy_paths(&s.path(""), &s.path("all"), true, false);
        // The destination is inside the source and is skipped.
        assert!(copied.is_ok());
        assert!(fs::symlink_metadata(s.path("all/link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!s.path("all/all").exists());
    }

    #[test]
    fn a_directory_copy_needs_recursive() {
        let s = Scratch::new("copydir");
        tree(&s);
        let e = copy_paths(&s.path("src"), &s.path("out"), false, false).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EISDIR));
        let copied = copy_paths(&s.path("README.md"), &s.path("r2"), false, false).unwrap();
        assert_eq!(copied, Copied { count: 1, bytes: 2 });
    }

    #[test]
    fn preserved_attributes_include_times() {
        let s = Scratch::new("preserve");
        tree(&s);
        let old = libc::timespec {
            tv_sec: 1_000_000_000,
            tv_nsec: 0,
        };
        let c = c_path(&s.path("src/deep")).unwrap();
        assert_eq!(
            unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [old, old].as_ptr(), 0) },
            0
        );
        copy_paths(&s.path("src"), &s.path("out"), true, true).unwrap();
        let meta = fs::metadata(s.path("out/deep")).unwrap();
        assert_eq!(meta.mtime(), 1_000_000_000);
    }

    #[test]
    fn a_recursive_chmod_leaves_links_alone() {
        let s = Scratch::new("chmod");
        tree(&s);
        fs::write(s.path("target"), "").unwrap();
        fs::set_permissions(s.path("target"), fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(s.path("target"), s.path("src/out")).unwrap();
        let req = ChmodRequest {
            path: s.text("src"),
            mode: 0o700,
            recursive: true,
        };
        let mut output = Vec::new();
        chmod(&req, &mut output);
        let resp: FsOkResponse = serde_json::from_slice(&frames(&output)[0].1).unwrap();
        // src, src/deep, src/main.rs, src/deep/lib.rs
        assert_eq!(resp.count, Some(4));
        let meta = fs::metadata(s.path("target")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o644);
        let meta = fs::metadata(s.path("src/deep/lib.rs")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o700);
    }

    #[test]
    fn a_recursive_chown_by_name_and_id() {
        let s = Scratch::new("chown");
        tree(&s);
        let req = ChownRequest {
            path: s.text("src"),
            uid: Some(Owner::Name("1234".into())),
            gid: Some(Owner::Id(4321)),
            recursive: true,
        };
        let mut output = Vec::new();
        chown(&req, &mut output);
        let resp: FsOkResponse = serde_json::from_slice(&frames(&output)[0].1).unwrap();
        assert_eq!(
            (resp.count, resp.uid, resp.gid),
            (Some(4), Some(1234), Some(4321))
        );
        let meta = fs::metadata(s.path("src/deep/lib.rs")).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (1234, 4321));

        let req = ChownRequest {
            path: s.text("README.md"),
            uid: Some(Owner::Name("nobody-at-all".into())),
            ..Default::default()
        };
        let mut output = Vec::new();
        chown(&req, &mut output);
        let frames = frames(&output);
        assert_eq!(frames[0].0, frame::ERROR);
        assert!(String::from_utf8_lossy(&frames[0].1).ends_with("(os error 22)"));
    }

    #[test]
    fn owners_by_name() {
        let passwd =
            "root:x:0:0:root:/root:/bin/bash\nsandbox:x:1000:1000::/home/sandbox:/bin/sh\n";
        assert_eq!(lookup(passwd, "root"), Some(0));
        assert_eq!(lookup(passwd, "sandbox"), Some(1000));
        assert_eq!(lookup(passwd, "sand"), None);
        let group = "root:x:0:\nstaff:x:50:alice,bob\n";
        assert_eq!(lookup(group, "staff"), Some(50));
    }

    #[test]
    fn watch_details() {
        use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};
        let s = Scratch::new("details");
        fs::write(s.path("f"), "abc").unwrap();
        let event = notify::Event::new(EventKind::Create(CreateKind::File)).add_path(s.path("f"));
        assert_eq!(
            details(&event),
            [WatchDetail {
                kind: watch_kind::CREATE,
                path: s.text("f"),
                size: Some(3),
                is_dir: false
            }]
        );
        let event =
            notify::Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(s.path("gone"));
        assert_eq!(
            details(&event),
            [WatchDetail {
                kind: watch_kind::DELETE,
                path: s.text("gone"),
                size: None,
                is_dir: true
            }]
        );
        let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(s.path("a"))
            .add_path(s.path("b"));
        assert!(details(&event).is_empty());
    }
}
