//! Popup rows using retained layout bounds and the shared shell assets.

use crate::chrome::app_icon;
use crate::draw::{theme, Canvas, Font, Image, Rect};
use crate::glyphs::{self, Glyph};
use crate::locale::{tr, Msg};

const TEXT_SIZE: f32 = 13.0;
const SMALL_SIZE: f32 = 12.0;
const LABEL_SIZE: f32 = 11.0;

fn dot(c: &mut Canvas, x: f32, y: f32, r: f32, color: u32) {
    super::painter::rounded(c, Rect::new(x, y - r / 2.0, r, r), r / 2.0, color);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowState {
    Rest,
    Hover,
    Selected,
}

pub enum Row<'a> {
    Search {
        query: &'a str,
        placeholder: &'a str,
    },
    Section(&'a str),
    /// An app, a window or a URL: 26 px with a 16 px icon.
    Item {
        name: &'a str,
        icon: Option<&'a Image>,
        icon_name: &'a str,
        detail: Option<&'a str>,
        current: bool,
        minimized: bool,
        state: RowState,
    },
    /// 24 px, text only.
    Action {
        name: &'a str,
        hint: Option<&'a str>,
        state: RowState,
    },
    Sep,
    Footer,
}

pub const SHEET_PAD: f32 = 5.0;

impl Row<'_> {
    pub fn height(&self) -> f32 {
        match self {
            Row::Search { .. } => 30.0,
            Row::Section(_) => 22.0,
            Row::Item { .. } => 26.0,
            Row::Action { .. } => 24.0,
            Row::Sep => 9.0,
            Row::Footer => 5.0 + 29.0 - SHEET_PAD,
        }
    }
}

/// The height of a sheet holding `rows`.
#[cfg(test)]
pub fn sheet_height(rows: &[Row]) -> f32 {
    2.0 * SHEET_PAD + rows.iter().map(Row::height).sum::<f32>()
}

/// Draws the rows into the sheet `s` of a canvas.
pub fn sheet(c: &mut Canvas, font: &mut Font, s: Rect, rows: &[Row], bounds: &[Rect]) {
    for (row, bounds) in rows.iter().zip(bounds) {
        let x = bounds.x;
        let w = bounds.w;
        let y = bounds.y;
        let h = bounds.h;
        match row {
            Row::Search { query, placeholder } => {
                let b = Rect::new(x, y, w, 28.0);
                super::painter::rounded(c, b, 6.0, theme().bg);
                super::painter::ring(c, b, 6.0, theme().hairline_hi);
                c.glyph(
                    &glyphs::SEARCH,
                    (x + 8.0) as i32,
                    (y + 6.0) as i32,
                    theme().dim,
                    1.0,
                );
                if query.is_empty() {
                    font.draw(c, placeholder, x + 34.0, y + 14.0, TEXT_SIZE, theme().dim);
                }
            }
            Row::Section(label) => {
                font.draw(c, label, x + 8.0, y + 12.0, LABEL_SIZE, theme().dim);
            }
            Row::Item {
                name,
                icon,
                icon_name,
                detail,
                current,
                minimized,
                state,
            } => {
                row_fill(c, Rect::new(x, y, w, h), *state);
                let mid = y + h / 2.0;
                let opacity = if *minimized { 0.5 } else { 1.0 };
                app_icon(c, font, *icon, icon_name, x + 8.0, y + 5.0, 16, opacity);
                let mut right = x + w - 8.0;
                if *current {
                    dot(c, right - 7.0, mid, 5.0, theme().text);
                    right -= 15.0;
                }
                let detail = if *minimized {
                    Some(tr(Msg::Minimized))
                } else {
                    *detail
                };
                if let Some(d) = detail {
                    let dw = font.width(d, SMALL_SIZE);
                    font.draw(c, d, (right - dw).round(), mid, SMALL_SIZE, theme().dim);
                    right -= dw + 8.0;
                }
                let color = if *minimized {
                    theme().dim
                } else {
                    theme().text
                };
                let n = font.fit(name, TEXT_SIZE, right - (x + 32.0));
                font.draw(c, &n, x + 32.0, mid, TEXT_SIZE, color);
            }
            Row::Action { name, hint, state } => {
                row_fill(c, Rect::new(x, y, w, h), *state);
                let mid = y + h / 2.0;
                let mut right = x + w - 8.0;
                if let Some(k) = hint {
                    let kw = font.width(k, SMALL_SIZE);
                    font.draw(c, k, (right - kw).round(), mid, SMALL_SIZE, theme().dim);
                    right -= kw + 8.0;
                }
                let n = font.fit(name, TEXT_SIZE, right - (x + 8.0));
                font.draw(c, &n, x + 8.0, mid, TEXT_SIZE, theme().text);
            }
            Row::Sep => {
                c.fill_rect(
                    (x + 3.0) as usize,
                    (y + 4.0) as usize,
                    (w - 6.0) as usize,
                    1,
                    theme().hairline,
                );
            }
            Row::Footer => footer(c, font, s, y + 5.0),
        }
    }
}

fn row_fill(c: &mut Canvas, b: Rect, state: RowState) {
    match state {
        RowState::Rest => {}
        RowState::Hover => super::painter::rounded(c, b, 5.0, theme().hover),
        RowState::Selected => super::painter::rounded(c, b, 5.0, theme().selected),
    }
}

/// The key hints under the Applications menu, from `y`.
fn footer(c: &mut Canvas, font: &mut Font, s: Rect, y: f32) {
    c.fill_rect(
        s.x as usize + 1,
        y as usize,
        s.w as usize - 2,
        1,
        theme().hairline,
    );
    let mid = y + 1.0 + 14.0;
    let key_y = mid - 9.0;
    let mut x = s.x + 13.0;
    let key = |c: &mut Canvas, font: &mut Font, x: &mut f32, g: Option<&Glyph>, t: &str| {
        let w = match g {
            Some(_) => 18.0,
            None => (8.0 + font.width(t, LABEL_SIZE)).round(),
        };
        super::painter::raised(c, Rect::new(*x, key_y, w, 18.0), 4.0, theme().keycap);
        match g {
            Some(g) => c.glyph(
                g,
                (*x + 1.0) as i32,
                (key_y + 1.0) as i32,
                theme().text,
                1.0,
            ),
            None => {
                font.draw(c, t, *x + 4.0, key_y + 9.0, LABEL_SIZE, theme().text);
            }
        }
        *x += w + 5.0;
    };
    let label = |c: &mut Canvas, font: &mut Font, x: &mut f32, t: &str, last: bool| {
        *x += font.draw(c, t, *x, mid, SMALL_SIZE, theme().dim);
        if !last {
            *x += 5.0 + 8.0 + 5.0;
        }
    };
    key(c, font, &mut x, Some(&glyphs::UP), "");
    key(c, font, &mut x, Some(&glyphs::DOWN), "");
    label(c, font, &mut x, tr(Msg::Move), false);
    key(c, font, &mut x, Some(&glyphs::ENTER), "");
    label(c, font, &mut x, tr(Msg::Open), false);
    key(c, font, &mut x, None, "Esc");
    label(c, font, &mut x, tr(Msg::Close), true);
}
