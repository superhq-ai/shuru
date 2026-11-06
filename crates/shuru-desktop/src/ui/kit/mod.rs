//! Form controls on the retained tree, for Settings and other windows of
//! the desktop. Each component lays out, paints and describes itself in
//! one place, so what a screen reader or an agent reads is what is drawn.
//!
//! The accessibility contract every component keeps:
//!
//! - A control has a role, a name, and its state: checked radios and
//!   switches carry `Toggled`, an open select `expanded`, the chosen option `selected`, a
//!   control that cannot act `disabled`.
//! - The keyboard reaches every control: Tab and Shift+Tab walk the rows
//!   in reading order, arrows move and check within a radio group, Enter
//!   and Space activate. The focused control shows a 3:1 focus ring.
//! - Text and controls meet WCAG AA on their surfaces in both appearances
//!   (see the contrast tests in `draw.rs`).
//! - Help text under a control is also its accessible description, and a
//!   section names the group of its rows.
//! - A form taller than its window scrolls with a visible scrollbar that
//!   the pointer drags and assistive tools read and set.
//! - Text in another language carries its language tag.

mod heading;
pub mod info;
mod segmented;
mod select;
mod status;
mod surface;
pub mod switch;

#[cfg(test)]
mod tests;

use accesskit::{NodeId, Role};

use super::widgets::{Content, Ink, TEXT_SIZE};
use super::{style, Builder, Node, Tree};
use crate::draw::Font;

pub use segmented::Segmented;
pub use select::Select;
pub use switch::Switch;

/// The height of a row in a section.
pub const ROW_H: f32 = 44.0;
/// Of a row in a section, the room left and right of its content.
pub const ROW_PAD: f32 = 12.0;

/// An option of a radio group or a select.
#[derive(Clone, Copy, Debug)]
pub struct Choice<'a> {
    pub id: u64,
    pub label: &'a str,
    /// The BCP 47 tag of the label when it is not in the language of the
    /// form, such as the name of a language in itself.
    pub lang: Option<&'a str>,
}

impl<'a> Choice<'a> {
    pub fn new(id: u64, label: &'a str) -> Self {
        Self {
            id,
            label,
            lang: None,
        }
    }

    pub fn in_lang(mut self, tag: &'a str) -> Self {
        self.lang = Some(tag);
        self
    }
}

/// A window of sections built top to bottom.
pub struct Form {
    b: Builder,
}

/// A section: its card, and whether a row went in it yet.
pub struct Section {
    card: usize,
    rows: usize,
}

impl Form {
    /// A form `width` wide, in the language `lang` (a BCP 47 tag).
    pub fn new(id: u64, name: &str, lang: &str, width: f32) -> Self {
        let mut root = Node::new(
            Content::Box,
            style().column().width(width).pad(20.0, 24.0, 28.0, 24.0),
        )
        .semantic(id, Role::Window, name);
        if let Some(s) = root.semantic.as_mut() {
            s.set_language(lang);
        }
        let mut b = Builder::new(root);
        b.form();
        Self { b }
    }

    /// The title at the top of the form, a level 1 heading.
    pub fn title(&mut self, id: u64, text: &str) {
        heading::add(&mut self.b, Builder::ROOT, id, text, 1);
    }

    /// A line that tells about the last thing the form did, read out by
    /// screen readers when it changes; an error interrupts them.
    pub fn status(&mut self, id: u64, text: &str, error: bool) {
        status::add(&mut self.b, id, text, error);
    }

    /// A section under a level 2 heading. Its rows sit on one card, which
    /// screen readers announce as a group named by the heading.
    pub fn section(&mut self, heading: u64, card: u64, title: &str) -> Section {
        heading::add(&mut self.b, Builder::ROOT, heading, title, 2);
        let mut node = Node::new(
            Content::Custom(Box::new(surface::Card)),
            style().column().pad(4.0, 4.0, 4.0, 4.0),
        )
        .semantic(card, Role::Group, title);
        if let Some(s) = node.semantic.as_mut() {
            s.set_labelled_by(vec![NodeId(heading)]);
        }
        Section {
            card: self.b.add(Builder::ROOT, node),
            rows: 0,
        }
    }

    /// A row of radio buttons: one of a few choices, all in view.
    pub fn segmented(&mut self, section: &mut Section, control: Segmented) {
        self.divide(section);
        segmented::add(&mut self.b, section.card, control);
    }

    /// A row that opens a list of options under it.
    pub fn select(&mut self, section: &mut Section, control: Select) {
        self.divide(section);
        select::add(&mut self.b, section.card, control);
    }

    /// A row that turns one setting on or off.
    pub fn switch(&mut self, section: &mut Section, control: Switch) {
        self.divide(section);
        switch::add(&mut self.b, section.card, control);
    }

    /// A row with a label and a value to read, not to change.
    pub fn info(&mut self, section: &mut Section, id: u64, label: &str, value: &str) {
        self.divide(section);
        info::add(&mut self.b, section.card, id, label, value);
    }

    pub fn finish(self, font: &mut Font) -> Tree {
        self.b.finish(font)
    }

    /// A hairline between the rows of a card.
    fn divide(&mut self, section: &mut Section) {
        if section.rows > 0 {
            surface::divider(&mut self.b, section.card);
        }
        section.rows += 1;
    }
}

/// The label at the start of a row, taking the room left.
fn label(b: &mut Builder, row: usize, text: &str) {
    b.add(
        row,
        Node::new(
            Content::Text {
                text: text.into(),
                size: TEXT_SIZE,
                ink: Ink::Text,
            },
            style().grow(),
        ),
    );
}

/// Help text under a control, inside its card.
fn note(b: &mut Builder, card: usize, text: &str) {
    let place = b.add(
        card,
        Node::new(
            Content::Box,
            style().column().pad(0.0, ROW_PAD, 10.0, ROW_PAD),
        ),
    );
    b.add(
        place,
        Node::new(
            Content::Paragraph {
                text: text.into(),
                size: 12.0,
                ink: Ink::Dim,
            },
            style(),
        ),
    );
}
