//! A status line: what the form last did, as a live region.

use accesskit::{Live, Role};

use super::super::widgets::{Content, Ink};
use super::super::{style, Builder, Node};

/// Polite while things go well; an error is assertive, so a screen reader
/// says it at once.
pub fn add(b: &mut Builder, id: u64, text: &str, error: bool) {
    let mut node = Node::new(Content::Box, style().column().margin(0.0, 0.0, 4.0, 4.0)).semantic(
        id,
        Role::Status,
        text,
    );
    if let Some(s) = node.semantic.as_mut() {
        s.set_live(if error { Live::Assertive } else { Live::Polite });
    }
    let line = b.add(Builder::ROOT, node);
    b.add(
        line,
        Node::new(
            Content::Paragraph {
                text: text.into(),
                size: 12.0,
                ink: if error { Ink::Error } else { Ink::Dim },
            },
            style(),
        ),
    );
}
