# nv-linux — STATE

## Where we left off
Checkpoint 2 landed: the NV loop works in a real window — type-to-search, autocomplete (gro|ceries), Return-to-create, Up/Down/Tab/Esc, Ctrl+J/K, per-note undo, autosave, and nothing lost on close or Ctrl+Q. Level 4 steps 1–8 pass with screenshots.

## Current focus
Checkpoint 3: live file changes, conflict copies, rename, Trash, restore, and live Omarchy theming.

## Next (1-3)
- [ ] Wire the watcher (external edits, deletes, renames) into the window
- [ ] Ctrl+R rename · Ctrl+Shift+Delete trash · Ctrl+S restore
- [ ] theming.rs live reload, then Level 4 steps 9–14

## Key files
- crates/nv-core (engine, no GUI) · crates/nv-app (GTK 4 window)

## Links
- Brain meta-note -> glitch-mem/Memory/projects/nv-linux.md
- People ->
- Live URL ->
