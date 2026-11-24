//! `ensemble-runner serve`: the runner, the one long-running process that
//! holds the helper teams on this computer.
//!
//! It works in its workspace, the folder its `runner.yaml` names: each git
//! repository directly inside it is a project, named for its folder, and gets
//! a worker that runs the tasks the hub keeps for that project ([`Queue`]).
//! Like a member of a team handed work, it also looks on the hub for the
//! account's tasks for its agents in projects it has no folder for: it clones
//! the `repository` such a task names into the workspace, asks on the task
//! where the project is when it names none, and gives the task back, saying
//! why, when this computer cannot clone it. The workspace and the hub are
//! looked at again every [`RESCAN`], so a repository added or cloned later
//! gets a worker too. With a hub, the runner also keeps one link that says it
//! is connected, with its agents and what it offers (`worker`). Its socket
//! takes the operations `ensemble-runner …` asks of it, and a reload of its
//! teams.
//!
//! What it keeps for a repository lives as long as the runner, so it is
//! leaked on purpose.

use crate::cmd::runner::RunnerContext;
use ensemble_agents::router::Ensemble;
use ensemble_config::{Config, RelayAt, Runner};
use ensemble_link::Link;
use ensemble_link::Token;
use ensemble_ops::local::{self, STOP, read_line};
use ensemble_ops::{Error, Registry};
use ensemble_tasks::board::{Board, Claim, End, LEASE, RemoteBoard, Waiting};
use ensemble_tasks::queue::Queue;
use ensemble_tasks::runs::Runs;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// How long stopping waits for the workers' running tasks to be put back.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// How often the workspace is looked at for repositories added since, and
/// the hub for tasks in projects the runner has no folder for.
const RESCAN: Duration = Duration::from_secs(30);
/// Seconds that stand in for [`RESCAN`], when set: for tests, which cannot
/// wait half a minute.
const RESCAN_VARIABLE: &str = "ENSEMBLE_RUNNER_RESCAN_SECONDS";
/// The most a clone of a task's repository may take.
const CLONE_LIMIT: Duration = Duration::from_secs(600);

/// What a runner keeps for one repository of its workspace.
struct Repository {
    ensemble: &'static Ensemble,
    queue: &'static Queue<'static>,
}

/// The runner: its teams, and the repositories it works in.
pub struct Daemon<'a> {
    runner: Runner,
    /// The hub server it names, and its token, replaced when you sign in again.
    hub: Option<(String, Token)>,
    /// Its teams, as one helper team: `<team>/<agent>`. Replaced when they are
    /// reloaded.
    team: RwLock<Config>,
    /// Where it serves the backends it reaches through the hub, when it
    /// does.
    relay: Option<RelayAt>,
    /// The runner's operations, and what they are given here.
    operations: (&'a Registry<RunnerContext>, RunnerContext),
    /// The repositories with a worker, by their folder.
    open: Mutex<HashMap<PathBuf, Repository>>,
    /// The repositories' workers, which stopping waits for.
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Its link to the hub for the tasks it has no folder for.
    hub_link: Option<Link>,
    /// Whether the hub could not be asked at the last look, so that is said
    /// once, not at every look.
    hub_down: AtomicBool,
    say: &'a (dyn Fn(&str) + Sync),
}

impl<'a> Daemon<'a> {
    /// A runner from `runner`, its teams loaded as `team`.
    pub fn new(
        runner: Runner,
        hub: Option<(String, Token)>,
        team: Config,
        relay: Option<RelayAt>,
        operations: &'a Registry<RunnerContext>,
        say: &'a (dyn Fn(&str) + Sync),
    ) -> Result<Self, String> {
        let operations = {
            let hub = hub.clone();
            let relink = move || match &hub {
                Some((url, token)) => read_token_again(url, token),
                None => Err("the runner names no hub".to_string()),
            };
            (operations, RunnerContext::running(relink)?)
        };
        Ok(Daemon {
            hub_link: hub
                .as_ref()
                .map(|(url, token)| Link::with_token(url, token.clone())),
            hub_down: AtomicBool::new(false),
            runner,
            hub,
            team: RwLock::new(team),
            relay,
            operations,
            open: Mutex::new(HashMap::new()),
            workers: Mutex::new(Vec::new()),
            say,
        })
    }

