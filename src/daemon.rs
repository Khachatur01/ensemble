//! `ensemble node serve`: a node, the one long-running process that holds a
//! helper team on this computer.
//!
//! It is tied to no project. Your agent CLI starts `ensemble mcp` in a folder,
//! which connects to the node's socket, says which folder it is in, and from
//! then on carries MCP both ways ([`bridge`]). The node keeps one worker per
//! folder it has served, which runs the tasks the hub keeps for the folder's
//! project (its name): they go on after the session ends, and start again when
//! the node does. With a hub server it also keeps one link, for whom it works
//! for: the agents and models it offers.
//!
//! What it keeps for a folder lives as long as the node, so it is leaked on
//! purpose: a folder served once is served until the node stops.

use crate::STOP;
use crate::cmd::node::NodeContext;
use ensemble::board::RemoteBoard;
use ensemble::config::{Config, Node};
use ensemble::folders::Folders;
use ensemble::knowledge::KnowledgeAccess;
use ensemble::link::{RemoteMemory, Token};
use ensemble::mcp;
use ensemble::ops::{Code, Error, Registry};
use ensemble::queue::Queue;
use ensemble::router::Ensemble;
use ensemble::runs::Runs;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// The longest a hello line may be.
const MAX_HELLO: usize = 64 * 1024;
/// How long stopping waits for the folders' running tasks to be put back.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// What a node keeps for one folder.
struct Folder {
    ensemble: &'static Ensemble,
    knowledge: Option<&'static RemoteMemory>,
    queue: &'static Queue<'static>,
}

/// The node: its team, and the folders it serves.
pub struct Daemon<'a> {
    node: Node,
    /// The hub server it names, and its token, replaced when you sign in again.
    hub: Option<(String, Token)>,
    /// Its teams, as one helper team: `<team>/<agent>`. Replaced when they are
    /// reloaded.
    team: RwLock<Config>,
    /// Where the keys its teams name are read from, as `node serve` was given.
    env_file: Option<PathBuf>,
    /// Counts reloads, so that open sessions tell their clients the tools
    /// changed.
    tools_changed: Arc<tokio::sync::watch::Sender<u64>>,
    /// The node's operations, and what they are given here; none when its
    /// `interfaces.cli` is off.
    operations: Option<(&'a Registry<NodeContext>, NodeContext)>,
    folders: Folders,
    open: Mutex<HashMap<PathBuf, Arc<Folder>>>,
    /// The folders' workers, which stopping waits for.
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The open sessions, which stopping ends.
    sessions: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    say: &'a (dyn Fn(&str) + Sync),
}

impl<'a> Daemon<'a> {
    /// A node from `node`, its teams loaded as `team`, keeping its state in
    /// `dir`.
    pub fn new(
        node: Node,
        hub: Option<(String, Token)>,
        team: Config,
        env_file: Option<PathBuf>,
        dir: &Path,
        operations: &'a Registry<NodeContext>,
        say: &'a (dyn Fn(&str) + Sync),
    ) -> Result<Self, String> {
        let tools_changed = Arc::new(tokio::sync::watch::Sender::new(0));
        let operations = if node.interfaces.cli {
            let (hub, changed) = (hub.clone(), tools_changed.clone());
            let relink = move || match &hub {
                Some((url, token)) => read_token_again(url, token, &changed),
                None => Err("the node names no hub".to_string()),
            };
            Some((operations, NodeContext::running(relink)?))
        } else {
            None
        };
        Ok(Daemon {
            node,
            hub,
            team: RwLock::new(team),
            env_file,
            tools_changed,
            operations,
            folders: Folders::new(dir),
            open: Mutex::new(HashMap::new()),
            workers: Mutex::new(Vec::new()),
            sessions: Mutex::new(Vec::new()),
            say,
        })
    }

