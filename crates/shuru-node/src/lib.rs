//! A Linux node runs each sandbox VM in its own jailed `shuru-vmm` process,
//! and its network in a jailed `shuru-netd` (RFC 0008). `shuru-node` is the
//! only privileged process: it opens every file, creates every socket and
//! hands the jailed processes descriptors.

pub mod cgroup;
pub mod control;
pub mod firewall;
pub mod jail;
pub mod seccomp;