    /// Run the workers of the workspace's repositories, and take what is asked
    /// on `socket`, until [`STOP`] is set. A reload of the teams, asked on the
    /// socket, is also given to `reloaded`, for what else holds them.
    pub async fn serve(
        &self,
        socket: &Path,
        reloaded: &(dyn Fn(&Config) -> Result<(), String> + Sync),
    ) -> Result<(), String> {
        let workspace = &self.runner.workspace;
        std::fs::create_dir_all(workspace)
            .map_err(|e| format!("cannot make the workspace {}: {e}", workspace.display()))?;
        let listener = local::listen(socket)?;
        (self.say)(&format!(
            "runner '{}' works in {}",
            self.runner.name,
            workspace.display()
        ));
        if self.hub.is_none() {
            (self.say)(
                "it names no hub, so it has no tasks to run; `ensemble-runner login <url>` names one",
            );
        }
        // What is asked on the socket is answered while the runner looks for
        // work, and while it clones.
        let answering = async {
            while !STOP.load(Ordering::Relaxed) {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    () = tokio::time::sleep(Duration::from_millis(200)) => continue,
                };
                let Ok((stream, _)) = accepted else { continue };
                if let Err(e) = self.answer(stream, reloaded).await {
                    (self.say)(&format!("a request was refused: {e}"));
                }
            }
            // Gone at once, so a command asks no runner that stops.
            drop(listener);
            let _ = std::fs::remove_file(socket);
        };
        let looking = async {
            let every = rescan_every();
            while !STOP.load(Ordering::Relaxed) {
                self.find_projects().await;
                self.scan();
                tokio::select! {
                    () = tokio::time::sleep(every) => {}
                    () = ensemble_core::runtime::until_set(&STOP) => {}
                }
            }
        };
        tokio::join!(answering, looking);
        (self.say)("stopping: running tasks are stopped and queued again");
        for repository in self.open.lock().unwrap().values() {
            repository.queue.close();
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

    /// Answer the one line asked on `stream`: `{"op": …, "input": …}` runs
    /// one of the runner's operations (`local`), and `{"reload": true}`
    /// reloads its teams. Each answers what it did.
    async fn answer(
        &self,
        mut stream: UnixStream,
        reloaded: &(dyn Fn(&Config) -> Result<(), String> + Sync),
    ) -> Result<(), String> {
        let line = read_line(&mut stream).await?;
        let asked = serde_json::from_str::<serde_json::Value>(&line).unwrap_or_default();
        let reply = if asked.get("op").is_some() {
            let (registry, context) = &self.operations;
            local::answer(registry, context, &asked).unwrap_or_else(
                || serde_json::json!({ "error": Error::invalid("`op` must name an operation") }),
            )
        } else if asked["reload"] == true {
            match self.reload(reloaded) {
                Ok(said) => serde_json::json!({"ok": true, "said": said}),
                Err(e) => serde_json::json!({"error": e}),
            }
        } else {
            serde_json::json!({"error": "ask for an operation (`op`) or a reload"})
        };
        stream
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())
    }

    /// Read the teams again and use them: in every repository from its next
    /// run on, and in what `reloaded` holds. A team that cannot be loaded
    /// leaves everything as it was, and says why.
    fn reload(
        &self,
        reloaded: &(dyn Fn(&Config) -> Result<(), String> + Sync),
    ) -> Result<String, String> {
        let (team, waiting) = crate::teams::load_all(&self.runner.llm, self.relay.as_ref())?;
        reloaded(&team)?;
        for repository in self.open.lock().unwrap().values() {
            repository.ensemble.set_config(team.clone());
        }
        let said = format!(
            "teams reloaded: {} agent(s){}",
            team.agents.len(),
            waiting.iter().map(|w| format!("; {w}")).collect::<String>()
        );
        *self.team.write().unwrap_or_else(|e| e.into_inner()) = team;
        (self.say)(&said);
        Ok(said)
    }

    /// Give each git repository of the workspace a worker, when it has none
    /// yet, and stop the workers of those that went away. Without a hub there
    /// are no tasks, and so no workers.
    fn scan(&self) {
        let Some((url, token)) = &self.hub else {
            return;
        };
        let found = repositories(&self.runner.workspace);
        let mut open = self.open.lock().unwrap();
        open.retain(|dir, repository| {
            let there = found.contains(dir);
            if !there {
                repository.queue.close();
                (self.say)(&format!("{} went away: its worker stops", dir.display()));
            }
            there
        });
        for dir in found {
            if open.contains_key(&dir) {
                continue;
            }
            match self.start(&dir, url, token) {
                Ok(repository) => {
                    open.insert(dir, repository);
                }
                Err(e) => (self.say)(&format!("{}: {e}", dir.display())),
            }
        }
    }

