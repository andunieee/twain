//! Comments on lines of files, shared between every twain process.
//!
//! Comments live in one SQLite database (`$XDG_DATA_HOME/twain/comments.db`, the host's one
//! also from Flatpak) that all twain processes — windows and `twain comment` invocations —
//! keep open at the same time, one row per comment keyed by repository root, repo-relative
//! path and id, with the comment as JSON. Replies are in a table of their own, keyed by their
//! comment's key and their own id. Its locks are byte-range locks on the database files,
//! which unlike LMDB's pid-keyed reader table work across Flatpak's pid namespaces.
//!
//! After committing a write, a process sends the key it wrote,
//!
//! ```text
//! <repo root> \0 <repo-relative path> \0 <comment id> [\0 <reply id>]
//! ```
//!
//! as a datagram to every socket in
//! `$XDG_DATA_HOME/twain/processes/` (each listening process binds one with a random name;
//! not the pid, since every Flatpak sandbox has its own pid namespace and they all collide),
//! and the receivers decide from the key whether they need to requery. Sockets left behind by
//! dead processes are removed by the first sender that gets refused.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::ops::Range;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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

/// A reply in a comment's thread. Replies have no lines of their own: they're shown wherever
/// their comment is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: String,
    /// Repository-relative path of the comment's file.
    pub path: String,
    /// Id of the comment replied to.
    pub comment: String,
    pub body: String,
    pub author: String,
    /// Unix timestamps in seconds.
    pub created: i64,
    pub updated: i64,
}

impl Reply {
    pub fn new(comment: &Comment, body: &str, author: &str) -> Reply {
        let now = now();
        Reply {
            id: String::new(),
            path: comment.path.clone(),
            comment: comment.id.clone(),
            body: body.to_string(),
            author: author.to_string(),
            created: now,
            updated: now,
        }
    }
}

/// A comment and its replies, oldest first.
#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub comment: Comment,
    pub replies: Vec<Reply>,
}

/// A comment or a reply, as found by id.
pub enum Item {
    Comment(Comment),
    Reply(Reply),
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
    // Inside Flatpak, XDG_DATA_HOME points into the sandbox's own ~/.var/app directory; use
    // the host's instead, so the Flatpak and a twain installed on the host share comments.
    let var = if std::env::var_os("FLATPAK_ID").is_some() {
        "HOST_XDG_DATA_HOME"
    } else {
        "XDG_DATA_HOME"
    };
    let base = std::env::var_os(var)
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

/// This process's socket name in `processes_dir`.
fn socket_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        format!("{}-{:016x}", std::process::id(), h.finish())
    })
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
    /// The comment's id, also for keys of its replies.
    pub id: String,
    pub reply: Option<String>,
}

impl Key {
    pub fn encode(root: &str, path: &str, id: &str, reply: Option<&str>) -> Vec<u8> {
        let mut parts = vec![root.as_bytes(), path.as_bytes(), id.as_bytes()];
        parts.extend(reply.map(str::as_bytes));
        parts.join(&0u8)
    }

    pub fn decode(bytes: &[u8]) -> Option<Key> {
        let mut parts = bytes.splitn(4, |&b| b == 0);
        let mut next = || {
            parts
                .next()
                .map(|p| String::from_utf8_lossy(p).into_owned())
        };
        Some(Key {
            root: next()?,
            path: next()?,
            id: next()?,
            reply: next(),
        })
    }
}

pub struct Store {
    conn: Mutex<rusqlite::Connection>,
}

fn db_err(e: rusqlite::Error) -> String {
    format!("comment database: {e}")
}

/// The process-wide store.
pub fn store() -> Result<&'static Store, String> {
    static STORE: OnceLock<Result<Store, String>> = OnceLock::new();
    STORE
        .get_or_init(Store::open)
        .as_ref()
        .map_err(Clone::clone)
}

