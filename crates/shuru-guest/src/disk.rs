use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use shuru_proto::{SealRequest, SealResponse, TrimResponse};

const FIFREEZE: libc::c_ulong = 0xc0045877;
const FITHAW: libc::c_ulong = 0xc0045878;
const FITRIM: libc::c_ulong = 0xc0185879;
const EXT4_IOC_RESIZE_FS: libc::c_ulong = 0x40086610;
const BLKGETSIZE64: libc::c_ulong = 0x80081272;

static BOOT_ERRORS: AtomicU32 = AtomicU32::new(0);
static BUSY: AtomicBool = AtomicBool::new(false);
static LEASE: Mutex<Option<Lease>> = Mutex::new(None);

struct Lease {
    id: u64,
    deadline: Instant,
    root: File,
    expired: bool,
}

pub fn busy() -> bool {
    BUSY.load(Ordering::Acquire)
}

fn check(ret: libc::c_int) -> io::Result<()> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

fn superblock() -> io::Result<crate::ext4::Superblock> {
    let mut sb = [0; 1024];
    File::open("/dev/vda")?.read_exact_at(&mut sb, 1024)?;
    crate::ext4::parse(&sb)
}

fn writable(root: &File) -> io::Result<()> {
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstatvfs(root.as_raw_fd(), &mut st) })?;
    if st.f_flag & libc::ST_RDONLY != 0 {
        return Err(invalid("root ext4 is read-only"));
    }
    superblock()?;
    let count: u32 = std::fs::read_to_string("/sys/fs/ext4/vda/errors_count")?
        .trim()
        .parse()
        .map_err(|_| invalid("invalid ext4 errors_count"))?;
    if count != BOOT_ERRORS.load(Ordering::Acquire) {
        return Err(invalid("root ext4 recorded an error during this boot"));
    }
    Ok(())
}

#[repr(C)]
struct TrimRange {
    start: u64,
    len: u64,
    minlen: u64,
}

fn trim_root(root: &File) -> io::Result<TrimResponse> {
    let mut range = TrimRange {
        start: 0,
        len: u64::MAX,
        minlen: 1024 * 1024,
    };
    if unsafe { libc::ioctl(root.as_raw_fd(), FITRIM as _, &mut range) } < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EOPNOTSUPP) {
            return Ok(TrimResponse {
                supported: false,
                bytes: 0,
            });
        }
        return Err(e);
    }
    Ok(TrimResponse {
        supported: true,
        bytes: range.len,
    })
}

pub fn check_root() -> io::Result<()> {
    writable(&File::open("/")?)
}

pub fn sync() -> io::Result<()> {
    let root = File::open("/")?;
    check(unsafe { libc::syncfs(root.as_raw_fd()) })
}

pub fn trim() -> io::Result<TrimResponse> {
    let _guard = LEASE.lock().unwrap();
    if busy() {
        return Err(invalid("root seal is active"));
    }
    let root = File::open("/")?;
    writable(&root)?;
    check(unsafe { libc::syncfs(root.as_raw_fd()) })?;
    trim_root(&root)
}

pub fn seal(req: SealRequest) -> io::Result<SealResponse> {
    if !(1000..=300_000).contains(&req.lease_ms) {
        return Err(invalid("seal lease must be 1000 to 300000 ms"));
    }
    let mut state = LEASE.lock().unwrap();
    if busy() {
        return Err(invalid("root seal is active"));
    }
    BUSY.store(true, Ordering::Release);
    let result = (|| {
        let root = File::open("/")?;
        let services = if req.stop_services {
            crate::services::shutdown()
        } else {
            Default::default()
        };
        writable(&root)?;
        check(unsafe { libc::syncfs(root.as_raw_fd()) })?;
        trim_root(&root)?;
        superblock()?;
        check(unsafe { libc::ioctl(root.as_raw_fd(), FIFREEZE as _, 0) })?;
        let verified = writable(&root).and_then(|_| {
            if superblock()?.needs_recovery {
                return Err(invalid("frozen root still needs journal recovery"));
            }
            Ok(())
        });
        if let Err(e) = verified {
            if unsafe { libc::ioctl(root.as_raw_fd(), FITHAW as _, 0) } < 0 {
                unsafe {
                    libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
                }
            }
            return Err(e);
        }
        *state = Some(Lease {
            id: req.id,
            deadline: Instant::now() + Duration::from_millis(req.lease_ms),
            root,
            expired: false,
        });
        Ok(SealResponse::Sealed(services))
    })();
    if result.is_err() {
        BUSY.store(false, Ordering::Release);
    }
    result
}