    /// Serve sessions on `socket` until [`STOP`] is set, after starting the
    /// workers of the folders served before. A reload of the teams, asked on
    /// the socket, is also given to `reloaded`, for what else holds them.
    pub async fn serve(
        &self,
        socket: &Path,
        reloaded: &(dyn Fn(&Config) -> Result<(), String> + Sync),
    ) -> Result<(), String> {
        for folder in self.folders.all().into_values().filter(|f| f.is_dir()) {
            if let Err(e) = self.folder(&folder) {
                (self.say)(&format!("{}: {e}", folder.display()));
            }
        }
        let listener = listen(socket)?;
        (self.say)(&format!(
            "node '{}' is ready for `ensemble mcp` sessions",
            self.node.name
        ));
        while !STOP.load(Ordering::Relaxed) {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = tokio::time::sleep(Duration::from_millis(200)) => continue,
            };
            let Ok((stream, _)) = accepted else { continue };
            match self.hello(stream, reloaded).await {
                Ok(None) => {}
                Ok(Some((folder, stream))) => {
                    let changed = self.tools_changed.subscribe();
                    // The folder outlives the session.
                    let session = tokio::spawn(async move {
                        let (input, output) = stream.into_split();
                        let _ = mcp::serve_session(
                            folder.ensemble,
                            folder.knowledge.map(|k| k as &dyn KnowledgeAccess),
                            folder.queue,
                            tokio::io::BufReader::new(input),
                            output,
                            false,
                            Some(changed),
                        )
                        .await;
                    });
                    let mut sessions = self.sessions.lock().unwrap();
                    sessions.retain(|s| !s.is_finished());
                    sessions.push(session);
                }
                Err(e) => (self.say)(&format!("a session was refused: {e}")),
            }
        }
        let _ = std::fs::remove_file(socket);
        // Their `ensemble mcp` connects again, to the node that runs next.
        for session in std::mem::take(&mut *self.sessions.lock().unwrap()) {
            session.abort();
        }
        (self.say)("stopping: running tasks are stopped and queued again");
        for folder in self.open.lock().unwrap().values() {
            folder.queue.close();
        }
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        let stopped = async {
            for worker in workers {
                let _ = worker.await;
            }
        };
        let _ = tokio::time::timeout(STOP_GRACE, stopped).await;
        Ok(())
    }

    /// Read a session's hello, `{"dir": "/abs/folder"}`, and answer it: the
    /// folder's worker, or why there is none. `{"reload": true}` reloads the
    /// teams instead, and `{"op": …, "input": …}` runs one of the node's
    /// operations (`local`); each answers what it did, and no session follows.
    async fn hello(
        &self,
        mut stream: UnixStream,
        reloaded: &(dyn Fn(&Config) -> Result<(), String> + Sync),
    ) -> Result<Option<(Arc<Folder>, UnixStream)>, String> {
        let line = read_line(&mut stream).await?;
        let asked = serde_json::from_str::<serde_json::Value>(&line).unwrap_or_default();
        if asked.get("op").is_some() {
            let reply = match &self.operations {
                Some((registry, context)) => crate::local::answer(registry, context, &asked)
                    .unwrap_or_else(|| serde_json::json!({ "error": Error::invalid("`op` must name an operation") })),
                None => serde_json::json!({ "error": Error::new(
                    Code::Forbidden,
                    "this node takes no commands: its `interfaces.cli` is off",
                ) }),
            };
            stream
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            return Ok(None);
        }
        if asked["reload"] == true {
            let reply = match self.reload(reloaded) {
                Ok(said) => serde_json::json!({"ok": true, "said": said}),
                Err(e) => serde_json::json!({"error": e}),
            };
            stream
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            return Ok(None);
        }
        let answer = serde_json::from_str::<serde_json::Value>(&line)
            .map_err(|e| format!("the hello is not JSON: {e}"))
            .and_then(|hello| {
                let dir = hello["dir"].as_str().map(PathBuf::from).unwrap_or_default();
                if !dir.is_absolute() || !dir.is_dir() {
                    return Err(format!("'{}' is not a folder", dir.display()));
                }
                self.folder(&dir)
            });
        let reply = match &answer {
            Ok(_) => serde_json::json!({"ok": true, "node": self.node.name}),
            Err(e) => serde_json::json!({"error": e}),
        };
        stream
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        Ok(Some((answer?, stream)))
    }

    /// Read the teams again and use them: in every folder from its next run
    /// on, in what `reloaded` holds, and in the tools sessions are offered. A
    /// team that cannot be loaded leaves everything as it was, and says why.
    fn reload(
        &self,
        reloaded: &(dyn Fn(&Config) -> Result<(), String> + Sync),
    ) -> Result<String, String> {
        let (team, waiting) =
            ensemble::teams::load_all(self.env_file.as_deref(), &ensemble::config::SystemEnv)?;
        reloaded(&team)?;
        for folder in self.open.lock().unwrap().values() {
            folder.ensemble.set_config(team.clone());
        }
        let said = format!(
            "teams reloaded: {} agent(s){}",
            team.agents.len(),
            waiting.iter().map(|w| format!("; {w}")).collect::<String>()
        );
        *self.team.write().unwrap_or_else(|e| e.into_inner()) = team;
        self.tools_changed.send_modify(|n| *n += 1);
        (self.say)(&said);
        Ok(said)
    }

    /// The worker of `dir`, started the first time it is asked for.
    fn folder(&self, dir: &Path) -> Result<Arc<Folder>, String> {
        let dir = dir
            .canonicalize()
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        if let Some(open) = self.open.lock().unwrap().get(&dir) {
            return Ok(open.clone());
        }
        let config = self.team.read().unwrap_or_else(|e| e.into_inner()).clone();
        // Each folder keeps its own state.
        let ensemble: &'static Ensemble =
            Box::leak(Box::new(Ensemble::new(config, Some(dir.clone()))));
        // The hub's knowledge, named for the folder in what is shared.
        let knowledge: Option<&'static RemoteMemory> = self.hub.as_ref().map(|(url, token)| {
            let project = dir.file_name().map(|n| n.to_string_lossy().into_owned());
            &*Box::leak(Box::new(RemoteMemory::with_token(
                url,
                token.clone(),
                project,
            )))
        });
        let memory = knowledge.map(|k| k as &dyn KnowledgeAccess);
        let state = ensemble.state_dir().map_err(|e| e.to_string())?;
        let mut runs = Runs::new(&state);
        if let Some(usage) = ensemble.usage_dir() {
            runs = runs.with_usage_dir(usage);
        }
        mcp::note_recall_without_knowledge(ensemble, memory);
        let mut queue = mcp::queue_for(ensemble, Some(runs), memory);
        // The tasks the hub keeps for the folder's project. Of two folders of
        // one name, the one used last runs them.
        if let (Some((url, token)), Some(project)) = (&self.hub, dir.file_name()) {
            let project = project.to_string_lossy().into_owned();
            let board = RemoteBoard::with_token(url, token.clone(), &project);
            let (folders, here) = (self.folders.clone(), dir.clone());
            queue = queue
                .with_board(board, &self.node.name)
                .with_turn(move || folders.of(&project).is_none_or(|f| f == here));
        }
        let queue: &'static Queue<'static> = Box::leak(Box::new(queue));
        // One worker per folder: when another process has it, sessions here
        // only read and write its tasks, and run their background
        // delegations.
        let lock = ensemble::lock::acquire(&state);
        if let Err(pid) = &lock {
            (self.say)(&format!(
                "{}: process {pid} runs this folder's tasks; sessions here only read and write them",
                dir.display()
            ));
        }
        let worker = tokio::spawn(async move {
            let lock = lock.ok();
            queue.work(lock.is_some()).await;
        });
        self.workers.lock().unwrap().push(worker);
        self.folders.remember(&dir)?;
        let folder = Arc::new(Folder {
            ensemble,
            knowledge,
            queue,
        });
        self.open
            .lock()
            .unwrap()
            .insert(dir.clone(), folder.clone());
        (self.say)(&format!("serving {}", dir.display()));
        Ok(folder)
    }
}

