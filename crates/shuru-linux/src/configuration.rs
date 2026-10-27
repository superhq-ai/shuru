use std::cell::RefCell;
use std::os::fd::RawFd;

use crate::bootloader::LinuxBootLoader;
use crate::directory_sharing::VirtioFileSystemDevice;
use crate::entropy::VirtioEntropyDevice;
use crate::error::{Result, VzError};
use crate::memory_balloon::VirtioMemoryBalloonDevice;
use crate::network::VirtioNetworkDevice;
use crate::serial::VirtioConsoleSerialPort;
use crate::socket::VirtioSocketDevice;
use crate::storage::StorageDevice;

pub(crate) struct ConfigData {
    pub cpu_count: usize,
    pub memory_size: u64,
    pub kernel_path: String,
    pub initrd_path: Option<String>,
    pub command_line: String,
    pub serial_read_fd: Option<RawFd>,
    pub serial_write_fd: Option<RawFd>,
    pub disk_path: Option<String>,
    pub disk_read_only: bool,
    pub network_fd: Option<RawFd>,
    pub network_mac: Option<[u8; 6]>,
    pub has_socket: bool,
    pub mounts: Vec<(String, String, bool)>, // (tag, host_path, read_only)
    pub has_balloon: bool,
}

pub struct VirtualMachineConfiguration {
    pub(crate) inner: RefCell<ConfigData>,
}

impl VirtualMachineConfiguration {
    pub fn new(boot_loader: &LinuxBootLoader, cpus: usize, memory: u64) -> Self {
        VirtualMachineConfiguration {
            inner: RefCell::new(ConfigData {
                cpu_count: cpus,
                memory_size: memory,
                kernel_path: boot_loader.kernel_path.clone(),
                initrd_path: boot_loader.initrd_path.borrow().clone(),
                command_line: boot_loader
                    .command_line
                    .borrow()
                    .clone()
                    .unwrap_or_default(),
                serial_read_fd: None,
                serial_write_fd: None,
                disk_path: None,
                disk_read_only: false,
                network_fd: None,
                network_mac: None,
                has_socket: false,
                mounts: Vec::new(),
                has_balloon: false,
            }),
        }
    }

    pub fn set_cpu_count(&self, cpus: usize) {
        self.inner.borrow_mut().cpu_count = cpus;
    }

    pub fn set_memory_size(&self, memory: u64) {
        self.inner.borrow_mut().memory_size = memory;
    }

    pub fn set_boot_loader(&self, boot_loader: &LinuxBootLoader) {
        let mut inner = self.inner.borrow_mut();
        inner.kernel_path = boot_loader.kernel_path.clone();
        inner.initrd_path = boot_loader.initrd_path.borrow().clone();
        if let Some(ref cmdline) = *boot_loader.command_line.borrow() {
            inner.command_line = cmdline.clone();
        }
    }

    pub fn set_serial_ports(&self, ports: &[VirtioConsoleSerialPort]) {
        if let Some(port) = ports.first() {
            let mut inner = self.inner.borrow_mut();
            inner.serial_read_fd = port.read_fd;
            inner.serial_write_fd = port.write_fd;
        }
    }

    pub fn set_storage_devices(&self, devices: &[&dyn StorageDevice]) {
        if let Some(device) = devices.first() {
            let mut inner = self.inner.borrow_mut();
            inner.disk_path = device.get_disk_path().map(|s| s.to_string());
            inner.disk_read_only = device.get_read_only();
        }
    }

    pub fn set_network_devices(&self, devices: &[VirtioNetworkDevice]) {
        if let Some(device) = devices.first() {
            let mut inner = self.inner.borrow_mut();
            inner.network_fd = device.fd;
            let mac = device.mac_bytes();
            if mac != [0; 6] {
                inner.network_mac = Some(mac);
            }
        }
    }

    pub fn set_socket_devices(&self, devices: &[VirtioSocketDevice]) {
        if !devices.is_empty() {
            self.inner.borrow_mut().has_socket = true;
        }
    }

    pub fn set_directory_sharing_devices(&self, devices: &[VirtioFileSystemDevice]) {
        let mut inner = self.inner.borrow_mut();
        for dev in devices {
            inner
                .mounts
                .push((dev.tag.clone(), dev.host_path.clone(), dev.read_only));
        }
    }

    pub fn set_entropy_devices(&self, _devices: &[VirtioEntropyDevice]) {
        // Entropy device is a no-op on KVM — /dev/urandom is available.
    }

    pub fn set_memory_balloon_devices(&self, devices: &[VirtioMemoryBalloonDevice]) {
        self.inner.borrow_mut().has_balloon = !devices.is_empty();
    }

    pub fn validate(&self) -> Result<()> {
        let inner = self.inner.borrow();
        if inner.kernel_path.is_empty() {
            return Err(VzError::new("kernel path is required"));
        }
        if inner.memory_size == 0 {
            return Err(VzError::new("memory size must be > 0"));
        }
        if inner.cpu_count == 0 {
            return Err(VzError::new("CPU count must be > 0"));
        }
        Ok(())
    }
}
