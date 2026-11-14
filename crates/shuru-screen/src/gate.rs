//! The input gate (RFC 0009): who, if anyone, may drive the desktop. One
//! viewer holds it at a time, or the agent, or no one. Extracted from the
//! hub so the streaming core stays voxl's; the server owns the gate and
//! applies its side effects (release held keys, tell each viewer).

use parking_lot::Mutex;

/// Who may send input. A pure value; transitions are total.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    /// No one drives the desktop.
    Shut,
    /// The agent drives it over `POST /input`.
    Agent,
    /// A named viewer drives it over its stream.
    Viewer(String),
}

impl Gate {
    /// Whether input from `viewer` (a stream's id, or `None`) lands in this state.
    pub fn may_viewer(&self, viewer: Option<&str>) -> bool {
        matches!((self, viewer), (Gate::Viewer(open), Some(v)) if open == v)
    }

    /// Whether the agent's `POST /input` lands in this state.
    pub fn may_agent(&self) -> bool {
        *self == Gate::Agent
    }

    /// The gate message a given viewer is told: whether its input lands now.
    pub fn message(&self, viewer: Option<&str>) -> String {
        serde_json::json!({ "type": "gate", "input": self.may_viewer(viewer) }).to_string()
    }

    /// The `/gate` view: who holds it.
    pub fn holder(&self) -> serde_json::Value {
        match self {
            Gate::Shut => serde_json::json!({ "viewer": null }),
            Gate::Agent => serde_json::json!({ "viewer": "agent" }),
            Gate::Viewer(v) => serde_json::json!({ "viewer": v }),
        }
    }
}

/// The current gate, shared by the server's handlers.
pub struct GateState {
    gate: Mutex<Gate>,
}

impl GateState {
    pub fn new() -> Self {
        GateState {
            gate: Mutex::new(Gate::Shut),
        }
    }

    pub fn get(&self) -> Gate {
        self.gate.lock().clone()
    }

    /// Moves to `next`. Returns the previous gate when it actually changed, so the
    /// caller can release held keys and re-tell viewers; `None` when it was already there.
    pub fn set(&self, next: Gate) -> Option<Gate> {
        let mut g = self.gate.lock();
        if *g == next {
            return None;
        }
        Some(std::mem::replace(&mut g, next))
    }

    pub fn may_viewer(&self, viewer: Option<&str>) -> bool {
        self.gate.lock().may_viewer(viewer)
    }

    pub fn may_agent(&self) -> bool {
        self.gate.lock().may_agent()
    }
}

impl Default for GateState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_open_viewer_may_input() {
        let g = GateState::new();
        assert!(!g.may_viewer(Some("a")));
        assert_eq!(g.set(Gate::Viewer("a".into())), Some(Gate::Shut));
        assert!(g.may_viewer(Some("a")));
        assert!(!g.may_viewer(Some("b")));
        assert!(!g.may_viewer(None));
        assert!(!g.may_agent());
    }

    #[test]
    fn agent_gate_is_separate_from_viewers() {
        let g = GateState::new();
        assert_eq!(g.set(Gate::Agent), Some(Gate::Shut));
        assert!(g.may_agent());
        assert!(!g.may_viewer(Some("a")));
        // Setting the same gate again is a no-op.
        assert_eq!(g.set(Gate::Agent), None);
    }

    #[test]
    fn messages_name_the_holder() {
        assert_eq!(Gate::Shut.holder()["viewer"], serde_json::Value::Null);
        assert_eq!(Gate::Agent.holder()["viewer"], "agent");
        assert_eq!(Gate::Viewer("v".into()).holder()["viewer"], "v");
        let m: serde_json::Value =
            serde_json::from_str(&Gate::Viewer("v".into()).message(Some("v"))).unwrap();
        assert_eq!(m["input"], true);
    }
}
