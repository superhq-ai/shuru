# RFC 0016: A retained shell UI tree

- Status: Draft
- Date: 2026-11-06
- Scope: shuru-desktop
- Depends on: RFC 0014 (agent desktop)

## Why

The launcher edits only the end of a string, its window is configured on
each key, and a one-item rail menu has a minimum height larger than its
content. These share a cause: geometry, painting and input each describe
the same controls separately. The shell needs one retained model of what
is open, where it is and what it can do.

## Parts

The UI lives in `src/ui`, inside the window manager process:

- taffy 0.15 lays out a flex column with padding and fixed row metrics.
  The content determines the sheet height. A screen-sized viewport clips
  long menus and selection scrolls into view.
- cosmic-text 0.19 shapes text as before. Its Editor owns the search
  value, cursor and selection. The caret stays steady: the desktop has
  no animation. The existing X11 connection handles UTF8_STRING on
  CLIPBOARD, including copy, cut and paste.
- tiny-skia 0.12 paints widget surfaces through a small painter boundary.
  The result composites into the existing CPU 0RGB ZPixmap buffer.
  Existing glyph and icon assets, theme tokens and Raised metrics stay
  shared with the rest of the shell.
- accesskit 0.25 core describes roles, labels, values and supported
  actions. There is no platform adapter or additional D-Bus stack.

## Tree

A popup retains nodes with stable ids derived from their identity, an
accesskit node, a row style mapped to taffy and computed bounds. The
container owns keyboard selection, focus and scroll. The same computed
bounds drive paint, pointer hit testing and the API. Search retains its
Editor across changes to the result list. Up and Down skip headings and
separators; Enter runs the selected action and Escape closes the sheet.
Pointer hover selects through the same state.

The popup X window is long-lived. Geometry requests and its shape update
only when geometry changes; painting precedes the first map. Changes to
selection or the caret repaint without mapping or configuring the window.
There is no timer when idle.

## Semantics API

`GET /tree` returns a flat list of the open shell popup nodes, each with
`id`, `role`, `name`, `value`, `states`, `actions` and `bounds`. Bounds are
screen pixels. States include focused, selected, disabled and expanded.
The menu container includes child ids. An empty list means no retained
shell UI is open.

`POST /tree/action` takes `{id, action, value?}`. The actions are `click`,
`focus` and `set_value`. Unsupported actions return 400 and absent nodes
return 404. Activation uses the menu input action path; setting a search
value uses the Editor and the normal result refresh path. This endpoint
only describes shell UI, not application accessibility trees.

## Scope and order

This slice ports launcher and app menu rows, layout, hit testing,
selection and search editing. The connected shell outline, panel and rail
pill labels still share the existing chrome drawing routines. The rest
of the shell stays on draw.rs. Small antialiasing differences at widget
curves can result from the new painter.

Next come the panel, rail and title bars, in that order, then a Settings
app using the same tree. The shell tree list has a single collection
point where those surfaces can join the popup nodes.

AT-SPI publishing through accesskit_unix is later work, once its memory,
CPU and D-Bus dependency cost have been measured. This slice keeps zbus 6.

## Tests

Layout tests cover a single action plus padding and a launcher with all
its rows. Navigation tests skip non-actionable rows, wrap and scroll.
Editor tests use the bundled Inter font without reading system fonts and
cover word motion, selection replacement and word deletion. JSON tests
check roles, values, actions, states and screen coordinates.
