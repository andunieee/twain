//! Comments on lines of files, shared between every twain process.
//!
//! Comments live in one LMDB environment (`$XDG_DATA_HOME/twain/comments`) that all twain
//! processes — windows and `twain comment` invocations — keep open at the same time. Keys are
//!
//! ```text
//! <repo root> \0 <repo-relative path> \0 <id>
//! ```
//!
//! so a repository's comments, or one file's, are a prefix scan away. Values are JSON.
//!
//! After committing a write, a process sends the key it wrote as a datagram to every socket in
//! `$XDG_DATA_HOME/twain/processes/` (each listening process binds one named after its pid),
//! and the receivers decide from the key whether they need to requery. Sockets left behind by
//! dead processes are removed by the first sender that gets refused.

use heed::types::Bytes;
use serde::{Deserialize, Serialize};
use std::io;
use std::ops::Range;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// LMDB's default limit on key size.
const MAX_KEY: usize = 511;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub fn index(self) -> usize {
        match self {
            Side::Old => 0,
            Side::New => 1,
        }
    }

    pub fn from_index(i: usize) -> Side {
        if i == 0 { Side::Old } else { Side::New }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    /// Repository-relative path of the file.
    pub path: String,
    pub side: Side,
    /// First and last commented line, 1-based and inclusive.
    pub start: usize,
    pub end: usize,
    pub body: String,
    pub author: String,
    /// Unix timestamps in seconds.
    pub created: i64,
    pub updated: i64,
    #[serde(default)]
    pub resolved: bool,
    /// The commented lines as they were when the comment was written, used to find them again
    /// after the file changes. Empty when unknown.
    #[serde(default)]
    pub lines: Vec<String>,
}

/// Where a comment's lines are in the current text of its file.
#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    /// 0-based, end-exclusive line range.
    pub lines: Range<usize>,
    /// The lines no longer match what was commented on; `lines` is the original position.
    pub outdated: bool,
}

impl Comment {
    pub fn new(
        path: &str,
        side: Side,
        lines: Range<usize>,
        body: &str,
        author: &str,
        text: &str,
    ) -> Comment {
        let now = now();
        let file_lines = split_lines(text);
        let snapshot = file_lines
            .get(lines.clone())
            .unwrap_or_default()
            .iter()
            .map(|s| s.to_string())
            .collect();
        Comment {
            id: String::new(),
            path: path.to_string(),
            side,
            start: lines.start + 1,
            end: lines.end.max(lines.start + 1),
            body: body.to_string(),
            author: author.to_string(),
            created: now,
            updated: now,
            resolved: false,
            lines: snapshot,
        }
    }

    /// Finds the commented lines in `text`: where they were if they're still there, else the
    /// nearest place they moved to, else the original position marked as outdated.
    pub fn locate(&self, text: &str) -> Location {
        let lines = split_lines(text);
        let n = lines.len();
        let start = (self.start.max(1) - 1).min(n.saturating_sub(1));
        let end = self.end.clamp(start + 1, n.max(start + 1));
        let original = start..end;
        let snap = &self.lines;
        if snap.is_empty() {
            return Location {
                lines: original,
                outdated: false,
            };
        }
        let matches_at = |i: usize| {
            i + snap.len() <= n
                && lines[i..i + snap.len()]
                    .iter()
                    .zip(snap)
                    .all(|(a, b)| a == b)
        };
        let want = self.start.max(1) - 1;
        if matches_at(want) {
            return Location {
                lines: want..want + snap.len(),
                outdated: false,
            };
        }
        match (0..n)
            .filter(|&i| matches_at(i))
            .min_by_key(|&i| i.abs_diff(want))
        {
            Some(i) => Location {
                lines: i..i + snap.len(),
                outdated: false,
            },
            None => Location {
                lines: original,
                outdated: true,
            },
        }
    }
}

/// Lines of a file, as the views show them (a trailing newline doesn't start a new line).
pub fn split_lines(text: &str) -> Vec<&str> {
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .collect()
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local/share")
        });
    base.join("twain")
}

fn processes_dir() -> PathBuf {
    data_dir().join("processes")
}

/// Socket addresses are limited to about 100 bytes. Longer paths are reached through a
/// directory file descriptor instead, which must be kept open while the address is used.
fn socket_addr(dir: &Path, name: &str) -> io::Result<(PathBuf, Option<std::fs::File>)> {
    let path = dir.join(name);
    if path.as_os_str().len() < 100 {
        return Ok((path, None));
    }
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(dir)?;
    let short = PathBuf::from(format!("/proc/self/fd/{}/{name}", file.as_raw_fd()));
    Ok((short, Some(file)))
}

/// A decoded database key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub root: String,
    pub path: String,
    pub id: String,
}

impl Key {
    pub fn encode(root: &str, path: &str, id: &str) -> Vec<u8> {
        [root.as_bytes(), path.as_bytes(), id.as_bytes()].join(&0u8)
    }

