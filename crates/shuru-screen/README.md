# shuru-screen

Streams an X11 display as H.264 over a WebSocket, takes mouse and keyboard
input for it by XTEST behind a gate, and answers stills as PNG. It serves
HTTP on a unix socket only, never TCP; whatever exposes it further brings
its own authentication.

Linux only. It starts from [voxl](https://github.com/tembo/voxl) 0.3.2 by
Tembo (MIT) and does not follow it: the X11 capture, the encoder settings,
the rate control, the paint-over and the XTEST input come from voxl; the
server, the protocol and the gate are new.

## How it works

- Capture reads the framebuffer of Xvfb in place when Xvfb runs with
  `-shmem`, or copies by MIT-SHM GetImage otherwise. XDamage says what
  changed, so a still screen costs no CPU, and x264 skips the macroblocks
  that did not change.
- x264 encodes with `ultrafast` and `zerolatency`, no B frames, and a
  keyframe only when a viewer needs one.
- The bitrate follows the acks of viewers: it backs off toward the rate
  they take frames at when acks show queueing, and probes up while the
  path is clear, between `--min-kbps` and `--max-kbps`.
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
Xvfb :1 -screen 0 1280x800x24 -shmem &
shuru-screen --display :1 --socket /run/screen.sock
```

| flag | default | |
|---|---|---|
| `--display` | `$DISPLAY` | the X display |
| `--socket` | | the unix socket, created with mode 0600 |
| `--framerate` | 30 | frames a second at most |
| `--max-kbps` | 6000 | highest bitrate |
| `--min-kbps` | 500 | lowest bitrate it backs off to |

It exits on SIGTERM or SIGINT, letting go of held keys and buttons and
removing the socket.

## Interface

Every response other than the stream is JSON or PNG. Errors are
`{"error": "..."}` with a 4xx or 5xx status.

### `GET /healthz`

`200`, `ok`.

### `GET /still[?width=N]`

The screen now, `image/png`, RGB. With `width`, scaled down to at most
that width, keeping the aspect. Works with no viewer connected.

### `GET /stream[?viewer=ID]`

A WebSocket. `ID` names the viewer for the gate; it may not be empty or
`agent`. Without it the stream only watches.

The server sends, in order:

1. A text message with the stream parameters:

   ```json
   {"type": "hello", "codec": "avc1.42c020", "format": "annexb", "width": 1280, "height": 800}
   ```

   `codec` goes to `VideoDecoder.configure` as it is; `format` is always
   `annexb`, so configure without a `description`.

2. A text message with the gate for this viewer, and again each time it
   changes:

   ```json
   {"type": "gate", "input": false}
   ```

3. Binary frames, the first of them a keyframe. Each is a 16 byte header
   and one H.264 access unit, Annex B, with SPS and PPS in front of each
   keyframe. Integers are big endian.

   | bytes | |
   |---|---|
   | 0 | kind, 1 for video |
   | 1 | flags: bit 0 keyframe, bit 1 paint-over (a sharpening pass) |
   | 2..4 | reserved, 0 |
   | 4..8 | frame number, u32, wrapping |
   | 8..16 | timestamp, u64, microseconds since the stream started |

   The payload is an `EncodedVideoChunk` with `type` `key` or `delta` from
   bit 0, and `timestamp` from bytes 8..16.

Frames are sent only when the screen changes. Clients may send text
messages, JSON:

| message | |
|---|---|
| `{"type": "ack", "frame": N}` | frame N is decoded. Acks steer the bitrate and how many frames are in flight; a client that never acks gets the stream at a fixed rate |
| `{"type": "keyframe"}` | send me a keyframe; frames before it are skipped |
| `{"type": "move", "x": X, "y": Y}` | pointer to X, Y in screen pixels |
| `{"type": "button", "button": B, "down": true}` | X button: 1 left, 2 middle, 3 right |
| `{"type": "wheel", "dx": 0, "dy": 120}` | scroll; 120 is one notch, positive `dy` scrolls down, `dx` right |
| `{"type": "key", "keysym": K, "down": true}` | an X keysym, such as 0x61 for `a` or 0xff0d for Return |

Input messages are applied only while the gate is open for this viewer,
and dropped otherwise. Unknown messages are ignored.

A viewer that reconnects keeps the gate, gets a new `hello` and starts at
a keyframe. A new viewer gets a keyframe without holding the others back
for long. Keys and buttons held by a viewer are let go when its stream
closes while it has the gate.

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
See NOTICE and LICENSE-voxl.
