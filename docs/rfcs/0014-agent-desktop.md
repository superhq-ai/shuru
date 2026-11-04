# RFC 0014: A desktop for agents

- Status: Draft
- Date: 2026-11-04
- Scope: new crate shuru-desktop, shuru-screen (in review), the desktop recipe
- Depends on: RFC 0004 (guest services), shuru-screen (X11 capture, H.264 stream, input gate)

## Summary

`shuru-desktop` is a window manager for a Linux desktop that an agent
uses: an X11 window manager, a small panel, and an API on a Unix socket
that tells the agent what is on the screen in words and refs, not only
pixels. It replaces Openbox and a panel in a guest, and runs next to
Xvfb and `shuru-screen`.

It is made for two users: the agent, which needs a screen that is the
same every time, says what can be clicked, and says when it has settled;
and the person watching, who needs to see the cursor, where the agent
clicked, and who is in control.

## Motivation

Agents that use computers run today on desktops made for people: XFCE
or GNOME with a VNC server on top. They cost a gigabyte of memory idle,
show notifications, overlap windows, animate, and give the agent
nothing but pixels. What works in practice, from computer use tools and
from OSWorld, is:

- a small fixed screen, so coordinates need no scaling (1024x768 to
  1280x800);
- the accessibility tree first, with element refs, and numbered marks
  on a screenshot only where the tree is thin (Electron, canvas);
- refusing an action made from an old observation;
- waiting for the screen to settle, not sleeping a guessed time;
- asking before actions that send, delete or pay, and never typing into
  a password field for the agent, and letting it sign in with a secret
  it never sees.

None of this needs a desktop environment. It needs a window manager
that knows what it is for.

## Shape

- **One process**, `shuru-desktop`, a static Rust binary on `x11rb`.
  It owns the X11 window management of the display and draws the panel
  and the overlays itself. Memory about 10 MB.
- **One app on screen at a time.** The focused top level window fills
  the screen less a 12 px margin; transient windows (dialogs, popups)
  are centred over their parent. No window overlaps another app.
- **Fixed geometry.** 1280x800 at 96 DPI, one font set, no animations,
  no compositing, a flat background.
- **The API is the agent interface.** Windows, apps, the accessibility
  tree, marks and settle waits are answered on a Unix socket, so the
  agent tool needs few screenshots and no pixel search.
- **The overlays are for people.** Cursor, click ripple, who is in
  control. They are drawn on a layer the agent can ask to leave out of
  its screenshots.

## The window manager

- Reparents nothing; top level windows are mapped full size, with a 28
  px strip above holding the app name. Override redirect windows (menus,
  tooltips) are left alone.
- Focus is explicit: the app on screen has it. Switching is by the API,
  by the panel, or by `alt+tab` for a person.
- EWMH enough for Chromium, GTK and Qt apps to behave: `_NET_ACTIVE_WINDOW`,
  `_NET_CLIENT_LIST`, `_NET_WM_STATE_FULLSCREEN`, `_NET_WM_WINDOW_TYPE`
  for dialogs and docks, `_NET_WM_NAME`.
- New windows of the focused app come to the front; a window of another
  app that maps on its own is listed, not raised, so a background app
  cannot steal the screen from the agent.
- No notification daemon is run. A `org.freedesktop.Notifications`
  service that keeps them and answers them on the API, not on screen,
  is part of slice 2.

## The panel

A small pill centred at the bottom: the apps of the desktop (from
`.desktop` files in a configured list) and the running ones. It is for
the person who takes over; the agent launches by name through the API.
It is hidden in screenshots the agent takes unless asked for.

## The API

HTTP on a Unix socket (`/run/shuru-desktop.sock`, mode 0600), JSON.

| route | answers |
|---|---|
| `GET /windows` | `[{ id, app, title, pid, focused, kind }]` |
| `POST /focus {id}` | focuses an app window |
| `POST /close {id}` | asks it to close (`WM_DELETE_WINDOW`) |
| `POST /launch {app, args?}` | starts a `.desktop` app as the desktop user, answers its window once mapped |
| `GET /observe?tree=1&marks=0` | an observation: id, the focused window, its accessibility tree as a compact list of elements, each `{ ref, role, name, value?, bounds, states }` |
| `GET /marks` | a PNG of the screen with a numbered tag on each element of the last observation, for when the tree is thin |
| `POST /act {observation, ref, action, text?, secret?}` | click, double, focus, type, type_secret, press on an element by ref; refused with `stale_observation` if the screen changed since that observation, `password_field` for type into a password field |
| `GET /settle?quiet_ms=300&max_ms=5000` | answers once the screen has not changed for `quiet_ms`, from the damage events of the display |
| `GET /field` | what has keyboard focus, and whether it is a password field |
| `POST /overlay {who, ripple?}` | `who` is `agent`, `person` or null; a ripple at a point |

