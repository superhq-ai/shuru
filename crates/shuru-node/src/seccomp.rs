//! The VMM syscall allowlist. Installed on every thread once the VM is
//! built, before the first `KVM_RUN`.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use seccompiler::{
    apply_filter_all_threads, BpfProgram, SeccompAction, SeccompCmpArgLen as Len,
    SeccompCmpOp as Op, SeccompCondition as Cond, SeccompFilter, SeccompRule,
};

const KVMIO: u64 = 0xae;
const IOC_WRITE: u64 = 1;

const fn io(nr: u64) -> u64 {
    (KVMIO << 8) | nr
}

const fn iow(nr: u64, size: usize) -> u64 {
    (IOC_WRITE << 30) | ((size as u64) << 16) | (KVMIO << 8) | nr
}

const KVM_RUN: u64 = io(0x80);
const KVM_IRQ_LINE: u64 = iow(0x61, size_of::<kvm_bindings::kvm_irq_level>());
const KVM_IOEVENTFD: u64 = iow(0x79, size_of::<kvm_bindings::kvm_ioeventfd>());
// An unsigned long in glibc, an int in musl.
#[allow(clippy::unnecessary_cast)]
const FIONBIO: u64 = libc::FIONBIO as u64;

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

fn allowlist(mismatch: SeccompAction) -> Result<BpfProgram> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        ALLOWED.iter().map(|&call| (call, Vec::new())).collect();
    rules.insert(
        libc::SYS_ioctl,
        [KVM_RUN, KVM_IRQ_LINE, KVM_IOEVENTFD, FIONBIO]
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
    let filter = SeccompFilter::new(
        rules,
        mismatch,
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into()?,
    )?;
    Ok(filter.try_into()?)
}

// clone3 takes its flags in memory a filter cannot read. Refusing it makes
// the C library fall back to clone, whose flags the allowlist checks.
fn refuse_clone3() -> Result<BpfProgram> {
    let rules = BTreeMap::from([(libc::SYS_clone3, Vec::new())]);
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        std::env::consts::ARCH.try_into()?,
    )?;
    Ok(filter.try_into()?)
}

/// With `log`, calls outside the list are logged by the kernel and allowed,
/// to find what a change needs.
pub fn install(log: bool) -> Result<()> {
    let mismatch = if log {
        SeccompAction::Log
    } else {
        SeccompAction::KillProcess
    };
    apply_filter_all_threads(&refuse_clone3()?).context("installing the clone3 filter")?;
    apply_filter_all_threads(&allowlist(mismatch)?).context("installing the allowlist")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kvm_requests_match_the_kernel() {
        assert_eq!(KVM_RUN, 0xae80);
        assert_eq!(KVM_IRQ_LINE, 0x4008_ae61);
        assert_eq!(KVM_IOEVENTFD, 0x4040_ae79);
    }

    #[test]
    fn both_filters_compile() {
        allowlist(SeccompAction::KillProcess).unwrap();
        refuse_clone3().unwrap();
    }
}
