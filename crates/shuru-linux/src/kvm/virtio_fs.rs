//! Minimal in-process virtio-fs device with a FUSE passthrough backend.
//!
//! Speaks the FUSE wire protocol directly over the virtio request queue and
//! translates each op into libc/std::fs calls against a host directory. No
//! external daemon (virtiofsd) or crate (fuse-backend-rs) required — it's
//! the same hand-rolled style as the other devices in this module.
//!
//! Coverage is the minimum the Linux virtio-fs client actually drives during
//! ordinary file I/O: INIT, LOOKUP, FORGET, GETATTR/SETATTR, OPEN/READ/WRITE,
//! OPENDIR/READDIR(PLUS), CREATE, UNLINK/MKDIR/RMDIR/RENAME, STATFS,
//! xattr ops (ENOSYS). Anything else returns ENOSYS; the kernel falls back
//! gracefully.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

use super::devices::{
    avail_idx, avail_ring_entry, write_used, Interrupt, VirtioBackend, VirtioQueueState,
};

// ===========================================================================
// FUSE wire protocol (subset)
// ===========================================================================

const FUSE_KERNEL_VERSION: u32 = 7;
// Pick a protocol minor old enough to be widely supported but new enough for
// READDIRPLUS / batch forget (>= 7.21). 7.31 matches Linux ~5.1+.
const FUSE_KERNEL_MINOR_VERSION: u32 = 31;
const FUSE_ROOT_ID: u64 = 1;

// opcodes
const FUSE_LOOKUP: u32 = 1;
const FUSE_FORGET: u32 = 2;
const FUSE_GETATTR: u32 = 3;
const FUSE_SETATTR: u32 = 4;
const FUSE_READLINK: u32 = 5;
const FUSE_SYMLINK: u32 = 6;
const FUSE_MKNOD: u32 = 8;
const FUSE_MKDIR: u32 = 9;
const FUSE_UNLINK: u32 = 10;
const FUSE_RMDIR: u32 = 11;
const FUSE_RENAME: u32 = 12;
const FUSE_LINK: u32 = 13;
const FUSE_OPEN: u32 = 14;
const FUSE_READ: u32 = 15;
const FUSE_WRITE: u32 = 16;
const FUSE_STATFS: u32 = 17;
const FUSE_RELEASE: u32 = 18;
const FUSE_FSYNC: u32 = 20;
const FUSE_SETXATTR: u32 = 21;
const FUSE_GETXATTR: u32 = 22;
const FUSE_LISTXATTR: u32 = 23;
const FUSE_REMOVEXATTR: u32 = 24;
const FUSE_FLUSH: u32 = 25;
const FUSE_INIT: u32 = 26;
const FUSE_OPENDIR: u32 = 27;
const FUSE_READDIR: u32 = 28;
const FUSE_RELEASEDIR: u32 = 29;
const FUSE_FSYNCDIR: u32 = 30;
const FUSE_ACCESS: u32 = 34;
const FUSE_CREATE: u32 = 35;
const FUSE_DESTROY: u32 = 38;
const FUSE_BATCH_FORGET: u32 = 42;
const FUSE_FALLOCATE: u32 = 43;
const FUSE_READDIRPLUS: u32 = 44;
const FUSE_RENAME2: u32 = 45;
const FUSE_LSEEK: u32 = 46;

// Init flags
const FUSE_ASYNC_READ: u32 = 1 << 0;
const FUSE_BIG_WRITES: u32 = 1 << 5;
const FUSE_DONT_MASK: u32 = 1 << 6;
const FUSE_DO_READDIRPLUS: u32 = 1 << 13;
const FUSE_READDIRPLUS_AUTO: u32 = 1 << 14;
const FUSE_ASYNC_DIO: u32 = 1 << 15;

const MAX_WRITE: u32 = 128 * 1024;

