//! A select: a row showing the chosen option that opens the list of
//! options under it, in the card, rather than in a popup. The list
//! scrolls with the form and cannot fall off the screen.

use accesskit::{Action, HasPopup, NodeId, Role};

use super::super::painter;
use super::super::widgets::{Content, Draw, Ink, Mark, Paint, TEXT_SIZE};
use super::super::{style, Builder, Node};
use super::surface::cell;
use super::{label, note, Choice, ROW_H, ROW_PAD};
use crate::draw::{theme, Canvas, Rect};

pub struct Select<'a> {
    pub id: u64,
    /// The id of the list of options while it is open.
    pub list: u64,
    pub label: &'a str,
    pub choices: &'a [Choice<'a>],
    pub chosen: Option<u64>,
    pub open: bool,
    pub description: Option<&'a str>,
}

struct Chevron {
    open: bool,
}

impl Draw for Chevron {
    fn paint(&self, c: &mut Canvas, _: &mut Paint, b: Rect, _: Mark) {
        let (x, y, w) = (b.x + 2.0, b.y + b.h / 2.0, 8.0);
        let d = if self.open { -2.0 } else { 2.0 };
        painter::stroke(
            c,
            &[(x, y - d), (x + w / 2.0, y + d), (x + w, y - d)],
            1.6,
            theme().dim,
        );
    }
}

struct Check;

impl Draw for Check {
    fn paint(&self, c: &mut Canvas, _: &mut Paint, b: Rect, _: Mark) {
        let (x, y) = (b.x + 1.0, b.y + b.h / 2.0);
        painter::stroke(
            c,
            &[(x, y), (x + 3.5, y + 3.5), (x + 10.0, y - 3.5)],
            1.8,
            theme().text,
        );
    }
}

fn mark(draw: impl Draw + 'static) -> Node {
    Node::new(
        Content::Custom(Box::new(draw)),
        style().width(12.0).height(12.0),
    )
}

fn actions(node: &mut Node, lang: Option<&str>) {
    if let Some(n) = node.semantic.as_mut() {
        n.add_action(Action::Focus);
        n.add_action(Action::ScrollIntoView);
        if let Some(tag) = lang {
            n.set_language(tag);
        }
    }
}

pub(super) fn add(b: &mut Builder, card: usize, s: Select) {
    let chosen = s.choices.iter().find(|c| Some(c.id) == s.chosen);
    let mut node = Node::new(
        cell(),
        style()
            .row()
            .center()
            .height(ROW_H)
            .pad(0.0, ROW_PAD, 0.0, ROW_PAD),
    )
    .semantic(s.id, Role::ComboBox, s.label);
    actions(&mut node, None);
    if let Some(n) = node.semantic.as_mut() {
        n.set_expanded(s.open);
        n.set_has_popup(HasPopup::Listbox);
        if let Some(c) = chosen {
            n.set_value(c.label);
        }
        if s.choices.is_empty() {
            n.set_disabled();
        }
        if s.open {
            n.set_controls(vec![NodeId(s.list)]);
        }
        if let Some(text) = s.description {
            n.set_description(text);
        }
    }
    let row = b.row_in(card, node);
    label(b, row, s.label);
    if let Some(c) = chosen {
        b.add(
            row,
            Node::new(
                Content::Text {
                    text: c.label.into(),
                    size: TEXT_SIZE,
                    ink: Ink::Dim,
                },
                style().max_width(260.0).margin(0.0, 10.0, 0.0, 8.0),
            ),
        );
    }
    b.add(row, mark(Chevron { open: s.open }));
    if s.open {
        let list = b.add(
            card,
            Node::new(Content::Box, style().column().pad(0.0, 0.0, 4.0, 12.0)).semantic(
                s.list,
                Role::ListBox,
                s.label,
            ),
        );
        for choice in s.choices {
            let on = Some(choice.id) == s.chosen;
            let mut option = Node::new(
                cell(),
                style()
                    .row()
                    .center()
                    .height(36.0)
                    .pad(0.0, ROW_PAD, 0.0, ROW_PAD),
            )
            .semantic(choice.id, Role::ListBoxOption, choice.label);
            actions(&mut option, choice.lang);
            if let Some(n) = option.semantic.as_mut() {
                n.set_selected(on);
            }
            let option = b.row_in(list, option);
            label(b, option, choice.label);
            if on {
                b.add(option, mark(Check));
            }
        }
    }
    if let Some(text) = s.description {
        note(b, card, text);
    }
}
