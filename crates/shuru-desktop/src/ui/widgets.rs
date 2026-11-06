//! What a node of the shell UI shows: its natural size and its paint.

use super::painter;
use super::text::Field;
use crate::chrome::app_icon;
use crate::draw::{theme, Canvas, Font, Image, Rect};
use crate::glyphs::Glyph;

pub const TEXT_SIZE: f32 = 13.0;
pub const SMALL_SIZE: f32 = 12.0;
pub const LABEL_SIZE: f32 = 11.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ink {
    Text,
    Dim,
}

impl Ink {
    fn color(self) -> u32 {
        match self {
            Ink::Text => theme().text,
            Ink::Dim => theme().dim,
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

pub enum Content {
    RaisedChoice,
    Switch {
        on: bool,
    },
    TitleSurface {
        ring: u32,
    },
    WindowLight {
        light: crate::chrome::Light,
        lit: bool,
        hover: bool,
        pressed: bool,
        maximized: bool,
    },
    CenteredText {
        text: String,
        size: f32,
        ink: Ink,
    },
    PanelControl {
        name: Option<String>,
        hover: bool,
    },
    Agent,
    RailTile {
        name: String,
        entry: usize,
        windows: usize,
        front: bool,
        attention: bool,
        hover: bool,
    },
    /// Layout only.
    Box,
    /// A row the pointer and the keyboard select.
    Choice,
    /// The sunken well around a text field.
    Well,
    Text {
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
}

/// What painting needs besides the canvas and the node.
pub struct Paint<'a> {
    pub font: &'a mut Font,
    pub icons: &'a dyn Fn(usize) -> Option<&'a Image>,
    pub field: Option<&'a mut Field>,
}

impl Content {
    /// The natural size of a leaf, for layout.
    pub fn measure(&self, font: &mut Font) -> (f32, f32) {
        match self {
            Content::Switch { .. } => (32.0, 18.0),
            Content::RaisedChoice => (0.0, 0.0),
            Content::TitleSurface { .. }
            | Content::WindowLight { .. }
            | Content::CenteredText { .. } => (0.0, 0.0),
            Content::PanelControl { .. } | Content::Agent | Content::RailTile { .. } => (0.0, 0.0),
            Content::Text { text, size, .. } => (font.width(text, *size), *size),
            Content::Glyph(g, _) => (g.w as f32, g.h as f32),
            Content::AppIcon { .. } => (16.0, 16.0),
            Content::Dot => (5.0, 5.0),
            Content::Key(Some(_), _) => (18.0, 18.0),
            Content::Key(None, t) => ((8.0 + font.width(t, LABEL_SIZE)).round(), 18.0),
            Content::Field { .. } => (0.0, 18.0),
            Content::Box | Content::Choice | Content::Well | Content::Rule => (0.0, 0.0),
        }
    }

    pub fn paint(&self, c: &mut Canvas, p: &mut Paint, b: Rect, mark: Mark) {
        let mid = b.y + b.h / 2.0;
        match self {
            Content::RaisedChoice => {
                let fill = match mark {
                    Mark::Rest => theme().raised,
                    Mark::Hover => theme().hover,
                    Mark::Selected => theme().selected,
                };
                painter::raised(
                    c,
                    b,
                    6.0,
                    crate::draw::Raised {
                        fill,
                        ring: theme().hairline_hi,
                        hi: fill,
                        shadow: theme().light_drop,
                    },
                );
            }
            Content::Switch { on } => {
                painter::rounded(c, b, 9.0, if *on { theme().text } else { theme().dim });
                let x = b.x + if *on { b.w - 16.0 } else { 2.0 };
                painter::rounded(c, Rect::new(x, b.y + 2.0, 14.0, 14.0), 7.0, theme().surface);
            }
            Content::TitleSurface { ring } => {
                let card = Rect::new(b.x + 1.0, b.y + 1.0, b.w - 2.0, 4.0 * crate::chrome::RADIUS);
                c.rounded_rect(card.inset(-1.0), crate::chrome::RADIUS + 1.0, *ring);
                c.rounded_rect(card, crate::chrome::RADIUS, theme().winbar);
                for x in [b.x as usize, (b.x + b.w - 1.0) as usize] {
                    c.fill_rect(
                        x,
                        (b.y + crate::chrome::RADIUS) as usize,
                        1,
                        (b.h - crate::chrome::RADIUS) as usize,
                        *ring,
                    );
                }
            }
            Content::WindowLight {
                light,
                lit,
                hover,
                pressed,
                maximized,
            } => {
                use crate::chrome::Light;
                use crate::{draw, glyphs};
                let lamp = match light {
                    Light::Close => draw::CLOSE,
                    Light::Minimize => draw::MINIMIZE,
                    Light::Maximize => draw::MAXIMIZE,
                };
                let style = if *pressed {
                    draw::Raised::pressed(lamp.ring, lamp.ring)
                } else if *lit {
                    lamp.raised(theme().light_drop)
                } else {
                    theme().light_idle
                };
                c.raised(b, b.w / 2.0, style);
                if *hover {
                    let g = match light {
                        Light::Close => &glyphs::LIGHT_CLOSE,
                        Light::Minimize => &glyphs::LIGHT_MIN,
                        Light::Maximize if *maximized => &glyphs::LIGHT_RESTORE,
                        Light::Maximize => &glyphs::LIGHT_MAX,
                    };
                    c.glyph(g, b.x as i32, b.y as i32, lamp.glyph, 1.0);
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
            Content::PanelControl { name, hover } => {
                if *hover {
                    c.rounded_rect(b, 6.0, theme().hover);
                    c.ring_in(b, 6.0, theme().hairline);
                }
                match name {
                    Some(name) => crate::chrome::app_label(c, p.font, b.x, b.y, name, (p.icons)(0)),
                    None => crate::chrome::apps_label(c, p.font, b.x, b.y),
                }
            }
            Content::Agent => {
                c.rounded_rect(Rect::new(b.x, mid - 4.0, 8.0, 8.0), 4.0, theme().green);
                p.font.draw(
                    c,
                    crate::locale::tr(crate::locale::Msg::Agent),
                    b.x + 16.0,
                    mid,
                    TEXT_SIZE,
                    theme().text,
                );
            }
            Content::RailTile {
                name,
                entry,
                windows,
                front,
                attention,
                hover,
            } => {
                if *front {
                    c.rounded_rect(b, 12.0, theme().rail_active);
                } else if *hover {
                    c.rounded_rect(b, 12.0, theme().hover);
                }
                crate::chrome::rail_icon(
                    c,
                    p.font,
                    &crate::chrome::RailItem {
                        name,
                        icon: (p.icons)(*entry),
                        windows: *windows,
                        front: *front,
                        attention: *attention,
                    },
                    b.x,
                    b.y,
                );
            }
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
        }
    }
}
