//! `twain comment ...`: reading and writing comments from the command line, for agents and
//! scripts. Talks to the same database as the windows, which pick up changes immediately.

use crate::comments::{self, Comment, Item, Reply, Side, Store, Thread};
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

Each comment starts a thread that others can reply to. Replies have ids of their own, and
wherever a command takes the id of a thread, the id of one of its replies works too.

commands:
  list [--file <path>] [--unresolved] [--author <name>] [--json]
      Show threads: each comment with the code it's attached to, then its replies.
      --author keeps the threads <name> wrote in.
  show <id>... [--json]
      Show the threads of these comments or replies.
  add --file <path> --line <n>[-<m>] [--old] [--author <name>] [-m <text>]...
      Add a comment on line n (through m). The text comes from -m (repeat for paragraphs)
      or stdin. The author defaults to \"agent\". Prints the new comment's id.
  reply <id> [--author <name>] [-m <text>]...
      Reply in a thread. Text and author as for add. Prints the new reply's id.
  edit <id> [-m <text>]...
      Replace a comment's or reply's text (from -m or stdin).
  resolve <id>...  /  unresolve <id>...
      Mark threads resolved, or not.
  rm <id>...
      Delete comments, with their replies, or single replies.
  clear [--file <path>] [--older-than <age>]
      Delete every comment in the repository, or in one file, with their replies. With
      --older-than, only the comments written before then: an age such as 90m, 12h, 7d or
      2w, or a date such as 2026-09-01.
  watch [--file <path>] [--author <name>] [--not-author <name>] [--once] [--timeout <secs>] [--json]
      Wait for comments or replies to be added, changed or removed, and print each change.
      With --once, exit after the first one; e.g. `twain comment watch --once --not-author
      agent` waits for a human to say something.

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
    /// Unix time from --older-than.
    before: Option<i64>,
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
            "--older-than" => a.before = Some(parse_age(&value("--older-than")?)?),
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
            "show" => ctx.show(&a),
            "add" => ctx.add(&a),
            "reply" => ctx.reply(&a),
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

    fn find_all(&self, ids: &[String]) -> Result<Vec<Item>, String> {
        if ids.is_empty() {
            return Err("no comment id given".into());
        }
        ids.iter()
            .map(|id| self.store.find(&self.root_key, id))
            .collect()
    }

    /// The comments of the threads with these ids, each once.
    fn find_threads(&self, ids: &[String]) -> Result<Vec<Thread>, String> {
        if ids.is_empty() {
            return Err("no comment id given".into());
        }
        let mut out: Vec<Thread> = Vec::new();
        for id in ids {
            let t = self.store.find_thread(&self.root_key, id)?;
            if !out
                .iter()
                .any(|o| (&o.comment.path, &o.comment.id) == (&t.comment.path, &t.comment.id))
            {
                out.push(t);
            }
        }
        Ok(out)
    }

    fn print_threads(&self, threads: Vec<Thread>, json: bool) {
        let mut texts = HashMap::new();
        let out: Vec<Located> = threads
            .into_iter()
            .map(|t| {
                let text = texts
                    .entry(t.comment.path.clone())
                    .or_insert_with(|| self.worktree_text(&t.comment.path));
                Located::new(t, text.as_deref())
            })
            .collect();
        if json {
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
    }

    fn list(&self, a: &Args) -> Result<(), String> {
        let file = self.file_filter(a)?;
        let threads = self
            .store
            .threads(&self.root_key, file.as_deref())?
            .into_iter()
            .filter(|t| !(a.unresolved && t.comment.resolved))
            .filter(|t| {
                a.author.as_ref().is_none_or(|x| {
                    t.comment.author == *x || t.replies.iter().any(|r| r.author == *x)
                })
            })
            .collect();
        self.print_threads(threads, a.json);
        Ok(())
    }

    fn show(&self, a: &Args) -> Result<(), String> {
        let threads = self.find_threads(&a.ids)?;
        self.print_threads(threads, a.json);
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

    fn reply(&self, a: &Args) -> Result<(), String> {
        let [id] = a.ids.as_slice() else {
            return Err("reply needs exactly one id".into());
        };
        let thread = self.store.find_thread(&self.root_key, id)?;
        let body = message(a)?;
        let author = a.author.as_deref().unwrap_or("agent");
        let mut r = Reply::new(&thread.comment, &body, author);
        self.store
            .put_replies(&self.root_key, std::slice::from_mut(&mut r))?;
        println!("{}", r.id);
        Ok(())
    }

    fn edit(&self, a: &Args) -> Result<(), String> {
        let [id] = a.ids.as_slice() else {
            return Err("edit needs exactly one id".into());
        };
        let body = message(a)?;
        match self.store.find(&self.root_key, id)? {
            Item::Comment(mut c) => {
                c.body = body;
                c.updated = comments::now();
                self.store.put(&self.root_key, std::slice::from_mut(&mut c))
            }
            Item::Reply(mut r) => {
                r.body = body;
                r.updated = comments::now();
                self.store
                    .put_replies(&self.root_key, std::slice::from_mut(&mut r))
            }
        }
    }

    fn set_resolved(&self, a: &Args, resolved: bool) -> Result<(), String> {
        let mut found: Vec<Comment> = self
            .find_threads(&a.ids)?
            .into_iter()
            .map(|t| t.comment)
            .collect();
        for c in &mut found {
            c.resolved = resolved;
            c.updated = comments::now();
        }
        self.store.put(&self.root_key, &mut found)
    }

    fn rm(&self, a: &Args) -> Result<(), String> {
        let (mut found, mut replies) = (Vec::new(), Vec::new());
        for item in self.find_all(&a.ids)? {
            match item {
                Item::Comment(c) => found.push(c),
                Item::Reply(r) => replies.push(r),
            }
        }
        self.store.delete(&self.root_key, &found)?;
        self.store.delete_replies(&self.root_key, &replies)
    }

    fn clear(&self, a: &Args) -> Result<(), String> {
        let file = self.file_filter(a)?;
        let found: Vec<Comment> = self
            .store
            .list(&self.root_key, file.as_deref())?
            .into_iter()
            .filter(|c| a.before.is_none_or(|t| c.created < t))
            .collect();
        let replies = self.store.delete(&self.root_key, &found)?;
        eprintln!(
            "deleted {} and {}",
            plural(found.len(), "comment"),
            plural(replies, "reply")
        );
        Ok(())
    }

    fn watch(&self, a: &Args) -> Result<(), String> {
        let file = self.file_filter(a)?;
        // Bind before reading the current state, so no write falls in between.
        let listener = comments::Listener::bind().map_err(|e| format!("cannot listen: {e}"))?;
        let mut known: HashMap<(String, String), Comment> = HashMap::new();
        let mut known_replies: HashMap<(String, String, String), Reply> = HashMap::new();
        for t in self.store.threads(&self.root_key, file.as_deref())? {
            for r in t.replies {
                known_replies.insert((r.path.clone(), r.comment.clone(), r.id.clone()), r);
            }
            known.insert((t.comment.path.clone(), t.comment.id.clone()), t.comment);
        }
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
            let worktree = self.worktree_text(&key.path);
            let (author, json, text) = if let Some(rid) = &key.reply {
                let id = (key.path.clone(), key.id.clone(), rid.clone());
                let stored = self
                    .store
                    .get_reply(&self.root_key, &key.path, &key.id, rid)?;
                let (event, reply) = match stored {
                    Some(r) => match known_replies.insert(id, r.clone()) {
                        Some(old) if old == r => continue,
                        Some(_) => ("updated", r),
                        None => ("added", r),
                    },
                    None => match known_replies.remove(&id) {
                        Some(old) => ("removed", old),
                        None => continue,
                    },
                };
                // Where the thread is, if it's still there.
                let parent = self
                    .store
                    .get(&self.root_key, &key.path, &key.id)?
                    .map(|c| {
                        Located::new(
                            Thread {
                                comment: c,
                                replies: Vec::new(),
                            },
                            worktree.as_deref(),
                        )
                    });
                let json = serde_json::json!({ "event": event, "reply": reply });
                let text = format!("{event} reply: {}", render_reply(&reply, parent.as_ref()));
                (reply.author, json, text)
            } else {
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
                let author = comment.author.clone();
                let thread = match event {
                    "removed" => Thread {
                        comment,
                        replies: Vec::new(),
                    },
                    _ => self.store.thread(&self.root_key, comment)?,
                };
                let located = Located::new(thread, worktree.as_deref());
                let json = serde_json::json!({ "event": event, "comment": located.to_json() });
                (author, json, format!("{event}: {}", located.render()))
            };
            if a.author.as_ref().is_some_and(|x| *x != author)
                || a.not_author.as_ref().is_some_and(|x| *x == author)
            {
                continue;
            }
            if a.json {
                println!("{json}");
            } else {
                print!("{text}");
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
            if a.once {
                return Ok(());
            }
        }
    }
}

fn plural(n: usize, word: &str) -> String {
    match (n, word.strip_suffix('y')) {
        (1, _) => format!("1 {word}"),
        (_, Some(stem)) => format!("{n} {stem}ies"),
        _ => format!("{n} {word}s"),
    }
}

fn format_time(t: i64) -> String {
    gtk::glib::DateTime::from_unix_local(t)
        .and_then(|d| d.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// A reply on its own, with where its thread is when known.
fn render_reply(r: &Reply, thread: Option<&Located>) -> String {
    let place = match thread {
        Some(l) => format!("{}:{}", r.path, l.lines()),
        None => r.path.clone(),
    };
    let mut out = format!(
        "{}  in thread {} on {place}  — {}, {}\n",
        r.id,
        r.comment,
        r.author,
        format_time(r.updated)
    );
    for l in r.body.lines() {
        out += &format!("    {l}\n");
    }
    out
}

/// A thread with the position of its lines in the current working tree.
struct Located {
    thread: Thread,
    start: usize,
    end: usize,
    outdated: bool,
}

impl Located {
    fn new(thread: Thread, worktree: Option<&str>) -> Located {
        let comment = &thread.comment;
        let (start, end, outdated) = match (comment.side, worktree) {
            (Side::New, Some(text)) => {
                let loc = comment.locate(text);
                (loc.lines.start + 1, loc.lines.end, loc.outdated)
            }
            (Side::New, None) => (comment.start, comment.end, true),
            (Side::Old, _) => (comment.start, comment.end, false),
        };
        Located {
            thread,
            start,
            end,
            outdated,
        }
    }

    fn lines(&self) -> String {
        if self.start == self.end {
            format!("{}", self.start)
        } else {
            format!("{}-{}", self.start, self.end)
        }
    }

    fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(&self.thread.comment).unwrap();
        v["start"] = self.start.into();
        v["end"] = self.end.into();
        v["outdated"] = self.outdated.into();
        v["replies"] = serde_json::to_value(&self.thread.replies).unwrap();
        v
    }

    fn render(&self) -> String {
        let c = &self.thread.comment;
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
        let mut out = format!(
            "{}  {}:{}{flags}  — {}, {}\n",
            c.id,
            c.path,
            self.lines(),
            c.author,
            format_time(c.updated)
        );
        for (i, l) in c.lines.iter().enumerate() {
            out += &format!("  {:>5} │ {l}\n", self.start + i);
        }
        for l in c.body.lines() {
            out += &format!("    {l}\n");
        }
        for r in &self.thread.replies {
            out += &format!("  ↳ {}  — {}, {}\n", r.id, r.author, format_time(r.updated));
            for l in r.body.lines() {
                out += &format!("      {l}\n");
            }
        }
        out
    }
}

/// The time an --older-than spec stands for, as a Unix timestamp: an age (`90m`, `12h`,
/// `7d`, `2w`, …) counted back from now, or a local date (`2026-09-01`).
fn parse_age(spec: &str) -> Result<i64, String> {
    let bad = || format!("bad age: {spec} (expected e.g. 30m, 12h, 7d, 2w or 2026-09-01)");
    let date: Vec<&str> = spec.split('-').collect();
    if let [y, m, d] = date.as_slice() {
        let (Ok(y), Ok(m), Ok(d)) = (y.parse(), m.parse(), d.parse()) else {
            return Err(bad());
        };
        return gtk::glib::DateTime::from_local(y, m, d, 0, 0, 0.0)
            .map(|t| t.to_unix())
            .map_err(|_| bad());
    }
    let split = spec.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (n, unit) = spec.split_at(split);
    let n: i64 = n.parse().map_err(|_| bad())?;
    let secs = match unit {
        "s" => 1,
        "m" | "min" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 7 * 86400,
        _ => return Err(bad()),
    };
    Ok(comments::now() - n.saturating_mul(secs))
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

    #[test]
    fn age_specs() {
        let now = comments::now();
        let near = |t: Result<i64, String>, want: i64| (t.unwrap() - want).abs() <= 2;
        assert!(near(parse_age("90m"), now - 90 * 60));
        assert!(near(parse_age("7d"), now - 7 * 86400));
        assert!(near(parse_age("2w"), now - 14 * 86400));
        assert!(parse_age("2026-09-01").is_ok());
        assert!(parse_age("7").is_err());
        assert!(parse_age("d").is_err());
        assert!(parse_age("7y").is_err());
        assert!(parse_age("2026-13-01").is_err());
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(1, "reply"), "1 reply");
        assert_eq!(plural(2, "reply"), "2 replies");
        assert_eq!(plural(0, "comment"), "0 comments");
    }
}
