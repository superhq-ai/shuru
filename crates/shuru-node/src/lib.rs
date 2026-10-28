//! A Linux node runs each sandbox VM in its own jailed `shuru-vmm` process
//! (RFC 0008). `shuru-node` is the only privileged process: it opens every
//! file, creates every socket and hands the VMM descriptors.

pub mod cgroup;
pub mod control;
pub mod jail;
