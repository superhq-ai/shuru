//! Seccomp filters. The VMM gets an allowlist, installed on every thread
//! once the VM is built and before the first `KVM_RUN`. The network
//! process runs tokio and two TLS libraries, so it gets a deny list: no
//! exec, no files, no tracing, no namespaces, IP sockets only.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use seccompiler::{
    apply_filter_all_threads, BpfProgram, SeccompAction, SeccompCmpArgLen as Len,
    SeccompCmpOp as Op, SeccompCondition as Cond, SeccompFilter, SeccompRule,
};

const KVMIO: u64 = 0xae;
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

const fn io(nr: u64) -> u64 {
    (KVMIO << 8) | nr
}

const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | (KVMIO << 8) | nr
}

const fn iow(nr: u64, size: usize) -> u64 {
    ioc(IOC_WRITE, nr, size)
}

const fn ior(nr: u64, size: usize) -> u64 {
    ioc(IOC_READ, nr, size)
}

const KVM_RUN: u64 = io(0x80);
const KVM_IRQ_LINE: u64 = iow(0x61, size_of::<kvm_bindings::kvm_irq_level>());
const KVM_IOEVENTFD: u64 = iow(0x79, size_of::<kvm_bindings::kvm_ioeventfd>());
// Reading the vCPUs and the GIC for a snapshot.
const KVM_GET_ONE_REG: u64 = iow(0xab, size_of::<kvm_bindings::kvm_one_reg>());
const KVM_GET_REG_LIST: u64 = ioc(
    IOC_READ | IOC_WRITE,
    0xb0,
    size_of::<kvm_bindings::kvm_reg_list>(),
);
const KVM_GET_MP_STATE: u64 = ior(0x98, size_of::<kvm_bindings::kvm_mp_state>());
const KVM_GET_VCPU_EVENTS: u64 = ior(0x9f, size_of::<kvm_bindings::kvm_vcpu_events>());
const KVM_GET_DEVICE_ATTR: u64 = iow(0xe2, size_of::<kvm_bindings::kvm_device_attr>());
// An unsigned long in glibc, an int in musl.
#[allow(clippy::unnecessary_cast)]
const FIONBIO: u64 = libc::FIONBIO as u64;
#[cfg(not(all(target_env = "musl", target_arch = "aarch64")))]
const SYS_KEXEC_FILE_LOAD: libc::c_long = libc::SYS_kexec_file_load;
// The libc crate leaves it out for musl on aarch64.
#[cfg(all(target_env = "musl", target_arch = "aarch64"))]
const SYS_KEXEC_FILE_LOAD: libc::c_long = 294;

const ALLOWED: &[libc::c_long] = &[
    libc::SYS_accept4,
    libc::SYS_brk,
    libc::SYS_clock_gettime,
    libc::SYS_clock_nanosleep,
    libc::SYS_close,
    libc::SYS_epoll_ctl,
    libc::SYS_epoll_pwait,
    libc::SYS_eventfd2,
    libc::SYS_exit,
    libc::SYS_exit_group,
    libc::SYS_fallocate,
    libc::SYS_fcntl,
    libc::SYS_fdatasync,
    libc::SYS_fsync,
    libc::SYS_futex,
    libc::SYS_getpid,
    libc::SYS_getrandom,
    libc::SYS_gettid,
    libc::SYS_lseek,
    libc::SYS_mremap,
    libc::SYS_munmap,
    libc::SYS_nanosleep,
    // Answered with an error by the refusals filter.
    libc::SYS_openat,
    libc::SYS_ppoll,
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    libc::SYS_read,
    libc::SYS_readv,
    libc::SYS_recvfrom,
    libc::SYS_recvmsg,
    libc::SYS_restart_syscall,
    libc::SYS_rseq,
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_sched_getaffinity,
    libc::SYS_sched_yield,
    libc::SYS_sendmsg,
    libc::SYS_sendto,
    libc::SYS_set_robust_list,
    libc::SYS_setsockopt,
    libc::SYS_shutdown,
    libc::SYS_sigaltstack,
    libc::SYS_tgkill,
    libc::SYS_write,
    libc::SYS_writev,
];

fn rule(conditions: Vec<Cond>) -> Result<SeccompRule> {
    Ok(SeccompRule::new(conditions)?)
}

fn equals(arg: u8, value: u64) -> Result<SeccompRule> {
    rule(vec![Cond::new(arg, Len::Qword, Op::Eq, value)?])
}

fn not_executable(arg: u8) -> Result<SeccompRule> {
    let exec = libc::PROT_EXEC as u64;
    rule(vec![Cond::new(arg, Len::Dword, Op::MaskedEq(exec), 0)?])
}

fn filter(
    rules: BTreeMap<i64, Vec<SeccompRule>>,
    mismatch: SeccompAction,
    matched: SeccompAction,
) -> Result<BpfProgram> {
    let filter = SeccompFilter::new(rules, mismatch, matched, std::env::consts::ARCH.try_into()?)?;
    Ok(filter.try_into()?)
}