// ===========================================================================
// Plain-data wire structs (repr(C, packed) so we can reinterpret raw bytes)
// ===========================================================================

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseInHeader {
    len: u32,
    opcode: u32,
    unique: u64,
    nodeid: u64,
    uid: u32,
    gid: u32,
    pid: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseOutHeader {
    len: u32,
    error: i32,
    unique: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseInitIn {
    major: u32,
    minor: u32,
    max_readahead: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseInitOut {
    major: u32,
    minor: u32,
    max_readahead: u32,
    flags: u32,
    max_background: u16,
    congestion_threshold: u16,
    max_write: u32,
    time_gran: u32,
    max_pages: u16,
    padding: u16,
    unused: [u32; 8],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseAttr {
    ino: u64,
    size: u64,
    blocks: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
    atimensec: u32,
    mtimensec: u32,
    ctimensec: u32,
    mode: u32,
    nlink: u32,
    uid: u32,
    gid: u32,
    rdev: u32,
    blksize: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseEntryOut {
    nodeid: u64,
    generation: u64,
    entry_valid: u64,
    attr_valid: u64,
    entry_valid_nsec: u32,
    attr_valid_nsec: u32,
    attr: FuseAttr,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseAttrOut {
    attr_valid: u64,
    attr_valid_nsec: u32,
    dummy: u32,
    attr: FuseAttr,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseGetattrIn {
    getattr_flags: u32,
    dummy: u32,
    fh: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseSetattrIn {
    valid: u32,
    padding: u32,
    fh: u64,
    size: u64,
    lock_owner: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
    atimensec: u32,
    mtimensec: u32,
    ctimensec: u32,
    mode: u32,
    unused4: u32,
    uid: u32,
    gid: u32,
    unused5: u32,
}

const FATTR_MODE: u32 = 1 << 0;
const FATTR_UID: u32 = 1 << 1;
const FATTR_GID: u32 = 1 << 2;
const FATTR_SIZE: u32 = 1 << 3;
const FATTR_ATIME: u32 = 1 << 4;
const FATTR_MTIME: u32 = 1 << 5;
const FATTR_ATIME_NOW: u32 = 1 << 7;
const FATTR_MTIME_NOW: u32 = 1 << 8;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseOpenIn {
    flags: u32,
    unused: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseCreateIn {
    flags: u32,
    mode: u32,
    umask: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseOpenOut {
    fh: u64,
    open_flags: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseReadIn {
    fh: u64,
    offset: u64,
    size: u32,
    read_flags: u32,
    lock_owner: u64,
    flags: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseWriteIn {
    fh: u64,
    offset: u64,
    size: u32,
    write_flags: u32,
    lock_owner: u64,
    flags: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseWriteOut {
    size: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseReleaseIn {
    fh: u64,
    flags: u32,
    release_flags: u32,
    lock_owner: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseMkdirIn {
    mode: u32,
    umask: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseForgetIn {
    nlookup: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseBatchForgetIn {
    count: u32,
    dummy: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseForgetOne {
    nodeid: u64,
    nlookup: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseStatfsOut {
    blocks: u64,
    bfree: u64,
    bavail: u64,
    files: u64,
    ffree: u64,
    bsize: u32,
    namelen: u32,
    frsize: u32,
    padding: u32,
    spare: [u32; 6],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseDirent {
    ino: u64,
    off: u64,
    namelen: u32,
    kind: u32,
    // name follows, padded to 8 bytes
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseGetxattrIn {
    size: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseAccessIn {
    mask: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseFsyncIn {
    fh: u64,
    fsync_flags: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FuseFlushIn {
    fh: u64,
    unused: u32,
    padding: u32,
    lock_owner: u64,
}

// ===========================================================================
// Virtio descriptor chain helper (read chain addresses + lengths)
// ===========================================================================

const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

#[derive(Clone, Copy)]
struct DescSpan {
    addr: u64,
    len: u32,
    writable: bool,
}

fn collect_desc_chain(
    mem: &GuestMemoryMmap,
    desc_addr: u64,
    head: u16,
    max_descs: u16,
) -> Vec<DescSpan> {
    let mut out = Vec::new();
    let mut idx = head;
    for _ in 0..max_descs {
        let off = idx as u64 * 16;
        let addr: u64 = mem.read_obj(GuestAddress(desc_addr + off)).unwrap_or(0);
        let len: u32 = mem.read_obj(GuestAddress(desc_addr + off + 8)).unwrap_or(0);
        let flags: u16 = mem
            .read_obj(GuestAddress(desc_addr + off + 12))
            .unwrap_or(0);
        let next: u16 = mem
            .read_obj(GuestAddress(desc_addr + off + 14))
            .unwrap_or(0);
        out.push(DescSpan {
            addr,
            len,
            writable: flags & VRING_DESC_F_WRITE != 0,
        });
        if flags & VRING_DESC_F_NEXT == 0 {
            break;
        }
        idx = next;
    }
    out
}

// ===========================================================================
// Request / response buffers that read/write through descriptor spans
// ===========================================================================

struct RequestReader<'a> {
    mem: &'a GuestMemoryMmap,
    spans: &'a [DescSpan],
    span_idx: usize,
    span_off: u32,
}

impl<'a> RequestReader<'a> {
    fn new(mem: &'a GuestMemoryMmap, spans: &'a [DescSpan]) -> Self {
        RequestReader {
            mem,
            spans,
            span_idx: 0,
            span_off: 0,
        }
    }

    fn read_bytes(&mut self, out: &mut [u8]) -> bool {
        let mut filled = 0;
        while filled < out.len() {
            let span = match self.spans.get(self.span_idx) {
                Some(s) if !s.writable => s,
                _ => return false,
            };
            let remaining = span.len.saturating_sub(self.span_off) as usize;
            if remaining == 0 {
                self.span_idx += 1;
                self.span_off = 0;
                continue;
            }
            let chunk = (out.len() - filled).min(remaining);
            let addr = GuestAddress(span.addr + self.span_off as u64);
            if self
                .mem
                .read_slice(&mut out[filled..filled + chunk], addr)
                .is_err()
            {
                return false;
            }
            filled += chunk;
            self.span_off += chunk as u32;
        }
        true
    }

    fn read_struct<T: Copy + Default>(&mut self) -> Option<T> {
        let size = std::mem::size_of::<T>();
        let mut buf = vec![0u8; size];
        if !self.read_bytes(&mut buf) {
            return None;
        }
        let mut val = T::default();
        unsafe {
            std::ptr::copy_nonoverlapping(buf.as_ptr(), &mut val as *mut T as *mut u8, size);
        }
        Some(val)
    }

    /// Read a null-terminated cstring (consumes the trailing NUL too).
    fn read_cstr(&mut self) -> Option<CString> {
        let mut bytes = Vec::new();
        let mut ch = [0u8; 1];
        loop {
            if !self.read_bytes(&mut ch) {
                return None;
            }
            if ch[0] == 0 {
                return CString::new(bytes).ok();
            }
            bytes.push(ch[0]);
        }
    }

    fn read_vec(&mut self, n: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; n];
        if !self.read_bytes(&mut buf) {
            return None;
        }
        Some(buf)
    }
}

struct ResponseWriter<'a> {
    mem: &'a GuestMemoryMmap,
    spans: &'a [DescSpan],
    span_idx: usize,
    span_off: u32,
    total_written: u32,
}

impl<'a> ResponseWriter<'a> {
    fn new(mem: &'a GuestMemoryMmap, spans: &'a [DescSpan]) -> Self {
        // skip over any read-only spans at the start (request buffer)
        let mut span_idx = 0;
        while let Some(s) = spans.get(span_idx) {
            if s.writable {
                break;
            }
            span_idx += 1;
        }
        ResponseWriter {
            mem,
            spans,
            span_idx,
            span_off: 0,
            total_written: 0,
        }
    }

    fn capacity(&self) -> u32 {
        self.spans
            .iter()
            .filter(|s| s.writable)
            .map(|s| s.len)
            .sum()
    }

    fn write_bytes(&mut self, input: &[u8]) -> bool {
        let mut written = 0;
        while written < input.len() {
            let span = match self.spans.get(self.span_idx) {
                Some(s) if s.writable => s,
                _ => return false,
            };
            let remaining = span.len.saturating_sub(self.span_off) as usize;
            if remaining == 0 {
                self.span_idx += 1;
                self.span_off = 0;
                continue;
            }
            let chunk = (input.len() - written).min(remaining);
            let addr = GuestAddress(span.addr + self.span_off as u64);
            if self
                .mem
                .write_slice(&input[written..written + chunk], addr)
                .is_err()
            {
                return false;
            }
            written += chunk;
            self.span_off += chunk as u32;
            self.total_written += chunk as u32;
        }
        true
    }

    fn write_struct<T: Copy>(&mut self, val: &T) -> bool {
        let size = std::mem::size_of::<T>();
        let bytes = unsafe { std::slice::from_raw_parts(val as *const T as *const u8, size) };
        self.write_bytes(bytes)
    }

    /// Overwrite the header at offset 0 of the first writable span.
    fn rewrite_header(&mut self, hdr: &FuseOutHeader) {
        if let Some(span) = self.spans.iter().find(|s| s.writable) {
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    hdr as *const FuseOutHeader as *const u8,
                    std::mem::size_of::<FuseOutHeader>(),
                )
            };
            let _ = self.mem.write_slice(bytes, GuestAddress(span.addr));
        }
    }
}

// ===========================================================================
// Inode table (maps FUSE nodeid <-> host path)
// ===========================================================================

struct InodeEntry {
    path: PathBuf,
    refcount: u64,
    host_ino: u64,
    host_dev: u64,
}

struct InodeTable {
    root: PathBuf,
    read_only: bool,
    by_id: HashMap<u64, InodeEntry>,
    by_hostid: HashMap<(u64, u64), u64>,
    next_id: AtomicU64,
}

impl InodeTable {
    fn new(root: PathBuf, read_only: bool) -> std::io::Result<Self> {
        let meta = std::fs::metadata(&root)?;
        let mut table = InodeTable {
            root: root.clone(),
            read_only,
            by_id: HashMap::new(),
            by_hostid: HashMap::new(),
            next_id: AtomicU64::new(2),
        };
        table.by_id.insert(
            FUSE_ROOT_ID,
            InodeEntry {
                path: root,
                refcount: 1,
                host_ino: meta.ino(),
                host_dev: meta.dev(),
            },
        );
        table
            .by_hostid
            .insert((meta.dev(), meta.ino()), FUSE_ROOT_ID);
        Ok(table)
    }

    fn path_of(&self, nodeid: u64) -> Option<PathBuf> {
        self.by_id.get(&nodeid).map(|e| e.path.clone())
    }

    /// Ensure (dev, ino) is mapped; bump its refcount and return the nodeid.
    fn intern(&mut self, path: PathBuf, meta: &std::fs::Metadata) -> u64 {
        let key = (meta.dev(), meta.ino());
        if let Some(&id) = self.by_hostid.get(&key) {
            if let Some(entry) = self.by_id.get_mut(&id) {
                entry.refcount = entry.refcount.saturating_add(1);
            }
            return id;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.by_id.insert(
            id,
            InodeEntry {
                path,
                refcount: 1,
                host_ino: meta.ino(),
                host_dev: meta.dev(),
            },
        );
        self.by_hostid.insert(key, id);
        id
    }

    fn forget(&mut self, nodeid: u64, n: u64) {
        if nodeid == FUSE_ROOT_ID {
            return;
        }
        let drop_it = if let Some(entry) = self.by_id.get_mut(&nodeid) {
            entry.refcount = entry.refcount.saturating_sub(n);
            entry.refcount == 0
        } else {
            false
        };
        if drop_it {
            if let Some(entry) = self.by_id.remove(&nodeid) {
                self.by_hostid.remove(&(entry.host_dev, entry.host_ino));
            }
        }
    }

    /// Ensure the resolved path stays within the shared root directory.
    fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }
}

// ===========================================================================
// File handle table (fh -> File)
// ===========================================================================

struct FhTable {
    files: HashMap<u64, std::fs::File>,
    next: AtomicU64,
}

impl FhTable {
    fn new() -> Self {
        FhTable {
            files: HashMap::new(),
            next: AtomicU64::new(1),
        }
    }

    fn insert(&mut self, file: std::fs::File) -> u64 {
        let fh = self.next.fetch_add(1, Ordering::Relaxed);
        self.files.insert(fh, file);
        fh
    }

    fn get(&self, fh: u64) -> Option<&std::fs::File> {
        self.files.get(&fh)
    }

    fn remove(&mut self, fh: u64) {
        self.files.remove(&fh);
    }
}

// ===========================================================================
// VirtioFsBackend — implements VirtioBackend
// ===========================================================================

const VIRTIO_ID_FS: u32 = 26;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

pub struct VirtioFsBackend {
    tag: String,
    inodes: Arc<Mutex<InodeTable>>,
    fhs: Arc<Mutex<FhTable>>,
}

impl VirtioFsBackend {
    pub fn new(tag: &str, host_path: &str, read_only: bool) -> std::io::Result<Self> {
        let canon = std::fs::canonicalize(host_path)?;
        let inodes = InodeTable::new(canon, read_only)?;
        Ok(VirtioFsBackend {
            tag: tag.to_string(),
            inodes: Arc::new(Mutex::new(inodes)),
            fhs: Arc::new(Mutex::new(FhTable::new())),
        })
    }
}

impl VirtioBackend for VirtioFsBackend {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_FS
    }

    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1
    }

    fn queue_count(&self) -> usize {
        // hiprio + 1 request queue
        2
    }

    fn queue_max_size(&self) -> u16 {
        128
    }

    fn config_read(&self, offset: u64) -> u32 {
        // virtio_fs_config: tag[36] + num_request_queues (u32) = 40 bytes
        if offset < 36 {
            let mut tag_bytes = [0u8; 36];
            let src = self.tag.as_bytes();
            let n = src.len().min(36);
            tag_bytes[..n].copy_from_slice(&src[..n]);
            let start = offset as usize;
            let mut out = [0u8; 4];
            let len = 4.min(36 - start);
            out[..len].copy_from_slice(&tag_bytes[start..start + len]);
            u32::from_le_bytes(out)
        } else if offset == 36 {
            1 // num_request_queues
        } else {
            0
        }
    }

    fn config_write(&mut self, _: u64, _: u32) {}

    fn activate(&mut self, _: &[VirtioQueueState], _: &GuestMemoryMmap, _: Arc<Interrupt>) {}

    fn process_queue(
        &mut self,
        queue_idx: u16,
        queues: &mut [VirtioQueueState],
        mem: &GuestMemoryMmap,
    ) -> bool {
        // Both queues (hiprio = 0, request = 1) are handled the same way.
        let q = match queues.get_mut(usize::from(queue_idx)) {
            Some(q) if q.ready && q.size > 0 => q,
            _ => return false,
        };

        let current_avail = avail_idx(mem, q.avail_addr);
        let used = q.last_avail_idx != current_avail;
        while q.last_avail_idx != current_avail {
            let head = avail_ring_entry(mem, q.avail_addr, q.size, q.last_avail_idx);
            let spans = collect_desc_chain(mem, q.desc_addr, head, q.size);

            let written = self.handle_request(mem, &spans);

            write_used(
                mem,
                q.used_addr,
                q.size,
                q.last_avail_idx,
                head as u32,
                written,
            );
            q.last_avail_idx = q.last_avail_idx.wrapping_add(1);
        }
        used
    }

    fn reset(&mut self) {
        self.fhs.lock().unwrap().files.clear();
    }
}

// ===========================================================================
// Request dispatch + per-op handlers
// ===========================================================================

impl VirtioFsBackend {
    fn handle_request(&self, mem: &GuestMemoryMmap, spans: &[DescSpan]) -> u32 {
        let mut reader = RequestReader::new(mem, spans);
        let mut writer = ResponseWriter::new(mem, spans);

        let hdr: FuseInHeader = match reader.read_struct() {
            Some(h) => h,
            None => return 0,
        };

        // Reserve space for the response header; it's rewritten at the end.
        let placeholder = FuseOutHeader::default();
        if !writer.write_struct(&placeholder) {
            return 0;
        }

        let (error, body_len) = match hdr.opcode {
            FUSE_INIT => self.op_init(&mut reader, &mut writer),
            FUSE_DESTROY => (0, 0),
            FUSE_FORGET => {
                self.op_forget(hdr.nodeid, &mut reader);
                return 0; // no reply
            }
            FUSE_BATCH_FORGET => {
                self.op_batch_forget(&mut reader);
                return 0;
            }
            FUSE_LOOKUP => self.op_lookup(hdr.nodeid, &mut reader, &mut writer),
            FUSE_GETATTR => self.op_getattr(hdr.nodeid, &mut reader, &mut writer),
            FUSE_SETATTR => self.op_setattr(hdr.nodeid, &mut reader, &mut writer),
            FUSE_OPENDIR => self.op_opendir(hdr.nodeid, &mut reader, &mut writer),
            FUSE_READDIR => self.op_readdir(hdr.nodeid, &mut reader, &mut writer, false),
            FUSE_READDIRPLUS => self.op_readdir(hdr.nodeid, &mut reader, &mut writer, true),
            FUSE_RELEASEDIR => self.op_release(&mut reader),
            FUSE_OPEN => self.op_open(hdr.nodeid, &mut reader, &mut writer, false),
            FUSE_CREATE => self.op_create(hdr.nodeid, &mut reader, &mut writer),
            FUSE_READ => self.op_read(&mut reader, &mut writer),
            FUSE_WRITE => self.op_write(&mut reader, &mut writer),
            FUSE_RELEASE => self.op_release(&mut reader),
            FUSE_FLUSH => self.op_flush(&mut reader),
            FUSE_FSYNC | FUSE_FSYNCDIR => self.op_fsync(&mut reader),
            FUSE_STATFS => self.op_statfs(hdr.nodeid, &mut writer),
            FUSE_ACCESS => (0, 0),
            FUSE_UNLINK => self.op_unlink(hdr.nodeid, &mut reader, false),
            FUSE_RMDIR => self.op_unlink(hdr.nodeid, &mut reader, true),
            FUSE_MKDIR => self.op_mkdir(hdr.nodeid, &mut reader, &mut writer),
            FUSE_RENAME | FUSE_RENAME2 => {
                self.op_rename(hdr.nodeid, &mut reader, hdr.opcode == FUSE_RENAME2)
            }
            FUSE_GETXATTR | FUSE_LISTXATTR => (-libc::ENOSYS, 0),
            FUSE_SETXATTR | FUSE_REMOVEXATTR => (-libc::ENOSYS, 0),
            FUSE_SYMLINK | FUSE_LINK | FUSE_MKNOD | FUSE_READLINK => (-libc::ENOSYS, 0),
            FUSE_FALLOCATE => (-libc::ENOSYS, 0),
            FUSE_LSEEK => (-libc::ENOSYS, 0),
            _ => (-libc::ENOSYS, 0),
        };

        let total_len = std::mem::size_of::<FuseOutHeader>() as u32 + body_len;
        let final_hdr = FuseOutHeader {
            len: total_len,
            error,
            unique: hdr.unique,
        };
        writer.rewrite_header(&final_hdr);
        total_len
    }

    fn op_init(&self, reader: &mut RequestReader, writer: &mut ResponseWriter) -> (i32, u32) {
        let init: FuseInitIn = match reader.read_struct() {
            Some(v) => v,
            None => return (-libc::EIO, 0),
        };
        let mut flags = init.flags;
        // Only advertise what we support.
        let allowed = FUSE_ASYNC_READ
            | FUSE_BIG_WRITES
            | FUSE_DONT_MASK
            | FUSE_DO_READDIRPLUS
            | FUSE_READDIRPLUS_AUTO
            | FUSE_ASYNC_DIO;
        flags &= allowed;

        let out = FuseInitOut {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION.min(init.minor),
            max_readahead: init.max_readahead,
            flags,
            max_background: 16,
            congestion_threshold: 12,
            max_write: MAX_WRITE,
            time_gran: 1,
            max_pages: (MAX_WRITE / 4096) as u16,
            padding: 0,
            unused: [0; 8],
        };
        if !writer.write_struct(&out) {
            return (-libc::EIO, 0);
        }
        (0, std::mem::size_of::<FuseInitOut>() as u32)
    }

    fn op_forget(&self, nodeid: u64, reader: &mut RequestReader) {
        if let Some(f) = reader.read_struct::<FuseForgetIn>() {
            self.inodes.lock().unwrap().forget(nodeid, f.nlookup);
        }
    }

    fn op_batch_forget(&self, reader: &mut RequestReader) {
        let hdr: FuseBatchForgetIn = match reader.read_struct() {
            Some(v) => v,
            None => return,
        };
        let mut inodes = self.inodes.lock().unwrap();
        for _ in 0..hdr.count {
            if let Some(one) = reader.read_struct::<FuseForgetOne>() {
                inodes.forget(one.nodeid, one.nlookup);
            } else {
                break;
            }
        }
    }

    fn op_lookup(
        &self,
        parent: u64,
        reader: &mut RequestReader,
        writer: &mut ResponseWriter,
    ) -> (i32, u32) {
        let name = match reader.read_cstr() {
            Some(n) => n,
            None => return (-libc::EIO, 0),
        };
        let mut inodes = self.inodes.lock().unwrap();
        let parent_path = match inodes.path_of(parent) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        let name_os = OsStr::from_bytes(name.as_bytes());
        if !is_plain_name(name_os) {
            return (-libc::EINVAL, 0);
        }
        let child_path = parent_path.join(name_os);
        let meta = match std::fs::symlink_metadata(&child_path) {
            Ok(m) => m,
            Err(e) => return (-io_errno(&e), 0),
        };
        if !inodes.contains(&child_path) {
            return (-libc::EACCES, 0);
        }
        let nodeid = inodes.intern(child_path.clone(), &meta);
        let out = FuseEntryOut {
            nodeid,
            generation: 0,
            entry_valid: 1,
            attr_valid: 1,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: attr_from_meta(nodeid, &meta),
        };
        if !writer.write_struct(&out) {
            return (-libc::EIO, 0);
        }
        (0, std::mem::size_of::<FuseEntryOut>() as u32)
    }

    fn op_getattr(
        &self,
        nodeid: u64,
        _reader: &mut RequestReader,
        writer: &mut ResponseWriter,
    ) -> (i32, u32) {
        let inodes = self.inodes.lock().unwrap();
        let path = match inodes.path_of(nodeid) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        drop(inodes);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => return (-io_errno(&e), 0),
        };
        let out = FuseAttrOut {
            attr_valid: 1,
            attr_valid_nsec: 0,
            dummy: 0,
            attr: attr_from_meta(nodeid, &meta),
        };
        if !writer.write_struct(&out) {
            return (-libc::EIO, 0);
        }
        (0, std::mem::size_of::<FuseAttrOut>() as u32)
    }

    fn op_setattr(
        &self,
        nodeid: u64,
        reader: &mut RequestReader,
        writer: &mut ResponseWriter,
    ) -> (i32, u32) {
        let req: FuseSetattrIn = match reader.read_struct() {
            Some(v) => v,
            None => return (-libc::EIO, 0),
        };
        let inodes = self.inodes.lock().unwrap();
        if inodes.read_only {
            return (-libc::EROFS, 0);
        }
        let path = match inodes.path_of(nodeid) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        drop(inodes);

        let cpath = match CString::new(path.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return (-libc::EINVAL, 0),
        };

        if req.valid & FATTR_MODE != 0 {
            if unsafe { libc::chmod(cpath.as_ptr(), req.mode as libc::mode_t) } < 0 {
                return (-errno(), 0);
            }
        }
        if req.valid & (FATTR_UID | FATTR_GID) != 0 {
            let uid = if req.valid & FATTR_UID != 0 {
                req.uid
            } else {
                u32::MAX
            };
            let gid = if req.valid & FATTR_GID != 0 {
                req.gid
            } else {
                u32::MAX
            };
            if unsafe { libc::chown(cpath.as_ptr(), uid, gid) } < 0 {
                return (-errno(), 0);
            }
        }
        if req.valid & FATTR_SIZE != 0 {
            if unsafe { libc::truncate(cpath.as_ptr(), req.size as libc::off_t) } < 0 {
                return (-errno(), 0);
            }
        }
        if req.valid & (FATTR_ATIME | FATTR_MTIME | FATTR_ATIME_NOW | FATTR_MTIME_NOW) != 0 {
            let now = libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW,
            };
            let omit = libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            };
            let atime = if req.valid & FATTR_ATIME_NOW != 0 {
                now
            } else if req.valid & FATTR_ATIME != 0 {
                libc::timespec {
                    tv_sec: req.atime as libc::time_t,
                    tv_nsec: req.atimensec as i64,
                }
            } else {
                omit
            };
            let mtime = if req.valid & FATTR_MTIME_NOW != 0 {
                now
            } else if req.valid & FATTR_MTIME != 0 {
                libc::timespec {
                    tv_sec: req.mtime as libc::time_t,
                    tv_nsec: req.mtimensec as i64,
                }
            } else {
                omit
            };
            let times = [atime, mtime];
            if unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0) } < 0 {
                return (-errno(), 0);
            }
        }

        // Return fresh attrs
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => return (-io_errno(&e), 0),
        };
        let out = FuseAttrOut {
            attr_valid: 1,
            attr_valid_nsec: 0,
            dummy: 0,
            attr: attr_from_meta(nodeid, &meta),
        };
        if !writer.write_struct(&out) {
            return (-libc::EIO, 0);
        }
        (0, std::mem::size_of::<FuseAttrOut>() as u32)
    }

    fn op_opendir(
        &self,
        nodeid: u64,
        _reader: &mut RequestReader,
        writer: &mut ResponseWriter,
    ) -> (i32, u32) {
        let inodes = self.inodes.lock().unwrap();
        let path = match inodes.path_of(nodeid) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        drop(inodes);
        match std::fs::File::open(&path) {
            Ok(f) => {
                let fh = self.fhs.lock().unwrap().insert(f);
                let out = FuseOpenOut {
                    fh,
                    open_flags: 0,
                    padding: 0,
                };
                if !writer.write_struct(&out) {
                    return (-libc::EIO, 0);
                }
                (0, std::mem::size_of::<FuseOpenOut>() as u32)
            }
            Err(e) => (-io_errno(&e), 0),
        }
    }

    fn op_readdir(
        &self,
        nodeid: u64,
        reader: &mut RequestReader,
        writer: &mut ResponseWriter,
        plus: bool,
    ) -> (i32, u32) {
        let r: FuseReadIn = match reader.read_struct() {
            Some(v) => v,
            None => return (-libc::EIO, 0),
        };
        let inodes_guard = self.inodes.lock().unwrap();
        let dir = match inodes_guard.path_of(nodeid) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        drop(inodes_guard);

        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => return (-io_errno(&e), 0),
        };

        let mut written: u32 = 0;
        let max = r.size;
        let mut offset = r.offset;
        let mut cur_offset: u64 = 0;

        for (idx, ent) in entries.enumerate() {
            if cur_offset < offset {
                cur_offset = idx as u64 + 1;
                continue;
            }
            let ent = match ent {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = ent.file_name();
            let meta = match ent.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let child_path = ent.path();

            let this_nodeid = if plus {
                let mut inodes = self.inodes.lock().unwrap();
                if !inodes.contains(&child_path) {
                    continue;
                }
                inodes.intern(child_path.clone(), &meta)
            } else {
                meta.ino()
            };

            let kind = mode_to_dt(meta.mode() as u32);
            let name_bytes = name.as_bytes();
            let namelen = name_bytes.len();
            let entry_size = if plus {
                std::mem::size_of::<FuseEntryOut>()
            } else {
                0
            } + std::mem::size_of::<FuseDirent>()
                + align_up(namelen, 8);

            if written as usize + entry_size > max as usize {
                break;
            }

            if plus {
                let e = FuseEntryOut {
                    nodeid: this_nodeid,
                    generation: 0,
                    entry_valid: 1,
                    attr_valid: 1,
                    entry_valid_nsec: 0,
                    attr_valid_nsec: 0,
                    attr: attr_from_meta(this_nodeid, &meta),
                };
                if !writer.write_struct(&e) {
                    return (-libc::EIO, written);
                }
                written += std::mem::size_of::<FuseEntryOut>() as u32;
            }

            let dirent = FuseDirent {
                ino: this_nodeid,
                off: (idx as u64) + 1,
                namelen: namelen as u32,
                kind,
            };
            if !writer.write_struct(&dirent) {
                return (-libc::EIO, written);
            }
            written += std::mem::size_of::<FuseDirent>() as u32;

            if !writer.write_bytes(name_bytes) {
                return (-libc::EIO, written);
            }
            let pad = align_up(namelen, 8) - namelen;
            if pad > 0 {
                let zeros = [0u8; 8];
                if !writer.write_bytes(&zeros[..pad]) {
                    return (-libc::EIO, written);
                }
            }
            written += (namelen + pad) as u32;
            cur_offset = idx as u64 + 1;
            offset = 0;
        }
        (0, written)
    }

    fn op_open(
        &self,
        nodeid: u64,
        reader: &mut RequestReader,
        writer: &mut ResponseWriter,
        _create: bool,
    ) -> (i32, u32) {
        let req: FuseOpenIn = match reader.read_struct() {
            Some(v) => v,
            None => return (-libc::EIO, 0),
        };
        let inodes = self.inodes.lock().unwrap();
        let path = match inodes.path_of(nodeid) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        let wants_write = req.flags as i32
            & (libc::O_WRONLY | libc::O_RDWR | libc::O_APPEND | libc::O_TRUNC)
            != 0;
        if wants_write && inodes.read_only {
            return (-libc::EROFS, 0);
        }
        drop(inodes);

        let cpath = match CString::new(path.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return (-libc::EINVAL, 0),
        };
        // Strip O_CREAT/O_EXCL — OPEN is only used for existing files.
        let flags = (req.flags as i32) & !(libc::O_CREAT | libc::O_EXCL);
        let fd = unsafe { libc::open(cpath.as_ptr(), flags) };
        if fd < 0 {
            return (-errno(), 0);
        }
        let file = unsafe { std::os::unix::io::FromRawFd::from_raw_fd(fd) };
        let fh = self.fhs.lock().unwrap().insert(file);
        let out = FuseOpenOut {
            fh,
            open_flags: 0,
            padding: 0,
        };
        if !writer.write_struct(&out) {
            return (-libc::EIO, 0);
        }
        (0, std::mem::size_of::<FuseOpenOut>() as u32)
    }

    fn op_create(
        &self,
        parent: u64,
        reader: &mut RequestReader,
        writer: &mut ResponseWriter,
    ) -> (i32, u32) {
        let req: FuseCreateIn = match reader.read_struct() {
            Some(v) => v,
            None => return (-libc::EIO, 0),
        };
        let name = match reader.read_cstr() {
            Some(n) => n,
            None => return (-libc::EIO, 0),
        };
        let mut inodes = self.inodes.lock().unwrap();
        if inodes.read_only {
            return (-libc::EROFS, 0);
        }
        let parent_path = match inodes.path_of(parent) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        let name_os = OsStr::from_bytes(name.as_bytes());
        if !is_plain_name(name_os) {
            return (-libc::EINVAL, 0);
        }
        let child = parent_path.join(name_os);
        let cpath = match CString::new(child.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return (-libc::EINVAL, 0),
        };
        let fd = unsafe {
            libc::open(
                cpath.as_ptr(),
                (req.flags as i32) | libc::O_CREAT,
                req.mode & !req.umask,
            )
        };
        if fd < 0 {
            return (-errno(), 0);
        }
        let meta = match std::fs::metadata(&child) {
            Ok(m) => m,
            Err(e) => {
                unsafe {
                    libc::close(fd);
                }
                return (-io_errno(&e), 0);
            }
        };
        let nodeid = inodes.intern(child.clone(), &meta);
        drop(inodes);

        let file = unsafe { std::os::unix::io::FromRawFd::from_raw_fd(fd) };
        let fh = self.fhs.lock().unwrap().insert(file);

        let entry = FuseEntryOut {
            nodeid,
            generation: 0,
            entry_valid: 1,
            attr_valid: 1,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: attr_from_meta(nodeid, &meta),
        };
        let open_out = FuseOpenOut {
            fh,
            open_flags: 0,
            padding: 0,
        };
        if !writer.write_struct(&entry) {
            return (-libc::EIO, 0);
        }
        if !writer.write_struct(&open_out) {
            return (-libc::EIO, 0);
        }
        (
            0,
            (std::mem::size_of::<FuseEntryOut>() + std::mem::size_of::<FuseOpenOut>()) as u32,
        )
    }

    fn op_read(&self, reader: &mut RequestReader, writer: &mut ResponseWriter) -> (i32, u32) {
        let req: FuseReadIn = match reader.read_struct() {
            Some(v) => v,
            None => return (-libc::EIO, 0),
        };
        let fhs = self.fhs.lock().unwrap();
        let file = match fhs.get(req.fh) {
            Some(f) => f,
            None => return (-libc::EBADF, 0),
        };
        let raw_fd = std::os::unix::io::AsRawFd::as_raw_fd(file);
        drop(fhs);

        let mut buf = vec![0u8; req.size as usize];
        let n = unsafe {
            libc::pread(
                raw_fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                req.offset as libc::off_t,
            )
        };
        if n < 0 {
            return (-errno(), 0);
        }
        if !writer.write_bytes(&buf[..n as usize]) {
            return (-libc::EIO, 0);
        }
        (0, n as u32)
    }

    fn op_write(&self, reader: &mut RequestReader, writer: &mut ResponseWriter) -> (i32, u32) {
        let req: FuseWriteIn = match reader.read_struct() {
            Some(v) => v,
            None => return (-libc::EIO, 0),
        };
        if self.inodes.lock().unwrap().read_only {
            return (-libc::EROFS, 0);
        }
        let data = match reader.read_vec(req.size as usize) {
            Some(v) => v,
            None => return (-libc::EIO, 0),
        };
        let fhs = self.fhs.lock().unwrap();
        let file = match fhs.get(req.fh) {
            Some(f) => f,
            None => return (-libc::EBADF, 0),
        };
        let raw_fd = std::os::unix::io::AsRawFd::as_raw_fd(file);
        drop(fhs);
        let n = unsafe {
            libc::pwrite(
                raw_fd,
                data.as_ptr() as *const libc::c_void,
                data.len(),
                req.offset as libc::off_t,
            )
        };
        if n < 0 {
            return (-errno(), 0);
        }
        let out = FuseWriteOut {
            size: n as u32,
            padding: 0,
        };
        if !writer.write_struct(&out) {
            return (-libc::EIO, 0);
        }
        (0, std::mem::size_of::<FuseWriteOut>() as u32)
    }

    fn op_release(&self, reader: &mut RequestReader) -> (i32, u32) {
        let req: FuseReleaseIn = match reader.read_struct() {
            Some(v) => v,
            None => return (-libc::EIO, 0),
        };
        self.fhs.lock().unwrap().remove(req.fh);
        (0, 0)
    }

    fn op_flush(&self, reader: &mut RequestReader) -> (i32, u32) {
        let _ = reader.read_struct::<FuseFlushIn>();
        (0, 0)
    }

    fn op_fsync(&self, reader: &mut RequestReader) -> (i32, u32) {
        let req: FuseFsyncIn = match reader.read_struct() {
            Some(v) => v,
            None => return (-libc::EIO, 0),
        };
        let fhs = self.fhs.lock().unwrap();
        let file = match fhs.get(req.fh) {
            Some(f) => f,
            None => return (-libc::EBADF, 0),
        };
        let raw_fd = std::os::unix::io::AsRawFd::as_raw_fd(file);
        drop(fhs);
        if unsafe { libc::fsync(raw_fd) } < 0 {
            return (-errno(), 0);
        }
        (0, 0)
    }

    fn op_statfs(&self, nodeid: u64, writer: &mut ResponseWriter) -> (i32, u32) {
        let inodes = self.inodes.lock().unwrap();
        let path = match inodes.path_of(nodeid) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        drop(inodes);
        let cpath = match CString::new(path.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return (-libc::EINVAL, 0),
        };
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } < 0 {
            return (-errno(), 0);
        }
        let out = FuseStatfsOut {
            blocks: stat.f_blocks as u64,
            bfree: stat.f_bfree as u64,
            bavail: stat.f_bavail as u64,
            files: stat.f_files as u64,
            ffree: stat.f_ffree as u64,
            bsize: stat.f_bsize as u32,
            namelen: stat.f_namemax as u32,
            frsize: stat.f_frsize as u32,
            padding: 0,
            spare: [0; 6],
        };
        if !writer.write_struct(&out) {
            return (-libc::EIO, 0);
        }
        (0, std::mem::size_of::<FuseStatfsOut>() as u32)
    }

    fn op_unlink(&self, parent: u64, reader: &mut RequestReader, is_dir: bool) -> (i32, u32) {
        let name = match reader.read_cstr() {
            Some(n) => n,
            None => return (-libc::EIO, 0),
        };
        let inodes = self.inodes.lock().unwrap();
        if inodes.read_only {
            return (-libc::EROFS, 0);
        }
        let parent_path = match inodes.path_of(parent) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        drop(inodes);
        let name_os = OsStr::from_bytes(name.as_bytes());
        if !is_plain_name(name_os) {
            return (-libc::EINVAL, 0);
        }
        let child = parent_path.join(name_os);
        let cpath = match CString::new(child.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return (-libc::EINVAL, 0),
        };
        let rc = unsafe {
            if is_dir {
                libc::rmdir(cpath.as_ptr())
            } else {
                libc::unlink(cpath.as_ptr())
            }
        };
        if rc < 0 {
            return (-errno(), 0);
        }
        (0, 0)
    }

    fn op_mkdir(
        &self,
        parent: u64,
        reader: &mut RequestReader,
        writer: &mut ResponseWriter,
    ) -> (i32, u32) {
        let req: FuseMkdirIn = match reader.read_struct() {
            Some(v) => v,
            None => return (-libc::EIO, 0),
        };
        let name = match reader.read_cstr() {
            Some(n) => n,
            None => return (-libc::EIO, 0),
        };
        let mut inodes = self.inodes.lock().unwrap();
        if inodes.read_only {
            return (-libc::EROFS, 0);
        }
        let parent_path = match inodes.path_of(parent) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        let name_os = OsStr::from_bytes(name.as_bytes());
        if !is_plain_name(name_os) {
            return (-libc::EINVAL, 0);
        }
        let child = parent_path.join(name_os);
        let cpath = match CString::new(child.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return (-libc::EINVAL, 0),
        };
        if unsafe { libc::mkdir(cpath.as_ptr(), req.mode & !req.umask) } < 0 {
            return (-errno(), 0);
        }
        let meta = match std::fs::metadata(&child) {
            Ok(m) => m,
            Err(e) => return (-io_errno(&e), 0),
        };
        let nodeid = inodes.intern(child.clone(), &meta);
        let out = FuseEntryOut {
            nodeid,
            generation: 0,
            entry_valid: 1,
            attr_valid: 1,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: attr_from_meta(nodeid, &meta),
        };
        if !writer.write_struct(&out) {
            return (-libc::EIO, 0);
        }
        (0, std::mem::size_of::<FuseEntryOut>() as u32)
    }

    fn op_rename(&self, parent: u64, reader: &mut RequestReader, v2: bool) -> (i32, u32) {
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct RenameIn {
            newdir: u64,
        }
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct Rename2In {
            newdir: u64,
            flags: u32,
            padding: u32,
        }

        let (newdir, _flags) = if v2 {
            match reader.read_struct::<Rename2In>() {
                Some(v) => (v.newdir, v.flags),
                None => return (-libc::EIO, 0),
            }
        } else {
            match reader.read_struct::<RenameIn>() {
                Some(v) => (v.newdir, 0),
                None => return (-libc::EIO, 0),
            }
        };

        let oldname = match reader.read_cstr() {
            Some(n) => n,
            None => return (-libc::EIO, 0),
        };
        let newname = match reader.read_cstr() {
            Some(n) => n,
            None => return (-libc::EIO, 0),
        };

        let inodes = self.inodes.lock().unwrap();
        if inodes.read_only {
            return (-libc::EROFS, 0);
        }
        let old_parent = match inodes.path_of(parent) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        let new_parent = match inodes.path_of(newdir) {
            Some(p) => p,
            None => return (-libc::ENOENT, 0),
        };
        drop(inodes);

        let old_os = OsStr::from_bytes(oldname.as_bytes());
        let new_os = OsStr::from_bytes(newname.as_bytes());
        if !is_plain_name(old_os) || !is_plain_name(new_os) {
            return (-libc::EINVAL, 0);
        }
        let old_path = old_parent.join(old_os);
        let new_path = new_parent.join(new_os);
        let old_c = match CString::new(old_path.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return (-libc::EINVAL, 0),
        };
        let new_c = match CString::new(new_path.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return (-libc::EINVAL, 0),
        };
        if unsafe { libc::rename(old_c.as_ptr(), new_c.as_ptr()) } < 0 {
            return (-errno(), 0);
        }
        (0, 0)
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

fn attr_from_meta(ino: u64, meta: &std::fs::Metadata) -> FuseAttr {
    FuseAttr {
        ino,
        size: meta.size(),
        blocks: meta.blocks(),
        atime: meta.atime() as u64,
        mtime: meta.mtime() as u64,
        ctime: meta.ctime() as u64,
        atimensec: meta.atime_nsec() as u32,
        mtimensec: meta.mtime_nsec() as u32,
        ctimensec: meta.ctime_nsec() as u32,
        mode: meta.mode() as u32,
        nlink: meta.nlink() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        rdev: meta.rdev() as u32,
        blksize: meta.blksize() as u32,
        padding: 0,
    }
}

fn mode_to_dt(mode: u32) -> u32 {
    match mode & libc::S_IFMT as u32 {
        m if m == libc::S_IFREG as u32 => libc::DT_REG as u32,
        m if m == libc::S_IFDIR as u32 => libc::DT_DIR as u32,
        m if m == libc::S_IFLNK as u32 => libc::DT_LNK as u32,
        m if m == libc::S_IFIFO as u32 => libc::DT_FIFO as u32,
        m if m == libc::S_IFSOCK as u32 => libc::DT_SOCK as u32,
        m if m == libc::S_IFCHR as u32 => libc::DT_CHR as u32,
        m if m == libc::S_IFBLK as u32 => libc::DT_BLK as u32,
        _ => libc::DT_UNKNOWN as u32,
    }
}

fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

fn is_plain_name(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." {
        return false;
    }
    !bytes.contains(&b'/') && !bytes.contains(&0)
}

fn errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn io_errno(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}
