//! What the panel, the rail and title bars draw in their trees.

use super::{app_label, apps_label, rail_icon, Bell, Light, RailItem, RADIUS};
use crate::draw::{self, theme, Canvas, Rect};
use crate::glyphs;
use crate::locale::{tr, Msg};
use crate::ui::painter;
use crate::ui::widgets::{Content, Draw, Mark, Paint, TEXT_SIZE};

fn custom(d: impl Draw + 'static) -> Content {
    Content::Custom(Box::new(d))
}

/// Applications, or the front app, on the panel.
pub struct PanelControl {
    pub name: Option<String>,
    pub hover: bool,
}

impl Draw for PanelControl {
    fn paint(&self, c: &mut Canvas, p: &mut Paint, b: Rect, _: Mark) {
        if self.hover {
            c.rounded_rect(b, 6.0, theme().hover);
            c.ring_in(b, 6.0, theme().hairline);
        }
        match &self.name {
            Some(name) => app_label(c, p.font, b.x, b.y, name, (p.icons)(0)),
            None => apps_label(c, p.font, b.x, b.y),
        }
    }
}

impl PanelControl {
    pub fn content(name: Option<String>, hover: bool) -> Content {
        custom(Self { name, hover })
    }
}

/// The notification control on the panel.
pub struct PanelBell {
    pub bell: Bell,
    pub hover: bool,
}

impl Draw for PanelBell {
    fn paint(&self, c: &mut Canvas, _: &mut Paint, b: Rect, _: Mark) {
        if self.hover {
            c.rounded_rect(b, 6.0, theme().hover);
            c.ring_in(b, 6.0, theme().hairline);
        }
        let ground = if self.hover {
            theme().hover
        } else {
            theme().bg
        };
        bell(c, b, self.bell, ground);
    }
}

impl PanelBell {
    pub fn content(bell: Bell, hover: bool) -> Content {
        custom(Self { bell, hover })
    }
}

/// A bell in the middle of `b`: struck through while do not disturb is
/// on, with an amber dot when a notification is unseen. `ground` is the
/// color under it, which parts the slash and the dot from the bell.
pub fn bell(c: &mut Canvas, b: Rect, bell: Bell, ground: u32) {
    let t = theme();
    let (x, y) = ((b.x + b.w / 2.0).round(), (b.y + b.h / 2.0).round());
    let outline = [
        (x - 5.5, y + 3.0),
        (x - 4.5, y + 1.5),
        (x - 4.5, y - 1.5),
        (x - 3.8, y - 3.9),
        (x - 2.0, y - 5.4),
        (x, y - 5.9),
        (x + 2.0, y - 5.4),
        (x + 3.8, y - 3.9),
        (x + 4.5, y - 1.5),
        (x + 4.5, y + 1.5),
        (x + 5.5, y + 3.0),
        (x - 5.5, y + 3.0),
    ];
    painter::stroke(c, &outline, 1.4, t.text);
    painter::stroke(c, &[(x - 1.6, y + 5.4), (x + 1.6, y + 5.4)], 1.4, t.text);
    if bell.quiet {
        let slash = [(x - 6.5, y - 6.5), (x + 6.5, y + 6.5)];
        painter::stroke(c, &slash, 3.6, ground);
        painter::stroke(c, &slash, 1.4, t.text);
    }
    if bell.unseen {
        let dot = Rect::new(x + 4.0, y - 9.0, 6.0, 6.0);
        painter::rounded(c, dot.inset(-1.0), 4.0, ground);
        painter::rounded(c, dot, 3.0, t.amber);
    }
}

/// The green dot and the word Agent while the agent has control.
pub struct Agent;

impl Draw for Agent {
    fn paint(&self, c: &mut Canvas, p: &mut Paint, b: Rect, _: Mark) {
        let mid = b.y + b.h / 2.0;
        c.rounded_rect(Rect::new(b.x, mid - 4.0, 8.0, 8.0), 4.0, theme().green);
        p.font
            .draw(c, tr(Msg::Agent), b.x + 16.0, mid, TEXT_SIZE, theme().text);
    }
}

impl Agent {
    pub fn content() -> Content {
        custom(Self)
    }
}

pub struct RailTile {
    pub name: String,
    pub entry: usize,
    pub windows: usize,
    pub front: bool,
    pub attention: bool,
    pub hover: bool,
}

impl Draw for RailTile {
    fn paint(&self, c: &mut Canvas, p: &mut Paint, b: Rect, _: Mark) {
        if self.front {
            c.rounded_rect(b, 12.0, theme().rail_active);
        } else if self.hover {
            c.rounded_rect(b, 12.0, theme().hover);
        }
        rail_icon(
            c,
            p.font,
            &RailItem {
                name: &self.name,
                icon: (p.icons)(self.entry),
                windows: self.windows,
                front: self.front,
                attention: self.attention,
            },
            b.x,
            b.y,
        );
    }
}

impl RailTile {
    pub fn content(self) -> Content {
        custom(self)
    }
}

/// The card of a title bar with the ring round it.
pub struct TitleSurface {
    pub ring: u32,
}

impl Draw for TitleSurface {
    fn paint(&self, c: &mut Canvas, _: &mut Paint, b: Rect, _: Mark) {
        let card = Rect::new(b.x + 1.0, b.y + 1.0, b.w - 2.0, 4.0 * RADIUS);
        c.rounded_rect(card.inset(-1.0), RADIUS + 1.0, self.ring);
        c.rounded_rect(card, RADIUS, theme().winbar);
        for x in [b.x as usize, (b.x + b.w - 1.0) as usize] {
            c.fill_rect(
                x,
                (b.y + RADIUS) as usize,
                1,
                (b.h - RADIUS) as usize,
                self.ring,
            );
        }
    }
}

impl TitleSurface {
    pub fn content(ring: u32) -> Content {
        custom(Self { ring })
    }
}

pub struct WindowLight {
    pub light: Light,
    pub lit: bool,
    pub hover: bool,
    pub pressed: bool,
    pub maximized: bool,
}

impl Draw for WindowLight {
    fn paint(&self, c: &mut Canvas, _: &mut Paint, b: Rect, _: Mark) {
        let lamp = match self.light {
            Light::Close => draw::CLOSE,
            Light::Minimize => draw::MINIMIZE,
            Light::Maximize => draw::MAXIMIZE,
        };
        let style = if self.pressed {
            draw::Raised::pressed(lamp.ring, lamp.ring)
        } else if self.lit {
            lamp.raised(theme().light_drop)
        } else {
            theme().light_idle
        };
        c.raised(b, b.w / 2.0, style);
        if self.hover {
            let g = match self.light {
                Light::Close => &glyphs::LIGHT_CLOSE,
                Light::Minimize => &glyphs::LIGHT_MIN,
                Light::Maximize if self.maximized => &glyphs::LIGHT_RESTORE,
                Light::Maximize => &glyphs::LIGHT_MAX,
            };
            c.glyph(g, b.x as i32, b.y as i32, lamp.glyph, 1.0);
        }
    }
}

impl WindowLight {
    pub fn content(self) -> Content {
        custom(self)
    }
}
