# Guest settings for shuru-desktop

What a Debian trixie guest image installs next to `shuru-desktop`, and
where. Paths are for an image that runs the desktop as one user; the
files under `~` go in the home of that user.

Each desktop release carries this directory as `shuru-desktop-guest.tar.gz`,
listed in its `SHA256SUMS`.

| file | installs to | does |
|---|---|---|
| `applications/*.desktop` | `/usr/local/share/applications/` | the apps of the rail, with their names and actions |
| `icons/hicolor/` | `/usr/local/share/icons/hicolor/` | the terminal icon at 16, 32 and 64 px; `icons/src/` holds its SVGs |
| `themes/SuperHQ/` | `/usr/share/themes/SuperHQ/` | the light GTK 3 theme (the default) |
| `themes/SuperHQ-dark/` | `/usr/share/themes/SuperHQ-dark/` | the dark GTK 3 theme |
| `gtk-3.0/settings.ini` | `/etc/gtk-3.0/settings.ini` | the theme, Inter, the window buttons on the left, no animations |
| `chromium/initial_preferences` | `/usr/lib/chromium/initial_preferences` | the first run settings of a new Chromium profile |
| `chromium/policy.json` | `/etc/chromium/policies/managed/superhq.json` | no sign in, no sync, no promotions, no default browser check, no password manager |
| `xterm/XTerm` | appended to `/etc/X11/app-defaults/XTerm` | the terminal colors and font, titles in UTF-8 |
| `mimeapps.list` | `/etc/xdg/mimeapps.list` | which app opens what |

## Packages

```sh
apt-get install --no-install-recommends \
  xvfb dbus-x11 dbus-user-session libglib2.0-bin dconf-gsettings-backend \
  adwaita-icon-theme fontconfig locales shared-mime-info xdg-utils \
  chromium chromium-l10n xterm
```

## Display

```sh
Xvfb :1 -screen 0 1280x800x24 -shmem -dpi 96 -s 0 -nodpms
```

`-s 0` turns the screen saver off and `-nodpms` power management, so the
screen never blanks while an agent works without input for a while.
`shuru-desktop` also sets the screen saver timeout to 0 when it starts.

## Session bus

`shuru-desktop` serves notifications and the appearance portal on the
session bus, and `gsettings` and Chromium use it too. The runtime starts
one for the desktop user and runs the desktop inside it, so every app
`shuru-desktop` launches shares it:

```sh
dbus-run-session -- shuru-desktop --display :1 --socket /run/shuru-desktop.sock --apps ...
```

or starts `dbus-daemon --session --fork --print-address` and passes
`DBUS_SESSION_BUS_ADDRESS` in the environment of the desktop service.

## Fonts

Inter is built into `shuru-desktop`. For other scripts the shell and the
apps fall back to the system fonts. The desktop release carries two,
pinned and listed in its `SHA256SUMS`:

- `noto-sans-cjk-sc-regular.otf`: Noto Sans CJK SC Regular, from
  googlefonts/noto-cjk `Sans2.004`, for Chinese and Japanese.
- `noto-color-emoji.ttf`: Noto Color Emoji, from googlefonts/noto-emoji
  `v2.051`, for emoji.

Setup downloads them from the release, checks them against `SHA256SUMS`,
and installs them:

```sh
install -d /usr/local/share/fonts
install -m 0644 noto-sans-cjk-sc-regular.otf noto-color-emoji.ttf /usr/local/share/fonts/
fc-cache -f
```

Do not install `fonts-noto-cjk`: it brings every CJK weight and region,
hundreds of MB, where one regular face is enough. `fonts-noto-core`
covers Arabic, Devanagari and other scripts if an image wants them.

## Locale

English (US) is the default; Simplified Chinese is the other language of
the shell. Generate both:

```sh
sed -i -e 's/^# *en_US.UTF-8/en_US.UTF-8/' -e 's/^# *zh_CN.UTF-8/zh_CN.UTF-8/' /etc/locale.gen
locale-gen
```

The runtime sets `LANG` in the environment of the desktop service,
`en_US.UTF-8` unless the user picked another language, and
`shuru-desktop` passes its environment to every app it launches. So with
`LANG=zh_CN.UTF-8` the shell, the `.desktop` names and Chromium (with
`chromium-l10n`) are in Chinese. Dates and numbers follow the locale
(en_US by default). The shell reads `LC_ALL`, then `LC_MESSAGES`, then
`LANG`.

## Cursors

`adwaita-icon-theme` provides the Adwaita cursors. `shuru-desktop` uses
the theme named by `XCURSOR_THEME` or the `Xcursor.theme` resource, and
Adwaita when neither is set. Set `XCURSOR_THEME=Adwaita` in the desktop
service so apps use the same one.

## Appearance

The setting is `~/.config/superhq/appearance`, `light` or `dark`; light
when the file is missing. `shuru-desktop` watches it and switches live:
the shell, the GTK theme over XSETTINGS, `color-scheme` in gsettings, the
portal setting, and `~/.config/gtk-3.0/settings.ini`. The two GTK themes
draw the window buttons as the shell draws its own: red, yellow and green
on the focused window, gray on the others.

## Chromium

`initial_preferences` and the policy give a new profile no first run
screens and no sign in prompt. Do not pass `--no-first-run`: it skips
`initial_preferences` too. Chromium follows the GTK theme for its frame
and for `prefers-color-scheme`; nothing in the flags or preferences pins
either.

## Default apps

The guest ships two apps, Chromium and the terminal. `mimeapps.list`
opens PDF, HTML, images, plain text, Markdown and directories in
Chromium, so `xdg-open` works for each of them. An image that adds an
editor or a file manager points the types at its ids.

## Icons

The shell draws app icons at 16 and 32 px (and 64 for HiDPI) from the
PNGs of the icon theme and `pixmaps`, scaled from the nearest size. The
terminal has an icon of its own, `superhq-terminal`, drawn at each size
from `icons/src/` (the 16 px one from its own simpler SVG):

```sh
rsvg-convert -w 64 -h 64 icons/src/superhq-terminal.svg -o icons/hicolor/64x64/apps/superhq-terminal.png
```