pub fn thaw(id: u64) -> io::Result<SealResponse> {
    let mut state = LEASE.lock().unwrap();
    let lease = state.as_mut().ok_or_else(|| invalid("no seal to thaw"))?;
    if lease.id != id {
        return Err(invalid("seal id does not match"));
    }
    let expired = lease.expired || Instant::now() >= lease.deadline;
    if !lease.expired {
        check(unsafe { libc::ioctl(lease.root.as_raw_fd(), FITHAW as _, 0) })?;
    }
    *state = None;
    BUSY.store(false, Ordering::Release);
    Ok(if expired {
        SealResponse::Expired
    } else {
        SealResponse::Thawed
    })
}

pub fn start_watchdog() {
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(50));
        // A blocked freeze is not cancellable. The lease starts after it returns.
        if let Ok(mut state) = LEASE.try_lock() {
            if let Some(lease) = state.as_mut() {
                if !lease.expired
                    && Instant::now() >= lease.deadline
                    && unsafe { libc::ioctl(lease.root.as_raw_fd(), FITHAW as _, 0) } == 0
                {
                    lease.expired = true;
                    BUSY.store(false, Ordering::Release);
                }
            }
        }
    });
}

fn mount(
    source: &std::ffi::CStr,
    target: &std::ffi::CStr,
    kind: &std::ffi::CStr,
    flags: libc::c_ulong,
    data: *const libc::c_void,
) -> io::Result<()> {
    check(unsafe { libc::mount(source.as_ptr(), target.as_ptr(), kind.as_ptr(), flags, data) })
}

pub fn boot() -> io::Result<()> {
    if unsafe { libc::getpid() } != 1 {
        return Err(invalid("agent must run as PID 1"));
    }
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    check(unsafe { libc::statfs(c"/".as_ptr(), &mut fs) })?;
    if fs.f_type != 0x858458f6 && fs.f_type != 0x01021994 {
        return Err(invalid("agent must boot from the image initramfs"));
    }
    mount(c"devtmpfs", c"/dev", c"devtmpfs", 0, std::ptr::null())?;
    let initial = superblock()?;
    BOOT_ERRORS.store(initial.errors, Ordering::Release);
    mount(
        c"/dev/vda",
        c"/newroot",
        c"ext4",
        libc::MS_RDONLY,
        c"errors=remount-ro,noinit_itable".as_ptr().cast(),
    )?;
    // A read-only mount can replay the journal and surface a recorded error.
    let sb = superblock()?;
    mount(
        c"/dev/vda",
        c"/newroot",
        c"ext4",
        libc::MS_REMOUNT,
        c"errors=remount-ro,noinit_itable".as_ptr().cast(),
    )?;
    let root = File::open("/newroot")?;
    let device = File::open("/dev/vda")?;
    let mut size: u64 = 0;
    check(unsafe { libc::ioctl(device.as_raw_fd(), BLKGETSIZE64 as _, &mut size) })?;
    check(unsafe { libc::fstatfs(root.as_raw_fd(), &mut fs) })?;
    let count = sb.blocks;
    let blocks = size / fs.f_bsize as u64;
    if blocks < count {
        return Err(invalid("root filesystem exceeds device size"));
    }
    if blocks > count {
        check(unsafe { libc::ioctl(root.as_raw_fd(), EXT4_IOC_RESIZE_FS as _, &blocks) })?;
        check(unsafe { libc::syncfs(root.as_raw_fd()) })?;
        let grown = superblock()?;
        let actual = grown.blocks;
        if actual != blocks {
            return Err(invalid("ext4 could not grow to the requested device size"));
        }
    }
    superblock()?;
    drop(device);
    drop(root);
    check(unsafe { libc::umount(c"/dev".as_ptr()) })?;
    std::fs::remove_file("/init")?;
    std::fs::remove_dir_all("/dev")?;
    check(unsafe { libc::chdir(c"/newroot".as_ptr()) })?;
    mount(c".", c"/", c"", libc::MS_MOVE, std::ptr::null())?;
    check(unsafe { libc::chroot(c".".as_ptr()) })?;
    check(unsafe { libc::chdir(c"/".as_ptr()) })?;
    Ok(())
}
