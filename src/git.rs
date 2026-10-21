//! Collecting changed files and their old/new contents by shelling out to `git`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Hash of git's empty tree, used as the base when the repository has no commits yet.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const MAX_FILE_SIZE: usize = 8 * 1024 * 1024;

/// Command line options.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub dir: Option<PathBuf>,
    pub staged: bool,
    /// Arguments before `--` (or all of them, when there's no `--`): revisions, or else
    /// paths, told apart once the repository is known.
    pub args: Vec<String>,
    /// Paths after `--`.
    pub paths: Vec<String>,
    /// Whether `--` was given, making every argument before it a revision.
    pub dashdash: bool,
}

pub const USAGE: &str = "\
usage: twain [-C <dir>] [--staged] [--foreground] [<rev> [<rev>]] [--] [<path>...]
       twain comment <command> ...   (see twain comment --help)

  twain                 HEAD vs working tree (staged + unstaged + untracked)
  twain --staged        HEAD vs index
  twain <rev>           the changes made by commit <rev>
  twain --staged <rev>  <rev> vs index
  twain <a>..<b>        <a> vs <b>   (also <a> <b>, and <a>...<b> for merge-base vs <b>)

  Twain detaches from the terminal unless --foreground is given.

  Given paths, only the files under them are shown; the others are hidden.";

impl Options {
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
        let mut opts = Options::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--" => {
                    opts.dashdash = true;
                    opts.paths.extend(args.by_ref());
                }
                "-h" | "--help" => return Err(String::new()),
                "--staged" | "--cached" => opts.staged = true,
                "-C" => opts.dir = Some(args.next().ok_or("-C needs a directory")?.into()),
                s if s.starts_with('-') => return Err(format!("unknown option: {s}")),
                _ => opts.args.push(arg),
            }
        }
        if opts.dashdash && opts.args.len() > 2 {
            return Err("at most two revisions can be given".into());
        }
        Ok(opts)
    }
}

/// Where the "new" side of the comparison comes from.
#[derive(Clone, Debug)]
enum NewSide {
    WorkTree,
    Index,
    Rev(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(String),
    Binary,
    TooLarge,
    /// The file does not exist on this side (added / deleted).
    Missing,
}

impl Content {
    pub fn text(&self) -> &str {
        match self {
            Content::Text(s) => s,
            _ => "",
        }
    }

    pub fn is_text(&self) -> bool {
        matches!(self, Content::Text(_) | Content::Missing)
    }
}

#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: String,
    /// Set when the file was renamed or copied.
    pub old_path: Option<String>,
    /// git status letter: A, D, M, R, C, T, U, or '?' for untracked.
    pub status: char,
    pub old: Content,
    pub new: Content,
    /// The file in the index, when the new side is the working tree (`Missing` if it isn't
    /// there). `None` otherwise, and for conflicted files.
    pub index: Option<Content>,
    /// How much of the change is staged, when the new side is the working tree.
    pub staging: Option<Staging>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Staging {
    Unstaged,
    Partly,
    Staged,
}

impl Staging {
    pub fn css_class(self) -> &'static str {
        match self {
            Staging::Unstaged => "unstaged",
            Staging::Partly => "partly-staged",
            Staging::Staged => "staged",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Staging::Unstaged => "Not staged",
            Staging::Partly => "Partly staged",
            Staging::Staged => "Staged",
        }
    }
}

/// How much of one file's change is staged, judging by its three contents. Equal contents
/// stand in for the repository-wide staged/unstaged queries of `load`: after one file's
/// index entry was written from `revise`, the texts know the answer with two comparisons.
/// Note that a content filter (autocrlf and the like) can make the worktree text differ
/// from its index blob even where nothing real changed.
pub fn staging_of(old: &Content, index: &Content, new: &Content) -> Staging {
    if *index == *new {
        Staging::Staged
    } else if *index == *old {
        Staging::Unstaged
    } else {
        Staging::Partly
    }
}

pub struct Comparison {
    pub root: PathBuf,
    /// Human readable description, e.g. "HEAD → working tree".
    pub description: String,
    pub files: Vec<FileChange>,
    /// Every other file on the new side, for showing the whole tree.
    pub unchanged: Vec<String>,
    /// Paths given on the command line, relative to the root ("" for all of it): the files
    /// outside them start out hidden. Empty when none were given.
    pub focus: Vec<String>,
}

