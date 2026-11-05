# shuru-desktop

The desktop of a guest that an agent uses (RFC 0014): a stacking X11
window manager with a panel, a rail of apps and menus, a notification
server, and an API on a unix socket. It runs on an X display next to Xvfb
and `shuru-screen`. Linux only.

```
shuru-desktop --display :1 --socket /run/shuru-desktop.sock \
  --apps chromium,terminal
```

`--apps` lists the apps of the rail: ids of `.desktop` files in the XDG
application dirs, or paths to them. `guest/` holds the settings a guest
image installs next to it; see `guest/README.md`.

## The desktop

- A 32 px panel at the top: the Applications menu, the app in front with
  its menu, and who is in control.
- A 64 px rail on the left with the apps of `--apps` and the running ones.
  A click brings an app to the front or starts it; a click on the app in
  front, or a right click on any, opens its menu: New Window and the
  actions of its `.desktop` file, its windows, Hide and Quit. Running
  windows show as dots, up to three; a window that asks for attention
  (`_NET_WM_STATE_DEMANDS_ATTENTION` or the urgency hint) puts a small dot
  on its item until it is focused.
- Top level windows open maximized in the work area (the screen less the
  panel and the rail). Windows float, move by their title bar, resize by
  a band round their edge and snap to the halves and the full work area.
  Dialogs and other transient windows float centered over their parent.
- Windows get a frame: a 1 px ring with 12 px corners, amber while a
  person is in control. Clients that draw no title bar get ours, with the
  three window lights (close, minimize, maximize) in red, yellow and
  green on the focused window and a neutral gray on the others.
  GTK clients that draw their own (client side decorations) keep theirs,
  including the shadow they declare in `_GTK_FRAME_EXTENTS`.