/// Listen on `socket`, in a folder only this user may enter. A socket left by
/// a node that died is replaced; one a running node answers on is not.
pub(crate) fn listen(socket: &Path) -> Result<UnixListener, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let dir = socket.parent().ok_or("the socket has no folder")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let meta = std::fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // SAFETY: a plain libc call with no arguments.
    let me = unsafe { libc::getuid() };
    if meta.uid() != me || meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{} is not a folder only you may enter; remove it and start the node again",
            dir.display()
        ));
    }
    if socket.exists() {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return Err(format!("a node already listens on {}", socket.display()));
        }
        let _ = std::fs::remove_file(socket);
    }
    UnixListener::bind(socket).map_err(|e| format!("cannot listen on {}: {e}", socket.display()))
}

/// One line, read a byte at a time so nothing after it is taken from the
/// stream.
pub(crate) async fn read_line(stream: &mut UnixStream) -> Result<String, String> {
    let mut line = Vec::new();
    loop {
        let byte = stream.read_u8().await.map_err(|e| e.to_string())?;
        if byte == b'\n' {
            return String::from_utf8(line).map_err(|e| e.to_string());
        }
        if line.len() == MAX_HELLO {
            return Err("the hello is too long".into());
        }
        line.push(byte);
    }
}

/// How long a session waits for its node to come back once it went away.
const COME_BACK: Duration = Duration::from_secs(30);
/// The id of the `initialize` a session says again to a node started anew.
const AGAIN: &str = "ensemble-again";