/// Whether `path` is one of `focus` or under one of them.
pub fn in_focus(focus: &[String], path: &str) -> bool {
    focus.iter().any(|f| {
        f.is_empty()
            || path == f
            || path
                .strip_prefix(f.as_str())
                .is_some_and(|r| r.starts_with('/'))
    })
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn git_str(root: &Path, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&git(root, args)?)
        .trim()
        .to_string())
}

fn decode(bytes: Vec<u8>) -> Content {
    if bytes.len() > MAX_FILE_SIZE {
        Content::TooLarge
    } else if bytes[..bytes.len().min(8000)].contains(&0) {
        Content::Binary
    } else {
        Content::Text(match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        })
    }
}

fn blob(root: &Path, spec: &str) -> Content {
    git(root, &["cat-file", "blob", spec]).map_or(Content::Missing, decode)
}

fn worktree_file(root: &Path, path: &str) -> Content {
    let full = root.join(path);
    match std::fs::symlink_metadata(&full) {
        Ok(m) if m.file_type().is_symlink() => std::fs::read_link(&full)
            .map_or(Content::Missing, |t| {
                Content::Text(t.to_string_lossy().into_owned())
            }),
        Ok(m) if m.is_dir() => Content::Missing, // submodule
        Ok(_) => std::fs::read(&full).map_or(Content::Missing, decode),
        Err(_) => Content::Missing,
    }
}

