use std::ffi::c_void;
use std::net::TcpStream;
use std::os::unix::io::FromRawFd;

use block2::RcBlock;
use crossbeam_channel::{bounded, Receiver, Sender};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AnyThread};
use objc2_foundation::{
    NSKeyValueObservingOptions, NSObject, NSObjectNSKeyValueObserverRegistration, NSString,
};
use objc2_virtualization::{
    VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioTraditionalMemoryBalloonDevice,
    VZVirtualMachine, VZVirtualMachineState,
};

use crate::configuration::VirtualMachineConfiguration;
use crate::error::{Result, VzError};
use crate::sys::queue::{Queue, QueueAttribute};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    Stopped = 0,
    Running = 1,
    Paused = 2,
    Error = 3,
    Starting = 4,
    Pausing = 5,
    Resuming = 6,
    Stopping = 7,
    Unknown = -1,
}

/// Wrapper asserting thread safety for types whose access is serialized via dispatch queue.
#[derive(Debug)]
struct ThreadSafe<T>(T);
unsafe impl<T> Send for ThreadSafe<T> {}
unsafe impl<T> Sync for ThreadSafe<T> {}

impl<T> std::ops::Deref for ThreadSafe<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Heap-allocated context for the KVO observer.
/// Must live on the heap so the pointer remains stable after VirtualMachine moves.
#[derive(Debug)]
struct ObserverContext {
    machine: ThreadSafe<Retained<VZVirtualMachine>>,
    notifier: Sender<VmState>,
    state_notifications: Receiver<VmState>,
}

impl ObserverContext {
    fn state(&self) -> VmState {
        let vz_state = unsafe { self.machine.state() };
        match vz_state {
            s if s == VZVirtualMachineState::Stopped => VmState::Stopped,
            s if s == VZVirtualMachineState::Running => VmState::Running,
            s if s == VZVirtualMachineState::Paused => VmState::Paused,
            s if s == VZVirtualMachineState::Error => VmState::Error,
            s if s == VZVirtualMachineState::Starting => VmState::Starting,
            s if s == VZVirtualMachineState::Pausing => VmState::Pausing,
            s if s == VZVirtualMachineState::Resuming => VmState::Resuming,
            s if s == VZVirtualMachineState::Stopping => VmState::Stopping,
            _ => VmState::Unknown,
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "VirtualMachineStateObserver"]
    #[derive(Debug)]
    struct VirtualMachineStateObserver;

    impl VirtualMachineStateObserver {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value_for_key_path(
            &self,
            key_path: Option<&NSString>,
            _object: Option<&AnyObject>,
            _change: Option<&AnyObject>,
            context: *mut c_void,
        ) {
            if let Some(msg) = key_path {
                let key = msg.to_string();
                if key == "state" {
                    let ctx: &ObserverContext =
                        unsafe { &*(context as *const ObserverContext) };
                    let _ = ctx.state_notifications.try_recv();
                    let _ = ctx.notifier.send(ctx.state());
                }
            }
        }
    }
);

unsafe impl Send for VirtualMachineStateObserver {}
unsafe impl Sync for VirtualMachineStateObserver {}

impl VirtualMachineStateObserver {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(Debug)]
pub struct VirtualMachine {
    ctx: Box<ObserverContext>,
    queue: Queue,
    observer: Retained<VirtualMachineStateObserver>,
}

impl VirtualMachine {
    pub fn new(config: &VirtualMachineConfiguration) -> Self {
        unsafe {
            let queue = Queue::create("com.virt.fwk.rs", QueueAttribute::Serial);
            let dispatch_queue = queue.as_dispatch2();
            let machine = VZVirtualMachine::initWithConfiguration_queue(
                VZVirtualMachine::alloc(),
                &config.inner,
                dispatch_queue,
            );

            let (sender, receiver) = bounded(1);
            let observer = VirtualMachineStateObserver::new();

            let ctx = Box::new(ObserverContext {
                machine: ThreadSafe(machine),
                notifier: sender,
                state_notifications: receiver,
            });

            // Use the Box's stable heap address as KVO context
            let ctx_ptr: *const ObserverContext = &*ctx;

            let key = NSString::from_str("state");
            ctx.machine.addObserver_forKeyPath_options_context(
                &observer,
                &key,
                NSKeyValueObservingOptions::New,
                ctx_ptr as *mut c_void,
            );

            VirtualMachine {
                ctx,
                queue,
                observer,
            }
        }
    }

    pub fn state_channel(&self) -> Receiver<VmState> {
        self.ctx.state_notifications.clone()
    }

    pub fn supported() -> bool {
        unsafe { VZVirtualMachine::isSupported() }
    }

    pub fn start(&self) -> Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        let machine = self.ctx.machine.0.clone();
        let machine = ThreadSafe(machine);

        let dispatch_block = RcBlock::new(move || {
            let inner_tx = tx.clone();
            let completion_handler = RcBlock::new(move |err: *mut objc2_foundation::NSError| {
                if err.is_null() {
                    inner_tx.send(Ok(())).unwrap();
                } else {
                    inner_tx
                        .send(Err(unsafe { VzError::from_ns_error(&*err) }))
                        .unwrap();
                }
            });

            unsafe {
                machine.startWithCompletionHandler(&completion_handler);
            }
        });

        self.queue.exec_block_async(&dispatch_block);

