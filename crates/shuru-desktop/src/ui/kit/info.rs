//! A row to read: a label and its value, such as a version. Screen
//! readers meet a group named by the label holding the value as text.

use accesskit::Role;

use super::super::widgets::{Content, Ink, TEXT_SIZE};
use super::super::{style, Builder, Node};
use super::{label, ROW_H, ROW_PAD};

/// The id of the value of an info row.
pub fn value_id(id: u64) -> u64 {
    id | 1 << 61
}

pub fn add(b: &mut Builder, card: usize, id: u64, name: &str, value: &str) {
    let row = b.add(
        card,
        Node::new(
            Content::Box,
            style()
                .row()
                .center()
                .height(ROW_H)
                .pad(0.0, ROW_PAD, 0.0, ROW_PAD),
        )
        .semantic(id, Role::Group, name),
    );
    label(b, row, name);
    b.add(
        row,
        Node::new(
            Content::Text {
                text: value.into(),
                size: TEXT_SIZE,
                ink: Ink::Dim,
            },
            style().max_width(320.0),
        )
        .semantic(value_id(id), Role::Label, value),
    );
}
