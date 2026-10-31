smoltcp 0.12.0 as published on crates.io, with upstream commit 336a1dd
applied to `src/socket/tcp.rs`: the window left after the data the guest
sent past the last ack saturates at zero. Without it a FIN at a zero
window edge underflows a sequence number and panics, which aborts
shuru-netd.

Only the library is kept, with its lints capped in `src/lib.rs` as cargo
caps them for registry crates. Drop this copy and the patch in the
workspace Cargo.toml once a release carries the fix.
