//! The pointer, sent beside the video: its image by XFixes when the shape
//! changes, its position polled at the frame rate. Viewers draw it, so
//! moving it costs no encode, and the X server is told to hide it.

use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use base64::Engine as _;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::hub::{pointer_message, wait_any, Hub};

/// A cursor as XFixes gives it: premultiplied ARGB, one u32 a pixel.
#[derive(Clone)]
pub struct CursorImage {
    pub width: usize,
    pub height: usize,
    pub hot_x: i32,
    pub hot_y: i32,
    /// Where the pointer was when the image was taken.
    pub x: i32,
    pub y: i32,
    pub serial: u32,
    pub argb: Vec<u32>,
}

impl CursorImage {
    pub fn fetch(conn: &RustConnection) -> Result<Self> {
        let r = conn.xfixes_get_cursor_image()?.reply()?;
        let (width, height) = (r.width as usize, r.height as usize);
        let mut argb = r.cursor_image;
        argb.resize(width * height, 0);
        Ok(CursorImage {
            width,
            height,
            hot_x: r.xhot as i32,
            hot_y: r.yhot as i32,
            x: r.x as i32,
            y: r.y as i32,
            serial: r.cursor_serial,
            argb,
        })
    }

    /// Straight RGBA, as a PNG holds it.
    pub fn rgba(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.argb.len() * 4);
        for &p in &self.argb {
            let a = p >> 24;
            let un = |c: u32| (c * 255 + a / 2).checked_div(a).unwrap_or(0).min(255) as u8;
            out.extend_from_slice(&[
                un((p >> 16) & 0xff),
                un((p >> 8) & 0xff),
                un(p & 0xff),
                a as u8,
            ]);
        }
        out
    }

    pub fn png(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(
            &mut out,
            self.width.max(1) as u32,
            self.height.max(1) as u32,
        );
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut rgba = self.rgba();
        rgba.resize(self.width.max(1) * self.height.max(1) * 4, 0);
        enc.write_header()?.write_image_data(&rgba)?;
        Ok(out)
    }

    pub fn message(&self) -> Result<String> {
        Ok(serde_json::json!({
            "type": "cursor",
            "image": base64::engine::general_purpose::STANDARD.encode(self.png()?),
            "hot_x": self.hot_x,
            "hot_y": self.hot_y,
            "serial": self.serial,
        })
        .to_string())
    }

    /// Draws the cursor over packed BGRX at its position.
    pub fn composite(&self, bgrx: &mut [u8], width: usize, height: usize) {
        let (left, top) = (self.x - self.hot_x, self.y - self.hot_y);
        for cy in 0..self.height {
            let y = top + cy as i32;
            if y < 0 || y >= height as i32 {
                continue;
            }
            for cx in 0..self.width {
                let x = left + cx as i32;
                if x < 0 || x >= width as i32 {
                    continue;
                }
                let p = self.argb[cy * self.width + cx];
                let a = p >> 24;
                if a == 0 {
                    continue;
                }
                let d = &mut bgrx[(y as usize * width + x as usize) * 4..][..3];
                let src = [p & 0xff, (p >> 8) & 0xff, (p >> 16) & 0xff];
                for (dc, sc) in d.iter_mut().zip(src) {
                    *dc = (sc + (*dc as u32 * (255 - a) + 127) / 255).min(255) as u8;
                }
            }
        }
    }
}

/// Starts the cursor thread on a connection of its own.
pub fn spawn(display: &str, hub: Arc<Hub>, fps: u32) -> Result<()> {
    let (conn, n) =
        RustConnection::connect(Some(display)).map_err(|e| anyhow!("X display {display}: {e}"))?;
    let root = conn.setup().roots[n].root;
    conn.xfixes_query_version(5, 0)?.reply()?;
    conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
    // Viewers draw the cursor. Hidden for as long as this connection lives,
    // so it is not in the frames and moving it changes no pixel.
    conn.xfixes_hide_cursor(root)?;
    conn.flush()?;
    let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    std::thread::Builder::new()
        .name("cursor".into())
        .spawn(move || {
            if let Err(e) = cursor_loop(&conn, root, &hub, interval) {
                eprintln!("shuru-screen: cursor stopped: {e:#}");
            }
        })?;
    Ok(())
}

