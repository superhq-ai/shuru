//! The surfaces rows sit on: the card of a section, the cell of a row,
//! and the hairline between rows.

use super::super::painter;
use super::super::widgets::{Content, Draw, Mark, Paint};
use super::super::{style, Builder, Node};
use super::ROW_PAD;
use crate::draw::{theme, Canvas, Rect};

pub struct Card;

impl Draw for Card {
    fn paint(&self, c: &mut Canvas, _: &mut Paint, b: Rect, _: Mark) {
        let k = &theme().controls;
        painter::rounded(c, b, 10.0, k.card);
        painter::ring(c, b, 10.0, k.card_ring);
    }
}

/// A row that lights under the pointer and the keyboard.
pub struct Cell;

impl Draw for Cell {
    fn paint(&self, c: &mut Canvas, _: &mut Paint, b: Rect, mark: Mark) {
        if mark != Mark::Rest {
            painter::rounded(c, b, 6.0, theme().controls.hover);
        }
    }
}

pub fn cell() -> Content {
    Content::Custom(Box::new(Cell))
}

pub fn divider(b: &mut Builder, card: usize) {
    b.add(
        card,
        Node::new(
            Content::Rule,
            style().height(1.0).margin(0.0, ROW_PAD, 0.0, ROW_PAD),
        ),
    );
}
