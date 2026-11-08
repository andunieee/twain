//! A small LSP client: enough to ask a language server where a symbol is defined and what
//! its type is. Servers are started on demand for the files they can handle, speak JSON-RPC
//! over stdio, and stay running for the life of the window.
//!
//! There is no server discovery to speak of: the handful of servers below are tried on PATH
//! and then in Mason's bin directory, which is where Neovim keeps them and which Neovim does
//! not put on PATH outside itself.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long to wait for a server to answer one request.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A server we know how to start, with the file extensions it serves.
struct Server {
    command: &'static str,
    args: &'static [&'static str],
    extensions: &'static [&'static str],
}

const SERVERS: &[Server] = &[
    Server {
        command: "rust-analyzer",
        args: &[],
        extensions: &["rs"],
    },
    Server {
        command: "clangd",
        args: &[],
        extensions: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx"],
    },
    Server {
        command: "gopls",
        args: &[],
        extensions: &["go"],
    },
    Server {
        command: "pyright-langserver",
        args: &["--stdio"],
        extensions: &["py", "pyi"],
    },
    Server {
        command: "typescript-language-server",
        args: &["--stdio"],
        extensions: &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"],
    },
    Server {
        command: "lua-language-server",
        args: &[],
        extensions: &["lua"],
    },
    Server {
        command: "bash-language-server",
        args: &["start"],
        extensions: &["sh", "bash"],
    },
];

fn language_id(extension: &str) -> &'static str {
    match extension {
        "rs" => "rust",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "go" => "go",
        "py" | "pyi" => "python",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "lua" => "lua",
        "sh" | "bash" => "shellscript",
        _ => "plaintext",
    }
}

/// What to ask the server about a position.
#[derive(Clone, Copy)]
pub enum Lookup {
    Definition,
    Hover,
}

/// One place a symbol is defined.
pub struct Location {
    /// Relative to the repository root when the file is inside it.
    pub path: String,
    /// 0-based, as LSP positions are.
    pub line: usize,
    /// The first few lines at that place, when they could be read.
    pub snippet: Option<String>,
}

pub enum Answer {
    Definition(Vec<Location>),
    /// The hover contents, with markdown fences stripped.
    Hover(String),
}

/// The servers of one window, started on first use and keyed by language.
pub struct Manager {
    root: Mutex<PathBuf>,
    clients: Mutex<HashMap<&'static str, Arc<Client>>>,
}

impl Manager {
    pub fn new() -> Arc<Manager> {
        Arc::new(Manager {
            root: Mutex::default(),
            clients: Mutex::default(),
        })
    }

    /// Points the manager at a repository, dropping the servers of a previous one.
    pub fn set_root(&self, root: PathBuf) {
        {
            let mut current = self.root.lock().unwrap();
            if *current == root {
                return;
            }
            *current = root;
        }
        self.clients.lock().unwrap().clear();
    }

