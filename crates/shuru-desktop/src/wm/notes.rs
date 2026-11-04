//! Notifications: up to three cards at the top right of the work area,
//! newest on top, and the recent ones for the agent.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Value};
use x11rb::protocol::xproto::*;

use super::{Wm, R};
use crate::bus::{self, Note};
use crate::chrome::{self, NOTE_W};
use crate::draw::{Canvas, Image};
use crate::icon;

pub const SHOWN: usize = 3;
const KEPT: usize = 50;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const GAP: i16 = 8;
const NOTE_ICON: usize = 20;

/// Why a notification closed, as the spec numbers it.
pub const EXPIRED: u32 = 1;
pub const DISMISSED: u32 = 2;
pub const CLOSED: u32 = 3;

pub struct Record {
    id: u32,
    app: String,
    summary: String,
    body: String,
    time: u64,
    actions: Vec<(String, String)>,
    open: bool,
    icon: Option<Image>,
    deadline: Option<Instant>,
}

#[derive(Default)]
pub struct Notes {
    /// Newest first.
    recent: VecDeque<Record>,
    next: u32,
    /// The ids on screen, newest first, where each card is and how it looks.
    shown: Vec<(u32, R, Canvas)>,
}

impl Notes {
    pub fn deadline(&self) -> Option<Instant> {
        self.recent
            .iter()
            .filter(|r| r.open)
            .filter_map(|r| r.deadline)
            .min()
    }
}

fn record_json(r: &Record) -> Value {
    json!({
        "id": r.id,
        "app": r.app,
        "summary": r.summary,
        "body": r.body,
        "time": r.time,
        "open": r.open,
        "actions": r.actions.iter().map(|(k, l)| json!({ "key": k, "label": l })).collect::<Vec<_>>(),
    })
}

