# RFC 0014: A desktop for agents

- Status: Draft
- Date: 2026-11-04
- Scope: shuru-desktop, shuru-screen, the desktop recipe
- Depends on: RFC 0004 (guest services), shuru-screen (X11 capture, H.264 stream, input gate)

## Summary

`shuru-desktop` is the desktop of a Linux guest that an agent uses: a
stacking X11 window manager with a panel, a rail of apps and menus, a
notification server, the appearance setting, and an API on a Unix socket
that tells the agent what is on the screen in words, not only pixels. It
runs next to Xvfb and `shuru-screen`.

It is made for two users: the agent, which needs a screen that is the
same every time, says what can be clicked, and says when it has settled;
and the person watching or taking over, who needs a desktop that feels
like one, sees the cursor and where the agent clicked, and knows who is
in control.

## Motivation

Agents that use computers run today on desktops made for people: XFCE
or GNOME with a VNC server on top. They cost a gigabyte of memory idle,
show unrelated popups, animate, and give the agent nothing but pixels.
What works in practice, from computer use tools and from OSWorld, is:

- a small fixed screen, so coordinates need no scaling (1024x768 to
  1280x800);
- the accessibility tree first, with element refs, and numbered marks
  on a screenshot only where the tree is thin (Electron, canvas);
- refusing an action made from an old observation;
- waiting for the screen to settle, not sleeping a guessed time;
- asking before actions that send, delete or pay, never typing into a
  password field for the agent, and letting it sign in with a secret it
  never sees.

None of this needs a desktop environment. It needs a window manager that
knows what it is for, and enough of a desktop that a person who takes
over is at home.

## Shape

- **One process**, `shuru-desktop`, a Rust binary on `x11rb`. It manages
  the windows of the display, draws the panel, the rail, menus, frames,
  notification cards and overlays itself with a small software
  rasterizer, and serves the API. About 6 MB resident, no CPU when
  idle.
- **Fixed geometry, followed when it changes.** 1280x800 at 96 DPI by
  default; when RandR changes the screen size, the shell follows.
- **No animations, no compositor.** Menus, cards and overlays show at
  once; motion only for hover states.
- **The API is the agent interface.** Windows, apps, appearance,
  notifications and settle waits are answered on a Unix socket, so the
  agent tool needs few screenshots and no pixel search.

## The desktop

- **Panel**, 32 px at the top: the Applications menu (search over apps
  and windows, or a URL), the app in front and its menu, and who is in
  control, with Hand back while a person has it.
- **Rail**, 64 px on the left: the configured apps and the running ones,
  dots for open windows, a fill for the app in front, a small dot for an
  app that asks for attention. A click focuses or starts; a right click,
  or a click on the app in front, opens its menu (New Window and the
  `.desktop` actions, its windows, Hide, Quit).
- **Windows** are framed: a 1 px ring, 12 px corners, amber while a
  person is in control. Clients without decorations get our title bar
  with the three window lights. Clients with their own (GTK client side
  decorations) keep them, and the frame honors the shadow they declare in
  `_GTK_FRAME_EXTENTS`.
- **Placement.** Top level windows open maximized in the work area;
  they float, move, resize by a band round the edge and snap to the
  halves and the whole work area. Transients float centered over their
  parent.
- **Focus.** The app in front has the keyboard. A window of another app
  that maps on its own is listed, not raised, so a background app cannot
  take the screen from the agent; a pager (`_NET_ACTIVE_WINDOW` with
  source 2) may still raise it.