    /// Look on the hub for the tasks that wait for an agent of this runner in
    /// a project it has no folder for, and get each such project: clone the
    /// repository a task names into the workspace, where the next scan gives
    /// it a worker; ask on the task where the project is when none names one;
    /// and give the task back, saying why, when this computer cannot clone it.
    /// A project whose task waits for an answer is left until it has one.
    async fn find_projects(&self) {
        let (Some((url, token)), Some(link)) = (&self.hub, &self.hub_link) else {
            return;
        };
        let agents: Vec<String> = {
            let team = self.team.read().unwrap_or_else(|e| e.into_inner());
            team.agent_names().into_iter().map(String::from).collect()
        };
        if agents.is_empty() {
            return;
        }
        let waiting = match ensemble_tasks::board::waiting(link, &self.runner.name, &agents).await {
            Ok(found) => {
                self.hub_down.store(false, Ordering::Relaxed);
                found
            }
            Err(e) => {
                if !self.hub_down.swap(true, Ordering::Relaxed) {
                    (self.say)(&format!(
                        "cannot ask {url} for tasks in projects without a folder here: {e}; trying again"
                    ));
                }
                return;
            }
        };
        let workspace = &self.runner.workspace;
        let mut projects: BTreeMap<String, Vec<Waiting>> = BTreeMap::new();
        for w in waiting {
            let dir = workspace.join(&w.project);
            if plain_name(&w.project) && !dir.join(".git").exists() {
                projects.entry(w.project.clone()).or_default().push(w);
            }
        }
        let me = self.runner.name.as_str();
        for (project, tasks) in projects {
            // Asked already: its answer says where the project is.
            if tasks.iter().any(|w| w.task.question.is_some()) {
                continue;
            }
            let dir = workspace.join(&project);
            let board = RemoteBoard::with_token(url, token.clone(), &project);
            if dir.exists() {
                let why = format!(
                    "{} is there on {}, and is not a git repository",
                    dir.display(),
                    self.runner.name
                );
                for w in &tasks {
                    self.give_back(&board, me, w, &why).await;
                }
                continue;
            }
            let Some(repository) = tasks.iter().find_map(|w| w.task.ticket.repository.clone())
            else {
                self.ask_where(&board, me, &tasks[0]).await;
                continue;
            };
            (self.say)(&format!(
                "cloning {repository} into {} for the tasks of {project}",
                dir.display()
            ));
            if let Err(why) = clone(&repository, &dir).await {
                let why = format!("{} cannot clone {repository}: {why}", self.runner.name);
                let named = tasks
                    .iter()
                    .filter(|w| w.task.ticket.repository.as_deref() == Some(&repository));
                for w in named {
                    self.give_back(&board, me, w, &why).await;
                }
            }
        }
    }

    /// Ask on task `w`, as the runner `me`, where its project is: the runner
    /// has no copy of it, and the task names no repository. The answer
    /// becomes the task's `repository`.
    async fn ask_where(&self, board: &RemoteBoard, me: &str, w: &Waiting) {
        let (id, project) = (w.task.id(), &w.project);
        // Held only for asking: nothing of the task is done here yet.
        match board.claim(id, me, LEASE, true).await {
            Ok(Claim::Taken) => {}
            _ => return,
        }
        let question = format!(
            "The runner {me} has no copy of the project {project}: which repository should it clone? Answer with its git URL or path, such as git@github.com:you/{project}.git."
        );
        let end = End::Ask {
            question,
            reply: String::new(),
            repository: true,
        };
        match board.finish(id, me, end, None).await {
            Ok(()) => (self.say)(&format!(
                "task {id} of {project}: asked which repository to clone"
            )),
            Err(e) => (self.say)(&format!("task {id} of {project}: cannot ask: {e}")),
        }
    }

    /// Give task `w` back, as the runner `me`, saying `why`, for another
    /// runner.
    async fn give_back(&self, board: &RemoteBoard, me: &str, w: &Waiting, why: &str) {
        let (id, project) = (w.task.id(), &w.project);
        match board.give_back(id, me, why).await {
            Ok(blocked) => (self.say)(&format!(
                "gave task {id} of {project} back: {why}{}",
                if blocked {
                    "; no other runner connected that may take it is left, so it is blocked"
                } else {
                    ""
                }
            )),
            Err(e) => (self.say)(&format!("task {id} of {project}: cannot give it back: {e}")),
        }
    }

