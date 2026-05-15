# CLI keybindings

Keep this document in sync with `crates/tau-config/config/built-in.cli-bindings.json5` and the action parser in `crates/tau-cli-term/src/lib.rs`.


## Built-in bindings

| Key | Action | Description |
| --- | --- | --- |
| `C-f` | `shell-prompt-insert` | Pick a file with `fzf` and insert it at the cursor. |
| `C-r` | `shell-prompt-insert` | Search files with ripgrep through `fzf` and insert the selected path. |
| `C-s` | `role-cycle` | Cycle to the next agent role. |
| `C-k`, `C-Up` | `prompt-previous` | Move to the previous prompt/history entry. |
| `C-j`, `C-Down` | `prompt-next` | Move to the next prompt/history entry. |
| `C-z` | `prompt-undo` | Undo the last edit in the current prompt/history entry. |
| `C-y` | `prompt-redo` | Redo the last undone edit in the current prompt/history entry. |
| `C-o`, `C-g` | `shell-prompt-edit` | Edit the current prompt in `$TAU_EDITOR`. |


## Built-in editing keys

These keys are handled by the raw terminal prompt and are not configurable bindings.

| Key | Behavior |
| --- | --- |
| `Enter` | Submit the prompt. |
| `Shift-Enter`, `Alt-Enter` | Insert a newline. |
| `C-d` on an empty prompt | Exit Tau. |
| `C-c` on an empty prompt | Print `Use Ctrl+D to exit`; does not exit. |
| `C-c` on a non-empty prompt | Clear the prompt; undoable with `prompt-undo`. |
| `C-a` / `Home` | Move to the beginning of the prompt. |
| `C-e` / `End` | Move to the end of the prompt. |
| `C-u` | Kill from cursor to the beginning of the prompt. |
| `C-w` | Kill the word before the cursor. |
| `Backspace`, `Delete` | Delete text around the cursor. |
| Arrow keys | Move within multiline input, completion candidates, or prompt history. |
| `Tab`, `Shift-Tab` | Cycle completion candidates; `Shift-Tab` outside completion is exposed to the CLI. |
| `Esc` | Dismiss the completion menu. |


## Configurable actions

Bindings live under `cli.bind` in config. The built-in bindings are merged below user bindings, so configuring one key does not remove the rest.

- `prompt-previous` — move backward in prompt history.
- `prompt-next` — move forward in prompt history.
- `prompt-undo` — undo an edit in the current prompt/history entry.
- `prompt-redo` — redo an undone edit in the current prompt/history entry.
- `fast-toggle` — toggle fast mode without editing the prompt draft.
- `role-cycle` — cycle to the next agent role.
- `shell-prompt-insert` — run `command` and insert stdout at the cursor.
- `shell-prompt-edit` — run `command` with the current prompt in `$TAU_PROMPT_PATH` and replace the prompt with the edited file content.
