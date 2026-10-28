use super::*;
use crate::WorkDisk;
use std::path::{Path, PathBuf};

struct Fixture {
    sandbox: Sandbox,
    disk: WorkDisk,
    dir: PathBuf,
}

impl Fixture {
    fn boot(source: Option<&str>, size: u64) -> Self {
        let data = std::env::var("SHURU_FAST_BOOT_TEST_DATA")
            .expect("set SHURU_FAST_BOOT_TEST_DATA to an isolated directory with built OS assets");
        let data = std::fs::canonicalize(data).unwrap();
        assert!(data.starts_with("/tmp") || data.starts_with("/private/tmp"));
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = data.join(format!(
            "test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let base = data.join("rootfs.ext4");
        let disk = WorkDisk::clone_from(
            source.unwrap_or(base.to_str().unwrap()),
            dir.to_str().unwrap(),
            size,
        )
        .unwrap();
        let sandbox = Sandbox::builder()
            .kernel(data.join("Image").to_str().unwrap())
            .initrd(data.join("initramfs.cpio").to_str().unwrap())
            .rootfs(disk.attach_path())
            .console(false)
            .verbose(true)
            .mount(MountConfig {
                host_path: data.to_str().unwrap().into(),
                guest_path: "/evidence".into(),
                read_only: true,
            })
            .build()
            .unwrap();
        sandbox.start().unwrap();
        Self { sandbox, disk, dir }
    }

    fn exec(&self, command: &str) -> String {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = self
            .sandbox
            .exec(&["sh", "-c", command], &mut out, &mut err)
            .unwrap();
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        String::from_utf8(out).unwrap()
    }

    fn dest(&self) -> String {
        self.dir.join("saved.ext4").to_str().unwrap().to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.sandbox.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
#[ignore = "requires built isolated OS assets and a native hypervisor"]
fn boot_grow_seal_and_boot_saved_disk() {
    for size in [4096, 8192, 65536] {
        let first = Fixture::boot(None, size);
        assert!(first
            .exec("readlink /proc/1/exe")
            .contains("/init (deleted)"));
        first.exec("test ! -e /usr/bin/shuru-init; echo marker > /marker; echo stale-agent > /usr/bin/shuru-init");
        assert_eq!(
            first
                .exec("blockdev --getsize64 /dev/vda")
                .trim()
                .parse::<u64>()
                .unwrap(),
            size * 1024 * 1024
        );
        let dest = first.dest();
        first
            .sandbox
            .save_disk(&dest, AfterSave::PowerOff, |p| first.disk.save_to(p))
            .unwrap();
        let second = Fixture::boot(Some(&dest), size);
        assert_eq!(second.exec("cat /marker").trim(), "marker");
        assert!(second
            .exec("readlink /proc/1/exe")
            .contains("/init (deleted)"));
        let relative = Path::new(&dest)
            .strip_prefix(first.dir.parent().unwrap())
            .unwrap();
        second.exec(&format!(
            "set -- $(od -An -tu4 -j1120 -N4 /evidence/{}); test $(( $1 & 4 )) -eq 0",
            relative.display()
        ));
    }
}

#[test]
#[ignore = "requires built isolated OS assets and a native hypervisor"]
fn running_checkpoint_resumes_writers_and_trim_reclaims_space() {
    use std::os::unix::fs::MetadataExt;
    let vm = Fixture::boot(None, 4096);
    vm.exec("dd if=/dev/urandom of=/large bs=1M count=256 status=none; sync");
    let allocated = std::fs::metadata(vm.disk.attach_path()).unwrap().blocks();
    vm.exec("rm /large; sync");
    let report = vm.sandbox.trim().unwrap();
    let trimmed = std::fs::metadata(vm.disk.attach_path()).unwrap().blocks();
    eprintln!(
        "trim supported={} submitted={} allocated_before={} allocated_after={}",
        report.supported,
        report.bytes,
        allocated * 512,
        trimmed * 512
    );
    assert!(report.supported);
    assert!(trimmed < allocated);
    vm.exec("nohup sh -c \"while :; do echo write >> /writer; sleep 0.01; done\" </dev/null >/run/writer.log 2>&1 &");
    vm.sandbox
        .save_disk(&vm.dest(), AfterSave::Thaw, |p| vm.disk.save_to(p))
        .unwrap();
    vm.exec("echo resumed > /resumed; test -s /writer");
    let restored = Fixture::boot(Some(&vm.dest()), 4096);
    restored.exec("test -s /writer; test ! -e /resumed");
}

#[test]
#[ignore = "requires built isolated OS assets and a native hypervisor"]
fn expired_seal_and_wrong_id_cannot_publish() {
    let vm = Fixture::boot(None, 4096);
    vm.exec("true");
    let request = shuru_proto::SealRequest {
        id: 71,
        stop_services: false,
        lease_ms: 1000,
    };
    let response: shuru_proto::SealResponse = vm
        .sandbox
        .disk_request(frame::SEAL_REQ, &serde_json::to_vec(&request).unwrap())
        .unwrap();
    assert_eq!(
        response,
        shuru_proto::SealResponse::Sealed(Default::default())
    );
    assert!(vm
        .sandbox
        .disk_request::<shuru_proto::SealResponse>(
            frame::THAW_REQ,
            &serde_json::to_vec(&shuru_proto::ThawRequest { id: 72 }).unwrap()
        )
        .is_err());
    std::thread::sleep(Duration::from_millis(1200));
    vm.exec("echo watchdog-thawed > /after-expiry");
    let response: shuru_proto::SealResponse = vm
        .sandbox
        .disk_request(
            frame::THAW_REQ,
            &serde_json::to_vec(&shuru_proto::ThawRequest { id: 71 }).unwrap(),
        )
        .unwrap();
    assert_eq!(response, shuru_proto::SealResponse::Expired);
    let dest = vm.dest();
    std::fs::write(&dest, b"previous checkpoint").unwrap();
    assert!(vm
        .sandbox
        .save_disk(&dest, AfterSave::Thaw, |_| bail!("injected clone failure"))
        .is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"previous checkpoint");
    vm.exec("echo thawed-after-failure > /after-failure");
    vm.sandbox.kill().unwrap();
    assert!(vm
        .sandbox
        .save_disk(&dest, AfterSave::PowerOff, |_| panic!(
            "stopped guest must not copy"
        ))
        .is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"previous checkpoint");
}

#[test]
#[ignore = "requires built isolated OS assets and a native hypervisor"]
fn legacy_dirty_disk_replays_and_ext4_errors_refuse_save_and_boot() {
    let old = Fixture::boot(None, 4096);
    old.exec("echo old-checkpoint > /legacy; sync");
    old.sandbox.kill().unwrap();
    // Deliberately manufacture a pre-seal checkpoint for compatibility testing.
    old.disk.save_to(&old.dest()).unwrap();
    let restored = Fixture::boot(Some(&old.dest()), 4096);
    assert_eq!(restored.exec("cat /legacy").trim(), "old-checkpoint");
    // 6.12 shuts the filesystem down on error, reads included, so nothing
    // from the disk can run after the trigger.
    restored.exec("echo injected-test-error > /sys/fs/ext4/vda/trigger_fs_error");
    std::thread::sleep(std::time::Duration::from_secs(1));
    let dest = restored.dest();
    std::fs::write(&dest, b"previous checkpoint").unwrap();
    assert!(restored
        .sandbox
        .save_disk(&dest, AfterSave::Thaw, |p| restored.disk.save_to(p))
        .is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"previous checkpoint");
    restored.sandbox.kill().unwrap();
    let damaged = restored.dir.join("damaged.ext4");
    // This fixture intentionally carries the error flag into a boot attempt.
    restored.disk.save_to(damaged.to_str().unwrap()).unwrap();
    let refused = Fixture::boot(Some(damaged.to_str().unwrap()), 4096);
    assert!(refused.sandbox.wait_ready().is_err());
}
