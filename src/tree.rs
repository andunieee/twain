//! The directory tree shown in the sidebar.

use crate::git::Staging;
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

pub struct Node {
    pub name: String,
    pub dir: bool,
    /// Index into the file list for changed files, `None` for unchanged files and directories.
    pub file: Option<usize>,
    /// Whether this is a changed file or a directory containing one.
    pub changed: bool,
    pub status: char,
    pub staging: Option<Staging>,
    /// For directories, the full paths of the directories merged into this node, outermost
    /// first ("src", "src/ui" for "src/ui").
    pub chain: Vec<String>,
    /// For directories with unchanged entries of their own, whether those are shown.
    pub unchanged_shown: Option<bool>,
    pub children: Vec<Rc<Node>>,
}

#[derive(Default)]
struct Dir<'a> {
    dirs: BTreeMap<&'a str, Dir<'a>>,
    files: Vec<(&'a str, Option<usize>, char, Option<Staging>)>,
    changed: bool,
}

struct Ctx<'a> {
    shown: &'a HashSet<String>,
    /// Directories that have unchanged files or unchanged subdirectories of their own.
    hiding: HashSet<&'a str>,
}

/// Builds a tree from `(path, status, staging)` triples of changed files plus the paths of `unchanged`
/// ones, which are only included in the directories listed in `shown` (the full paths, with
/// "" for the top level). Directories come before files and chains of single-child
/// directories are merged ("src/ui" instead of "src" → "ui") unless the outer one is shown.
/// Returns the top level as a directory node with an empty name.
pub fn build(paths: &[(&str, char, Option<Staging>)], unchanged: &[&str], shown: &HashSet<String>) -> Node {
    let mut root = Dir::default();
    let mut changed_dirs = HashSet::new();
    for (i, &(path, status, staging)) in paths.iter().enumerate() {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let mut d = &mut root;
        if !dir.is_empty() {
            for (end, _) in dir.match_indices('/').chain([(dir.len(), "")]) {
                changed_dirs.insert(&path[..end]);
            }
            for p in dir.split('/') {
                d = d.dirs.entry(p).or_default();
                d.changed = true;
            }
        }
        d.files.push((name, Some(i), status, staging));
    }

    let mut hiding = HashSet::new();
    for &path in unchanged {
        // Each ancestor whose next component is this file or an unchanged directory.
        let mut start = 0;
        for (end, _) in path.match_indices('/').chain([(path.len(), "")]) {
            let parent = &path[..start.max(1) - 1];
            if end == path.len() || !changed_dirs.contains(&path[..end]) {
                hiding.insert(parent);
            }
            start = end + 1;
        }
        // Descend while the directories exist (have changes) or their parent is shown.
        let mut d = &mut root;
        let mut start = 0;
        loop {
            let parent = &path[..start.max(1) - 1];
            let open = shown.contains(parent);
            match path[start..].find('/') {
                None => {
                    if open {
                        d.files.push((&path[start..], None, ' ', None));
                    }
                    break;
                }
                Some(len) => {
                    let name = &path[start..start + len];
                    if !open && !d.dirs.contains_key(name) {
                        break;
                    }
                    d = d.dirs.entry(name).or_default();
                    start += len + 1;
                }
            }
        }
    }
    let ctx = Ctx { shown, hiding };
    Node {
        name: String::new(),
        dir: true,
        file: None,
        changed: !paths.is_empty(),
        status: ' ',
        staging: None,
        chain: vec![String::new()],
        unchanged_shown: ctx.hiding.contains("").then(|| shown.contains("")),
        children: convert(root, "", &ctx),
    }
}

fn convert(dir: Dir, prefix: &str, ctx: &Ctx) -> Vec<Rc<Node>> {
    let join = |name: &str| {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        }
    };
    let mut out = Vec::new();
    for (name, mut sub) in dir.dirs {
        let mut chain = vec![join(name)];
        let mut name = name.to_string();
        while sub.files.is_empty()
            && sub.dirs.len() == 1
            && !ctx.shown.contains(chain.last().unwrap())
        {
            let (child_name, child) = sub.dirs.pop_first().unwrap();
            chain.push(format!("{}/{child_name}", chain.last().unwrap()));
            name = format!("{name}/{child_name}");
            sub = child;
        }
        let last = chain.last().unwrap().clone();
        let unchanged_shown = chain
            .iter()
            .any(|p| ctx.hiding.contains(p.as_str()))
            .then(|| ctx.shown.contains(&last));
        out.push(Rc::new(Node {
            name,
            dir: true,
            file: None,
            changed: sub.changed,
            status: ' ',
            staging: None,
            children: convert(sub, &last, ctx),
            chain,
            unchanged_shown,
        }));
    }
    let mut files = dir.files;
    files.sort_by(|a, b| a.0.cmp(b.0));
    for (name, file, status, staging) in files {
        out.push(Rc::new(Node {
            name: name.into(),
            dir: false,
            file,
            changed: file.is_some(),
            status,
            staging,
            chain: Vec::new(),
            unchanged_shown: None,
            children: Vec::new(),
        }));
    }
    out
}

/// Changed file indices in depth-first (display) order.
pub fn file_order(nodes: &[Rc<Node>]) -> Vec<usize> {
    let mut out = Vec::new();
    for n in nodes {
        match n.file {
            Some(i) => out.push(i),
            None => out.extend(file_order(&n.children)),
        }
    }
    out
}
