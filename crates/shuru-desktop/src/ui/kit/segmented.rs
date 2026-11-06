//! A segmented control: a radio group whose radios sit side by side in a
//! well, the checked one raised.

use accesskit::{Action, Role, Toggled};

use super::super::painter;
use super::super::widgets::{Content, Draw, Ink, Mark, Paint, TEXT_SIZE};
use super::super::{style, Builder, Node};
use super::{label, note, Choice, ROW_H, ROW_PAD};
use crate::draw::{theme, Canvas, Rect};

pub struct Segmented<'a> {
    /// The id of the group; each choice has its own.
    pub id: u64,
    pub label: &'a str,
    pub choices: &'a [Choice<'a>],
    pub chosen: u64,
    /// Help shown under the row and given as the description of the group.
    pub description: Option<&'a str>,
}

struct Track;

impl Draw for Track {
    fn paint(&self, c: &mut Canvas, _: &mut Paint, b: Rect, _: Mark) {
        painter::rounded(c, b, 8.0, theme().controls.track);
    }
}

struct Segment {
    on: bool,
}

impl Draw for Segment {
    fn paint(&self, c: &mut Canvas, _: &mut Paint, b: Rect, mark: Mark) {
        let k = &theme().controls;
        if self.on {
            painter::rounded(c, b, 6.0, k.on);
            painter::ring(c, b, 6.0, k.on_ring);
        } else if mark == Mark::Hover {
            painter::rounded(c, b, 6.0, k.hover);
        }
    }
}

pub(super) fn add(b: &mut Builder, card: usize, s: Segmented) {
    let mut group = Node::new(
        Content::Box,
        style()
            .row()
            .center()
            .height(ROW_H)
            .pad(0.0, ROW_PAD - 4.0, 0.0, ROW_PAD),
    )
    .semantic(s.id, Role::RadioGroup, s.label);
    if let (Some(node), Some(text)) = (group.semantic.as_mut(), s.description) {
        node.set_description(text);
    }
    let row = b.row_in(card, group);
    label(b, row, s.label);
    let track = b.add(
        row,
        Node::new(
            Content::Custom(Box::new(Track)),
            style().row().center().pad(2.0, 2.0, 2.0, 2.0),
        ),
    );
    for choice in s.choices {
        let on = choice.id == s.chosen;
        let mut node = Node::new(
            Content::Custom(Box::new(Segment { on })),
            style()
                .row()
                .center()
                .height(28.0)
                .pad(0.0, 14.0, 0.0, 14.0),
        )
        .semantic(choice.id, Role::RadioButton, choice.label);
        if let Some(n) = node.semantic.as_mut() {
            n.set_toggled(if on { Toggled::True } else { Toggled::False });
            n.add_action(Action::Focus);
            n.add_action(Action::ScrollIntoView);
            if let Some(tag) = choice.lang {
                n.set_language(tag);
            }
        }
        let segment = b.add(track, node);
        b.add(
            segment,
            Node::new(
                Content::Text {
                    text: choice.label.into(),
                    size: TEXT_SIZE,
                    ink: if on { Ink::Text } else { Ink::Dim },
                },
                style(),
            ),
        );
    }
    if let Some(text) = s.description {
        note(b, card, text);
    }
}