/// Sends the cursor shown now; false if the server refused its image.
fn send_image(conn: &RustConnection, hub: &Hub) -> bool {
    let Ok(img) = CursorImage::fetch(conn) else {
        return false;
    };
    let Ok(msg) = img.message() else {
        return false;
    };
    *hub.cursor_image.lock().unwrap() = Some(img);
    *hub.cursor.lock().unwrap() = Some(msg.clone());
    hub.broadcast(&msg);
    true
}

fn cursor_loop(conn: &RustConnection, root: u32, hub: &Hub, interval: Duration) -> Result<()> {
    let mut warned = false;
    let mut shape = true;
    let mut next_poll = Instant::now();
    loop {
        while let Some(ev) = conn.poll_for_event()? {
            if let Event::XfixesCursorNotify(_) = ev {
                shape = true;
            }
        }
        // The SECURITY extension refuses the image of a cursor whose client
        // has gone, such as one xsetroot set on the root window. Viewers
        // keep the last image then.
        if shape && !send_image(conn, hub) && !warned {
            eprintln!(
                "shuru-screen: the X server refuses the cursor image; start Xvfb with -extension SECURITY"
            );
            warned = true;
        }
        shape = false;
        let watched = !hub.clients().is_empty();
        if watched && Instant::now() >= next_poll {
            next_poll = Instant::now() + interval;
            let p = conn.query_pointer(root)?.reply()?;
            let at = (p.root_x as i32, p.root_y as i32);
            let mut last = hub.pointer.lock().unwrap();
            // Moves between polls collapse into one message.
            if *last != Some(at) {
                *last = Some(at);
                drop(last);
                hub.broadcast(&pointer_message(at.0, at.1));
            }
        }
        let timeout = if watched {
            next_poll
                .saturating_duration_since(Instant::now())
                .as_millis() as i32
                + 1
        } else {
            500
        };
        wait_any(&[conn.stream().as_fd()], timeout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrow() -> CursorImage {
        // 2x1: opaque white, then half transparent black (premultiplied).
        CursorImage {
            width: 2,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            x: 1,
            y: 1,
            serial: 7,
            argb: vec![0xffff_ffff, 0x8000_0000],
        }
    }

    #[test]
    fn rgba_is_straight_alpha() {
        assert_eq!(arrow().rgba(), vec![255, 255, 255, 255, 0, 0, 0, 128]);
    }

    #[test]
    fn composites_at_the_hotspot() {
        let (w, h) = (3, 2);
        let mut px = vec![200u8; w * h * 4];
        arrow().composite(&mut px, w, h);
        // (1, 1) is white, (2, 1) is half way to black.
        assert_eq!(&px[(w + 1) * 4..(w + 1) * 4 + 3], &[255, 255, 255]);
        assert_eq!(&px[(w + 2) * 4..(w + 2) * 4 + 3], &[100, 100, 100]);
        assert_eq!(&px[0..3], &[200, 200, 200]);
    }

    #[test]
    fn composite_clips_at_the_edges() {
        let mut c = arrow();
        c.x = 2;
        let mut px = vec![0u8; 3 * 2 * 4];
        c.composite(&mut px, 3, 2);
        assert_eq!(&px[(3 + 2) * 4..(3 + 2) * 4 + 3], &[255, 255, 255]);
    }

    #[test]
    fn message_carries_a_png() {
        let m: serde_json::Value = serde_json::from_str(&arrow().message().unwrap()).unwrap();
        assert_eq!(m["type"], "cursor");
        assert_eq!(m["serial"], 7);
        let png = base64::engine::general_purpose::STANDARD
            .decode(m["image"].as_str().unwrap())
            .unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
