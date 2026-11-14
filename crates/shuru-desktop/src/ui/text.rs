//! A single line editor with a steady caret and horizontal scrolling.

use crate::draw::{theme, Canvas, Font, Rect};
use cosmic_text::{
    Action, Attrs, Buffer, Color, Cursor, Edit, Editor, Metrics, Motion, Selection, Shaping, Wrap,
};

pub struct Field {
    pub editor: Editor<'static>,
    scroll: f32,
    pub dragging: bool,
    pub paste_pending: bool,
    pub paste_chunks: Option<Vec<u8>>,
}

impl Field {
    pub fn new(font: &mut Font) -> Self {
        let mut buffer = Buffer::new(&mut font.fonts, Metrics::new(13.0, 18.0));
        buffer.set_wrap(Wrap::None);
        buffer.set_text(
            "",
            &Attrs::new()
                .family(cosmic_text::Family::SansSerif)
                .weight(cosmic_text::Weight::MEDIUM),
            Shaping::Advanced,
            None,
        );
        Self {
            editor: Editor::new(buffer),
            scroll: 0.0,
            dragging: false,
            paste_pending: false,
            paste_chunks: None,
        }
    }

    pub fn value(&self) -> String {
        self.editor.with_buffer(|b| {
            b.lines
                .iter()
                .map(|l| l.text())
                .collect::<Vec<_>>()
                .join("")
        })
    }

    pub fn selection_indices(&self) -> (usize, usize) {
        let value = self.value();
        let position = |byte: usize| value.get(..byte).unwrap_or(&value).chars().count();
        let focus = position(self.editor.cursor().index);
        let anchor = match self.editor.selection() {
            Selection::Normal(cursor) => position(cursor.index),
            _ => focus,
        };
        (anchor, focus)
    }

    pub fn insert(&mut self, text: &str) {
        self.editor
            .insert_string(&text.replace(['\n', '\r'], " "), None);
    }

    pub fn select_all(&mut self, font: &mut Font) {
        self.editor
            .set_selection(Selection::Normal(Cursor::new(0, 0)));
        self.editor
            .action(&mut font.fonts, Action::Motion(Motion::BufferEnd));
    }

    pub fn key(
        &mut self,
        font: &mut Font,
        base: u32,
        ch: Option<char>,
        shift: bool,
        ctrl: bool,
    ) -> bool {
        let before = (self.value(), self.editor.cursor(), self.editor.selection());
        let motion = match base {
            0xff51 if ctrl => Some(Motion::LeftWord),
            0xff53 if ctrl => Some(Motion::RightWord),
            0xff51 => Some(Motion::Left),
            0xff53 => Some(Motion::Right),
            0xff50 => Some(Motion::Home),
            0xff57 => Some(Motion::End),
            _ => None,
        };
        if ctrl && base == 0x61 {
            self.select_all(font);
        } else if let Some(motion) = motion {
            if shift {
                if self.editor.selection() == Selection::None {
                    self.editor
                        .set_selection(Selection::Normal(self.editor.cursor()));
                }
            } else {
                self.editor.set_selection(Selection::None);
            }
            self.editor.action(&mut font.fonts, Action::Motion(motion));
        } else if base == 0xff08 || base == 0xffff {
            if ctrl && self.editor.selection() == Selection::None {
                self.editor
                    .set_selection(Selection::Normal(self.editor.cursor()));
                self.editor.action(
                    &mut font.fonts,
                    Action::Motion(if base == 0xff08 {
                        Motion::PreviousWord
                    } else {
                        Motion::NextWord
                    }),
                );
            }
            self.editor.action(
                &mut font.fonts,
                if base == 0xff08 {
                    Action::Backspace
                } else {
                    Action::Delete
                },
            );
        } else if let Some(ch) = ch.filter(|_| !ctrl) {
            self.editor.action(&mut font.fonts, Action::Insert(ch));
        } else {
            return false;
        }
        before != (self.value(), self.editor.cursor(), self.editor.selection())
    }

    pub fn pointer(&mut self, font: &mut Font, x: f32, drag: bool) {
        let x = (x + self.scroll) as i32;
        self.editor.action(
            &mut font.fonts,
            if drag {
                Action::Drag { x, y: 8 }
            } else {
                Action::Click { x, y: 8 }
            },
        );
    }

    pub fn draw(&mut self, font: &mut Font, c: &mut Canvas, b: Rect) {
        self.editor.shape_as_needed(&mut font.fonts, false);
        let cursor = self.editor.cursor();
        let cx = self
            .editor
            .with_buffer(|buffer| {
                buffer
                    .layout_runs()
                    .find_map(|r| r.cursor_position(&cursor))
            })
            .unwrap_or(0.0);
        self.scroll = self.scroll.min(cx).max(cx - b.w + 2.0).max(0.0);
        let color = |rgb: u32| Color::rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
        self.editor.draw(
            &mut font.fonts,
            &mut font.swash,
            color(theme().text),
            color(theme().text),
            color(theme().selection),
            color(theme().text),
            |x, y, w, h, rgba| {
                // One origin on the pixel grid, rounded, so glyphs neither
                // lean left nor shift as the field scrolls.
                let (ox, oy) = ((b.x - self.scroll).round(), b.y.round());
                for dy in 0..h {
                    for dx in 0..w {
                        let x = ox + (x + dx as i32) as f32;
                        let y = oy + (y + dy as i32) as f32;
                        if x >= b.x && x < b.x + b.w && y >= b.y && y < b.y + b.h {
                            c.blend(
                                x as usize,
                                y as usize,
                                rgba.0 & 0xffffff,
                                rgba.a() as f32 / 255.0,
                            );
                        }
                    }
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_words_selection_and_deletion() {
        let mut font = Font::for_test();
        let mut field = Field::new(&mut font);
        field.insert("hello world");
        field.key(&mut font, 0xff50, None, false, false);
        field.key(&mut font, 0xff53, None, false, true);
        assert_eq!(field.editor.cursor().index, 5);
        field.key(&mut font, 0xff57, None, false, false);
        field.key(&mut font, 0xff51, None, true, true);
        assert_eq!(field.editor.copy_selection().as_deref(), Some("world"));
        field.insert("desktop");
        assert_eq!(field.value(), "hello desktop");
        field.key(&mut font, 0xff08, None, false, true);
        assert_eq!(field.value(), "hello ");
        field.key(&mut font, 0xff50, None, false, false);
        field.key(&mut font, 0xffff, None, false, true);
        assert_eq!(field.value(), " ");
        field.key(&mut font, 0x61, None, false, true);
        field.insert("你好");
        assert_eq!(field.value(), "你好");
    }
}