        rx.recv()
            .map_err(|_| VzError::new("VM start channel closed"))?
    }

    pub fn stop(&self) -> Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        let machine = self.ctx.machine.0.clone();
        let machine = ThreadSafe(machine);

        let dispatch_block = RcBlock::new(move || {
            let inner_tx = tx.clone();
            let completion_handler = RcBlock::new(move |err: *mut objc2_foundation::NSError| {
                if err.is_null() {
                    inner_tx.send(Ok(())).unwrap();
                } else {
                    inner_tx
                        .send(Err(unsafe { VzError::from_ns_error(&*err) }))
                        .unwrap();
                }
            });

            unsafe {
                machine.stopWithCompletionHandler(&completion_handler);
            }
        });

        self.queue.exec_block_async(&dispatch_block);

        rx.recv()
            .map_err(|_| VzError::new("VM stop channel closed"))?
    }

    /// How much memory the guest should keep. The framework rounds the
    /// value down to a megabyte and clamps it to the configured size; the
    /// guest responds in its own time. Fails without a balloon device.
    pub fn set_memory_target(&self, bytes: u64) -> Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        let machine = ThreadSafe(self.ctx.machine.0.clone());

        let dispatch_block = RcBlock::new(move || {
            let devices = unsafe { machine.memoryBalloonDevices() };
            let result = match devices.firstObject() {
                Some(device) => {
                    let device: Retained<VZVirtioTraditionalMemoryBalloonDevice> =
                        unsafe { Retained::cast_unchecked(device) };
                    unsafe { device.setTargetVirtualMachineMemorySize(bytes) };
                    Ok(())
                }
                None => Err(VzError::new("no memory balloon device")),
            };
            let _ = tx.send(result);
        });

        self.queue.exec_block_async(&dispatch_block);

        rx.recv().map_err(|_| VzError::new("VM queue closed"))?
    }

    pub fn can_start(&self) -> bool {
        self.queue
            .exec_sync(move || unsafe { self.ctx.machine.canStart() })
    }

    pub fn can_stop(&self) -> bool {
        self.queue
            .exec_sync(move || unsafe { self.ctx.machine.canRequestStop() })
    }

    pub fn can_pause(&self) -> bool {
        self.queue
            .exec_sync(move || unsafe { self.ctx.machine.canPause() })
    }

    pub fn can_resume(&self) -> bool {
        self.queue
            .exec_sync(move || unsafe { self.ctx.machine.canResume() })
    }

    pub fn can_request_stop(&self) -> bool {
        self.queue
            .exec_sync(move || unsafe { self.ctx.machine.canRequestStop() })
    }

    /// Connects to a vsock port on the guest and returns a TcpStream.
    /// Must dispatch on the VM's queue per Apple Virtualization framework requirements.
    pub fn connect_to_vsock_port(&self, port: u32) -> Result<TcpStream> {
        let (tx, rx) = std::sync::mpsc::channel::<Result<TcpStream>>();
        let machine = self.ctx.machine.0.clone();
        let machine = ThreadSafe(machine);

        let dispatch_block = RcBlock::new(move || {
            let devices = unsafe { machine.socketDevices() };
            let count = devices.len();
            if count == 0 {
                tx.send(Err(VzError::new("No socket devices found on the VM")))
                    .ok();
                return;
            }

            let device_obj = devices.objectAtIndex(0);
            // Downcast VZSocketDevice to VZVirtioSocketDevice
            let device: &VZVirtioSocketDevice =
                unsafe { &*(&*device_obj as *const _ as *const VZVirtioSocketDevice) };

            let inner_tx = tx.clone();
            let completion_handler = RcBlock::new(
                move |conn: *mut VZVirtioSocketConnection, err: *mut objc2_foundation::NSError| {
                    if !err.is_null() {
                        let error = unsafe { VzError::from_ns_error(&*err) };
                        inner_tx.send(Err(error)).ok();
                    } else if conn.is_null() {
                        inner_tx
                            .send(Err(VzError::new("vsock connection returned null")))
                            .ok();
                    } else {
                        let fd = unsafe { (*conn).fileDescriptor() };
                        // dup the fd so it survives after the connection object is released
                        let duped = unsafe { libc::dup(fd) };
                        if duped < 0 {
                            inner_tx
                                .send(Err(VzError::new("failed to dup vsock fd")))
                                .ok();
                        } else {
                            let stream = unsafe { TcpStream::from_raw_fd(duped) };
                            inner_tx.send(Ok(stream)).ok();
                        }
                    }
                },
            );

            unsafe {
                device.connectToPort_completionHandler(port, &completion_handler);
            }
        });

        self.queue.exec_block_async(&dispatch_block);

        // The framework never calls the completion handler when the VM
        // stops while a connect is pending.
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if matches!(self.state(), VmState::Stopped | VmState::Error) {
                        return Err(VzError::new("VM stopped during vsock connect"));
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(VzError::new("vsock connection channel closed"));
                }
            }
        }
    }

    pub fn state(&self) -> VmState {
        self.ctx.state()
    }
}

impl Drop for VirtualMachine {
    fn drop(&mut self) {
        let key_path = NSString::from_str("state");
        let ctx_ptr: *const ObserverContext = &*self.ctx;

        unsafe {
            self.ctx.machine.removeObserver_forKeyPath_context(
                &self.observer,
                &key_path,
                ctx_ptr as *mut c_void,
            );
        }
    }
}
