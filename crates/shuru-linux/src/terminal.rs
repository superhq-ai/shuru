use std::os::fd::RawFd;

/// Saved terminal state for later restoration.
pub struct TerminalState {
    fd: RawFd,
    termios: libc::termios,
}

impl TerminalState {
    /// Save the current terminal attributes and switch to raw mode.
    /// Returns `None` if the fd is not a terminal.
    pub fn enter_raw_mode(fd: RawFd) -> Option<Self> {
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(fd, libc::TCSANOW, &raw);
            Some(TerminalState { fd, termios: saved })
        }
    }

    pub fn restore(&self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.termios);
        }
    }
}

impl Drop for TerminalState {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Get the terminal size (rows, cols) for the given fd.
/// Returns (24, 80) as fallback if the ioctl fails.
pub fn terminal_size(fd: RawFd) -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 {
            (ws.ws_row, ws.ws_col)
        } else {
            (24, 80)
        }
    }
}

/// Read bytes from a raw file descriptor.
/// Returns the number of bytes read, or 0 on EOF/error.
pub fn read_raw(fd: RawFd, buf: &mut [u8]) -> usize {
    unsafe {
        let n = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
        if n > 0 {
            n as usize
        } else {
            0
        }
    }
}

// --- epoll + signalfd-based stdin relay ---

/// Events returned by `StdinRelay::wait()`.
pub enum StdinEvent {
    /// stdin has data available to read.
    Ready,
    /// Terminal was resized (SIGWINCH).
    Resize,
    /// Shutdown was signaled by the other thread.
    Shutdown,
}

/// Write-end of the shutdown pipe.
pub struct ShutdownSignal {
    pipe_write: RawFd,
}

unsafe impl Send for ShutdownSignal {}

impl ShutdownSignal {
    pub fn signal(&self) {
        unsafe {
            libc::write(self.pipe_write, [1u8].as_ptr() as *const libc::c_void, 1);
        }
    }
}

impl Drop for ShutdownSignal {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.pipe_write);
        }
    }
}

// Event source identifiers stored in epoll_event.u64
const EV_STDIN: u64 = 0;
const EV_PIPE: u64 = 1;
const EV_SIGNAL: u64 = 2;

/// epoll-based event multiplexer for stdin, SIGWINCH, and shutdown.
/// Linux equivalent of macOS kqueue-based StdinRelay.
pub struct StdinRelay {
    epoll_fd: RawFd,
    signal_fd: RawFd,
    pipe_read: RawFd,
}

impl StdinRelay {
    /// Create a new relay watching the given stdin fd.
    pub fn new(stdin_fd: RawFd) -> Option<(StdinRelay, ShutdownSignal)> {
        unsafe {
            // Create shutdown pipe
            let mut fds = [0i32; 2];
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return None;
            }
            let pipe_read = fds[0];
            let pipe_write = fds[1];

            // Create epoll instance
            let epoll_fd = libc::epoll_create1(0);
            if epoll_fd < 0 {
                libc::close(pipe_read);
                libc::close(pipe_write);
                return None;
            }

            // Register stdin
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: EV_STDIN,
            };
            if libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, stdin_fd, &mut ev) < 0 {
                libc::close(epoll_fd);
                libc::close(pipe_read);
                libc::close(pipe_write);
                return None;
            }

            // Register shutdown pipe
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: EV_PIPE,
            };
            libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, pipe_read, &mut ev);

            // Create signalfd for SIGWINCH
            let mut sigset: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut sigset);
            libc::sigaddset(&mut sigset, libc::SIGWINCH);
            // Block SIGWINCH so signalfd receives it
            libc::sigprocmask(libc::SIG_BLOCK, &sigset, std::ptr::null_mut());

            let signal_fd = libc::signalfd(-1, &sigset, libc::SFD_NONBLOCK);
            if signal_fd < 0 {
                // signalfd not available — proceed without resize support
                libc::close(epoll_fd);
                libc::close(pipe_read);
                libc::close(pipe_write);
                return None;
            }

            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: EV_SIGNAL,
            };
            libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, signal_fd, &mut ev);

            Some((
                StdinRelay {
                    epoll_fd,
                    signal_fd,
                    pipe_read,
                },
                ShutdownSignal { pipe_write },
            ))
        }
    }

    /// Block until stdin is readable, a resize signal arrives, or shutdown
    /// is signaled.
    pub fn wait(&self) -> StdinEvent {
        unsafe {
            let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];

            let n = libc::epoll_wait(self.epoll_fd, events.as_mut_ptr(), 4, -1);
            if n < 1 {
                return StdinEvent::Shutdown;
            }

            for event in &events[..n as usize] {
                match event.u64 {
                    EV_STDIN => return StdinEvent::Ready,
                    EV_PIPE => return StdinEvent::Shutdown,
                    EV_SIGNAL => {
                        // Drain the signalfd
                        let mut info: libc::signalfd_siginfo = std::mem::zeroed();
                        libc::read(
                            self.signal_fd,
                            &mut info as *mut _ as *mut libc::c_void,
                            std::mem::size_of::<libc::signalfd_siginfo>(),
                        );
                        return StdinEvent::Resize;
                    }
                    _ => {}
                }
            }

            StdinEvent::Shutdown
        }
    }
}

impl Drop for StdinRelay {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.epoll_fd);
            libc::close(self.signal_fd);
            libc::close(self.pipe_read);
            // Unblock SIGWINCH
            let mut sigset: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut sigset);
            libc::sigaddset(&mut sigset, libc::SIGWINCH);
            libc::sigprocmask(libc::SIG_UNBLOCK, &sigset, std::ptr::null_mut());
        }
    }
}
