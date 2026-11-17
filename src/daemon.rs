//! `ensemble node serve`: a node, the one long-running process that holds a
//! helper team on this computer.
//!
//! It is tied to no project. Your agent CLI starts `ensemble mcp` in a folder,
//! which connects to the node's socket, says which folder it is in, and from
//! then on carries MCP both ways ([`bridge`]). The node keeps one worker per
//! folder it has served: that folder's tasks go on after the session ends,
//! and start again when the node does. With a hub server it also keeps one
//! link: the tasks the hub sends you, and, for whom it works for, the agents
//! and models it offers.
//!
//! What it keeps for a folder lives as long as the node, so it is leaked on
//! purpose: a folder served once is served until the node stops.

use crate::STOP;
use ensemble::config::{Config, Node};
use ensemble::folders::Folders;
use ensemble::knowledge::KnowledgeAccess;
use ensemble::link::RemoteMemory;
use ensemble::mcp;
use ensemble::queue::Queue;
use ensemble::router::Ensemble;
use ensemble::tasks::Store;
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
    /// The hub server it names, and its token.
    hub: Option<(String, String)>,
    /// Its teams, as one helper team: `<team>/<agent>`. Replaced when they are
    /// reloaded.
    team: RwLock<Config>,
    /// Where the keys its teams name are read from, as `node serve` was given.
    env_file: Option<PathBuf>,
    /// Counts reloads, so that open sessions tell their clients the tools
    /// changed.
    tools_changed: tokio::sync::watch::Sender<u64>,
    folders: Folders,
    open: Mutex<HashMap<PathBuf, Arc<Folder>>>,
    /// The folders' workers, which stopping waits for.
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    say: &'a (dyn Fn(&str) + Sync),
}

impl<'a> Daemon<'a> {
    /// A node from `node`, its teams loaded as `team`, keeping its state in
    /// `dir`.
    pub fn new(
        node: Node,
        hub: Option<(String, String)>,
        team: Config,
        env_file: Option<PathBuf>,
        dir: &Path,
        say: &'a (dyn Fn(&str) + Sync),
    ) -> Self {
        Daemon {
            node,
            hub,
            team: RwLock::new(team),
            env_file,
            tools_changed: tokio::sync::watch::Sender::new(0),
            folders: Folders::new(dir),
            open: Mutex::new(HashMap::new()),
            workers: Mutex::new(Vec::new()),
            say,
        }
    }

    pub fn folders(&self) -> &Folders {
        &self.folders
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
                    tokio::spawn(async move {
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
                }
                Err(e) => (self.say)(&format!("a session was refused: {e}")),
            }
        }
        let _ = std::fs::remove_file(socket);
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
    /// teams instead, and answers what it found; no session follows.
    async fn hello(
        &self,
        mut stream: UnixStream,
        reloaded: &(dyn Fn(&Config) -> Result<(), String> + Sync),
    ) -> Result<Option<(Arc<Folder>, UnixStream)>, String> {
        let line = read_line(&mut stream).await?;
        if serde_json::from_str::<serde_json::Value>(&line).is_ok_and(|h| h["reload"] == true) {
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
            &*Box::leak(Box::new(RemoteMemory::new(url, token, project)))
        });
        let memory = knowledge.map(|k| k as &dyn KnowledgeAccess);
        let state = ensemble.state_dir().map_err(|e| e.to_string())?;
        let mut store = Store::new(&state);
        if let Some(usage) = ensemble.usage_dir() {
            store = store.with_usage_dir(usage);
        }
        mcp::note_recall_without_knowledge(ensemble, memory);
        let queue: &'static Queue<'static> =
            Box::leak(Box::new(mcp::queue_for(ensemble, Some(store), memory)));
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
fn listen(socket: &Path) -> Result<UnixListener, String> {
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
async fn read_line(stream: &mut UnixStream) -> Result<String, String> {
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

/// `ensemble mcp` with no `--team`: carry MCP between stdio and the node on
/// `socket`, as a session in `dir`. Ends when the node closes the session,
/// which it does once stdin ends and its calls are answered.
pub async fn bridge(socket: &Path, dir: &Path, node: &str) -> Result<(), String> {
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
    let (mut from_node, mut to_node) = stream.into_split();
    let up = async {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut to_node).await;
        let _ = to_node.shutdown().await;
    };
    let down = async {
        let mut stdout = tokio::io::stdout();
        let copied = tokio::io::copy(&mut from_node, &mut stdout).await;
        let _ = stdout.flush().await;
        copied
    };
    // The session is over when the node closes it; stdin may never end.
    tokio::select! {
        copied = down => copied.map(|_| ()).map_err(|e| e.to_string()),
        () = async { up.await; std::future::pending::<()>().await } => Ok(()),
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
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("no node is running ({e})"))?;
    stream
        .write_all(b"{\"reload\": true}\n")
        .await
        .map_err(|e| e.to_string())?;
    let reply: serde_json::Value = serde_json::from_str(&read_line(&mut stream).await?)
        .map_err(|e| format!("the node answered something else: {e}"))?;
    match reply["error"].as_str() {
        Some(e) => Err(format!("the node kept its teams as they were: {e}")),
        None => Ok(reply["said"]
            .as_str()
            .unwrap_or("teams reloaded")
            .to_string()),
    }
}
