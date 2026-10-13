//! `twain comment ...`: reading and writing comments from the command line, for agents and
//! scripts. Talks to the same database as the windows, which pick up changes immediately.

use crate::comments::{self, Comment, Side, Store};
use crate::git;
use std::collections::HashMap;
use std::io::{IsTerminal, Read};
use std::path::{Component, PathBuf};

pub const USAGE: &str = "\
usage: twain comment <command> [-C <dir>] [options]

Comments are attached to lines of files in a git repository. Line numbers are 1-based and,
unless --old is given, refer to the file as it is in the working tree (the right-hand side of
the diff). Comments follow their lines when the file changes; when the lines are gone the
comment is reported as outdated.

commands:
  list [--file <path>] [--unresolved] [--author <name>] [--json]
      Show comments, with the code they're attached to.
  add --file <path> --line <n>[-<m>] [--old] [--author <name>] [-m <text>]...
      Add a comment on line n (through m). The text comes from -m (repeat for paragraphs)
      or stdin. The author defaults to \"agent\". Prints the new comment's id.
  edit <id> [-m <text>]...
      Replace a comment's text (from -m or stdin).
  resolve <id>...  /  unresolve <id>...
  rm <id>...
  clear [--file <path>]
      Delete every comment in the repository, or in one file.
  watch [--file <path>] [--author <name>] [--not-author <name>] [--once] [--timeout <secs>] [--json]
      Wait for comments to be added, changed or removed, and print each change. With
      --once, exit after the first one; e.g. `twain comment watch --once --not-author agent`
      waits for a human to say something.

Ids may be abbreviated to any unambiguous prefix. -C <dir> runs in another repository.";

#[derive(Default)]
struct Args {
    dir: Option<PathBuf>,
    file: Option<String>,
    line: Option<String>,
    old: bool,
    author: Option<String>,
    not_author: Option<String>,
    messages: Vec<String>,
    json: bool,
    unresolved: bool,
    once: bool,
    timeout: Option<u64>,
    ids: Vec<String>,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "-C" => a.dir = Some(value("-C")?.into()),
            "-f" | "--file" => a.file = Some(value("--file")?),
            "-l" | "--line" | "--lines" => a.line = Some(value("--line")?),
            "--old" => a.old = true,
            "--new" => a.old = false,
            "--author" => a.author = Some(value("--author")?),
            "--not-author" => a.not_author = Some(value("--not-author")?),
            "-m" | "--message" => a.messages.push(value("-m")?),
            "--json" => a.json = true,
            "--unresolved" => a.unresolved = true,
            "--once" => a.once = true,
            "--timeout" => {
                let v = value("--timeout")?;
                a.timeout = Some(v.parse().map_err(|_| format!("bad timeout: {v}"))?);
            }
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option: {s}")),
            _ => a.ids.push(arg),
        }
    }
    Ok(a)
}

/// Runs `twain comment <args>`; returns the exit status.
pub fn run(mut args: impl Iterator<Item = String>) -> i32 {
    let Some(cmd) = args.next() else {
        eprintln!("{USAGE}");
        return 2;
    };
    if matches!(cmd.as_str(), "-h" | "--help" | "help") {
        println!("{USAGE}");
        return 0;
    }
    let result = parse(args).and_then(|a| {
        let ctx = Ctx::new(&a)?;
        match cmd.as_str() {
            "list" | "ls" => ctx.list(&a),
            "add" => ctx.add(&a),
            "edit" => ctx.edit(&a),
            "resolve" => ctx.set_resolved(&a, true),
            "unresolve" => ctx.set_resolved(&a, false),
            "rm" | "delete" => ctx.rm(&a),
            "clear" => ctx.clear(&a),
            "watch" => ctx.watch(&a),
            _ => Err(format!("unknown command: {cmd}\n\n{USAGE}")),
        }
    });
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("twain: {e}");
            1
        }
    }
}

struct Ctx {
    store: &'static Store,
    /// Canonical repository root, as the windows see it.
    root: PathBuf,
    root_key: String,
    /// Directory relative paths are resolved against.
    base: PathBuf,
}

impl Ctx {
    fn new(a: &Args) -> Result<Ctx, String> {
        let root = git::repo_root(a.dir.as_deref())?;
        let base = match &a.dir {
            Some(d) => d.clone(),
            None => std::env::current_dir().map_err(|e| e.to_string())?,
        };
        let base = base
            .canonicalize()
            .map_err(|e| format!("{}: {e}", base.display()))?;
        Ok(Ctx {
            store: comments::store()?,
            root_key: root.to_string_lossy().into_owned(),
            root,
            base,
        })
    }

