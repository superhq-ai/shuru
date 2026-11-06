//! Headings: the title of a form and the names of its sections.

use accesskit::Role;

use super::super::widgets::{Content, Ink};
use super::super::{style, Builder, Node};

/// A heading of `level` 1 (the title) or 2 (a section).
pub fn add(b: &mut Builder, parent: usize, id: u64, text: &str, level: usize) {
    let (size, margin) = if level == 1 {
        (22.0, (4.0, 12.0))
    } else {
        (14.0, (18.0, 8.0))
    };
    let mut node = Node::new(
        Content::Text {
            text: text.into(),
            size,
            ink: Ink::Text,
        },
        style()
            .height(size + 6.0)
            .margin(margin.0, 0.0, margin.1, 4.0),
    )
    .semantic(id, Role::Heading, text);
    if let Some(s) = node.semantic.as_mut() {
        s.set_level(level);
    }
    b.add(parent, node);
}