/// `ensemble mcp` with no `--team`: carry MCP between stdio and the node on
/// `socket`, as a session in `dir`, one message a line. Ends when the node
/// closes the session once stdin ended and its calls are answered.
///
/// A node that goes away while the session goes on, as when it starts again
/// with a changed node.yaml, is waited for: the requests it left unanswered
/// are answered with an error to try again, the session's `initialize` is
/// said again to the node that comes back, and the client is told its tools
/// may have changed. The client never sees the session end.
pub async fn bridge(socket: &Path, dir: &Path, node: &str) -> Result<(), String> {
    use std::collections::HashSet;
    use tokio::io::AsyncBufReadExt;
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    // The client's `initialize` and `notifications/initialized`, as it said
    // them, and the ids of its requests the node has not answered yet.
    let mut opening: Vec<serde_json::Value> = Vec::new();
    let mut waiting: HashSet<String> = HashSet::new();
    let mut again = false;
    loop {
        let stream = if again {
            reopen(socket, dir, node).await?
        } else {
            open(socket, dir, node).await?
        };
        let (from_node, mut to_node) = stream.into_split();
        let mut from_node = tokio::io::BufReader::new(from_node).lines();
        if again && !opening.is_empty() {
            for said in &opening {
                let mut said = said.clone();
                if said.get("id").is_some() {
                    said["id"] = serde_json::json!(AGAIN);
                }
                send(&mut to_node, &said.to_string()).await?;
                // Its answer is the bridge's, not the client's.
                if said["id"] == AGAIN {
                    while let Some(line) = from_node.next_line().await.map_err(|e| e.to_string())? {
                        let answer: serde_json::Value =
                            serde_json::from_str(&line).unwrap_or_default();
                        if answer["id"] == AGAIN {
                            break;
                        }
                        send(&mut stdout, &line).await?;
                    }
                }
            }
            let changed = serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" });
            send(&mut stdout, &changed.to_string()).await?;
        }
        // Carry messages both ways until stdin ends or the node goes away.
        let gone = loop {
            tokio::select! {
                line = stdin.next_line() => match line {
                    Ok(Some(line)) => {
                        let said: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                        match said["method"].as_str() {
                            Some("initialize") => opening = vec![said.clone()],
                            Some("notifications/initialized") => opening.push(said.clone()),
                            _ => {}
                        }
                        if said.get("method").is_some() && let Some(id) = said.get("id") {
                            waiting.insert(id.to_string());
                        }
                        if send(&mut to_node, &line).await.is_err() {
                            break true;
                        }
                    }
                    // The client is done: the node answers what it has, then
                    // closes the session.
                    _ => {
                        let _ = to_node.shutdown().await;
                        while let Ok(Some(line)) = from_node.next_line().await {
                            send(&mut stdout, &line).await?;
                        }
                        return Ok(());
                    }
                },
                line = from_node.next_line() => match line {
                    Ok(Some(line)) => {
                        let said: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                        if said.get("method").is_none() && let Some(id) = said.get("id") {
                            waiting.remove(&id.to_string());
                        }
                        send(&mut stdout, &line).await?;
                    }
                    _ => break true,
                },
            }
        };
        if gone {
            for id in waiting.drain() {
                let id: serde_json::Value = serde_json::from_str(&id).unwrap_or_default();
                let answer = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32000, "message": "the node started again while this ran; try again" },
                });
                send(&mut stdout, &answer.to_string()).await?;
            }
            again = true;
        }
    }
}