    /// Asks the server for the file at `path` (relative to the root) about one position.
    /// `text` is what the caller shows for that file, which is what the server is told it
    /// contains: the working tree may differ from what the diff shows.
    pub fn lookup(
        &self,
        path: &str,
        text: &str,
        line: u32,
        character: u32,
        what: Lookup,
    ) -> Result<Answer, String> {
        let abs = self.root.lock().unwrap().join(path);
        let (client, language) = self.client_for(&abs)?;
        let uri = file_uri(&abs);
        client.document(&uri, text, language)?;
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        });
        match what {
            Lookup::Definition => {
                let result = client.query("textDocument/definition", params)?;
                Ok(Answer::Definition(self.locations(&result)))
            }
            Lookup::Hover => {
                let result = client.query("textDocument/hover", params)?;
                match result.get("contents").and_then(hover_text) {
                    Some(text) => Ok(Answer::Hover(text)),
                    None => Err("no type information here".into()),
                }
            }
        }
    }

    /// The client for a file, started if this is the first file of its language.
    fn client_for(&self, path: &Path) -> Result<(Arc<Client>, &'static str), String> {
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let server = SERVERS
            .iter()
            .find(|s| s.extensions.contains(&extension.as_str()))
            .ok_or_else(|| {
                if extension.is_empty() {
                    "No language server for files without an extension".to_string()
                } else {
                    format!("No language server configured for .{extension} files")
                }
            })?;
        let language = language_id(&extension);
        let mut clients = self.clients.lock().unwrap();
        if let Some(client) = clients.get(server.command) {
            return Ok((client.clone(), language));
        }
        let program = find_server(server.command).ok_or_else(|| {
            format!(
                "{} was not found on PATH or in Mason's bin directory",
                server.command
            )
        })?;
        let root = self.root.lock().unwrap().clone();
        let client = Client::start(&program, server.args, &root, server.command)?;
        clients.insert(server.command, client.clone());
        Ok((client, language))
    }

    /// Reads the definition result, which LSP spells three ways: a Location, an array of
    /// them, or LocationLinks.
    fn locations(&self, result: &Value) -> Vec<Location> {
        let root = self.root.lock().unwrap();
        let one = |value: &Value| {
            let uri = value
                .get("uri")
                .or_else(|| value.get("targetUri"))
                .and_then(Value::as_str)?;
            // The selection is the name itself, which is where to jump; the target range is
            // the whole definition, which is more useful to show.
            let selection = value
                .get("targetSelectionRange")
                .or_else(|| value.get("range"))?;
            let target = value
                .get("targetRange")
                .or_else(|| value.get("range"))
                .unwrap_or(selection);
            let line = selection.get("start")?.get("line")?.as_u64()? as usize;
            let start = target
                .get("start")
                .and_then(|start| start.get("line"))
                .and_then(Value::as_u64)
                .map_or(line, |start| start as usize);
            let end = target.get("end")?.get("line")?.as_u64()? as usize;
            let path = uri_to_path(uri)?;
            let relative = path.strip_prefix(&*root).unwrap_or(&path);
            let snippet = read_snippet(&path, start.min(line), end.max(line));
            Some(Location {
                path: relative.to_string_lossy().into_owned(),
                line,
                snippet,
            })
        };
        let mut locations: Vec<Location> = match result {
            Value::Array(items) => items.iter().filter_map(one).collect(),
            Value::Null => Vec::new(),
            value => one(value).into_iter().collect(),
        };
        locations.dedup_by(|a, b| a.path == b.path && a.line == b.line);
        locations
    }
}

