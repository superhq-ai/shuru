//! What differs between guest architectures: the memory map, the interrupt
//! controller, how the kernel boots, the platform devices and the vCPU part
//! of a snapshot. The guest architecture is always the host's.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;
