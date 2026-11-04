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

const fn iowr(nr: u64, size: usize) -> u64 {
    ioc(IOC_READ | IOC_WRITE, nr, size)
}

const KVM_RUN: u64 = io(0x80);
const KVM_IRQ_LINE: u64 = iow(0x61, size_of::<kvm_bindings::kvm_irq_level>());
const KVM_IOEVENTFD: u64 = iow(0x79, size_of::<kvm_bindings::kvm_ioeventfd>());
// Reading the vCPUs and the interrupt controllers for a snapshot.
const KVM_GET_MP_STATE: u64 = ior(0x98, size_of::<kvm_bindings::kvm_mp_state>());
const KVM_GET_VCPU_EVENTS: u64 = ior(0x9f, size_of::<kvm_bindings::kvm_vcpu_events>());
#[cfg(target_arch = "aarch64")]
const KVM_GET_ONE_REG: u64 = iow(0xab, size_of::<kvm_bindings::kvm_one_reg>());
#[cfg(target_arch = "aarch64")]
const KVM_GET_REG_LIST: u64 = iowr(0xb0, size_of::<kvm_bindings::kvm_reg_list>());
#[cfg(target_arch = "aarch64")]
const KVM_GET_DEVICE_ATTR: u64 = iow(0xe2, size_of::<kvm_bindings::kvm_device_attr>());
#[cfg(target_arch = "aarch64")]
const SNAPSHOT_READS: &[u64] = &[KVM_GET_ONE_REG, KVM_GET_REG_LIST, KVM_GET_DEVICE_ATTR];
#[cfg(target_arch = "x86_64")]
const SNAPSHOT_READS: &[u64] = &[
    iowr(0x62, size_of::<kvm_bindings::kvm_irqchip>()),
    ior(0x7c, size_of::<kvm_bindings::kvm_clock_data>()),
    ior(0x81, size_of::<kvm_bindings::kvm_regs>()),
    ior(0x83, size_of::<kvm_bindings::kvm_sregs>()),
    iowr(0x88, size_of::<kvm_bindings::kvm_msrs>()),
    ior(0x8e, size_of::<kvm_bindings::kvm_lapic_state>()),
    iowr(0x91, size_of::<kvm_bindings::kvm_cpuid2>()),
    ior(0x9f, size_of::<kvm_bindings::kvm_pit_state2>()),
    ior(0xa1, size_of::<kvm_bindings::kvm_debugregs>()),
    io(0xa3),
    ior(0xa4, size_of::<kvm_bindings::kvm_xsave>()),
    ior(0xa6, size_of::<kvm_bindings::kvm_xcrs>()),
];
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