    pub fn decode(bytes: &[u8]) -> Option<Key> {
        let mut parts = bytes.splitn(3, |&b| b == 0);
        let mut next = || {
            parts
                .next()
                .map(|p| String::from_utf8_lossy(p).into_owned())
        };
        Some(Key {
            root: next()?,
            path: next()?,
            id: next()?,
        })
    }
}

fn prefix(root: &str, path: Option<&str>) -> Vec<u8> {
    let mut p = root.as_bytes().to_vec();
    p.push(0);
    if let Some(path) = path {
        p.extend_from_slice(path.as_bytes());
        p.push(0);
    }
    p
}

pub struct Store {
    env: heed::Env,
    db: heed::Database<Bytes, Bytes>,
}

fn db_err(e: heed::Error) -> String {
    format!("comment database: {e}")
}

/// The process-wide store. LMDB environments must only be opened once per process.
pub fn store() -> Result<&'static Store, String> {
    static STORE: OnceLock<Result<Store, String>> = OnceLock::new();
    STORE
        .get_or_init(Store::open)
        .as_ref()
        .map_err(Clone::clone)
}

impl Store {
    fn open() -> Result<Store, String> {
        let dir = data_dir().join("comments");
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        // SAFETY: the environment is opened once per process (see `store`), and the files are
        // only ever opened by LMDB itself.
        let env = unsafe {
            heed::EnvOpenOptions::new()
                .map_size(1 << 30)
                .max_dbs(1)
                .open(&dir)
        }
        .map_err(db_err)?;
        let mut wtxn = env.write_txn().map_err(db_err)?;
        let db = env.create_database(&mut wtxn, None).map_err(db_err)?;
        wtxn.commit().map_err(db_err)?;
        Ok(Store { env, db })
    }

    /// Comments of a repository, or of one file in it, ordered by file, then line.
    pub fn list(&self, root: &str, path: Option<&str>) -> Result<Vec<Comment>, String> {
        let rtxn = self.env.read_txn().map_err(db_err)?;
        let mut out = Vec::new();
        for entry in self
            .db
            .prefix_iter(&rtxn, &prefix(root, path))
            .map_err(db_err)?
        {
            let (_, value) = entry.map_err(db_err)?;
            match serde_json::from_slice::<Comment>(value) {
                Ok(c) => out.push(c),
                Err(e) => eprintln!("twain: skipping unreadable comment: {e}"),
            }
        }
        out.sort_by(|a, b| {
            (&a.path, a.side, a.start, a.created).cmp(&(&b.path, b.side, b.start, b.created))
        });
        Ok(out)
    }

    pub fn get(&self, root: &str, path: &str, id: &str) -> Result<Option<Comment>, String> {
        let rtxn = self.env.read_txn().map_err(db_err)?;
        let value = self
            .db
            .get(&rtxn, &Key::encode(root, path, id))
            .map_err(db_err)?;
        Ok(value.and_then(|v| serde_json::from_slice(v).ok()))
    }

    /// Finds a comment of the repository by id, or by an unambiguous id prefix.
    pub fn find(&self, root: &str, id: &str) -> Result<Comment, String> {
        let mut found: Vec<Comment> = self
            .list(root, None)?
            .into_iter()
            .filter(|c| c.id.starts_with(id))
            .collect();
        match found.len() {
            0 => Err(format!("no comment with id {id}")),
            1 => Ok(found.pop().unwrap()),
            _ if found.iter().any(|c| c.id == id) => {
                Ok(found.into_iter().find(|c| c.id == id).unwrap())
            }
            _ => Err(format!("id {id} is ambiguous")),
        }
    }

    /// Inserts or replaces comments, giving a fresh id to those without one, and notifies the
    /// other processes.
    pub fn put(&self, root: &str, comments: &mut [Comment]) -> Result<(), String> {
        let mut wtxn = self.env.write_txn().map_err(db_err)?;
        let mut keys = Vec::new();
        for comment in comments.iter_mut() {
            if comment.id.is_empty() {
                comment.id = loop {
                    let id = new_id();
                    let key = Key::encode(root, &comment.path, &id);
                    if self.db.get(&wtxn, &key).map_err(db_err)?.is_none() {
                        break id;
                    }
                };
            }
            let key = Key::encode(root, &comment.path, &comment.id);
            if key.len() > MAX_KEY {
                return Err(format!(
                    "path too long to store comments for: {}",
                    comment.path
                ));
            }
            let value = serde_json::to_vec(comment).map_err(|e| e.to_string())?;
            self.db.put(&mut wtxn, &key, &value).map_err(db_err)?;
            keys.push(key);
        }
        wtxn.commit().map_err(db_err)?;
        notify(&keys);
        Ok(())
    }

    /// Deletes comments and notifies the other processes.
    pub fn delete(&self, root: &str, comments: &[Comment]) -> Result<(), String> {
        let mut wtxn = self.env.write_txn().map_err(db_err)?;
        let mut keys = Vec::new();
        for c in comments {
            let key = Key::encode(root, &c.path, &c.id);
            if self.db.delete(&mut wtxn, &key).map_err(db_err)? {
                keys.push(key);
            }
        }
        wtxn.commit().map_err(db_err)?;
        notify(&keys);
        Ok(())
    }
}