    /// Turns a path given on the command line into a repository-relative one.
    fn rel_path(&self, path: &str) -> Result<String, String> {
        let mut abs = self.base.join(path);
        // Resolve symlinked directories, but not the file itself (it may be a tracked symlink,
        // or not exist any more).
        if let (Some(parent), Some(name)) = (abs.parent(), abs.file_name())
            && let Ok(parent) = parent.canonicalize()
        {
            abs = parent.join(name);
        }
        let mut norm = PathBuf::new();
        for c in abs.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    norm.pop();
                }
                c => norm.push(c),
            }
        }
        let rel = norm
            .strip_prefix(&self.root)
            .map_err(|_| format!("{path} is outside the repository"))?;
        if rel.as_os_str().is_empty() {
            return Err(format!("{path} is not a file"));
        }
        Ok(rel.to_string_lossy().into_owned())
    }

    fn worktree_text(&self, path: &str) -> Option<String> {
        std::fs::read(self.root.join(path))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    fn file_filter(&self, a: &Args) -> Result<Option<String>, String> {
        a.file.as_deref().map(|f| self.rel_path(f)).transpose()
    }

    fn find_all(&self, ids: &[String]) -> Result<Vec<Comment>, String> {
        if ids.is_empty() {
            return Err("no comment id given".into());
        }
        ids.iter()
            .map(|id| self.store.find(&self.root_key, id))
            .collect()
    }

    fn list(&self, a: &Args) -> Result<(), String> {
        let file = self.file_filter(a)?;
        let mut texts = HashMap::new();
        let mut out = Vec::new();
        for c in self.store.list(&self.root_key, file.as_deref())? {
            if a.unresolved && c.resolved || a.author.as_ref().is_some_and(|x| *x != c.author) {
                continue;
            }
            let text = texts
                .entry(c.path.clone())
                .or_insert_with(|| self.worktree_text(&c.path));
            out.push(Located::new(c, text.as_deref()));
        }
        if a.json {
            let values: Vec<_> = out.iter().map(Located::to_json).collect();
            println!("{}", serde_json::to_string_pretty(&values).unwrap());
        } else if out.is_empty() {
            eprintln!("no comments");
        } else {
            for (i, l) in out.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                print!("{}", l.render());
            }
        }
        Ok(())
    }

    fn add(&self, a: &Args) -> Result<(), String> {
        let file = a.file.as_deref().ok_or("add needs --file")?;
        let path = self.rel_path(file)?;
        let lines = parse_lines(a.line.as_deref().ok_or("add needs --line")?)?;
        let body = message(a)?;
        let side = if a.old { Side::Old } else { Side::New };
        let text = match side {
            Side::New => self.worktree_text(&path),
            Side::Old => None,
        };
        if let Some(text) = &text {
            let n = comments::split_lines(text).len();
            if lines.end > n {
                return Err(format!("{path} has only {n} lines"));
            }
        } else if side == Side::New {
            return Err(format!(
                "{path} does not exist in the working tree (use --old for removed lines)"
            ));
        }
        let author = a.author.as_deref().unwrap_or("agent");
        let mut c = Comment::new(
            &path,
            side,
            lines,
            &body,
            author,
            text.as_deref().unwrap_or(""),
        );
        self.store
            .put(&self.root_key, std::slice::from_mut(&mut c))?;
        println!("{}", c.id);
        Ok(())
    }

    fn edit(&self, a: &Args) -> Result<(), String> {
        let [id] = a.ids.as_slice() else {
            return Err("edit needs exactly one id".into());
        };
        let mut c = self.store.find(&self.root_key, id)?;
        c.body = message(a)?;
        c.updated = comments::now();
        self.store.put(&self.root_key, std::slice::from_mut(&mut c))
    }

    fn set_resolved(&self, a: &Args, resolved: bool) -> Result<(), String> {
        let mut found = self.find_all(&a.ids)?;
        for c in &mut found {
            c.resolved = resolved;
            c.updated = comments::now();
        }
        self.store.put(&self.root_key, &mut found)
    }

    fn rm(&self, a: &Args) -> Result<(), String> {
        let found = self.find_all(&a.ids)?;
        self.store.delete(&self.root_key, &found)
    }

    fn clear(&self, a: &Args) -> Result<(), String> {
        let file = self.file_filter(a)?;
        let all = self.store.list(&self.root_key, file.as_deref())?;
        self.store.delete(&self.root_key, &all)?;
        eprintln!(
            "deleted {} comment{}",
            all.len(),
            if all.len() == 1 { "" } else { "s" }
        );
        Ok(())
    }

    fn watch(&self, a: &Args) -> Result<(), String> {
        let file = self.file_filter(a)?;
        // Bind before reading the current state, so no write falls in between.
        let listener = comments::Listener::bind().map_err(|e| format!("cannot listen: {e}"))?;
        let mut known: HashMap<(String, String), Comment> = self
            .store
            .list(&self.root_key, file.as_deref())?
            .into_iter()
            .map(|c| ((c.path.clone(), c.id.clone()), c))
            .collect();
        let deadline = a
            .timeout
            .map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s));
        loop {
            let key = match deadline {
                Some(d) => {
                    let left = d.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return Err("timed out".into());
                    }
                    listener.recv_timeout(left)
                }
                None => listener.recv(),
            };
            let key = match key {
                Ok(Some(k)) => k,
                Ok(None) => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err("timed out".into());
                }
                Err(e) => return Err(e.to_string()),
            };
            if key.root != self.root_key || file.as_ref().is_some_and(|f| *f != key.path) {
                continue;
            }
            let id = (key.path.clone(), key.id.clone());
            let (event, comment) = match self.store.get(&self.root_key, &key.path, &key.id)? {
                Some(c) => match known.insert(id, c.clone()) {
                    Some(old) if old == c => continue,
                    Some(_) => ("updated", c),
                    None => ("added", c),
                },
                None => match known.remove(&id) {
                    Some(old) => ("removed", old),
                    None => continue,
                },
            };
            if a.author.as_ref().is_some_and(|x| *x != comment.author)
                || a.not_author.as_ref().is_some_and(|x| *x == comment.author)
            {
                continue;
            }
            let located = Located::new(comment, self.worktree_text(&key.path).as_deref());
            if a.json {
                let v = serde_json::json!({ "event": event, "comment": located.to_json() });
                println!("{v}");
            } else {
                print!("{event}: {}", located.render());
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
            if a.once {
                return Ok(());
            }
        }
    }
}

