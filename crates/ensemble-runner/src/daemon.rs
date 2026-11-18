//! `ensemble-runner serve`: the runner, the one long-running process that
//! holds the helper teams on this computer.
//!
//! It works in its workspace, the folder its `runner.yaml` names: each git
//! repository directly inside it is a project, named for its folder, and gets
//! a worker that runs the tasks the hub keeps for that project ([`Queue`]).
//! The workspace is looked at again every [`RESCAN`], so a repository added
//! later gets a worker too. With a hub server and `offers`, the runner also
//! keeps one link for whom it works for: the agents and models it offers.
//! Its socket takes the operations `ensemble-runner …` asks of it, and a
//! reload of its teams.
//!
//! What it keeps for a repository lives as long as the runner, so it is
//! leaked on purpose.

use crate::cmd::runner::RunnerContext;
use ensemble_agents::router::Ensemble;
use ensemble_config::{Config, Runner};
use ensemble_link::Token;
use ensemble_ops::local::{self, STOP, read_line};
use ensemble_ops::{Code, Error, Registry};
use ensemble_tasks::board::RemoteBoard;
use ensemble_tasks::queue::Queue;
use ensemble_tasks::runs::Runs;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// How long stopping waits for the workers' running tasks to be put back.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// How often the workspace is looked at for repositories added since.
const RESCAN: Duration = Duration::from_secs(30);

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
    /// Where the keys its teams name are read from, as `ensemble-runner serve` was
    /// given.
    env_file: Option<PathBuf>,
    /// The runner's operations, and what they are given here; none when its
    /// `interfaces.cli` is off.
    operations: Option<(&'a Registry<RunnerContext>, RunnerContext)>,
    /// The repositories with a worker, by their folder.
    open: Mutex<HashMap<PathBuf, Repository>>,
    /// The repositories' workers, which stopping waits for.
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    say: &'a (dyn Fn(&str) + Sync),
}

impl<'a> Daemon<'a> {
    /// A runner from `runner`, its teams loaded as `team`.
    pub fn new(
        runner: Runner,
        hub: Option<(String, Token)>,
        team: Config,
        env_file: Option<PathBuf>,
        operations: &'a Registry<RunnerContext>,
        say: &'a (dyn Fn(&str) + Sync),
    ) -> Result<Self, String> {
        let operations = if runner.interfaces.cli {
            let hub = hub.clone();
            let relink = move || match &hub {
                Some((url, token)) => read_token_again(url, token),
                None => Err("the runner names no hub".to_string()),
            };
            Some((operations, RunnerContext::running(relink)?))
        } else {
            None
        };
        Ok(Daemon {
            runner,
            hub,
            team: RwLock::new(team),
            env_file,
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
        let mut scanned: Option<Instant> = None;
        while !STOP.load(Ordering::Relaxed) {
            if scanned.is_none_or(|at| at.elapsed() >= RESCAN) {
                self.scan();
                scanned = Some(Instant::now());
            }
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = tokio::time::sleep(Duration::from_millis(200)) => continue,
            };
            let Ok((stream, _)) = accepted else { continue };
            if let Err(e) = self.answer(stream, reloaded).await {
                (self.say)(&format!("a request was refused: {e}"));
            }
        }
        let _ = std::fs::remove_file(socket);
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
            match &self.operations {
                Some((registry, context)) => local::answer(registry, context, &asked)
                    .unwrap_or_else(|| serde_json::json!({ "error": Error::invalid("`op` must name an operation") })),
                None => serde_json::json!({ "error": Error::new(
                    Code::Forbidden,
                    "this runner takes no commands: its `interfaces.cli` is off",
                ) }),
            }
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
        let (team, waiting) =
            crate::teams::load_all(self.env_file.as_deref(), &ensemble_config::SystemEnv)?;
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

/// The git repositories directly inside `workspace`, by their whole path.
pub(crate) fn repositories(workspace: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(workspace) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
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
    let kept = ensemble_link::Tokens::load()?
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
