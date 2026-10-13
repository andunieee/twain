# twain

A GTK4 diff viewer for git repositories. Like meld, but shows every changed file in one
scrolling sequence instead of one tab per file.

- Side-by-side diffs like meld: old and new files each flow normally in their own column with
  its own scrollbar, kept in sync as you scroll, with curved links between changed chunks and
  intra-line highlights
- Only changed regions are shown (3 lines of context); click a file header to see the full file
- Syntax highlighting and real line numbers (GtkSourceView)
- File tree sidebar: click to jump to a file, double-click to show it in full
  (menu → Show Unchanged Files adds the rest of the repository, dimmed)
- Ctrl+F search across the whole diff

## Usage

```
twain                 HEAD vs working tree (staged + unstaged + untracked)
twain --staged        HEAD vs index
twain <rev>           <rev> vs working tree
twain <a> <b>         <a> vs <b>   (also <a>..<b> and <a>...<b>)
twain -C <dir> ...    run in another repository
```

| Key                         | Action                 |
| --------------------------- | ---------------------- |
| Ctrl+F                      | Search                 |
| Enter / Ctrl+G / F3         | Next match             |
| Shift+Enter / Ctrl+Shift+G  | Previous match         |
| F5 / Ctrl+R                 | Reload                 |
| Ctrl+W / Ctrl+Q             | Close                  |

## Comments

Select one or more lines in either side and press `C` (or right-click → Add Comment) to
comment on them; Ctrl+Enter saves. Comments can be edited, resolved and deleted from their
card, and follow their lines as the file changes (or are marked *outdated* when the lines
are gone).

Comments are stored in an LMDB database in `~/.local/share/twain/comments`, shared by every
twain process, so agents and scripts can read and write them with `twain comment`, and open
windows update immediately:

```
twain comment list [--file <path>] [--unresolved] [--json]
twain comment add --file src/main.rs --line 10-14 -m "Why not use a BTreeMap here?"
twain comment edit <id> -m <text>          twain comment resolve|unresolve <id>...
twain comment rm <id>...                   twain comment clear [--file <path>]
twain comment watch [--once] [--not-author agent] [--timeout <secs>] [--json]
```

Line numbers are 1-based and refer to the working tree file unless `--old` is given. `watch`
blocks until comments change, so an agent can wait for a reply from the person reviewing:
`twain comment watch --once --not-author agent`. See `twain comment --help`.

Keys are `<repo root>\0<path>\0<id>`; each process listening for changes binds a datagram
socket at `~/.local/share/twain/processes/<pid>`, and writers send the keys they write to
all of them.

## Building

Needs GTK ≥ 4.12, GtkSourceView 5 and libadwaita ≥ 1.6 (on Arch:
`pacman -S gtk4 gtksourceview5 libadwaita`).

```
cargo install --path .
```
