# nv-linux

nv-linux is a native Linux rebuild of [Notational Velocity](https://github.com/scrod/nv), the modeless, mouseless Mac note app, for Omarchy (Arch + Hyprland).
Searching is creating: one box at the top, every keystroke filters every note, Return opens the match or creates a new note.
Notes are plain text files in a folder you own, and the app wears your current Omarchy theme and font.

**Status:** usable; packaged for Arch/Omarchy.

## Install (Arch / Omarchy)

You need gtk4 (already on Omarchy) and a Rust toolchain via mise; the repo's `mise.toml` pins it.
From a normal terminal (sudo is asked for only at the install step):

```sh
cd packaging
mise exec -- makepkg -si -d
```

mise supplies cargo.
`-d` stops pacman from installing a second, system Rust just to satisfy makedepends.
This installs `/usr/bin/nv-linux`, the launcher entry and the icon, so it shows in the Omarchy app launcher (Super+Space).
Uninstall with `sudo pacman -R nv-linux`.

## Open it with Super+Alt+N

Omarchy's Hyprland config is Lua.
Add this line to `~/.config/hypr/bindings.lua`, then save (Hyprland reloads automatically):

```lua
o.bind("SUPER + ALT + N", "Notes", "nv-linux")
```

Pressing it again while nv-linux is running brings the existing window forward (single instance).

## How it works

Searching is creating: type in the box and every keystroke filters every note; all your words must appear, in order, in the title or in the body.
Return opens the match or creates a note titled with what you typed.
Typing autocompletes the best-matching title.
Esc from anywhere clears the search.
Edits save automatically.

## Keys

Search field:

| Key | Action |
| --- | --- |
| Typing | Filter + autocomplete |
| Return | Open the selected note, or create one named after what you typed |
| Up/Down | Move through the list |
| Tab | Jump to the editor |
| Ctrl+Home / Ctrl+End | First / last note |
| Home / End | Caret to start / end |
| Ctrl+U | Delete to start of field |
| Ctrl+Backspace | Delete word backward |

Everywhere:

| Key | Action |
| --- | --- |
| Esc | Clear search, show all notes, back to the search field |
| Ctrl+L | Focus search |
| Ctrl+J / Ctrl+K | Next / previous note |
| Ctrl+R | Rename the selected note |
| Ctrl+Shift+Delete | Move the selected note to the Trash |
| Ctrl+S | Restore a note that was deleted on disk while open |
| Ctrl+Z / Ctrl+Y | Undo / redo in the editor |
| Ctrl+Q | Quit (saves first) |
| Return in the list | Go to the editor |

## Your notes

One note = one plain UTF-8 text file `<title>.txt` in `~/Notes` (created on first run).
The title is the filename.
Edit them with anything; changes made outside the app show up live, and if both sides changed, your edits are kept as a `<title> (conflict YYYY-MM-DD HHMM)` copy — nothing is ever lost.
Deleting moves notes to the desktop Trash, never a permanent delete.
A file that isn't valid UTF-8 opens read-only and is never rewritten.

## Settings

`~/.config/nv-linux/config.toml` is written with defaults on first run:

```toml
notes_dir = "/home/you/Notes"
extension = "txt"      # or "md"
sort = "modified"      # or "title"
autosave_ms = 500
```

Colours follow your current Omarchy theme (`~/.local/state/omarchy/current`) and change live when you switch themes.
The app uses the system `monospace` font, selected by Omarchy's font setting (Omarchy menu > Style > Font).

Environment overrides (handy for testing):

- `NV_NOTES_DIR`: notes folder, not saved to config.
- `NV_CONFIG_DIR`: config folder.
- `NV_THEME_DIR`: stands in for `~/.local/state/omarchy/current`.
- `NV_DEV=1`: development app id, allows a second copy alongside the installed one.

## Build from source

Use `mise exec -- cargo run -p nv-app` for a debug run; `mise exec -- cargo test --workspace` for the tests.
Workspace layout: `crates/nv-core` (engine, no GUI) and `crates/nv-app` (the GTK 4 window).

## Licence

GPL-3.0-or-later, see LICENSE; behaviour reimplemented from the [GPL-3.0 original](https://github.com/scrod/nv).

State: see STATE.md
