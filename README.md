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
| Ctrl+Shift+Y | Sync with Dropbox now |
| Ctrl+Shift+D | Connect (or reconnect) Dropbox |
| Ctrl+Q | Quit (saves first) |
| Return in the list | Go to the editor |

## Your notes

One note = one plain UTF-8 text file `<title>.txt` in `~/Notes` (created on first run).
The title is the filename.
Edit them with anything; changes made outside the app show up live, and if both sides changed, your edits are kept as a `<title> (conflict YYYY-MM-DD HHMM)` copy — nothing is ever lost.
Deleting moves notes to the desktop Trash, never a permanent delete.
A file that isn't valid UTF-8 opens read-only and is never rewritten.

## Dropbox sync

Dropbox sync is optional; it keeps your notes folder in step with a private folder in your Dropbox (`Dropbox/Apps/<your app name>/`), so notes follow you between machines.
Only note files with the configured extension sync; nothing else in the folder does.

Create your own Dropbox app once:

1. Go to https://www.dropbox.com/developers/apps → Create app → Scoped access → App folder → give it a unique name, such as `nv-linux-<yourname>`.
2. On the Permissions tab, tick `account_info.read`, `files.metadata.read`, `files.metadata.write`, `files.content.read`, and `files.content.write` → Submit.
3. On the Settings tab, copy the App key, not the app secret; nv-linux never uses a secret.
   Leave Redirect URIs empty.
4. Add it to `~/.config/nv-linux/config.toml`:

   ```toml
   [sync]
   dropbox_app_key = "<your app key>"
   ```

The app stays in Dropbox's "development" status, limited to your own account, which is all it needs.

In nv-linux, press Ctrl+Shift+D to open Dropbox's allow page in your browser, then paste the code Dropbox shows into the search field (hidden as you type) and press Enter.
The app says `Connected to Dropbox as <email>`.
From a terminal, `nv-linux sync connect` reads the code from the terminal, runs a first sync, and prints how many notes went up and came down; `--no-open` skips opening the browser.

A small mark at the bottom-right of the window shows ↻ while syncing, ✓ with how long ago it synced, `offline`, `reconnect Dropbox · Ctrl+Shift+D` when the sign-in has expired, or ⚠ with a count of conflict copies.
Nothing is shown when sync isn't connected.

- Sync runs on launch, then within a couple of seconds of any change on either side while the app is open; Ctrl+Shift+Y syncs now.
- If a note changed in both places, both versions are kept: the note keeps this machine's text and the other version becomes a `<title> (conflict YYYY-MM-DD HHMM)` note — nothing is lost.
- A note deleted in Dropbox goes to your desktop Trash; a note you delete here is deleted in Dropbox, which keeps it in its deleted files.
- If one pass would remove more than 10 notes, or at least 3 notes and more than half of them, sync stops and asks you to check, then run `nv-linux sync now --allow-deletes`.
- Files of 150 MB or more are skipped.

Your sign-in lives in your desktop keyring (Secret Service, such as GNOME Keyring), never in the config file.
If no keyring is available, it falls back to a private file at `~/.local/state/nv-linux/dropbox-login.json`, readable only by you.
Sync bookkeeping lives in `~/.local/state/nv-linux/`.

**Don't also point the Dropbox desktop client at your notes folder — two syncers on one folder fight; nv-linux refuses to sync a notes folder inside the Dropbox client's folder.**

| Command | Action |
| --- | --- |
| `nv-linux sync connect` | Connect to Dropbox and run a first sync |
| `nv-linux sync status` | Show account, last sync, waiting changes, and conflict copies; exit code 0 when healthy, 1 when something needs you — scriptable |
| `nv-linux sync now [--allow-deletes]` | Run one sync pass |
| `nv-linux sync disconnect` | Sign out and forget sync bookkeeping; never touches a note, here or in Dropbox |

To turn sync off without disconnecting, set `enabled = false` under `[sync]`.

## Settings

`~/.config/nv-linux/config.toml` is written with defaults on first run:

```toml
notes_dir = "/home/you/Notes"
extension = "txt"      # or "md"
sort = "modified"      # or "title"
autosave_ms = 500
```

The optional `[sync]` table is described under [Dropbox sync](#dropbox-sync).

Colours follow your current Omarchy theme (`~/.local/state/omarchy/current`) and change live when you switch themes.
The app uses the system `monospace` font, selected by Omarchy's font setting (Omarchy menu > Style > Font).

Environment overrides (handy for testing):

- `NV_NOTES_DIR`: notes folder, not saved to config.
- `NV_CONFIG_DIR`: config folder.
- `NV_STATE_DIR`: sync state folder (instead of `~/.local/state/nv-linux`).
- `NV_THEME_DIR`: stands in for `~/.local/state/omarchy/current`.
- `NV_DEV=1`: development app id, allows a second copy alongside the installed one.

## Build from source

Use `mise exec -- cargo run -p nv-app` for a debug run; `mise exec -- cargo test --workspace` for the tests.
Workspace layout: `crates/nv-core` (engine, no GUI), `crates/nv-sync` (Dropbox sync engine, no GUI), and `crates/nv-app` (the GTK 4 window).

## Licence

GPL-3.0-or-later, see LICENSE; behaviour reimplemented from the [GPL-3.0 original](https://github.com/scrod/nv).

State: see STATE.md
