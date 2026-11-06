//! A switch: one setting that is on or off, changed at once.

use accesskit::{Action, Role, Toggled};

use super::super::painter;
use super::super::widgets::{Content, Draw, Mark, Paint};
use super::super::{style, Builder, Node};
use super::surface::Cell;
use super::{label, note, ROW_H, ROW_PAD};
use crate::draw::{theme, Canvas, Rect};

/// The size of the track.
pub const TRACK_W: f32 = 34.0;
pub const TRACK_H: f32 = 20.0;

pub struct Switch<'a> {
    pub id: u64,
    pub label: &'a str,
    pub on: bool,
    /// Help shown under the row and given as the description of the switch.
    pub description: Option<&'a str>,
}

/// The track and its knob, which sits at the end when the switch is on.
pub struct Knob {
    pub on: bool,
}

impl Draw for Knob {
    fn paint(&self, c: &mut Canvas, _: &mut Paint, b: Rect, _: Mark) {
        let k = &theme().controls;
        let track = Rect::new(b.x, b.y + (b.h - TRACK_H) / 2.0, TRACK_W, TRACK_H);
        let r = TRACK_H / 2.0;
        let d = TRACK_H - 6.0;
        let x = if self.on {
            track.x + TRACK_W - 3.0 - d
        } else {
            track.x + 3.0
        };
        let knob = Rect::new(x, track.y + 3.0, d, d);
        if self.on {
            painter::rounded(c, track, r, theme().text);
            painter::rounded(c, knob, d / 2.0, k.card);
        } else {
            painter::rounded(c, track, r, k.track);
            painter::ring(c, track, r, k.thumb);
            painter::rounded(c, knob, d / 2.0, k.thumb);
        }
    }
}

impl Knob {
    pub fn content(on: bool) -> Content {
        Content::Custom(Box::new(Self { on }))
    }
}

pub(super) fn add(b: &mut Builder, card: usize, s: Switch) {
    let mut node = Node::new(
        Content::Custom(Box::new(Cell)),
        style()
            .row()
            .center()
            .height(ROW_H)
            .pad(0.0, ROW_PAD, 0.0, ROW_PAD),
    )
    .semantic(s.id, Role::Switch, s.label);
    if let Some(n) = node.semantic.as_mut() {
        n.set_toggled(if s.on { Toggled::True } else { Toggled::False });
        n.add_action(Action::Focus);
        n.add_action(Action::ScrollIntoView);
        if let Some(text) = s.description {
            n.set_description(text);
        }
    }
    let row = b.row_in(card, node);
    label(b, row, s.label);
    b.add(
        row,
        Node::new(Knob::content(s.on), style().width(TRACK_W).height(TRACK_H)),
    );
    if let Some(text) = s.description {
        note(b, card, text);
    }
}