/// Write `line`, and its end, and flush it.
async fn send(to: &mut (impl AsyncWriteExt + Unpin), line: &str) -> Result<(), String> {
    to.write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    to.flush().await.map_err(|e| e.to_string())
}

/// A session of the node on `socket`, in `dir`.
async fn open(socket: &Path, dir: &Path, node: &str) -> Result<UnixStream, String> {
    let mut stream = UnixStream::connect(socket).await.map_err(|e| {
        format!("no node '{node}' is running ({e}). Start it with `ensemble node serve`")
    })?;
    let hello = serde_json::json!({ "dir": dir });
    stream
        .write_all(format!("{hello}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let reply: serde_json::Value = serde_json::from_str(&read_line(&mut stream).await?)
        .map_err(|e| format!("the node answered something else: {e}"))?;
    if let Some(e) = reply["error"].as_str() {
        return Err(format!("node '{node}' refused the session: {e}"));
    }
    Ok(stream)
}

/// [`open`], once the node comes back, waiting for it up to [`COME_BACK`].
async fn reopen(socket: &Path, dir: &Path, node: &str) -> Result<UnixStream, String> {
    let until = std::time::Instant::now() + COME_BACK;
    loop {
        match open(socket, dir, node).await {
            Ok(stream) => return Ok(stream),
            Err(e) if std::time::Instant::now() >= until => {
                return Err(format!("the node went away, and did not come back: {e}"));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

/// Tell the node on `socket` about the folder `dir`, so that its worker runs
/// there: a session that says hello and ends. What `submit`, `assign` and
/// `answer` do, so that a folder no session has opened yet is served too.
pub async fn touch(socket: &Path, dir: &Path) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("no node is running ({e})"))?;
    let hello = serde_json::json!({ "dir": dir });
    stream
        .write_all(format!("{hello}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let reply: serde_json::Value = serde_json::from_str(&read_line(&mut stream).await?)
        .map_err(|e| format!("the node answered something else: {e}"))?;
    match reply["error"].as_str() {
        Some(e) => Err(format!("the node refused this folder: {e}")),
        None => Ok(()),
    }
}

/// Ask the node on `socket` to reload its teams, and what it said.
pub async fn reload(socket: &Path) -> Result<String, String> {
    ask(socket, "reload")
        .await
        .map_err(|e| format!("the node kept its teams as they were: {e}"))
}

/// Send the node on `socket` `{"<what>": true}`, and what it said it did.
async fn ask(socket: &Path, what: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("no node is running ({e})"))?;
    stream
        .write_all(format!("{}\n", serde_json::json!({ what: true })).as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let reply: serde_json::Value = serde_json::from_str(&read_line(&mut stream).await?)
        .map_err(|e| format!("the node answered something else: {e}"))?;
    match reply["error"].as_str() {
        Some(e) => Err(e.to_string()),
        None => Ok(reply["said"].as_str().unwrap_or("done").to_string()),
    }
}

/// Read the token of the hub at `url` again, as `ensemble node login` kept
/// it, into `token`, which every link to the hub follows; tell the open
/// sessions their tools may have changed. What it did.
fn read_token_again(
    url: &str,
    token: &Token,
    tools_changed: &tokio::sync::watch::Sender<u64>,
) -> Result<String, String> {
    let kept = ensemble::link::Tokens::load()?
        .get(url)
        .map(String::from)
        .ok_or_else(|| format!("no token is kept for {url}; `ensemble node login {url}`"))?;
    if !token.set(&kept) {
        return Ok(format!("the node uses this token for {url} already"));
    }
    tools_changed.send_modify(|n| *n += 1);
    Ok(format!(
        "signed in to {url} again: the node uses the new token"
    ))
}