impl Store {
    fn open() -> Result<Store, String> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let conn = rusqlite::Connection::open(dir.join("comments.db")).map_err(db_err)?;
        // Writers wait for each other instead of failing.
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(db_err)?;
        conn.pragma_update(None, "journal_mode", "wal")
            .map_err(db_err)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS comments (
                root TEXT NOT NULL,
                path TEXT NOT NULL,
                id TEXT NOT NULL,
                comment TEXT NOT NULL,
                PRIMARY KEY (root, path, id)
            ) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS replies (
                root TEXT NOT NULL,
                path TEXT NOT NULL,
                comment TEXT NOT NULL,
                id TEXT NOT NULL,
                reply TEXT NOT NULL,
                PRIMARY KEY (root, path, comment, id)
            ) WITHOUT ROWID;",
        )
        .map_err(db_err)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Comments of a repository, or of one file in it, ordered by file, then line.
    pub fn list(&self, root: &str, path: Option<&str>) -> Result<Vec<Comment>, String> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(
                "SELECT comment FROM comments WHERE root = ?1 AND (?2 IS NULL OR path = ?2)",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map((root, path), |row| row.get::<_, String>(0))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for value in rows {
            match serde_json::from_str::<Comment>(&value.map_err(db_err)?) {
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
        use rusqlite::OptionalExtension;
        let value: Option<String> = self
            .conn()
            .query_row(
                "SELECT comment FROM comments WHERE root = ?1 AND path = ?2 AND id = ?3",
                (root, path, id),
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;
        Ok(value.and_then(|v| serde_json::from_str(&v).ok()))
    }

    /// Replies of a repository, or of one file in it, ordered by comment, then age.
    pub fn list_replies(&self, root: &str, path: Option<&str>) -> Result<Vec<Reply>, String> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(
                "SELECT reply FROM replies WHERE root = ?1 AND (?2 IS NULL OR path = ?2)",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map((root, path), |row| row.get::<_, String>(0))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for value in rows {
            match serde_json::from_str::<Reply>(&value.map_err(db_err)?) {
                Ok(r) => out.push(r),
                Err(e) => eprintln!("twain: skipping unreadable reply: {e}"),
            }
        }
        out.sort_by(|a, b| {
            (&a.path, &a.comment, a.created, &a.id).cmp(&(&b.path, &b.comment, b.created, &b.id))
        });
        Ok(out)
    }

    /// Comments of a repository, or of one file in it, with their replies.
    pub fn threads(&self, root: &str, path: Option<&str>) -> Result<Vec<Thread>, String> {
        let mut replies: HashMap<(String, String), Vec<Reply>> = HashMap::new();
        for r in self.list_replies(root, path)? {
            replies
                .entry((r.path.clone(), r.comment.clone()))
                .or_default()
                .push(r);
        }
        Ok(self
            .list(root, path)?
            .into_iter()
            .map(|comment| Thread {
                replies: replies
                    .remove(&(comment.path.clone(), comment.id.clone()))
                    .unwrap_or_default(),
                comment,
            })
            .collect())
    }

    pub fn get_reply(
        &self,
        root: &str,
        path: &str,
        comment: &str,
        id: &str,
    ) -> Result<Option<Reply>, String> {
        use rusqlite::OptionalExtension;
        let value: Option<String> = self
            .conn()
            .query_row(
                "SELECT reply FROM replies
                 WHERE root = ?1 AND path = ?2 AND comment = ?3 AND id = ?4",
                (root, path, comment, id),
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;
        Ok(value.and_then(|v| serde_json::from_str(&v).ok()))
    }

    /// A comment and its replies.
    pub fn thread(&self, root: &str, comment: Comment) -> Result<Thread, String> {
        let replies = self
            .list_replies(root, Some(&comment.path))?
            .into_iter()
            .filter(|r| r.comment == comment.id)
            .collect();
        Ok(Thread { comment, replies })
    }

    /// Finds a comment or reply of the repository by id, or by an unambiguous id prefix.
    pub fn find(&self, root: &str, id: &str) -> Result<Item, String> {
        let mut found: Vec<(String, Item)> = self
            .list(root, None)?
            .into_iter()
            .map(|c| (c.id.clone(), Item::Comment(c)))
            .chain(
                self.list_replies(root, None)?
                    .into_iter()
                    .map(|r| (r.id.clone(), Item::Reply(r))),
            )
            .filter(|(i, _)| i.starts_with(id))
            .collect();
        if found.len() > 1 && found.iter().any(|(i, _)| i == id) {
            found.retain(|(i, _)| i == id);
        }
        match found.len() {
            0 => Err(format!("no comment or reply with id {id}")),
            1 => Ok(found.pop().unwrap().1),
            _ => Err(format!("id {id} is ambiguous")),
        }
    }

    /// Finds the thread of a comment, or of a reply, by id or unambiguous id prefix.
    pub fn find_thread(&self, root: &str, id: &str) -> Result<Thread, String> {
        let comment = match self.find(root, id)? {
            Item::Comment(c) => c,
            Item::Reply(r) => self
                .get(root, &r.path, &r.comment)?
                .ok_or_else(|| format!("the comment of reply {id} is gone"))?,
        };
        self.thread(root, comment)
    }

    /// Inserts or replaces comments, giving a fresh id to those without one, and notifies the
    /// other processes.
    pub fn put(&self, root: &str, comments: &mut [Comment]) -> Result<(), String> {
        let mut conn = self.conn();
        // IMMEDIATE takes the write lock up front, so the id check below can't race.
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        let mut keys = Vec::new();
        for comment in comments.iter_mut() {
            if comment.id.is_empty() {
                comment.id = loop {
                    let id = new_id();
                    let taken: bool = tx
                        .query_row(
                            "SELECT EXISTS (SELECT 1 FROM comments
                             WHERE root = ?1 AND path = ?2 AND id = ?3)",
                            (root, &comment.path, &id),
                            |row| row.get(0),
                        )
                        .map_err(db_err)?;
                    if !taken {
                        break id;
                    }
                };
            }
            let value = serde_json::to_string(comment).map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT OR REPLACE INTO comments (root, path, id, comment) VALUES (?1, ?2, ?3, ?4)",
                (root, &comment.path, &comment.id, &value),
            )
            .map_err(db_err)?;
            keys.push(Key::encode(root, &comment.path, &comment.id, None));
        }
        tx.commit().map_err(db_err)?;
        drop(conn);
        notify(&keys);
        Ok(())
    }

    /// Inserts or replaces replies, giving a fresh id to those without one, and notifies the
    /// other processes. Fails if a reply's comment is gone.
    pub fn put_replies(&self, root: &str, replies: &mut [Reply]) -> Result<(), String> {
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_err)?;
        let mut keys = Vec::new();
        for reply in replies.iter_mut() {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM comments
                     WHERE root = ?1 AND path = ?2 AND id = ?3)",
                    (root, &reply.path, &reply.comment),
                    |row| row.get(0),
                )
                .map_err(db_err)?;
            if !exists {
                return Err(format!("comment {} no longer exists", reply.comment));
            }
            if reply.id.is_empty() {
                // Unique in the repository, among comments too, so that ids rarely need more
                // than their prefix to be found.
                reply.id = loop {
                    let id = new_id();
                    let taken: bool = tx
                        .query_row(
                            "SELECT EXISTS (SELECT 1 FROM replies WHERE root = ?1 AND id = ?2)
                             OR EXISTS (SELECT 1 FROM comments WHERE root = ?1 AND id = ?2)",
                            (root, &id),
                            |row| row.get(0),
                        )
                        .map_err(db_err)?;
                    if !taken {
                        break id;
                    }
                };
            }
            let value = serde_json::to_string(reply).map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT OR REPLACE INTO replies (root, path, comment, id, reply)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (root, &reply.path, &reply.comment, &reply.id, &value),
            )
            .map_err(db_err)?;
            keys.push(Key::encode(
                root,
                &reply.path,
                &reply.comment,
                Some(&reply.id),
            ));
        }
        tx.commit().map_err(db_err)?;
        drop(conn);
        notify(&keys);
        Ok(())
    }

    /// Deletes comments with their replies, and notifies the other processes. Returns how
    /// many replies went with them.
    pub fn delete(&self, root: &str, comments: &[Comment]) -> Result<usize, String> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db_err)?;
        let mut keys = Vec::new();
        let mut deleted_replies = 0;
        for c in comments {
            let n = tx
                .execute(
                    "DELETE FROM comments WHERE root = ?1 AND path = ?2 AND id = ?3",
                    (root, &c.path, &c.id),
                )
                .map_err(db_err)?;
            if n > 0 {
                keys.push(Key::encode(root, &c.path, &c.id, None));
            }
            let mut stmt = tx
                .prepare_cached(
                    "DELETE FROM replies WHERE root = ?1 AND path = ?2 AND comment = ?3
                     RETURNING id",
                )
                .map_err(db_err)?;
            let replies = stmt
                .query_map((root, &c.path, &c.id), |row| row.get::<_, String>(0))
                .map_err(db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_err)?;
            drop(stmt);
            deleted_replies += replies.len();
            for r in &replies {
                keys.push(Key::encode(root, &c.path, &c.id, Some(r)));
            }
        }
        tx.commit().map_err(db_err)?;
        drop(conn);
        notify(&keys);
        Ok(deleted_replies)
    }

    /// Deletes replies and notifies the other processes.
    pub fn delete_replies(&self, root: &str, replies: &[Reply]) -> Result<(), String> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(db_err)?;
        let mut keys = Vec::new();
        for r in replies {
            let n = tx
                .execute(
                    "DELETE FROM replies WHERE root = ?1 AND path = ?2 AND comment = ?3 AND id = ?4",
                    (root, &r.path, &r.comment, &r.id),
                )
                .map_err(db_err)?;
            if n > 0 {
                keys.push(Key::encode(root, &r.path, &r.comment, Some(&r.id)));
            }
        }
        tx.commit().map_err(db_err)?;
        drop(conn);
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
    let me = socket_name();
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
        let name = socket_name();
        let path = dir.join(name);
        let (addr, _dir_fd) = socket_addr(&dir, name)?;
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
        let k = Key::encode("/repo", "src/a b.rs", "0123abcd", None);
        let d = Key::decode(&k).unwrap();
        assert_eq!(
            (d.root.as_str(), d.path.as_str(), d.id.as_str(), d.reply),
            ("/repo", "src/a b.rs", "0123abcd", None)
        );
        let k = Key::encode("/repo", "a", "0123abcd", Some("4567ef01"));
        let d = Key::decode(&k).unwrap();
        assert_eq!(
            (d.id.as_str(), d.reply.as_deref()),
            ("0123abcd", Some("4567ef01"))
        );
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
