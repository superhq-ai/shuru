/// A virtio memory balloon. The guest reports free pages back to the host
/// on its own, and `VirtualMachine::set_memory_target` can ask it to give
/// up more.
pub struct VirtioMemoryBalloonDevice;

impl VirtioMemoryBalloonDevice {
    pub fn new() -> Self {
        VirtioMemoryBalloonDevice
    }
}

impl Default for VirtioMemoryBalloonDevice {
    fn default() -> Self {
        Self::new()
    }
}