Coordinates are in screen pixels of the fixed 1280x800 screen.
Element refs are stable for the life of an observation.

### The accessibility tree

Read from AT-SPI over D-Bus: the guest runs `at-spi2-registryd`, and
apps are started with accessibility on (`GTK_MODULES`, `QT_ACCESSIBILITY=1`,
Chromium with `--force-renderer-accessibility`). The tree is cut to the
focused window, to visible and enabled elements, and to roles that can
be acted on or read (buttons, links, fields, lists, rows, headings,
text). A cap of 400 elements an observation keeps it small for a model;
past it, elements of the visible area first.

Where an app gives no tree (canvas, some Electron apps), `/observe`
says so and the agent uses `/marks`, which then tags regions found by
the boxes of what the tree does have, or nothing, and the agent falls
back to coordinates.

### Settling

`shuru-screen` already listens for damage on the display. Both use the
DAMAGE extension directly; `shuru-desktop` keeps a last change time and
answers `/settle` from it, so a screenshot is taken once a page has
stopped moving, not after a guessed sleep.

### Input

Clicks and keys go through the input gate of `shuru-screen` when it
runs (only the holder of the gate may send input), else through XTEST
directly. Long text is set on the clipboard and pasted, which is faster
than typing and keeps keyboard layout out of it; a field that refuses
paste gets typed text. Key chords are checked against the keysyms the
display has.

## Safety in the desktop

- **Password fields take secrets, not text.** `/act` with `type` into a
  password field is refused, never skipped: nothing is typed, and the
  answer is `password_field` with the reason, so the agent does not go
  on as if it had typed. The agent then has two ways on:
  - `/act` with `type_secret` and the name of a secret: the desktop
    types the placeholder of that secret (RFC 0002, RFC 0013). The proxy
    puts the real value in only on requests to the hosts bound to the
    secret, so the value is never in the guest, a screenshot or the
    context of the model, and on any other site the placeholder is a
    useless string. A name the guest has no secret for answers
    `no_such_secret`.
  - ask the person: the tool marks the step as needing them, and a
    person types it after taking over. No screenshot is taken for the
    agent while a person is in control.
- Password fields are found in the accessibility tree. An app that
  gives no tree (canvas, some Electron apps) cannot be checked, and
  typing there is not refused; the tool says so in what it tells the
  agent, and taking over is the way in for those.
- The overlay shows who is in control, with a border when it is a
  person, so a watcher never mistakes the agent for themselves.
- Typed text is never logged. The API logs routes and refs only.
- Asking before an outward action is the tool of the agent, not the
  window manager: it does not know what a button does. It does give the
  tool the element name and role to ask with.

## The guest

A recipe or image installs Xvfb, Chromium, `at-spi2-core`, and
`shuru-desktop` and `shuru-screen`, and runs them as guest services
under an unprivileged `desktop` user:

```
xvfb      Xvfb :1 -screen 0 1280x800x24 -shmem -dpi 96
desktop   shuru-desktop --display :1 --socket /run/shuru-desktop.sock
screen    shuru-screen --display :1 --socket /run/shuru-screen.sock
chromium  chromium --force-renderer-accessibility --no-first-run ...
```

Chromium gets a policy file: no first run, no default browser check, no
translate bar, no password saving, downloads to `~/Downloads` without a
prompt.

## Slices

1. The window manager: layout, focus, EWMH, the panel, `/windows`,
   `/focus`, `/close`, `/launch`, `/settle`, the overlays. Replaces
   Openbox.
2. `/observe` with AT-SPI, `/act` by ref with stale refusal, `/field`
   with the password refusal and `type_secret`, `/marks`, the
   notification service.
3. One process with `shuru-screen`, sharing the X connection and the
   damage tracking.

## Not in this RFC

- Wayland. A compositor on smithay could replace Xvfb, the window
  manager and capture in one; this RFC stays on X11 until the API shows
  what it must keep.
- More than one screen, or a screen size that changes.
- Sound.

## Testing

- In a Linux container with Xvfb: windows map full size, a dialog is
  centred over its parent, focus follows the API, an app that maps in
  the background does not take focus.
- `/settle` answers within `quiet_ms` of the last change of a scripted
  animation, and waits while it runs.
- `/observe` on a GTK test app and on Chromium with a known page lists
  the expected roles and names; `/act` clicks the right element; an
  `/act` after the page changed answers `stale_observation`.
- `/act type` into a password field is refused with `password_field`
  and types nothing; `type_secret` types the placeholder of that secret;
  an unknown secret answers `no_such_secret`.
- Memory of `shuru-desktop` under 20 MB with Chromium open.