/// The top level of the repository containing `dir` (default: the current directory).
pub fn repo_root(dir: Option<&Path>) -> Result<PathBuf, String> {
    let cwd = match dir {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    Ok(PathBuf::from(git_str(
        &cwd,
        &["rev-parse", "--show-toplevel"],
    )?))
}

/// Resolves the options against the repository and loads every changed file.
pub fn load(opts: &Options) -> Result<Comparison, String> {
    let cwd = match &opts.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let root = repo_root(Some(&cwd))?;
    let (revs, focus) = split_args(&root, &cwd, opts)?;

    let has_head = git(&root, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok();
    let head = || {
        if has_head {
            "HEAD".to_string()
        } else {
            EMPTY_TREE.to_string()
        }
    };
    let default_new = if opts.staged {
        NewSide::Index
    } else {
        NewSide::WorkTree
    };

    let (old_rev, new_side, old_label) = match revs.as_slice() {
        [] => (head(), default_new, head()),
        [r] if r.contains("...") => {
            let (a, b) = r.split_once("...").unwrap();
            let (a, b) = (or_head(a), or_head(b));
            let base = git_str(&root, &["merge-base", &a, &b])?;
            (base, NewSide::Rev(b), format!("merge-base({a}, …)"))
        }
        [r] if r.contains("..") => {
            let (a, b) = r.split_once("..").unwrap();
            (or_head(a), NewSide::Rev(or_head(b)), or_head(a))
        }
        [r] if opts.staged => (r.clone(), NewSide::Index, r.clone()),
        // A single commit: the changes it made, against its (first) parent.
        [r] => {
            let parent = format!("{r}^");
            let base = if is_commit(&root, &parent) {
                parent
            } else {
                EMPTY_TREE.to_string()
            };
            (base.clone(), NewSide::Rev(r.clone()), base)
        }
        [a, b] => (a.clone(), NewSide::Rev(b.clone()), a.clone()),
        _ => unreachable!(),
    };
    // Fail early with a readable message on a bad revision.
    git(
        &root,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{old_rev}^{{tree}}"),
        ],
    )
    .map_err(|_| format!("unknown revision: {old_label}"))?;

    let mut args = vec![
        "diff",
        "-z",
        "--name-status",
        "-M",
        "--no-ext-diff",
        "--no-color",
    ];
    let new_label = match &new_side {
        NewSide::WorkTree => {
            args.push(&old_rev);
            "working tree".to_string()
        }
        NewSide::Index => {
            args.extend(["--cached", &old_rev]);
            "index".to_string()
        }
        NewSide::Rev(r) => {
            args.extend([old_rev.as_str(), r.as_str()]);
            r.clone()
        }
    };
    args.push("--");
    let listing = git(&root, &args)?;

    let mut entries: Vec<(char, Option<String>, String)> = Vec::new();
    let mut fields = listing.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let Some(status) = fields.next() {
        let status = status[0] as char;
        let mut next = || {
            fields
                .next()
                .map(|f| String::from_utf8_lossy(f).into_owned())
        };
        let Some(first) = next() else { break };
        if status == 'R' || status == 'C' {
            let Some(second) = next() else { break };
            entries.push((status, Some(first), second));
        } else {
            entries.push((status, None, first));
        }
    }
    if matches!(new_side, NewSide::WorkTree) {
        let untracked = git(&root, &["ls-files", "-z", "--others", "--exclude-standard"])?;
        for f in untracked.split(|&b| b == 0).filter(|f| !f.is_empty()) {
            entries.push(('?', None, String::from_utf8_lossy(f).into_owned()));
        }
    }

    // Which files have staged (old side → index) and unstaged (index → working tree) changes.
    let (staged, unstaged) = if matches!(new_side, NewSide::WorkTree) {
        let names = |args: &[&str]| -> Result<std::collections::HashSet<String>, String> {
            Ok(git(&root, args)?
                .split(|&b| b == 0)
                .filter(|f| !f.is_empty())
                .map(|f| String::from_utf8_lossy(f).into_owned())
                .collect())
        };
        let common = ["diff", "-z", "--name-only", "--no-ext-diff", "--no-renames"];
        (
            names(&[&common[..], &["--cached", &old_rev, "--"]].concat())?,
            names(&[&common[..], &["--"]].concat())?,
        )
    } else {
        Default::default()
    };

    let listing = match &new_side {
        NewSide::WorkTree | NewSide::Index => git(&root, &["ls-files", "-z"])?,
        NewSide::Rev(r) => git(&root, &["ls-tree", "-r", "-z", "--name-only", r])?,
    };
    let changed: std::collections::HashSet<&str> =
        entries.iter().map(|(_, _, path)| path.as_str()).collect();
    let unchanged: Vec<String> = listing
        .split(|&b| b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .filter(|f| !changed.contains(f.as_str()))
        .collect();

    let files = entries
        .into_iter()
        .map(|(status, old_path, path)| {
            let old = match status {
                'A' | '?' => Content::Missing,
                _ => blob(
                    &root,
                    &format!("{old_rev}:{}", old_path.as_deref().unwrap_or(&path)),
                ),
            };
            let new = match status {
                'D' => Content::Missing,
                _ => match &new_side {
                    NewSide::WorkTree => worktree_file(&root, &path),
                    NewSide::Index => blob(&root, &format!(":{path}")),
                    NewSide::Rev(r) => blob(&root, &format!("{r}:{path}")),
                },
            };
            let worktree = matches!(new_side, NewSide::WorkTree);
            let index = match status {
                _ if !worktree => None,
                'U' => None,
                '?' => Some(Content::Missing),
                _ => Some(blob(&root, &format!(":{path}"))),
            };
            let staging = worktree.then(|| {
                let touched = |set: &std::collections::HashSet<String>| {
                    set.contains(&path) || old_path.as_ref().is_some_and(|p| set.contains(p))
                };
                match (touched(&staged), status == '?' || touched(&unstaged)) {
                    (true, false) => Staging::Staged,
                    (true, true) => Staging::Partly,
                    _ => Staging::Unstaged,
                }
            });
            FileChange {
                path,
                old_path,
                status,
                old,
                new,
                index,
                staging,
            }
        })
        .collect();

    Ok(Comparison {
        description: format!("{} → {new_label}", short(&old_label)),
        root,
        files,
        unchanged,
        focus,
    })
}

fn is_commit(root: &Path, rev: &str) -> bool {
    git(
        root,
        &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")],
    )
    .is_ok()
}

/// Whether `arg` names a revision or a range of them (`a..b`, `a...b`, either side optional
/// but not both).
fn is_rev(root: &Path, arg: &str) -> bool {
    match arg.split_once("...").or_else(|| arg.split_once("..")) {
        Some(("", "")) => false,
        Some((a, b)) => [a, b].iter().all(|r| r.is_empty() || is_commit(root, r)),
        None => is_commit(root, arg),
    }
}

/// Tells the revisions from the paths among the arguments, like git: without `--`, the
/// leading arguments that are revisions are taken as such, and the rest must be existing
/// paths. The paths are returned relative to the root.
fn split_args(
    root: &Path,
    cwd: &Path,
    opts: &Options,
) -> Result<(Vec<String>, Vec<String>), String> {
    let (revs, paths) = if opts.dashdash {
        (opts.args.clone(), opts.paths.clone())
    } else {
        let mut revs = Vec::new();
        let mut paths = Vec::new();
        for arg in &opts.args {
            if paths.is_empty() && is_rev(root, arg) {
                revs.push(arg.clone());
            } else if std::fs::symlink_metadata(cwd.join(arg)).is_ok() {
                paths.push(arg.clone());
            } else {
                return Err(format!(
                    "unknown revision or path not in the working tree: {arg}\n\
                     (use '--' to separate paths from revisions)"
                ));
            }
        }
        (revs, paths)
    };
    if revs.len() > 2 {
        return Err("at most two revisions can be given".into());
    }
    if paths.is_empty() {
        return Ok((revs, paths));
    }
    let prefix = git_str(cwd, &["rev-parse", "--show-prefix"])?;
    let paths = paths
        .iter()
        .map(|p| repo_path(root, &prefix, p))
        .collect::<Result<_, _>>()?;
    Ok((revs, paths))
}

/// A path given on the command line, relative to the root: from `prefix` (the current
/// directory, relative to the root) when it's relative.
fn repo_path(root: &Path, prefix: &str, arg: &str) -> Result<String, String> {
    let outside = || format!("{arg}: outside the repository");
    let joined = if Path::new(arg).is_absolute() {
        let abs = Path::new(arg);
        let rel = abs.strip_prefix(root).map(Path::to_path_buf).or_else(|_| {
            let canon = std::fs::canonicalize(abs).map_err(|_| outside())?;
            canon
                .strip_prefix(root)
                .map(Path::to_path_buf)
                .map_err(|_| outside())
        })?;
        rel.to_string_lossy().into_owned()
    } else {
        format!("{prefix}{arg}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop().ok_or_else(outside)?;
            }
            _ => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

/// Sets the index entry of `path` to `text`, or removes it if `None`.
pub fn set_index(root: &Path, path: &str, text: Option<&str>) -> Result<(), String> {
    let Some(text) = text else {
        return git(root, &["update-index", "--force-remove", "--", path]).map(|_| ());
    };
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["hash-object", "-w", "--stdin", "--no-filters"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run git: {e}"))?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("failed to write to git: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // Keep the mode the file has in the index, or in HEAD, or else take the working tree's.
    let first_word = |out: String| out.split_whitespace().next().map(str::to_string);
    let mode = git_str(root, &["ls-files", "-s", "--", path])
        .ok()
        .and_then(first_word)
        .or_else(|| {
            git_str(root, &["ls-tree", "HEAD", "--", path])
                .ok()
                .and_then(first_word)
        })
        .unwrap_or_else(|| {
            use std::os::unix::fs::PermissionsExt;
            let exec = std::fs::metadata(root.join(path))
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
            if exec { "100755" } else { "100644" }.to_string()
        });
    git(
        root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("{mode},{hash},{path}"),
        ],
    )
    .map(|_| ())
}

fn or_head(s: &str) -> String {
    if s.is_empty() {
        "HEAD".into()
    } else {
        s.into()
    }
}

fn short(rev: &str) -> &str {
    if rev == EMPTY_TREE { "empty" } else { rev }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_of_walks_the_transitions() {
        let (old, new) = (
            Content::Text("1\n2\n".into()),
            Content::Text("1\nTWO\n3\n".into()),
        );
        // Nothing staged; then one line; then fully staged.
        assert_eq!(staging_of(&old, &old, &new), Staging::Unstaged);
        assert_eq!(
            staging_of(&old, &Content::Text("1\nTWO\n2\n".into()), &new),
            Staging::Partly
        );
        assert_eq!(staging_of(&old, &new, &new), Staging::Staged);

        // Added file: absent from the index is unstaged, then staged.
        let (old, new) = (Content::Missing, Content::Text("n\n".into()));
        assert_eq!(staging_of(&old, &Content::Missing, &new), Staging::Unstaged);
        assert_eq!(staging_of(&old, &new, &new), Staging::Staged);

        // Deleted file: the index keeping the content is unstaged; empty index is staged.
        let (old, new) = (Content::Text("n\n".into()), Content::Missing);
        assert_eq!(staging_of(&old, &old, &new), Staging::Unstaged);
        assert_eq!(staging_of(&old, &Content::Missing, &new), Staging::Staged);
    }

    #[test]
    fn missing_is_not_empty_text() {
        // `staging_of` relies on an empty blob differing from an absent file.
        assert_ne!(Content::Missing, Content::Text(String::new()));
    }

    #[test]
    fn revisions_and_paths() {
        let root = std::env::temp_dir().join(format!("twain-args-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("d")).unwrap();
        let run = |args: &[&str]| git_str(&root, args).unwrap();
        let commit = |m: &str| {
            run(&["add", "."]);
            run(&[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                m,
            ]);
        };
        run(&["init", "-q"]);
        std::fs::write(root.join("a"), "1\n").unwrap();
        commit("one");
        std::fs::write(root.join("d/b"), "2\n").unwrap();
        commit("two");
        std::fs::write(root.join("a"), "3\n").unwrap();
        commit("three");
        let load = |args: &[&str]| {
            let mut opts = Options::parse(args.iter().map(|s| s.to_string())).unwrap();
            opts.dir = Some(root.clone());
            let cmp = super::load(&opts)?;
            let mut paths: Vec<_> = cmp.files.into_iter().map(|f| f.path).collect();
            paths.sort();
            Ok::<_, String>((paths, cmp.focus))
        };
        let none: Vec<String> = Vec::new();
        assert_eq!(load(&["HEAD"]).unwrap(), (vec!["a".into()], none.clone()));
        assert_eq!(
            load(&["HEAD~1"]).unwrap(),
            (vec!["d/b".into()], none.clone())
        );
        assert_eq!(load(&["HEAD~2"]).unwrap(), (vec!["a".into()], none.clone()));
        assert_eq!(
            load(&["HEAD~2..HEAD"]).unwrap(),
            (vec!["a".into(), "d/b".into()], none.clone())
        );
        assert_eq!(load(&["HEAD~2..", "d"]).unwrap().1, vec!["d".to_string()]);
        assert_eq!(
            load(&["HEAD~2", "HEAD", "--", "gone", "./d/b"]).unwrap().1,
            vec!["gone".to_string(), "d/b".to_string()]
        );
        assert!(load(&["nonexistent"]).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn repo_paths() {
        let root = Path::new("/r");
        assert_eq!(repo_path(root, "src/", "a.rs").unwrap(), "src/a.rs");
        assert_eq!(repo_path(root, "src/", "../b/./c/").unwrap(), "b/c");
        assert_eq!(repo_path(root, "src/", "..").unwrap(), "");
        assert!(repo_path(root, "", "../x").is_err());
        assert_eq!(repo_path(root, "src/", "/r/d/e").unwrap(), "d/e");
        assert!(in_focus(&["src".into()], "src/a.rs"));
        assert!(!in_focus(&["src".into()], "srcx/a.rs"));
        assert!(in_focus(&["".into()], "a"));
    }

    #[test]
    fn set_index_stages_and_unstages() {
        let root = std::env::temp_dir().join(format!("twain-index-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| git_str(&root, args).unwrap();
        run(&["init", "-q"]);
        std::fs::write(root.join("a"), "1\n2\n3\n").unwrap();
        std::fs::write(root.join("gone"), "x\n").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ]);
        let staged = || run(&["diff", "--cached", "--name-status"]);

        set_index(&root, "a", Some("1\nTWO\n3\n")).unwrap();
        assert_eq!(run(&["show", ":a"]), "1\nTWO\n3");
        assert_eq!(staged(), "M\ta");
        set_index(&root, "a", Some("1\n2\n3\n")).unwrap();
        assert_eq!(staged(), "");

        std::fs::write(root.join("new"), "n\n").unwrap();
        set_index(&root, "new", Some("n\n")).unwrap();
        set_index(&root, "gone", None).unwrap();
        assert_eq!(staged(), "D\tgone\nA\tnew");
        // Unstaging a new file leaves it untracked.
        set_index(&root, "new", None).unwrap();
        assert_eq!(staged(), "D\tgone");
        assert_eq!(run(&["status", "--short", "new"]), "?? new");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
