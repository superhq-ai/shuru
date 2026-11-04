# shuru-screen

Streams an X11 display as H.264 over a WebSocket, takes mouse and keyboard
input for it by XTEST behind a gate, and answers stills as PNG. It serves
HTTP on a unix socket only, never TCP; whatever exposes it further brings
its own authentication.

Linux only. It starts from [voxl](https://github.com/tembo/voxl) 0.3.2 by
Tembo (MIT) and does not follow it: the X11 capture, the encoder settings,
the rate control, the paint-over, the XTEST input and the Xvfb scroll patch
come from voxl; the server, the protocol and the gate are new.

## How it works

- Capture reads the framebuffer of Xvfb in place when Xvfb runs with
  `-shmem`, or copies by MIT-SHM GetImage otherwise. XDamage says what
  changed, so a still screen costs no CPU, and x264 skips the macroblocks
  that did not change.
- x264 encodes with `ultrafast` and `zerolatency`, no B frames, and a
  keyframe only when a viewer needs one. With `--profile high`, CABAC and
  the 8x8 transform are back on; the rest of `ultrafast` stays.
- The cursor is not in the video. The X server is told to hide it, and
  viewers get its image when the shape changes and its position at up to
  the frame rate, and draw it themselves, so moving the pointer costs no
  encode.
- When the screen changes size by RandR, capture and the encoder start
  again at the new size and every viewer gets a new `hello` and a
  keyframe.
- The bitrate follows the viewers, between `--min-kbps` and `--max-kbps`.
  Each new viewer starts it over at a safe rate, 1500 kbps or
  `--min-kbps` if higher, and it grows by half every 250 ms while the
  viewers keep up, so a fast viewer is at full rate in about a second.
  A viewer that acks steers it by the delay of its acks. A viewer that
  does not is measured by its send queue: the bytes waiting for it over
  the rate it reads them, or over the safe rate until that is measured.
  More than about 200 ms of video waiting steps the bitrate down toward
  that rate, and it steps back up once the queue drains. Past about
  400 ms, frames wait until it drains. A keyframe is at most about 400 ms
  of video at the current bitrate.
- Blocks sent while they moved are sent again at higher quality once they
  have been still for a moment (the paint-over), so text sharpens when the
  screen settles.

## Building

Needs x264 and libyuv from the system, linked dynamically. On Debian:

```sh
apt-get install libx264-dev libyuv-dev
cargo build -p shuru-screen --release
```

## Running

```sh
Xvfb :1 -screen 0 1280x800x24 -shmem -extension SECURITY &
shuru-screen --display :1 --socket /run/screen.sock
```

`-extension SECURITY` turns off the SECURITY extension of the X server,
which is for untrusted clients such as `ssh -X` and has no use here. With
it on, the server refuses the image of a cursor whose client has exited,
such as a root cursor set by `xsetroot`; viewers and stills then keep the
last cursor image they had, and shuru-screen says so once in its log.

| flag | default | |
|---|---|---|
| `--display` | `$DISPLAY` | the X display |
| `--socket` | | the unix socket, created with mode 0600 |
| `--framerate` | 30 | frames a second at most |
| `--max-kbps` | 6000 | highest bitrate |
| `--min-kbps` | 500 | lowest bitrate it backs off to |
| `--profile` | `high` | `baseline` (Constrained Baseline) or `high` |

`SHURU_SCREEN_DEBUG=1` logs each bitrate change. It exits on SIGTERM or
SIGINT, letting go of held keys and buttons and removing the socket.

### Smooth scrolling

Stock Xvfb gives the XTEST pointer no scroll axes, so wheel input becomes
clicks of buttons 4 to 7, one per 120 units; smaller deltas add up until
they make a click. With an Xvfb built with
`patches/xorg-server-xtest-scroll.patch`, the deltas go out as they are,
on the scroll axes, and X clients that take smooth scrolling scroll by the
pixel. `patches/build-xvfb-deb.sh` builds that Xvfb as a Debian trixie
package, in Docker. shuru-screen finds the axes on its own at start.

## Interface

Every response other than the stream is JSON or PNG. Errors are
`{"error": "..."}` with a 4xx or 5xx status.

### `GET /healthz`

`200`, `ok`.

### `GET /still[?width=N][&cursor=1]`

The screen now, `image/png`, RGB. With `width`, scaled down to at most
that width, keeping the aspect. With `cursor=1`, the cursor is drawn in
where it is; without it, or with `cursor=0`, it is not. Works with no
viewer connected.

### `GET /stream[?viewer=ID]`

A WebSocket. `ID` names the viewer for the gate; it may not be empty or
`agent`. Without it the stream only watches.

The server sends, in order:

1. A text message with the stream parameters:

   ```json
   {"type": "hello", "codec": "avc1.640020", "format": "annexb", "width": 1280, "height": 800}
   ```

   `codec` goes to `VideoDecoder.configure` as it is; it names the
   profile and level of the stream, `avc1.64....` for High and
   `avc1.42....` for Constrained Baseline. `format` is always `annexb`, so
   configure without a `description`.

2. A text message with the gate for this viewer, and again each time it
   changes:

   ```json
   {"type": "gate", "input": false}
   ```

3. A text message with the cursor, once known, and again each time its
   shape changes:

   ```json
   {"type": "cursor", "image": "<base64 PNG>", "hot_x": 7, "hot_y": 7, "serial": 1}
   ```

   `image` is an RGBA PNG; its pixel `hot_x`, `hot_y` goes at the pointer.
   `serial` is the X cursor serial: the same serial is the same image.

4. A text message with the pointer position, once known, and again when
   it moves, at most once a frame interval; moves in between are
   coalesced into the latest:

   ```json
   {"type": "pointer", "x": 640, "y": 400}
   ```

5. Binary frames, the first of them a keyframe. Each is a 16 byte header
   and one H.264 access unit, Annex B, with SPS and PPS in front of each
   keyframe. Integers are big endian.

   | bytes | |
   |---|---|
   | 0 | kind, 1 for video |
   | 1 | flags: bit 0 keyframe, bit 1 paint-over (a sharpening pass) |
   | 2..4 | reserved, 0 |
   | 4..8 | frame number, u32, wrapping |
   | 8..16 | timestamp, u64, microseconds since the server started |

   The payload is an `EncodedVideoChunk` with `type` `key` or `delta` from
   bit 0, and `timestamp` from bytes 8..16.

Frames are sent only when the screen changes; `cursor`, `pointer` and
`gate` messages come between frames whenever they change. When the screen
changes size, a new `hello` with the new size and codec comes, and the
next frame is a keyframe at that size; reconfigure the decoder then.
Frame numbers and timestamps carry on across it. Unknown message types
are to be ignored.

Clients may send text messages, JSON:

| message | |
|---|---|
| `{"type": "ack", "frame": N}` | frame N is decoded. Acks steer the bitrate and how many frames are in flight; without them the send queue does |
| `{"type": "keyframe"}` | send me a keyframe; frames before it are skipped |
| `{"type": "move", "x": X, "y": Y}` | pointer to X, Y in screen pixels |
| `{"type": "button", "button": B, "down": true}` | X button: 1 left, 2 middle, 3 right |
| `{"type": "wheel", "dx": 0, "dy": 120}` | scroll; 120 is one notch, positive `dy` scrolls down, `dx` right; any size, smaller deltas add up |
| `{"type": "key", "keysym": K, "down": true}` | an X keysym, such as 0x61 for `a` or 0xff0d for Return |

Input messages are applied only while the gate is open for this viewer,
and dropped otherwise. Unknown messages are ignored.

A viewer that reconnects keeps the gate, gets a new `hello`, the cursor
and the pointer, and starts at a keyframe. A new viewer gets a keyframe
without holding the others back for long. Keys and buttons held by a
viewer are let go when its stream closes while it has the gate.

### `GET /gate`, `POST /gate`

Who may send input:

```json
{"viewer": "ID"}
{"viewer": "agent"}
{"viewer": null}
```

`POST` sets it and answers with the same body. A viewer ID opens the gate
for the streams of that viewer, `agent` for `POST /input`, `null` shuts
it. On every change, keys and buttons held are let go and each stream gets
a `gate` message.

### `POST /input`

A JSON list of input events, the same `move`, `button`, `wheel` and `key`
messages as on the stream, applied in order:

```json
[{"type": "move", "x": 400, "y": 300}, {"type": "button", "button": 1, "down": true}, {"type": "button", "button": 1, "down": false}]
```

`200` with `{"applied": N}` while the gate is `agent`, `409` otherwise.

## License

Apache-2.0 source; the binary links x264 and is distributed under the GPL.
See NOTICE and LICENSE-voxl. `patches/xorg-server-xtest-scroll.patch`
changes xorg-server, which is under the X11 license.
