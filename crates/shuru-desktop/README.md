# shuru-desktop

The window manager of a desktop an agent uses (RFC 0014, slice 1). It runs
on an X display next to Xvfb and `shuru-screen`, lays out one app at a
time, draws a panel and overlays for the person watching, and answers an
API on a unix socket. Linux only.

```
shuru-desktop --display :1 --socket /run/shuru-desktop.sock \
  --apps chromium,org.gnome.Nautilus,/usr/share/applications/xterm.desktop
```

`--apps` lists the apps of the panel: ids of `.desktop` files in the XDG
application dirs, or paths to them.

## Layout

- Top level windows fill the screen less a 12 px margin, under a 28 px
  strip with the app name. The panel floats over the bottom of the window.
- Dialogs and other transient windows are centred over their parent.
- Override redirect windows (menus, tooltips) are left alone.
- A window of the app on screen comes to the front and takes the
  keyboard. A window of another app that maps on its own is listed and
  stacked under the others; it does not take the screen. A pager
  (`_NET_ACTIVE_WINDOW` with source 2) may still raise it.
- A fullscreen window (`_NET_WM_STATE_FULLSCREEN`) covers the whole
  screen, strip and panel included.
- A person switches apps with the panel or with Alt+Tab, and a click
  focuses a window of the app on screen.

## API

HTTP on the unix socket, made with mode 0600. Bodies are JSON. Errors are
`{"error": "..."}` with a 4xx or 5xx status.

| route | answers |
|---|---|
| `GET /healthz` | `{"ok": true}` |
| `GET /windows` | `[{id, app, class, title, pid, focused, kind, fullscreen}]`, most recently focused first; `kind` is `normal`, `dialog`, `utility`, `splash`, `dock` or `popup` |
| `POST /focus {id}` | the window, now focused; 404 for an unknown id |
| `POST /close {id}` | `{id, closing, how}`; `how` is `delete` (`WM_DELETE_WINDOW`) or `kill` for a window without it |
| `POST /launch {app, args?, timeout_ms?}` | the first window of the app once it maps; 404 for an unknown app, 502 if it exits with an error first, 504 after `timeout_ms` (15 s by default) |
| `GET /settle?quiet_ms=300&max_ms=5000` | `{settled, waited_ms, quiet_ms}` once the screen has not changed for `quiet_ms`, or at `max_ms` with `settled: false` |
| `GET /overlay` | `{who}` |
| `POST /overlay {who?, ripple?}` | `who` is `agent`, `person` or null; `ripple` is `{x, y}`; answers `{who}` |

`app` in `/launch` is a `.desktop` id, its `Name` or the program it runs,
looked up in `--apps` first and then in the XDG application dirs. `args`
fill the `%f %F %u %U` field codes of its Exec line, or go at its end when
it has none. Apps run as the user of `shuru-desktop`, in a process group of
their own, with `DISPLAY` set and accessibility on (`QT_ACCESSIBILITY=1`,
`GNOME_ACCESSIBILITY=1`).

`/settle` comes from the DAMAGE extension on the root window. Damage that
falls inside our own windows (strip, panel, badge, border, cursor, ripple)
or where they were in the last second does not count.

## Overlays

- **Cursor.** The X cursor is hidden with XFixes and drawn in an overlay
  window of our own from the current cursor image, so it shows in any
  capture of the screen.
- **Click ripple.** A ring at each click (from XInput 2 raw button events)
  while a person is not in control, and at `ripple` points of
  `/overlay`. It shows for 500 ms.
- **Who is in control.** With `who` set, a badge at the right of the
  strip says Agent or You. While a person is in control the screen has an
  amber border and the badge a Done button that hands control back to the
  agent.

Overlays let clicks through (an empty SHAPE input region) and are kept
over every other window.

The font is Inter Medium cut to Latin, under the SIL Open Font License
(`fonts/LICENSE-Inter.txt`).