    /// A worker for the repository `dir`, which runs the tasks the hub at
    /// `url` keeps for its project, the folder's name.
    fn start(&self, dir: &Path, url: &str, token: &Token) -> Result<Repository, String> {
        let project = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or("it has no name to keep tasks under")?;
        let config = self.team.read().unwrap_or_else(|e| e.into_inner()).clone();
        let ensemble: &'static Ensemble =
            Box::leak(Box::new(Ensemble::new(config, Some(dir.to_path_buf()))));
        let state = ensemble.state_dir().map_err(|e| e.to_string())?;
        let mut runs = Runs::new(&state);
        if let Some(usage) = ensemble.usage_dir() {
            runs = runs.with_usage_dir(usage);
        }
        let board = RemoteBoard::with_token(url, token.clone(), &project);
        let queue: &'static Queue<'static> = Box::leak(Box::new(Queue::new(
            ensemble,
            runs,
            board,
            &self.runner.name,
        )));
        // One worker per repository: another process that has it runs its
        // tasks.
        match ensemble_core::lock::acquire(&state) {
            Ok(lock) => {
                let worker = tokio::spawn(async move {
                    let _lock = lock;
                    queue.work().await;
                });
                self.workers.lock().unwrap().push(worker);
                (self.say)(&format!(
                    "working on the tasks of {project} in {}",
                    dir.display()
                ));
            }
            Err(pid) => (self.say)(&format!(
                "{}: process {pid} runs its tasks; this runner leaves them to it",
                dir.display()
            )),
        }
        Ok(Repository { ensemble, queue })
    }
}

/// How often the workspace and the hub are looked at: [`RESCAN`], or the
/// seconds [`RESCAN_VARIABLE`] says.
fn rescan_every() -> Duration {
    std::env::var(RESCAN_VARIABLE)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or(RESCAN, Duration::from_secs)
}

/// Whether `project` may be a folder of the workspace: one name, not hidden.
fn plain_name(project: &str) -> bool {
    !project.is_empty() && !project.starts_with('.') && !project.contains(['/', '\\'])
}

/// Clone `repository`, a git URL or path, into `into`, asking nobody for
/// anything: with no terminal to ask on and SSH in batch mode, a repository
/// this computer has no key or password for fails at once, saying why. A
/// clone that fails, or takes longer than [`CLONE_LIMIT`], leaves nothing.
async fn clone(repository: &str, into: &Path) -> Result<(), String> {
    let mut git = tokio::process::Command::new("git");
    git.args(["clone", "--quiet", "--"])
        .arg(repository)
        .arg(into)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    // Whoever set how git reaches SSH keeps it.
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        git.env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=30",
        );
    }
    let failed = match tokio::time::timeout(CLONE_LIMIT, git.output()).await {
        Ok(Ok(out)) if out.status.success() => return Ok(()),
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stderr).trim().to_string(),
        Ok(Err(e)) => format!("cannot run git: {e}"),
        Err(_) => format!("it took more than {}s", CLONE_LIMIT.as_secs()),
    };
    let _ = std::fs::remove_dir_all(into);
    Err(failed)
}

/// The git repositories directly inside `workspace`, by their whole path:
/// the owner's projects. A hidden folder is none.
pub(crate) fn repositories(workspace: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(workspace) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| plain_name(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .filter(|dir| dir.join(".git").exists())
        .filter_map(|dir| dir.canonicalize().ok())
        .collect();
    found.sort();
    found
}

/// Ask the runner on `socket` to reload its teams, and what it said.
pub async fn reload(socket: &Path) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("no runner is running ({e})"))?;
    stream
        .write_all(format!("{}\n", serde_json::json!({ "reload": true })).as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let reply: serde_json::Value = serde_json::from_str(&read_line(&mut stream).await?)
        .map_err(|e| format!("the runner answered something else: {e}"))?;
    match reply["error"].as_str() {
        Some(e) => Err(format!("the runner kept its teams as they were: {e}")),
        None => Ok(reply["said"].as_str().unwrap_or("done").to_string()),
    }
}

/// Read the token of the hub at `url` again, as `ensemble-runner login` kept
/// it, into `token`, which every link to the hub follows. What it did.
fn read_token_again(url: &str, token: &Token) -> Result<String, String> {
    let kept = crate::kept::Tokens::load()?
        .get(url)
        .map(String::from)
        .ok_or_else(|| format!("no token is kept for {url}; `ensemble-runner login {url}`"))?;
    if !token.set(&kept) {
        return Ok(format!("the runner uses this token for {url} already"));
    }
    Ok(format!(
        "signed in to {url} again: the runner uses the new token"
    ))
}