fn vmm_allowlist(mismatch: SeccompAction) -> Result<BpfProgram> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        ALLOWED.iter().map(|&call| (call, Vec::new())).collect();
    rules.insert(
        libc::SYS_ioctl,
        [
            KVM_RUN,
            KVM_IRQ_LINE,
            KVM_IOEVENTFD,
            KVM_GET_ONE_REG,
            KVM_GET_REG_LIST,
            KVM_GET_MP_STATE,
            KVM_GET_VCPU_EVENTS,
            KVM_GET_DEVICE_ATTR,
            FIONBIO,
        ]
        .into_iter()
        .map(|request| equals(1, request))
        .collect::<Result<_>>()?,
    );
    rules.insert(libc::SYS_mmap, vec![not_executable(2)?]);
    rules.insert(libc::SYS_mprotect, vec![not_executable(2)?]);
    rules.insert(
        libc::SYS_madvise,
        vec![equals(2, libc::MADV_DONTNEED as u64)?],
    );
    let thread = libc::CLONE_THREAD as u64;
    rules.insert(
        libc::SYS_clone,
        vec![rule(vec![Cond::new(
            0,
            Len::Qword,
            Op::MaskedEq(thread),
            thread,
        )?])?],
    );
    rules.insert(libc::SYS_prctl, vec![equals(0, libc::PR_SET_NAME as u64)?]);
    filter(rules, mismatch, SeccompAction::Allow)
}

// clone3 takes its flags in memory a filter cannot read. Refusing it makes
// the C library fall back to clone, whose flags a filter can check.
fn refuse_clone3() -> Result<BpfProgram> {
    let rules = BTreeMap::from([(libc::SYS_clone3, Vec::new())]);
    filter(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
    )
}

const NETD_DENIED: &[libc::c_long] = &[
    libc::SYS_acct,
    libc::SYS_add_key,
    libc::SYS_bpf,
    libc::SYS_capset,
    libc::SYS_chroot,
    libc::SYS_delete_module,
    libc::SYS_execve,
    libc::SYS_execveat,
    libc::SYS_fanotify_init,
    libc::SYS_finit_module,
    libc::SYS_init_module,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_io_uring_setup,
    SYS_KEXEC_FILE_LOAD,
    libc::SYS_kexec_load,
    libc::SYS_keyctl,
    libc::SYS_mount,
    libc::SYS_name_to_handle_at,
    libc::SYS_open_by_handle_at,
    libc::SYS_perf_event_open,
    libc::SYS_personality,
    libc::SYS_pivot_root,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_ptrace,
    libc::SYS_quotactl,
    libc::SYS_reboot,
    libc::SYS_request_key,
    libc::SYS_setgid,
    libc::SYS_setgroups,
    libc::SYS_setns,
    libc::SYS_setregid,
    libc::SYS_setresgid,
    libc::SYS_setresuid,
    libc::SYS_setreuid,
    libc::SYS_setuid,
    libc::SYS_swapoff,
    libc::SYS_swapon,
    libc::SYS_umount2,
    libc::SYS_unshare,
    libc::SYS_userfaultfd,
];

fn netd_kills(matched: SeccompAction) -> Result<BpfProgram> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        NETD_DENIED.iter().map(|&call| (call, Vec::new())).collect();
    let new_user = libc::CLONE_NEWUSER as u64;
    rules.insert(
        libc::SYS_clone,
        vec![rule(vec![Cond::new(
            0,
            Len::Qword,
            Op::MaskedEq(new_user),
            new_user,
        )?])?],
    );
    filter(rules, SeccompAction::Allow, matched)
}

// The roots are empty, so opening fails anyway. glibc opens files on its
// own, such as the CPU count when malloc adds an arena, and gets an error
// here instead of a kill.
fn refusals(mut rules: BTreeMap<i64, Vec<SeccompRule>>) -> Result<BpfProgram> {
    for call in [libc::SYS_openat, libc::SYS_openat2] {
        rules.insert(call, Vec::new());
    }
    filter(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
    )
}

fn netd_refusals() -> Result<BpfProgram> {
    let ip_only = rule(vec![
        Cond::new(0, Len::Dword, Op::Ne, libc::AF_INET as u64)?,
        Cond::new(0, Len::Dword, Op::Ne, libc::AF_INET6 as u64)?,
    ])?;
    refusals(BTreeMap::from([(libc::SYS_socket, vec![ip_only])]))
}

fn violation(log: bool) -> SeccompAction {
    if log {
        SeccompAction::Log
    } else {
        SeccompAction::KillProcess
    }
}

fn apply(programs: &[BpfProgram]) -> Result<()> {
    for program in programs {
        apply_filter_all_threads(program).context("installing a seccomp filter")?;
    }
    Ok(())
}

/// With `log`, violations are logged by the kernel and allowed, to find
/// what a change needs.
pub fn install_vmm(log: bool) -> Result<()> {
    apply(&[
        refuse_clone3()?,
        refusals(BTreeMap::new())?,
        vmm_allowlist(violation(log))?,
    ])
}

pub fn install_netd(log: bool) -> Result<()> {
    apply(&[
        refuse_clone3()?,
        netd_refusals()?,
        netd_kills(violation(log))?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kvm_requests_match_the_kernel() {
        assert_eq!(KVM_RUN, 0xae80);
        assert_eq!(KVM_IRQ_LINE, 0x4008_ae61);
        assert_eq!(KVM_IOEVENTFD, 0x4040_ae79);
        assert_eq!(KVM_GET_ONE_REG, 0x4010_aeab);
        assert_eq!(KVM_GET_REG_LIST, 0xc008_aeb0);
        assert_eq!(KVM_GET_MP_STATE, 0x8004_ae98);
        assert_eq!(KVM_GET_VCPU_EVENTS, 0x8040_ae9f);
        assert_eq!(KVM_GET_DEVICE_ATTR, 0x4018_aee2);
    }

    #[test]
    fn filters_compile() {
        vmm_allowlist(SeccompAction::KillProcess).unwrap();
        refuse_clone3().unwrap();
        refusals(BTreeMap::new()).unwrap();
        netd_refusals().unwrap();
        netd_kills(SeccompAction::KillProcess).unwrap();
    }
}