/// A short random id; unique enough within a file, and checked on insert.
fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{:08x}", h.finish() as u32)
}

/// Sends each written key to every other process's socket.
fn notify(keys: &[Vec<u8>]) {
    if keys.is_empty() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(processes_dir()) else {
        return;
    };
    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };
    // Never block on a process that isn't reading its socket.
    let _ = sock.set_nonblocking(true);
    let me = std::process::id().to_string();
    let dir = processes_dir();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == me {
            continue;
        }
        let path = entry.path();
        let Ok((addr, _dir_fd)) = socket_addr(&dir, &name) else {
            continue;
        };
        for key in keys {
            match sock.send_to(key, &addr) {
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    // Nobody is bound to it any more: the process is gone.
                    let _ = std::fs::remove_file(&path);
                    break;
                }
                Err(_) => break,
            }
        }
    }
}

/// This process's socket, receiving the keys other processes write.
pub struct Listener {
    sock: UnixDatagram,
    path: PathBuf,
}

impl Listener {
    pub fn bind() -> io::Result<Listener> {
        let dir = processes_dir();
        std::fs::create_dir_all(&dir)?;
        let name = std::process::id().to_string();
        let path = dir.join(&name);
        // Left behind by an earlier process with the same pid.
        let _ = std::fs::remove_file(&path);
        let (addr, _dir_fd) = socket_addr(&dir, &name)?;
        let sock = UnixDatagram::bind(&addr)?;
        Ok(Listener { sock, path })
    }

    /// A duplicate of the socket's file descriptor, to watch for readability.
    pub fn dup_fd(&self) -> io::Result<std::os::fd::OwnedFd> {
        Ok(self.sock.try_clone()?.into())
    }

    /// Waits for the next key.
    pub fn recv(&self) -> io::Result<Option<Key>> {
        self.sock.set_nonblocking(false)?;
        self.sock.set_read_timeout(None)?;
        let mut buf = vec![0; 64 * 1024];
        let n = self.sock.recv(&mut buf)?;
        Ok(Key::decode(&buf[..n]))
    }

    /// Waits for the next key for at most `timeout`.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> io::Result<Option<Key>> {
        self.sock.set_nonblocking(false)?;
        self.sock.set_read_timeout(Some(timeout))?;
        let mut buf = vec![0; 64 * 1024];
        let n = self.sock.recv(&mut buf)?;
        Ok(Key::decode(&buf[..n]))
    }

    /// Every key received so far, without waiting.
    pub fn drain(&self) -> Vec<Key> {
        let _ = self.sock.set_nonblocking(true);
        let mut buf = vec![0; 64 * 1024];
        let mut out = Vec::new();
        while let Ok(n) = self.sock.recv(&mut buf) {
            out.extend(Key::decode(&buf[..n]));
        }
        out
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(start: usize, end: usize, lines: &[&str]) -> Comment {
        Comment {
            start,
            end,
            lines: lines.iter().map(|s| s.to_string()).collect(),
            ..Comment::new("f", Side::New, 0..1, "", "", "")
        }
    }

    #[test]
    fn key_round_trip() {
        let k = Key::encode("/repo", "src/a b.rs", "0123abcd");
        let d = Key::decode(&k).unwrap();
        assert_eq!(
            (d.root.as_str(), d.path.as_str(), d.id.as_str()),
            ("/repo", "src/a b.rs", "0123abcd")
        );
        assert!(k.starts_with(&prefix("/repo", Some("src/a b.rs"))));
        assert!(!k.starts_with(&prefix("/repo", Some("src/a"))));
        assert!(!Key::encode("/repo2", "x", "y").starts_with(&prefix("/repo", None)));
    }

    #[test]
    fn locate_follows_moved_lines() {
        let c = comment(2, 3, &["b", "c"]);
        assert_eq!(
            c.locate("a\nb\nc\nd\n"),
            Location {
                lines: 1..3,
                outdated: false
            }
        );
        assert_eq!(
            c.locate("x\ny\na\nb\nc\n"),
            Location {
                lines: 3..5,
                outdated: false
            }
        );
        assert_eq!(
            c.locate("a\nB\nc\n"),
            Location {
                lines: 1..3,
                outdated: true
            }
        );
        // Past the end of a file that shrank.
        assert_eq!(
            comment(9, 9, &["z"]).locate("a\n"),
            Location {
                lines: 0..1,
                outdated: true
            }
        );
    }

    #[test]
    fn new_snapshots_lines() {
        let c = Comment::new("f", Side::New, 1..3, "hi", "me", "a\nb\nc\nd\n");
        assert_eq!((c.start, c.end), (2, 3));
        assert_eq!(c.lines, vec!["b", "c"]);
    }
}
