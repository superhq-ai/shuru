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

- taffy 0.15 lays out nested nodes with the part of flexbox the shell
  uses: rows and columns, padding, margins, fixed and least and most
  widths, and children that grow or give way. Leaves are measured from
  their content, text by its shaped width, so a sheet is as tall as its
  rows and an app menu as wide as its longest action, within bounds. A
  viewport the height of the screen clips long menus, and the selection
  scrolls into view. Nodes keep a style of their own, which taffy styles
  are made from at layout: those are not `Send`, and the window manager
  state moves to its own thread.
- cosmic-text 0.19 shapes text as before. Its Editor owns the search
  value, cursor and selection. The caret stays steady: the desktop has
  no animation. The existing X11 connection handles UTF8_STRING on
  CLIPBOARD, including copy, cut and paste.
- tiny-skia 0.12 rasterises shapes into masks of only the pixels they
  touch, blended into the existing 0RGB canvas, which gains a clip
  rectangle that every drawing call keeps to. The painter is a few
  functions, so vello_cpu can take its place later. Glyphs, icons, theme
  colors and the Raised family stay shared with the rest of the shell.
- accesskit 0.25 core describes roles, labels, values and supported
  actions. There is no platform adapter or additional D-Bus stack.

## Tree

A popup retains a tree of nodes. Each has a style, a content (a choice,
text, a glyph, an app icon, a key cap, a hairline, the text field) and
children; the ones agents see also carry an accesskit node and an id
stable for what they are, such as the action of a menu item. The rows of
a sheet are the children of its root, and the tree owns keyboard
selection, focus and scroll over them. The same computed
bounds drive paint, pointer hit testing and the API. Search retains its
Editor across changes to the result list. Up and Down skip headings and
separators; Enter runs the selected action and Escape closes the sheet.
Pointer hover selects through the same state.

The popup X window is long-lived. Geometry requests and its shape update
only when geometry changes; painting precedes the first map. Changes to
selection or the caret repaint without mapping or configuring the window.
There is no timer when idle.

## Semantics API

`GET /tree` returns the nodes agents see in the open shell popup, each
with `id`, `role`, `name`, `value`, `states`, `actions` and `bounds`, and
`children`, the ids of the nearest seen nodes under it. Presentational
nodes (labels inside a row, icons, key caps) are left out, so the menu
lists its items and the search input. Bounds are screen pixels, after
scrolling. States are focused, selected, disabled and expanded. An empty
list means no retained shell UI is open.

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

AT-SPI publishing comes next, so the shell joins GTK apps and Chromium
in one accessibility tree. accesskit_atspi_common, which maps an
accesskit tree to AT-SPI and has no D-Bus code, stays a dependency; the
D-Bus side that accesskit_unix provides on zbus 5 and its own executor
is written here on zbus 6 and the tokio runtime of the desktop: finding
the accessibility bus, registering with the registry, serving the
Accessible, Component, Action, Text, Value and Selection interfaces of
each node, and sending events. The guest image gains at-spi2-core, and
Chromium starts with renderer accessibility.

## Tests

Layout tests cover a one item menu, a launcher with all its rows and the
place of its text field, an app menu that widens for a long action, and
a label that gives way to a detail in its row. Paint tests check that a
scrolled selection stays inside its view. Navigation tests skip non-actionable rows, wrap and scroll.
Editor tests use the bundled Inter font without reading system fonts and
cover word motion, selection replacement and word deletion. JSON tests
check roles, values, actions, states and screen coordinates.
