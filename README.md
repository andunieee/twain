# twain

A GTK4 diff viewer for git repositories. Like meld, but shows every changed file in one
scrolling sequence instead of one tab per file.

- Side-by-side diffs with meld-style curved links between changed chunks and intra-line highlights
- Only changed regions are shown (3 lines of context); click a file header to see the full file
- Syntax highlighting and real line numbers (GtkSourceView)
- File tree sidebar: click to jump to a file, double-click to show it in full
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

## Building

Needs GTK ≥ 4.12, GtkSourceView 5 and libadwaita ≥ 1.6 (on Arch:
`pacman -S gtk4 gtksourceview5 libadwaita`).

```
cargo install --path .
```