fn clamp(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

impl Wm {
    pub(super) fn notify(&mut self, note: Note) -> Result<u32> {
        let icon = self.note_icon(&note);
        let id = if note.replaces != 0 && self.notes.recent.iter().any(|r| r.id == note.replaces) {
            note.replaces
        } else {
            self.notes.next += 1;
            self.notes.next
        };
        let deadline = match note.timeout_ms {
            0 => None,
            t if t < 0 => Some(Instant::now() + DEFAULT_TIMEOUT),
            t => Some(Instant::now() + Duration::from_millis(t as u64)),
        };
        let record = Record {
            id,
            app: clamp(&note.app, 80),
            summary: clamp(&note.summary, 200),
            body: clamp(&note.body, 400),
            time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            actions: note.actions,
            open: true,
            icon,
            deadline,
        };
        self.notes.recent.retain(|r| r.id != id);
        self.notes.recent.push_front(record);
        self.notes.recent.truncate(KEPT);
        self.layout_notes()?;
        Ok(id)
    }

    fn note_icon(&mut self, note: &Note) -> Option<Image> {
        let of = |pick: &dyn Fn(&super::Entry) -> bool| {
            self.entries
                .iter()
                .find(|x| pick(x))
                .and_then(|x| x.large.as_ref())
                .map(|i| icon::scale(i, NOTE_ICON))
        };
        note.entry
            .as_deref()
            .and_then(|e| of(&|x| x.app.id == e || x.app.is_named(e)))
            .or_else(|| of(&|x| x.app.is_named(&note.app)))
            .or_else(|| {
                (!note.icon.is_empty())
                    .then(|| icon::load(&note.icon, NOTE_ICON as u32))
                    .flatten()
            })
    }

    /// Closes a notification and tells its client why.
    pub(super) fn close_note(&mut self, id: u32, reason: u32) -> Result<bool> {
        let Some(r) = self.notes.recent.iter_mut().find(|r| r.id == id && r.open) else {
            return Ok(false);
        };
        r.open = false;
        self.bus_send(bus::Event::Closed { id, reason });
        self.layout_notes()?;
        Ok(true)
    }

    /// Runs an action of a notification, as a click or the agent does.
    pub(super) fn note_action(&mut self, id: u32, key: &str) -> Result<bool> {
        let Some(r) = self.notes.recent.iter().find(|r| r.id == id && r.open) else {
            return Ok(false);
        };
        if key != "default" && !r.actions.iter().any(|(k, _)| k == key) {
            return Ok(false);
        }
        self.bus_send(bus::Event::Action {
            id,
            key: key.to_string(),
        });
        self.close_note(id, DISMISSED)
    }

    pub(super) fn notes_json(&self) -> Value {
        Value::Array(self.notes.recent.iter().map(record_json).collect())
    }

    pub(super) fn bus_send(&self, ev: bus::Event) {
        if let Some(tx) = &self.bus {
            let _ = tx.send(ev);
        }
    }

    pub(super) fn expire_notes(&mut self, now: Instant) -> Result<()> {
        let due: Vec<u32> = self
            .notes
            .recent
            .iter()
            .filter(|r| r.open && r.deadline.is_some_and(|d| d <= now))
            .map(|r| r.id)
            .collect();
        for id in due {
            self.close_note(id, EXPIRED)?;
        }
        Ok(())
    }

    /// Puts the newest open notifications on the screen.
    pub(super) fn layout_notes(&mut self) -> Result<()> {
        let (wx, wy, ww, _) = chrome::work_area(self.sw, self.sh);
        let open: Vec<usize> = (0..self.notes.recent.len())
            .filter(|&i| self.notes.recent[i].open)
            .take(SHOWN)
            .collect();
        let mut shown = Vec::new();
        let mut y = wy + GAP;
        for (slot, &i) in open.iter().enumerate() {
            let win = self.ours.notes[slot];
            let r = &self.notes.recent[i];
            let (c, mask) =
                chrome::note(&mut self.font, r.icon.as_ref(), &r.app, &r.summary, &r.body);
            let rect = (
                wx + ww as i16 - NOTE_W as i16 - 1 - GAP,
                y - 1,
                c.w as u16,
                c.h as u16,
            );
            self.place_window(win, rect)?;
            self.shape(win, &mask)?;
            self.conn.map_window(win)?;
            self.put(win, &c, 0, 0)?;
            y += c.h as i16 - 2 + GAP;
            shown.push((r.id, rect, c));
        }
        for slot in open.len()..SHOWN {
            self.conn.unmap_window(self.ours.notes[slot])?;
        }
        for (_, r, _) in std::mem::replace(&mut self.notes.shown, shown) {
            self.moved(r);
        }
        self.restack()
    }

    pub(super) fn note_rects(&self) -> Vec<R> {
        self.notes.shown.iter().map(|(_, r, _)| *r).collect()
    }

    pub(super) fn note_slot(&self, w: Window) -> Option<usize> {
        self.ours.notes.iter().position(|&n| n == w)
    }

    /// A click on a card runs its default action when it has one, and
    /// dismisses it either way; its close button only dismisses.
    pub(super) fn note_press(&mut self, slot: usize, x: i16, y: i16) -> Result<()> {
        let Some(&(id, _, _)) = self.notes.shown.get(slot) else {
            return Ok(());
        };
        let b = chrome::note_close();
        let on_close =
            (b.x..b.x + b.w).contains(&(x as f32)) && (b.y..b.y + b.h).contains(&(y as f32));
        let has_default = !on_close
            && self
                .notes
                .recent
                .iter()
                .find(|r| r.id == id)
                .is_some_and(|r| r.actions.iter().any(|(k, _)| k == "default"));
        if has_default {
            self.note_action(id, "default")?;
        } else {
            self.close_note(id, DISMISSED)?;
        }
        Ok(())
    }

    /// Repaints a card the server lost.
    pub(super) fn expose_note(&self, slot: usize) -> Result<()> {
        match self.notes.shown.get(slot) {
            Some((_, _, c)) => self.put(self.ours.notes[slot], c, 0, 0),
            None => Ok(()),
        }
    }
}
