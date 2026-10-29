mod kvm;

mod anonfile;
mod bootloader;
mod configuration;
mod directory_sharing;
mod entropy;
mod error;
mod memory_balloon;
pub mod network;
mod serial;
mod socket;
mod storage;
pub mod terminal;
mod vm;

pub use anonfile::{open_anonymous, reflink};
pub use bootloader::LinuxBootLoader;
pub use configuration::VirtualMachineConfiguration;
pub use directory_sharing::{SharedDirectory, VirtioFileSystemDevice};
pub use entropy::VirtioEntropyDevice;
pub use error::{Result, VzError};
pub use memory_balloon::VirtioMemoryBalloonDevice;
pub use network::{FileHandleNetworkAttachment, MACAddress, VirtioNetworkDevice};
pub use serial::{FileHandleSerialAttachment, VirtioConsoleSerialPort};
pub use socket::VirtioSocketDevice;
pub use storage::{
    DiskImageAttachment, DiskImageCachingMode, DiskImageSynchronizationMode, NbdAttachment,
    StorageAttachment, StorageDevice, VirtioBlockDevice,
};
pub use vm::{VirtualMachine, VmDescriptors, VmImage, VmState};
