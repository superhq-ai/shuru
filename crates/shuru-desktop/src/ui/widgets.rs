//! What a node of the shell UI shows: its natural size and its paint.

use super::painter;
use super::text::Field;
use crate::chrome::app_icon;
use crate::draw::{theme, Canvas, Font, Image, Rect};
use crate::glyphs::Glyph;

pub const TEXT_SIZE: f32 = 13.0;
pub const SMALL_SIZE: f32 = 12.0;
pub const LABEL_SIZE: f32 = 11.0;
/// The line pitch of wrapped text, as a multiple of its size.
pub const LEADING: f32 = 1.45;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ink {
    Text,
    Dim,
    Error,
}

impl Ink {
    pub fn color(self) -> u32 {
        match self {
            Ink::Text => theme().text,
            Ink::Dim => theme().dim,
            Ink::Error => theme().controls.error,
        }
    }
}

/// How a choice shows the pointer and the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Rest,
    Hover,
    Selected,
}

/// Content a surface of the shell draws itself, such as a window light.
pub trait Draw: Send {
    fn paint(&self, c: &mut Canvas, p: &mut Paint, b: Rect, mark: Mark);
}

pub enum Content {
    /// Layout only.
    Box,
    /// A row the pointer and the keyboard select.
    Choice,
    /// The sunken well around a text field.
    Well,
    /// One line, cut with an ellipsis when it does not fit.
    Text {
        text: String,
        size: f32,
        ink: Ink,
    },
    /// Text wrapped to the width it is given.
    Paragraph {
        text: String,
        size: f32,
        ink: Ink,
    },
    CenteredText {
        text: String,
        size: f32,
        ink: Ink,
    },
    Glyph(&'static Glyph, Ink),
    AppIcon {
        entry: Option<usize>,
        name: String,
        opacity: f32,
    },
    /// The mark of the current window.
    Dot,
    /// A hairline filling the node.
    Rule,
    /// A key cap with a glyph or a short label.
    Key(Option<&'static Glyph>, &'static str),
    /// The text field, painted by its editor.
    Field {
        placeholder: String,
    },
    /// Drawn by its owner: a control of `ui::kit` or a surface of the shell.
    Custom(Box<dyn Draw>),
}

/// What painting needs besides the canvas and the node.
pub struct Paint<'a> {
    pub font: &'a mut Font,
    pub icons: &'a dyn Fn(usize) -> Option<&'a Image>,
    pub field: Option<&'a mut Field>,
}

impl Content {
    /// The natural size of a leaf, for layout, within `max` wide when the
    /// layout knows it.
    pub fn measure(&self, font: &mut Font, max: Option<f32>) -> (f32, f32) {
        match self {
            Content::Text { text, size, .. } => (font.width(text, *size), *size),
            Content::Paragraph { text, size, .. } => {
                let width = font.width(text, *size);
                match max {
                    Some(max) if width > max => {
                        let lines = font.wrap(text, *size, max, usize::MAX).len().max(1);
                        (max, lines as f32 * size * LEADING)
                    }
                    _ => (width, size * LEADING),
                }
            }
            Content::Glyph(g, _) => (g.w as f32, g.h as f32),
            Content::AppIcon { .. } => (16.0, 16.0),
            Content::Dot => (5.0, 5.0),
            Content::Key(Some(_), _) => (18.0, 18.0),
            Content::Key(None, t) => ((8.0 + font.width(t, LABEL_SIZE)).round(), 18.0),
            Content::Field { .. } => (0.0, 18.0),
            Content::Box
            | Content::Choice
            | Content::Well
            | Content::Rule
            | Content::CenteredText { .. }
            | Content::Custom(_) => (0.0, 0.0),
        }
    }

    pub fn paint(&self, c: &mut Canvas, p: &mut Paint, b: Rect, mark: Mark) {
        let mid = b.y + b.h / 2.0;
        match self {
            Content::Box => {}
            Content::Choice => match mark {
                Mark::Rest => {}
                Mark::Hover => painter::rounded(c, b, 5.0, theme().hover),
                Mark::Selected => painter::rounded(c, b, 5.0, theme().selected),
            },
            Content::Well => {
                painter::rounded(c, b, 6.0, theme().bg);
                painter::ring(c, b, 6.0, theme().hairline_hi);
            }
            Content::Text { text, size, ink } => {
                let t = p.font.fit(text, *size, b.w + 0.5);
                p.font.draw(c, &t, b.x, mid, *size, ink.color());
            }
            Content::Paragraph { text, size, ink } => {
                let pitch = size * LEADING;
                for (i, line) in p
                    .font
                    .wrap(text, *size, b.w + 0.5, usize::MAX)
                    .iter()
                    .enumerate()
                {
                    let y = b.y + pitch * (i as f32 + 0.5);
                    p.font.draw(c, line, b.x, y, *size, ink.color());
                }
            }
            Content::CenteredText { text, size, ink } => {
                let text = p.font.fit(text, *size, b.w);
                let width = p.font.width(&text, *size);
                p.font.draw(
                    c,
                    &text,
                    (b.x + (b.w - width) / 2.0).round(),
                    mid,
                    *size,
                    ink.color(),
                );
            }
            Content::Glyph(g, ink) => c.glyph(g, b.x as i32, b.y as i32, ink.color(), 1.0),
            Content::AppIcon {
                entry,
                name,
                opacity,
            } => {
                let icon = entry.and_then(|e| (p.icons)(e));
                app_icon(c, p.font, icon, name, b.x, b.y, 16, *opacity);
            }
            Content::Dot => painter::rounded(c, b, b.w / 2.0, theme().text),
            Content::Rule => c.fill_rect(
                b.x as usize,
                b.y as usize,
                b.w as usize,
                b.h.max(1.0) as usize,
                theme().hairline,
            ),
            Content::Key(glyph, label) => {
                painter::raised(c, b, 4.0, theme().keycap);
                match glyph {
                    Some(g) => {
                        c.glyph(g, (b.x + 1.0) as i32, (b.y + 1.0) as i32, theme().text, 1.0)
                    }
                    None => {
                        p.font
                            .draw(c, label, b.x + 4.0, mid, LABEL_SIZE, theme().text);
                    }
                }
            }
            Content::Field { placeholder } => {
                if let Some(field) = p.field.as_deref_mut() {
                    if field.value().is_empty() {
                        p.font
                            .draw(c, placeholder, b.x + 2.0, mid, TEXT_SIZE, theme().dim);
                    }
                    field.draw(p.font, c, b);
                }
            }
            Content::Custom(d) => d.paint(c, p, b, mark),
        }
    }
}