- **Keys.** Super opens the Applications menu, Alt+Tab cycles apps,
  Alt+` the windows of the app in front.
- **Take over.** With `who` set to `person`, the panel says so, a toast
  says the agent is paused, frames ring amber, and the click ripple stops.

## Window manager hints

EWMH and ICCCM enough for Chromium, GTK, Qt and X toolkits to behave:

- `_NET_SUPPORTED`, `_NET_SUPPORTING_WM_CHECK`, `_NET_CLIENT_LIST`,
  `_NET_CLIENT_LIST_STACKING`, `_NET_ACTIVE_WINDOW` (published by the
  window manager and honored from clients by the focus rule above),
  `_NET_WORKAREA`, `_NET_DESKTOP_GEOMETRY`, `_NET_CLOSE_WINDOW`,
  `_NET_WM_MOVERESIZE`, `_NET_FRAME_EXTENTS`.
- `_NET_WM_STATE`: maximized horizontally and vertically, hidden,
  fullscreen, above, demands attention, modal, focused. Fullscreen covers
  the panel and the rail and returns to the previous state. Above keeps a
  window over the others. Demands attention, and the urgency bit of
  `WM_HINTS`, mark the rail until the window is focused.
- `WM_TRANSIENT_FOR` groups transients with their parent; a modal
  transient takes the focus whenever its parent would.
- `WM_NORMAL_HINTS` minimum and maximum sizes and resize increments bound
  every resize.
- RandR screen changes: the panel, the rail, the work area and the snap
  targets are recomputed, maximized windows fill the new work area,
  floating ones are moved and shrunk to stay on screen.
- The X screen saver is off: Xvfb runs with `-s 0 -nodpms`, and the
  window manager sets the screen saver timeout to 0 at start.

## Cursors

Cursors come from the Xcursor theme (Adwaita by default, `XCURSOR_THEME`
or the `Xcursor.theme` resource otherwise), with the X cursor font as a
fallback: the arrow by default and on the root, resize arrows on window
edges and corners, the move cursor while dragging a title bar, a pointer
over panel controls and rail items, and the text cursor in the search
field.

The real cursor is hidden with XFixes and drawn in an overlay window from
the current cursor image, so captures show it. Raw XInput 2 events move
it; while the pointer moves it is also polled every 100 ms for warps,
and once a second when idle.

## Appearance

Light, the default, and Dark. The colors of the shell are a theme table
with the same tokens in both. The setting lives in the guest at
`~/.config/superhq/appearance`; the window manager watches the file with
inotify and switches live, redrawing everything it draws. A switch is
carried to apps four ways:

- XSETTINGS: the window manager owns `_XSETTINGS_S0` and names the GTK
  theme (`SuperHQ` or `SuperHQ-dark`, both in the guest), so running
  GTK 3 apps restyle.
- `gsettings`: `org.gnome.desktop.interface color-scheme` is set to
  `prefer-light` or `prefer-dark`, and `gtk-theme` to the theme.
- `~/.config/gtk-3.0/settings.ini`: the theme and
  `gtk-application-prefer-dark-theme` for apps that start later.
- The settings portal: `org.freedesktop.portal.Settings` answers
  `org.freedesktop.appearance color-scheme` and signals `SettingChanged`,
  for libadwaita and other portal clients.

Chromium follows the GTK theme for its frame and for web
`prefers-color-scheme`; there is no pinned frame or page scheme. Its
frame switches live; pages follow the theme Chromium starts with and lag
one switch behind on a live change, a limit of how Chromium reads the
GTK theme.

`GET /appearance` and `PUT /appearance` read and set it.

## Notifications

The window manager owns `org.freedesktop.Notifications` on the session
bus (zbus): `GetCapabilities` (body, actions), `Notify` with
`replaces_id`, `CloseNotification`, `GetServerInformation`, and the
`NotificationClosed` and `ActionInvoked` signals. The runtime starts a
session bus for the desktop user and runs the desktop in it.

A notification is a card at the top right of the work area: the app
icon, the summary, up to two lines of body and a close button. Up to
three show, newest on top. `expire_timeout` is honored, 5 s by default.
A click runs the default action, or dismisses. The agent sees the last 50
with `GET /notifications` and acts with
`POST /notifications/{id}/action`, so a download finished or a message
arrived is something it can read, not only a picture.

## Text and localization

Text is shaped with cosmic-text: HarfBuzz shaping (harfrust), bidi, font
fallback through fontconfig, and swash for glyphs, color emoji included
(CBDT and COLRv0). Inter is the first font and is built in; system fonts
cover the rest. Shaped lines and glyphs are cached, so redraws cost no
shaping. Ellipsizing is by shaped width. The layout is the same in every
language; right to left text is shaped and shown in its order, but the
shell is not mirrored.

The guest ships two fallback fonts from the desktop release, pinned by
sha256: Noto Sans CJK SC Regular (Chinese and Japanese) and Noto Color
Emoji. They go to `/usr/local/share/fonts`, not `fonts-noto-cjk`, which
is hundreds of MB.

The shell has a string table keyed by message id, with English as the
fallback. The default locale is English (US): the guest default is
`LANG=en_US.UTF-8`, the English strings use US spelling, and anything
date or number related follows en_US. The second language is Simplified
Chinese: `zh_CN`, `zh_SG` and `zh_Hans` map to it, everything else is
English. The locale is read from `LC_ALL`, then `LC_MESSAGES`, then
`LANG`. App names and actions come from the `.desktop` files by the
Desktop Entry locale rules (`Name[zh_CN]`, then `Name[zh]`, then
`Name`). The runtime passes `LANG` to the desktop, and the desktop to the
apps it launches, so Chromium and GTK apps speak the same language.

## Input

`shuru-screen` holds the input gate: the agent sends input with
`POST /input` while the gate is `agent`, a person through the viewer
stream while it is theirs. Besides moves, buttons, wheel and keys, a
`text` event types any Unicode string: characters in the keymap are
pressed as they are, others are bound to a spare keycode for the moment
they are typed, so `你好，世界` reaches Chromium and a terminal without an
input method.

## The API

HTTP on a Unix socket (`/run/shuru-desktop.sock`, mode 0600), JSON.

| route | answers |
|---|---|
| `GET /windows` | `[{id, app, class, title, pid, focused, kind, fullscreen, state, geometry}]` |
| `POST /focus {id, maximize?}` | focuses an app window, maximized unless asked not to |
| `POST /close {id}` | asks it to close (`WM_DELETE_WINDOW`), or kills a client without it |
| `POST /launch {app, args?, timeout_ms?, maximize?}` | starts a `.desktop` app as the desktop user, answers its window once mapped |
| `GET /settle?quiet_ms=300&max_ms=5000` | answers once the screen has not changed for `quiet_ms`, from the damage events of the display |
| `GET /overlay`, `POST /overlay {who, ripple?}` | who is in control; a ripple at a point |
| `GET /appearance`, `PUT /appearance {appearance}` | light or dark |
| `GET /notifications` | the recent notifications, open or closed |
| `POST /notifications/{id}/action {key?}` | runs an action, `default` by default |

Planned for the next slice:

| route | answers |
|---|---|
| `GET /observe?tree=1&marks=0` | an observation: id, the focused window, its accessibility tree as a compact list of elements, each `{ref, role, name, value?, bounds, states}` |
| `GET /marks` | a PNG of the screen with a numbered tag on each element of the last observation |
| `POST /act {observation, ref, action, text?, secret?}` | click, double, focus, type, type_secret, press on an element by ref; refused with `stale_observation` if the screen changed since that observation, `password_field` for type into a password field |
| `GET /field` | what has keyboard focus, and whether it is a password field |

Coordinates are in screen pixels.

### The accessibility tree

Read from AT-SPI over D-Bus: the guest runs `at-spi2-registryd`, and
apps are started with accessibility on (`GTK_MODULES`,
`QT_ACCESSIBILITY=1`, Chromium with `--force-renderer-accessibility`).
The tree is cut to the focused window, to visible and enabled elements,
and to roles that can be acted on or read. A cap of 400 elements an
observation keeps it small for a model.

Where an app gives no tree (canvas, some Electron apps), `/observe` says
so and the agent uses `/marks`, or coordinates.

### Settling

`shuru-desktop` keeps a last change time from the DAMAGE extension on the
root, leaving out damage from its own windows, and answers `/settle` from
it, so a screenshot is taken once a page has stopped moving.

## Safety in the desktop

- **Password fields take secrets, not text.** `/act` with `type` into a
  password field is refused, never skipped: nothing is typed, and the
  answer is `password_field` with the reason. The agent then has two
  ways on:
  - `/act` with `type_secret` and the name of a secret: the desktop
    types the placeholder of that secret (RFC 0002, RFC 0013). The proxy
    puts the real value in only on requests to the hosts bound to the
    secret, so the value is never in the guest, a screenshot or the
    context of the model. A name the guest has no secret for answers
    `no_such_secret`.
  - ask the person, who types it after taking over. No screenshot is
    taken for the agent while a person is in control.
- Password fields are found in the accessibility tree. An app that gives
  no tree cannot be checked; the tool says so, and taking over is the way
  in for those.
- The panel and the frames show who is in control, so a watcher never
  mistakes the agent for themselves.
- Typed text is never logged. The API logs routes only.
- Asking before an outward action is the tool of the agent, not the
  window manager: it does not know what a button does.

## The guest

A recipe or image installs Xvfb, Chromium, `at-spi2-core`, the guest
settings of `shuru-desktop` (`crates/shuru-desktop/guest`: GTK themes,
Chromium preferences and policy, desktop entries, `mimeapps.list`, xterm
resources), the two fonts, the `en_US.UTF-8` and `zh_CN.UTF-8` locales,
and runs as guest services under an unprivileged `desktop` user:

```
xvfb      Xvfb :1 -screen 0 1280x800x24 -shmem -dpi 96 -s 0 -nodpms
desktop   dbus-run-session -- shuru-desktop --display :1 --socket /run/shuru-desktop.sock --apps ...
screen    shuru-screen --display :1 --socket /run/shuru-screen.sock
```

with `LANG` and `XCURSOR_THEME=Adwaita` in the environment of the desktop
service. Chromium gets `initial_preferences` and a managed policy: no
first run or sign in screens, no sync, no default browser check, no
password saving. PDF, HTML and images open in Chromium, text and Markdown
in the editor, directories in Files.

## Slices

1. The window manager: layout, focus, EWMH, the panel, `/windows`,
   `/focus`, `/close`, `/launch`, `/settle`, the overlays. Done.
2. The desktop: stacking windows, rail, menus, frames, appearance,
   notifications, fonts and localization, cursors, RandR, text input.
   This change.
3. `/observe` with AT-SPI, `/act` by ref with stale refusal, `/field`
   with the password refusal and `type_secret`, `/marks`.
4. One process with `shuru-screen`, sharing the X connection and the
   damage tracking.

## Not in this RFC

- Wayland. A compositor on smithay could replace Xvfb, the window
  manager and capture in one; this RFC stays on X11 until the API shows
  what it must keep.
- More than one screen.
- Clipboard sync between the guest and the viewer.
- Languages beyond English and Simplified Chinese, and mirrored layouts
  for right to left languages.
- Sound.

## Testing

In a Debian trixie container with Xvfb, Chromium, xterm and GTK test
apps, against screenshots of the design:

- windows open maximized, float, snap, resize by their band and by
  their size increments; dialogs center over their parent; a modal dialog
  keeps the focus from its parent; above, fullscreen and attention show
  in `_NET_WM_STATE` and on screen;
- the screen shrinks and grows with `xrandr` and the work area, panel,
  rail and windows follow;
- light and dark switch live from `PUT /appearance` and from the file,
  for the shell, a GTK app, the Chromium frame;
- `notify-send` with replaces, actions and timeouts, and a Chromium web
  notification, on screen and through the API;
- titles and menus in English, Simplified Chinese, Japanese, emoji,
  Arabic and Hindi; the shell and Chromium with `LANG=zh_CN.UTF-8`;
- `/input` text types Chinese into Chromium and xterm;
- memory of `shuru-desktop` about 6 MB with Chromium open, and no CPU
  time over 30 s idle.
