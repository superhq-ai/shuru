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

`GET /tree` returns the nodes agents see in the panel, rail, title bars and open
shell popup, each
with `id`, `role`, `name`, `value`, `states`, `actions` and `bounds`, and
`children`, the ids of the nearest seen nodes under it. Presentational
nodes (labels inside a row, icons, key caps) are left out, so the menu
lists its items and the search input. Bounds are screen pixels, after
scrolling. States are focused, selected, disabled, expanded and checked
(a checked radio or switch). The panel and rail remain in the list while
no popup is open.

`POST /tree/action` takes `{id, action, value?}`. The actions are `click`,
`focus` and `set_value`. Unsupported actions return 400 and absent nodes
return 404. Activation uses the menu input action path; setting a search
value uses the Editor and the normal result refresh path. This endpoint
only describes shell UI, not application accessibility trees.

## Scope and order

This slice ports launcher and app menu rows, layout, hit testing,
selection and search editing. The panel and rail each retain a tree with taffy layout, semantic buttons
and hit testing by node. Their Content variants share the existing pill
labels and icon drawing routines to preserve the pixels. The connected
shell outline and the rest of the shell stay on draw.rs. Small antialiasing differences at widget
curves can result from the new painter.

Title bars retain their own trees, with named buttons for Close, Minimize
and Maximize or Restore. Pointer release and semantic clicks use the same
window actions. Bars of minimized and fullscreen windows leave the shell
collection. The shell tree list has a single collection point where those
surfaces join the popup nodes.

Content that only one surface draws is a `Draw` implementation owned by
that surface (`Content::Custom`), so the shell faces live in `chrome` and
the form controls in the kit, and `widgets.rs` keeps the shared leaves:
text, wrapped text, glyphs, icons, key caps, rules and the text field.

## Kit

`src/ui/kit` holds the form controls Settings and later windows build
from, one file per component: the card and row surfaces, headings, a
status line, an information row, a segmented control (a radio group), a
select with an inline list box, and a switch. A `Form` builds a window of
sections from them. Each component lays out, paints and describes itself
in one place, and keeps one accessibility contract:

- A control has a role, a name and its state: checked radios and switches
  carry `Toggled`, an open select `expanded`, the chosen option
  `selected`, a control that cannot act `disabled`.
- Tab and Shift+Tab walk the controls in reading order, arrows move and
  check within a radio group, Enter and Space activate, Escape closes an
  open list. Page Up, Page Down, Home and End scroll.
- The focused control shows a ring at 3:1 against its surface, only for
  the keyboard. Text meets WCAG AA (4.5:1) on its surface in both
  appearances, checked by tests from the theme colors.
- Help under a control is also its description. A section is a group
  named by its level 2 heading; the window title is level 1.
- A status line is a live region: polite, assertive for an error.
- A form taller than its window shows a scrollbar. The pointer drags its
  thumb or pages on its track, and it is an accesskit scroll bar with a
  value that assistive tools read and set.
- Text in another language, such as the name of a language in itself,
  carries its language tag.

## Settings

Settings uses the kit as a normal managed X11 client, launched by
`shuru-desktop --settings`, and publishes its own retained tree through
the shared AT-SPI module. It has four sections: General (appearance and
language), Default apps (browser and terminal), Notifications (do not
disturb) and About. Every setting is a file in `~/.config/superhq` or an
XDG file, written by a rename, and both Settings and the window manager
watch the directory with inotify, so a change from either, the API or a
shell applies live in both.

Language is the file `language`, holding `en_US.UTF-8` or `zh_CN.UTF-8`.
The desktop seeds it from `LC_ALL`, `LC_MESSAGES` or `LANG` when there is
none, and from then on the file wins: the runtime picks the first
language and the person changes it inside the guest. A change switches
the panel, rail, title bars, menus and Settings at once; apps that are
open keep theirs until they start again.

The default browser is the handler of `x-scheme-handler/http` in
`mimeapps.list`; choosing one sets HTTP, HTTPS, HTML and XHTML in the
user file and keeps every other line. The default terminal is the first
entry of `xdg-terminals.list`, as `xdg-terminal-exec` reads it. Desktop
entries with `Terminal=true` run in it, with its `X-TerminalArgExec` or
`-e`, and in xterm when no terminal is set. Addresses typed in the
launcher open in the default browser.

## Notification center

A bell at the right end of the panel opens the notification center, a
sheet aligned to the right edge of its pill. The bell shows an amber dot
when a notification came since the center was last open, and a slash
while do not disturb is on.

The center lists the notifications that are not closed, newest first:
the app icon and name, the summary and the body on one line, the actions
of the notification under it, a dismiss button per notification, and
Clear all. A do not disturb switch sits at the top. A click on a
notification runs its default action when it has one and dismisses it
either way; Delete and BackSpace dismiss the one under the keyboard.
Dismissing, clearing and the switch keep the sheet open and rebuild it.

A banner that times out leaves its notification in the center, open to
its actions; its client hears `NotificationClosed` only when it is
dismissed, cleared, closed by the client or pushed out of the last 50.
Do not disturb is the file `do-not-disturb`, `on` or `off`; while it is
on, new notifications go to the center without a banner. The rows are
menu items, the switch a checkable menu item and the dismiss buttons are
named after their notification, so the center reads the same on AT-SPI
and in `GET /tree`.

AT-SPI publishing lets the shell join GTK apps and Chromium
in one accessibility tree. accesskit_atspi_common, which maps an
accesskit tree to AT-SPI and has no D-Bus code, stays a dependency; the
D-Bus side that accesskit_unix provides on zbus 5 and its own executor
lives here on zbus 6 and the tokio runtime of the desktop: finding
the accessibility bus, registering with the registry, serving the
Accessible, Component, Action, Text, Value and Selection interfaces of
each node, and sending events. The bridge sends retained tree changes after input settles in the window
manager loop, without polling the accessibility bus or adding an idle
timer. Missing accessibility services leave the desktop operational. The
guest needs at-spi2-core; the console image already installs it, and the
Chromium desktop entry already enables renderer accessibility.

## Tests

Layout tests cover a one item menu, a launcher with all its rows and the
place of its text field, an app menu that widens for a long action, and
a label that gives way to a detail in its row. Paint tests check that a
scrolled selection stays inside its view. Navigation tests skip non-actionable rows, wrap and scroll.
Editor tests use the bundled Inter font without reading system fonts and
cover word motion, selection replacement and word deletion. JSON tests
check roles, values, actions, states and screen coordinates.

Kit tests cover the radio group and its keys, a closed, open and empty
select, help as description, language tags, heading levels, status
liveness, the scrollbar and its dragging, and a focus ring for the
keyboard only in both themes. Settings tests cover each default, a failed
save, the reading order and the form in Chinese. Center tests cover the
dismiss button apart from its row, the switch semantics, keyboard order
and the empty center. Contrast tests check text, focus rings and thumbs
in both themes.
