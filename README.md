# twain

A diff viewer for git repositories. Like meld, but every changed file is shown in one
scrolling sequence instead of one tab per file.

![twain showing a diff](screenshot.png)

## Usage

Run `twain` inside your project and it shows all your pending changes: staged, unstaged and
untracked. It can also compare revisions (`twain main`, `twain v1.0 v1.1`) or just what's
staged (`twain --staged`); see `twain --help`.

## Comments

Select lines and press `C` to leave a comment on them. Comments can also be listed, added
and answered from outside the app with `twain comment`, so scripts and coding agents can
take part in the review, and open windows update immediately. See `twain comment --help`.

## Install

Download the Flatpak bundle from the [latest release](https://github.com/andunieee/twain/releases/latest)
and run:

```
flatpak install twain-linux-x86_64.flatpak
```

Or build from source (needs GTK 4, GtkSourceView 5 and libadwaita):

```
cargo install --git https://github.com/andunieee/twain
```
