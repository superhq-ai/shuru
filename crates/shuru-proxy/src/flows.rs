//! When each open connection of a session last carried bytes each way, so
//! the host can tell a guest waiting on a remote from one with nothing to
//! wait for.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

#[derive(Default)]
pub struct Flows {
    open: Mutex<HashMap<u64, Flow>>,
}

struct Flow {
    from_guest: Instant,
    to_guest: Option<Instant>,
}

/// How long ago an open connection last carried bytes from the guest and
/// to it, in milliseconds. `to_guest_ms` is `None` while the remote has
/// sent nothing yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FlowAge {
    pub from_guest_ms: u64,
    pub to_guest_ms: Option<u64>,
}

impl Flows {
    fn open(&self) -> MutexGuard<'_, HashMap<u64, Flow>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The guest opened connection `serial`, so it waits on the remote.
    pub(crate) fn opened(&self, serial: u64) {
        self.open().insert(
            serial,
            Flow {
                from_guest: Instant::now(),
                to_guest: None,
            },
        );
    }

    pub(crate) fn sent_by_guest(&self, serial: u64) {
        if let Some(flow) = self.open().get_mut(&serial) {
            flow.from_guest = Instant::now();
        }
    }

    pub(crate) fn sent_to_guest(&self, serial: u64) {
        if let Some(flow) = self.open().get_mut(&serial) {
            flow.to_guest = Some(Instant::now());
        }
    }

    pub(crate) fn closed(&self, serial: u64) {
        self.open().remove(&serial);
    }

    pub fn ages(&self) -> Vec<FlowAge> {
        let now = Instant::now();
        let ms = |at: Instant| now.saturating_duration_since(at).as_millis() as u64;
        self.open()
            .values()
            .map(|flow| FlowAge {
                from_guest_ms: ms(flow.from_guest),
                to_guest_ms: flow.to_guest.map(ms),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flow_is_tracked_from_open_to_close() {
        let flows = Flows::default();
        flows.opened(1);
        let ages = flows.ages();
        assert_eq!(ages.len(), 1);
        assert_eq!(ages[0].to_guest_ms, None);
        flows.sent_to_guest(1);
        assert!(flows.ages()[0].to_guest_ms.is_some());
        flows.closed(1);
        assert!(flows.ages().is_empty());
    }

    #[test]
    fn bytes_for_a_closed_flow_are_ignored() {
        let flows = Flows::default();
        flows.sent_by_guest(7);
        flows.sent_to_guest(7);
        assert!(flows.ages().is_empty());
    }
}