/// A comment with the position of its lines in the current working tree.
struct Located {
    comment: Comment,
    start: usize,
    end: usize,
    outdated: bool,
}

impl Located {
    fn new(comment: Comment, worktree: Option<&str>) -> Located {
        let (start, end, outdated) = match (comment.side, worktree) {
            (Side::New, Some(text)) => {
                let loc = comment.locate(text);
                (loc.lines.start + 1, loc.lines.end, loc.outdated)
            }
            (Side::New, None) => (comment.start, comment.end, true),
            (Side::Old, _) => (comment.start, comment.end, false),
        };
        Located {
            comment,
            start,
            end,
            outdated,
        }
    }

    fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(&self.comment).unwrap();
        v["start"] = self.start.into();
        v["end"] = self.end.into();
        v["outdated"] = self.outdated.into();
        v
    }

    fn render(&self) -> String {
        let c = &self.comment;
        let lines = if self.start == self.end {
            format!("{}", self.start)
        } else {
            format!("{}-{}", self.start, self.end)
        };
        let mut flags = String::new();
        if c.side == Side::Old {
            flags.push_str(" (old side)");
        }
        if c.resolved {
            flags.push_str(" [resolved]");
        }
        if self.outdated {
            flags.push_str(" [outdated]");
        }
        let when = gtk::glib::DateTime::from_unix_local(c.updated)
            .and_then(|d| d.format("%Y-%m-%d %H:%M"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        let mut out = format!(
            "{}  {}:{lines}{flags}  — {}, {when}\n",
            c.id, c.path, c.author
        );
        for (i, l) in c.lines.iter().enumerate() {
            out += &format!("  {:>5} │ {l}\n", self.start + i);
        }
        for l in c.body.lines() {
            out += &format!("    {l}\n");
        }
        out
    }
}

fn parse_lines(spec: &str) -> Result<std::ops::Range<usize>, String> {
    let bad = || format!("bad line range: {spec} (expected <n> or <n>-<m>)");
    let num = |s: &str| {
        s.trim()
            .parse::<usize>()
            .ok()
            .filter(|&n| n > 0)
            .ok_or_else(bad)
    };
    let (a, b) = match spec.split_once(['-', ':', ',']) {
        Some((a, b)) => (num(a)?, num(b)?),
        None => (num(spec)?, num(spec)?),
    };
    if b < a {
        return Err(bad());
    }
    Ok(a - 1..b)
}

fn message(a: &Args) -> Result<String, String> {
    let body = if !a.messages.is_empty() {
        a.messages.join("\n\n")
    } else if !std::io::stdin().is_terminal() {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| e.to_string())?;
        s
    } else {
        return Err("give the comment text with -m or on stdin".into());
    };
    let body = body.trim().to_string();
    if body.is_empty() {
        return Err("empty comment".into());
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_specs() {
        assert_eq!(parse_lines("3"), Ok(2..3));
        assert_eq!(parse_lines("3-5"), Ok(2..5));
        assert_eq!(parse_lines("3:5"), Ok(2..5));
        assert!(parse_lines("5-3").is_err());
        assert!(parse_lines("0").is_err());
    }
}