/// The servers we know, in the directories they're installed in: PATH, then Mason's.
fn find_server(command: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.as_ref().map(|home| home.join(".local").join("share")));
    if let Some(data) = data {
        // Mason installs a wrapper per server here, and only adds the directory to PATH
        // inside Neovim, so a diff viewer has to look for it itself.
        let app = std::env::var("NVIM_APPNAME").unwrap_or_else(|_| "nvim".into());
        dirs.push(data.join(app).join("mason").join("bin"));
    }
    if let Some(home) = home {
        // Neovim from Flathub keeps its data under the app's own directory.
        dirs.push(home.join(".var/app/io.neovim.nvim/data/nvim/mason/bin"));
    }
    dirs.into_iter().map(|dir| dir.join(command)).find(|path| {
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

struct Client {
    inner: Mutex<Inner>,
    name: &'static str,
    ready: Arc<Readiness>,
}

/// Whether the server has finished loading the workspace. rust-analyzer says so with its
/// non-standard `experimental/serverStatus` notification; servers that don't send one are
/// assumed ready.
#[derive(Default)]
struct Readiness {
    quiescent: Mutex<Option<bool>>,
    changed: std::sync::Condvar,
}

impl Readiness {
    fn set(&self, quiescent: bool) {
        *self.quiescent.lock().unwrap() = Some(quiescent);
        self.changed.notify_all();
    }

    /// Whether the server is ready, or has never said it isn't.
    fn ready(&self) -> bool {
        *self.quiescent.lock().unwrap() != Some(false)
    }

    fn wait(&self, deadline: Instant) {
        let mut state = self.quiescent.lock().unwrap();
        while *state == Some(false) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
    }
}

struct RpcError {
    message: String,
    /// LSP's `ContentModified`: the server is still getting to the answer, try again.
    retry: bool,
}

struct Inner {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    incoming: mpsc::Receiver<Incoming>,
    next_id: i64,
    version: i64,
    /// What the server was last told each open document contains.
    open: HashMap<String, String>,
}

enum Incoming {
    Response {
        id: i64,
        result: Result<Value, RpcError>,
    },
}

impl Client {
    fn start(
        program: &Path,
        args: &[&str],
        root: &Path,
        name: &'static str,
    ) -> Result<Arc<Client>, String> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Servers are chatty on stderr; it would only end up in the terminal.
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start {name}: {e}"))?;
        let stdin = Arc::new(Mutex::new(
            child.stdin.take().ok_or("the server has no stdin")?,
        ));
        let stdout = child.stdout.take().ok_or("the server has no stdout")?;
        let (sender, incoming) = mpsc::channel();
        let ready = Arc::new(Readiness::default());
        reader(stdout, stdin.clone(), sender, ready.clone());
        let client = Arc::new(Client {
            inner: Mutex::new(Inner {
                child,
                stdin,
                incoming,
                next_id: 1,
                version: 1,
                open: HashMap::new(),
            }),
            name,
            ready,
        });
        client.initialize(root)?;
        Ok(client)
    }

    fn initialize(&self, root: &Path) -> Result<(), String> {
        let root_uri = file_uri(root);
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "clientInfo": { "name": "twain", "version": env!("CARGO_PKG_VERSION") },
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": name }],
                "capabilities": {
                    // Columns are counted in UTF-16 code units, LSP's default.
                    "general": { "positionEncodings": ["utf-16"] },
                    "textDocument": {
                        "definition": { "linkSupport": true },
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                    },
                    // rust-analyzer reports when it has finished loading the workspace.
                    "experimental": { "serverStatusNotification": true },
                    "workspace": { "workspaceFolders": true },
                },
            }),
        )
        .map_err(|error| error.message)?;
        self.notify("initialized", json!({}))
    }

    /// Tells the server what an open document contains, if it isn't up to date already.
    fn document(&self, uri: &str, text: &str, language: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.open.get(uri).is_some_and(|open| open == text) {
            return Ok(());
        }
        let (method, params) = if inner.open.contains_key(uri) {
            inner.version += 1;
            (
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": inner.version },
                    "contentChanges": [{ "text": text }],
                }),
            )
        } else {
            (
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language,
                        "version": inner.version,
                        "text": text,
                    },
                }),
            )
        };
        let stdin = inner.stdin.clone();
        let mut stdin = stdin.lock().unwrap();
        write_frame(
            &mut *stdin,
            &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        )?;
        inner.open.insert(uri.to_string(), text.to_string());
        Ok(())
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let inner = self.inner.lock().unwrap();
        let mut stdin = inner.stdin.lock().unwrap();
        write_frame(
            &mut *stdin,
            &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        )
    }

    /// One request, waiting up to `TIMEOUT` for its answer.
    fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        let mut stdin = inner.stdin.lock().unwrap();
        write_frame(
            &mut *stdin,
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        )
        .map_err(|message| RpcError {
            message,
            retry: false,
        })?;
        drop(stdin);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let timed_out = || RpcError {
                message: format!("{} did not answer {method} in time", self.name),
                retry: false,
            };
            if left.is_zero() {
                return Err(timed_out());
            }
            match inner.incoming.recv_timeout(left) {
                // A response to a request that already timed out: it has nowhere to go.
                Ok(Incoming::Response { id: got, .. }) if got != id => continue,
                Ok(Incoming::Response { result, .. }) => return result,
                Err(RecvTimeoutError::Timeout) => return Err(timed_out()),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(RpcError {
                        message: format!("{} exited", self.name),
                        retry: false,
                    });
                }
            }
        }
    }

    /// A request for the user: waiting for the initial load to finish, and retrying while
    /// the server says it is still working on it.
    fn query(&self, method: &str, params: Value) -> Result<Value, String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.ready.wait(deadline);
            match self.request(method, params.clone()) {
                Ok(result) => {
                    let empty = result.is_null() || result.as_array().is_some_and(Vec::is_empty);
                    if empty && !self.ready.ready() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(150));
                        continue;
                    }
                    return Ok(result);
                }
                Err(error) if error.retry && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(150));
                }
                Err(error) => return Err(error.message),
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads the server's messages, answers its own requests, and forwards the responses to
/// `request`, which is the only thing waiting on them.
fn reader(
    stdout: ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    sender: Sender<Incoming>,
    ready: Arc<Readiness>,
) {
    std::thread::Builder::new()
        .name("lsp-reader".into())
        .spawn(move || {
            let mut stdout = BufReader::new(stdout);
            while let Ok(message) = read_frame(&mut stdout) {
                let method = message.get("method").and_then(Value::as_str);
                let id = message.get("id").filter(|id| !id.is_null());
                if let (Some(method), Some(id)) = (method, id) {
                    // A request from the server. Answer it so the server doesn't wait, but
                    // we have nothing to contribute to any of them.
                    let result = match method {
                        "workspace/configuration" => {
                            let items = message
                                .get("params")
                                .and_then(|p| p.get("items"))
                                .and_then(Value::as_array)
                                .map_or(0, Vec::len);
                            Value::Array(vec![Value::Null; items])
                        }
                        "workspace/applyEdit" => json!({ "applied": false }),
                        _ => Value::Null,
                    };
                    let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                    if stdin
                        .lock()
                        .map(|mut stdin| write_frame(&mut *stdin, &reply))
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                if method == Some("experimental/serverStatus") {
                    let quiescent = message
                        .get("params")
                        .and_then(|params| params.get("quiescent"))
                        .and_then(Value::as_bool);
                    if let Some(quiescent) = quiescent {
                        ready.set(quiescent);
                    }
                    continue;
                }
                // A notification (diagnostics, progress): nothing here listens to them.
                let Some(id) = message.get("id").and_then(Value::as_i64) else {
                    continue;
                };
                let result = match message.get("error") {
                    Some(error) => Err(RpcError {
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("the language server failed")
                            .to_string(),
                        // ContentModified: the server is still working on the answer.
                        retry: error.get("code").and_then(Value::as_i64) == Some(-32801),
                    }),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                if sender.send(Incoming::Response { id, result }).is_err() {
                    break;
                }
            }
        })
        .ok();
}

fn read_frame(reader: &mut impl BufRead) -> Result<Value, String> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(|e| format!("could not read from the language server: {e}"))?
            == 0
        {
            return Err("the language server exited".into());
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let length = length.ok_or("a message without a length")?;
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|e| format!("could not read from the language server: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("bad message from the language server: {e}"))
}

fn write_frame(writer: &mut impl Write, message: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(message).map_err(|e| e.to_string())?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).map_err(|e| e.to_string())?;
    writer.write_all(&body).map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())
}

/// A `file://` URI for a path. LSP clients are supposed to percent-encode everything but
/// unreserved characters, which is what servers expect back too.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && i + 2 < raw.len()
            && let Ok(byte) = u8::from_str_radix(&rest[i + 1..i + 3], 16)
        {
            bytes.push(byte);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
}

/// The lines of a definition, dedented enough to read in a card. At most a couple dozen, so
/// that a huge one can't take over the screen; the caller shows fewer where space is tight.
fn read_snippet(path: &Path, start: usize, end: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let start = start.min(lines.len() - 1);
    let end = end.max(start).min(start + 23).min(lines.len() - 1);
    let lines = &lines[start..=end];
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0)
        .min(12);
    let snippet = lines
        .iter()
        .map(|line| line.get(indent..).unwrap_or(line).trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    Some(snippet)
}

fn hover_text(contents: &Value) -> Option<String> {
    let text = match contents {
        Value::String(text) => text.clone(),
        Value::Object(object) => object.get("value")?.as_str()?.to_string(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| hover_text(item))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => return None,
    };
    // Markdown, with the code fences taken out: there is no renderer here, and the
    // signatures read fine as they are.
    let text = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .filter(|line| line.trim() != "---")
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}