// glibc calls these on x86_64, where they exist, for poll and epoll_wait.
#[cfg(target_arch = "x86_64")]
const ALLOWED_LEGACY: &[libc::c_long] = &[libc::SYS_epoll_wait, libc::SYS_poll];
#[cfg(not(target_arch = "x86_64"))]
const ALLOWED_LEGACY: &[libc::c_long] = &[];

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
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = ALLOWED
        .iter()
        .chain(ALLOWED_LEGACY)
        .map(|&call| (call, Vec::new()))
        .collect();
    rules.insert(
        libc::SYS_ioctl,
        [
            KVM_RUN,
            KVM_IRQ_LINE,
            KVM_IOEVENTFD,
            KVM_GET_MP_STATE,
            KVM_GET_VCPU_EVENTS,
            FIONBIO,
        ]
        .into_iter()
        .chain(SNAPSHOT_READS.iter().copied())
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
    // Stacked filters take the strictest action, so clone3 must pass here
    // for the ENOSYS of refuse_clone3 to be what the caller gets.
    rules.insert(libc::SYS_clone3, Vec::new());
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

// Port I/O, segment descriptors and a.out libraries, on x86_64 only.
#[cfg(target_arch = "x86_64")]
const NETD_DENIED_ARCH: &[libc::c_long] = &[
    libc::SYS_ioperm,
    libc::SYS_iopl,
    libc::SYS_modify_ldt,
    libc::SYS_uselib,
];
#[cfg(not(target_arch = "x86_64"))]
const NETD_DENIED_ARCH: &[libc::c_long] = &[];

fn netd_kills(matched: SeccompAction) -> Result<BpfProgram> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = NETD_DENIED
        .iter()
        .chain(NETD_DENIED_ARCH)
        .map(|&call| (call, Vec::new()))
        .collect();
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

#[cfg(target_arch = "x86_64")]
const OPENS: &[libc::c_long] = &[
    libc::SYS_openat,
    libc::SYS_openat2,
    libc::SYS_open,
    libc::SYS_creat,
];
#[cfg(not(target_arch = "x86_64"))]
const OPENS: &[libc::c_long] = &[libc::SYS_openat, libc::SYS_openat2];

// The roots are empty, so opening fails anyway. glibc opens files on its
// own, such as the CPU count when malloc adds an arena, and gets an error
// here instead of a kill.
fn refusals(mut rules: BTreeMap<i64, Vec<SeccompRule>>) -> Result<BpfProgram> {
    for &call in OPENS {
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

/// x32 system calls carry the x86_64 architecture in the filter input and
/// their own numbers, from bit 30 up, which no rule above names. A deny
/// list would let them through, so they are refused before the rest.
#[cfg(target_arch = "x86_64")]
fn refuse_x32(action: SeccompAction) -> Vec<BpfProgram> {
    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const JGE_K: u16 = 0x35;
    const RET_K: u16 = 0x06;
    const AUDIT_ARCH_X86_64: u32 = 62 | 0x8000_0000 | 0x4000_0000;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    let op = |code, k, jt, jf| seccompiler::sock_filter { code, jt, jf, k };
    let refuse = u32::from(action);
    let allow = u32::from(SeccompAction::Allow);
    vec![vec![
        op(LD_W_ABS, 4, 0, 0),
        op(JEQ_K, AUDIT_ARCH_X86_64, 1, 0),
        op(RET_K, refuse, 0, 0),
        op(LD_W_ABS, 0, 0, 0),
        op(JGE_K, X32_SYSCALL_BIT, 0, 1),
        op(RET_K, refuse, 0, 0),
        op(RET_K, allow, 0, 0),
    ]]
}

#[cfg(not(target_arch = "x86_64"))]
fn refuse_x32(_action: SeccompAction) -> Vec<BpfProgram> {
    Vec::new()
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
    apply(&vmm_programs(log)?)
}

fn vmm_programs(log: bool) -> Result<Vec<BpfProgram>> {
    let mut programs = refuse_x32(violation(log));
    programs.extend([
        refuse_clone3()?,
        refusals(BTreeMap::new())?,
        vmm_allowlist(violation(log))?,
    ]);
    Ok(programs)
}

pub fn install_netd(log: bool) -> Result<()> {
    apply(&netd_programs(log)?)
}

fn netd_programs(log: bool) -> Result<Vec<BpfProgram>> {
    let mut programs = refuse_x32(violation(log));
    programs.extend([
        refuse_clone3()?,
        netd_refusals()?,
        netd_kills(violation(log))?,
    ]);
    Ok(programs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kvm_requests_match_the_kernel() {
        assert_eq!(KVM_RUN, 0xae80);
        assert_eq!(KVM_IRQ_LINE, 0x4008_ae61);
        assert_eq!(KVM_IOEVENTFD, 0x4040_ae79);
        assert_eq!(KVM_GET_MP_STATE, 0x8004_ae98);
        assert_eq!(KVM_GET_VCPU_EVENTS, 0x8040_ae9f);
        #[cfg(target_arch = "aarch64")]
        assert_eq!(SNAPSHOT_READS, [0x4010_aeab, 0xc008_aeb0, 0x4018_aee2]);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            SNAPSHOT_READS,
            [
                0xc208_ae62,
                0x8030_ae7c,
                0x8090_ae81,
                0x8138_ae83,
                0xc008_ae88,
                0x8400_ae8e,
                0xc008_ae91,
                0x8070_ae9f,
                0x8080_aea1,
                0xaea3,
                0x9000_aea4,
                0x8188_aea6,
            ]
        );
    }

    #[test]
    fn filters_compile() {
        vmm_allowlist(SeccompAction::KillProcess).unwrap();
        refuse_clone3().unwrap();
        refusals(BTreeMap::new()).unwrap();
        netd_refusals().unwrap();
        netd_kills(SeccompAction::KillProcess).unwrap();
    }

    /// glibc 2.38 and later create threads with clone3 on aarch64, and fall
    /// back to clone only when it answers ENOSYS.
    #[test]
    fn the_filters_answer_clone3_with_enosys() {
        clone3_answers_enosys(vmm_programs(false).unwrap());
        clone3_answers_enosys(netd_programs(false).unwrap());
    }

    fn clone3_answers_enosys(programs: Vec<BpfProgram>) {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let code = match apply(&programs) {
                Err(_) => 100,
                Ok(()) => {
                    let r =
                        unsafe { libc::syscall(libc::SYS_clone3, std::ptr::null::<u8>(), 0usize) };
                    if r == -1 {
                        std::io::Error::last_os_error()
                            .raw_os_error()
                            .unwrap_or(101)
                    } else {
                        102
                    }
                }
            };
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(
            !libc::WIFSIGNALED(status),
            "killed by signal {}",
            libc::WTERMSIG(status)
        );
        assert_eq!(libc::WEXITSTATUS(status), libc::ENOSYS);
    }

    /// A getpid with the x32 bit set ends the process, though getpid
    /// itself is allowed.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x32_calls_end_the_process() {
        for programs in [vmm_programs(false).unwrap(), netd_programs(false).unwrap()] {
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                if apply(&programs).is_err() {
                    unsafe { libc::_exit(100) };
                }
                unsafe { libc::syscall(0x4000_0000 | libc::SYS_getpid) };
                unsafe { libc::_exit(0) };
            }
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            assert!(
                libc::WIFSIGNALED(status),
                "exited {}",
                libc::WEXITSTATUS(status)
            );
            assert_eq!(libc::WTERMSIG(status), libc::SIGSYS);
        }
    }
}