- Keys: Super opens the Applications menu with search, Alt+Tab cycles
  apps, Alt+` cycles the windows of the app in front.
- A fullscreen window covers the panel and the rail and goes back to its
  place after.
- An empty desktop says No windows open.
- Notifications show as cards at the top right of the work area, up to
  three, newest on top (see below).
- No animations. Overlays and menus show at once.

## API

HTTP on the unix socket, made with mode 0600. Bodies are JSON. Errors are
`{"error": "..."}` with a 4xx or 5xx status.

| route | answers |
|---|---|
| `GET /healthz` | `{"ok": true}` |
| `GET /tree` | open shell popup nodes: `[{id, role, name, value, states, actions, bounds}]`; empty when closed; the `menu` node includes child ids; bounds are `{x, y, width, height}` in screen pixels; states are `{focused, selected, disabled, expanded}` |
| `POST /tree/action {id, action, value?}` | acts on a shell node: `click`, `focus` or `set_value`; `set_value` requires a string `value`; answers `{ok: true}`, 404 for an absent node, 400 for an unsupported action |
| `GET /windows` | `[{id, app, class, title, pid, focused, kind, fullscreen, state, geometry}]`, most recently focused first; `kind` is `normal`, `dialog`, `utility`, `splash`, `dock` or `popup`; `state` is `maximized`, `floating` or `minimized`; `geometry` is `{x, y, width, height}` of the window on screen |
| `POST /focus {id, maximize?}` | the window, now focused; a main window is maximized unless `maximize` is false; 404 for an unknown id |
| `POST /close {id}` | `{id, closing, how}`; `how` is `delete` (`WM_DELETE_WINDOW`) or `kill` for a window without it |
| `POST /launch {app, args?, timeout_ms?, maximize?}` | the first window of the app once it maps, maximized unless `maximize` is false; 404 for an unknown app, 502 if it exits with an error first, 504 after `timeout_ms` (15 s by default) |
| `GET /settle?quiet_ms=300&max_ms=5000` | `{settled, waited_ms, quiet_ms}` once the screen has not changed for `quiet_ms`, or at `max_ms` with `settled: false` |
| `GET /overlay` | `{who}` |
| `POST /overlay {who?, ripple?}` | `who` is `agent`, `person` or null; `ripple` is `{x, y}`; answers `{who}` |
| `GET /appearance` | `{"appearance": "light"}` or `dark` |
| `PUT /appearance {appearance}` | sets it, saves it and switches live; answers the same |
| `GET /notifications` | the last 50, newest first: `[{id, app, summary, body, time, open, actions: [{key, label}]}]` |
| `POST /notifications/{id}/action {key?}` | runs an action of an open notification (`default` when no key), which closes it; 404 for a closed or unknown one, or a key it does not have |

`app` in `/launch` is a `.desktop` id, its `Name` or the program it runs,
looked up in `--apps` first and then in the XDG application dirs. `args`
fill the `%f %F %u %U` field codes of its Exec line, or go at its end when
it has none. Apps run as the user of `shuru-desktop`, in a process group of
their own, with `DISPLAY` set and accessibility on (`QT_ACCESSIBILITY=1`,
`GNOME_ACCESSIBILITY=1`).

`/settle` comes from the DAMAGE extension on the root window. Damage that
falls inside our own windows (panel, rail, frames, menus, cards, cursor,
ripple) or where they were in the last second does not count.

## Window manager hints

EWMH and ICCCM as apps expect them:

- `_NET_SUPPORTED`, `_NET_SUPPORTING_WM_CHECK`, `_NET_CLIENT_LIST`,
  `_NET_CLIENT_LIST_STACKING` (bottom to top), `_NET_ACTIVE_WINDOW` (set
  by us, and asked for by clients; another app asking for the screen is
  refused unless it comes from a pager), `_NET_WORKAREA`,
  `_NET_DESKTOP_GEOMETRY`, `_NET_CLOSE_WINDOW`, `_NET_WM_MOVERESIZE`,
  `_NET_FRAME_EXTENTS`, `_NET_REQUEST_FRAME_EXTENTS`.
- `_NET_WM_STATE`: maximized horizontally and vertically, hidden,
  fullscreen, above (kept over the other windows), demands attention,
  modal, focused.
- `WM_TRANSIENT_FOR`: transients float over their parent and go with it.
  A modal one (`_NET_WM_STATE_MODAL`) takes the focus whenever its parent
  would get it.
- `WM_NORMAL_HINTS`: resizes keep to the minimum and maximum size and to
  the size increments from the base size (an xterm resizes by cells).
- `WM_HINTS`: the input hint and the urgency hint.
- RandR: when the screen changes size the panel, the rail, the work area
  and the snap targets follow, maximized windows fill the new work area
  and floating ones stay on screen.
- The X screen saver is turned off at start (`SetScreenSaver` with a
  timeout of 0); run Xvfb with `-s 0` as well.

## Cursors

The cursors come from the Xcursor theme named by `XCURSOR_THEME` or the
`Xcursor.theme` resource, Adwaita when neither is set, with the X cursor
font as a fallback: the default arrow on the root window and elsewhere,
resize arrows on the edges and corners of a floating window, the move
cursor while a title bar is dragged, a pointer over the controls of the
panel and the items of the rail, and the text cursor over the search
field.

The X cursor is hidden with XFixes and drawn in an overlay window of our
own from the current cursor image, so it shows in any capture of the
screen. A click ripple shows at each click while a person is not in
control, and at `ripple` points of `/overlay`.

## Appearance

Light (the default) and Dark. The setting is the file
`$XDG_CONFIG_HOME/superhq/appearance` (`~/.config/superhq/appearance`),
holding `light` or `dark`. `shuru-desktop` watches it with inotify and
switches live, so `PUT /appearance`, a settings app and
`echo dark > ~/.config/superhq/appearance` all work. A switch:

- redraws the panel, the rail, menus, frames and cards in the theme,
- names the GTK theme over XSETTINGS (`Net/ThemeName`: `SuperHQ` or
  `SuperHQ-dark`), so running GTK 3 apps restyle,
- sets `org.gnome.desktop.interface color-scheme` (`prefer-light` or
  `prefer-dark`) and `gtk-theme` with `gsettings`, and writes
  `gtk-application-prefer-dark-theme` in `~/.config/gtk-3.0/settings.ini`
  for apps that start later,
- answers `org.freedesktop.appearance color-scheme` on the
  `org.freedesktop.portal.Settings` interface and sends `SettingChanged`,
  which libadwaita and other portal clients follow.

Chromium follows the GTK theme: its frame and tab strip switch live, and
web pages get `prefers-color-scheme` from the theme it starts with. A live
switch reaches pages one switch late (Chromium judges dark from the theme
colors before GTK has loaded the new theme); restart Chromium for pages to
follow exactly.

## Notifications

`shuru-desktop` owns `org.freedesktop.Notifications` on the session bus
(it needs `DBUS_SESSION_BUS_ADDRESS`; without it there are no
notifications). It implements `GetCapabilities` (`body`, `actions`),
`Notify` with `replaces_id`, `CloseNotification`, `GetServerInformation`,
and sends `NotificationClosed` and `ActionInvoked`.

A card is 340 px wide at the top right of the work area: the app icon (by
the `desktop-entry` hint, the app name or `app_icon`), the summary, the
body in up to two lines and a close button. Up to three show at once,
newest on top. `expire_timeout` is honored, 5 s by default, 0 for never.
A click runs the `default` action when there is one and dismisses the
card; the close button only dismisses. The agent reads them with
`GET /notifications` and acts with `POST /notifications/{id}/action`.

## Text and languages

Text is shaped with cosmic-text (HarfBuzz shaping through harfrust, bidi,
font fallback, swash for rasterizing). Inter Medium is built in and is
the first font; anything it does not cover falls back to the system fonts
through fontconfig, including color emoji (CBDT and COLRv0) and complex
scripts such as Arabic and Devanagari. Text that does not fit is cut by
its shaped width and ends in an ellipsis. The layout is the same in every
language: no mirroring for right to left text.

Shaped lines and rasterized glyphs are cached (512 lines, 4096 glyphs),
so redrawing costs no shaping. An idle desktop draws nothing; it looks at
the pointer once a second (warps make no input events) and measures no
CPU time over 30 s.

The shell speaks English (en_US, the default) and Simplified Chinese. The
locale comes from `LC_ALL`, then `LC_MESSAGES`, then `LANG`; `zh_CN`,
`zh_SG` and `zh_Hans` are Simplified Chinese and everything else is
English. App names and actions come from `.desktop` files by the Desktop
Entry rules (`Name[zh_CN]`, then `Name[zh]`, then `Name`).

## Fonts

Inter Medium, under the SIL Open Font License (`fonts/LICENSE-Inter.txt`).
The releases also carry Noto Sans CJK SC Regular and Noto Color Emoji,
which a guest installs for Chinese, Japanese and emoji; see
`guest/README.md`.
