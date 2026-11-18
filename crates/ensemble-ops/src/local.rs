//! Operations run by the process whose state they change. A running hub or
//! runner takes them on its local socket, which only its OS user may open: one
//! line asks `{"op": name, "input": {…}}`, and one answers `{"ok": output}` or
//! `{"error": error}`, as the one who runs it asked (`Caller::Local`). When it
//! is not running, the command line runs the operation itself, on the files
//! ([`Route`]).
//!
//! The running process stops on SIGINT or SIGTERM ([`STOP`]), and an operation
//! that changes what it cannot use as it runs has it start again in the same
//! process ([`restart`]).

use crate::{Caller, Code, Error, Registry};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// The longest line a socket takes.
const MAX_LINE: usize = 64 * 1024;

/// Set by SIGINT and SIGTERM, so that a running hub or runner stops cleanly.
pub static STOP: AtomicBool = AtomicBool::new(false);

/// Set with [`STOP`] when the running hub or runner stops to start again in the
/// same process, with its settings as they are now.
static RESTART: AtomicBool = AtomicBool::new(false);

/// Have SIGINT and SIGTERM set [`STOP`].
pub fn stop_on_signals() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Have the running hub or runner stop and start again, in this process.
pub fn restart() {
    RESTART.store(true, Ordering::Relaxed);
    STOP.store(true, Ordering::Relaxed);
}

/// Whether it stopped to start again; if so, it may run again.
pub fn restarting() -> bool {
    let again = RESTART.swap(false, Ordering::Relaxed);
    if again {
        STOP.store(false, Ordering::Relaxed);
    }
    again
}

/// Listen on `socket`, in a folder only this user may enter. A socket left by
/// a process that died is replaced; one a running process answers on is not.
pub fn listen(socket: &Path) -> Result<UnixListener, String> {
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
            "{} is not a folder only you may enter; remove it and start again",
            dir.display()
        ));
    }
    if socket.exists() {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return Err(format!("a process already listens on {}", socket.display()));
        }
        let _ = std::fs::remove_file(socket);
    }
    UnixListener::bind(socket).map_err(|e| format!("cannot listen on {}: {e}", socket.display()))
}

/// One line, read a byte at a time so nothing after it is taken from the
/// stream.
pub async fn read_line(stream: &mut UnixStream) -> Result<String, String> {
    let mut line = Vec::new();
    loop {
        let byte = stream.read_u8().await.map_err(|e| e.to_string())?;
        if byte == b'\n' {
            return String::from_utf8(line).map_err(|e| e.to_string());
        }
        if line.len() == MAX_LINE {
            return Err("the line is too long".into());
        }
        line.push(byte);
    }
}

/// Where an operation runs.
pub enum Route<C> {
    /// In the running process, over its socket.
    Running(PathBuf),
    /// Here, on this context: nothing runs it.
    Here(C),
}

impl<C> Route<C> {
    /// The running process listening on `socket`, else here, on what
    /// `context` makes.
    pub fn to(socket: &Path, context: impl FnOnce() -> Result<C, String>) -> Result<Self, String> {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return Ok(Route::Running(socket.to_path_buf()));
        }
        context().map(Route::Here)
    }

    /// Run `name` with `input`, from `registry`, where it runs.
    pub fn call(&self, registry: &Registry<C>, name: &str, input: Value) -> Result<Value, Error> {
        match self {
            Route::Here(context) => registry.run(context, &Caller::Local, name, input),
            Route::Running(socket) => ensemble_core::runtime::block_on(ask(socket, name, &input))
                .map_err(|e| {
                Error::new(Code::Unavailable, format!("the running process: {e}"))
            })?,
        }
    }
}

/// Ask the process on `socket` to run `name` with `input`: its answer, or why
/// it could not be asked.
async fn ask(socket: &Path, name: &str, input: &Value) -> Result<Result<Value, Error>, String> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("cannot reach it ({e})"))?;
    let asked = json!({ "op": name, "input": input });
    stream
        .write_all(format!("{asked}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let reply: Value = serde_json::from_str(&read_line(&mut stream).await?)
        .map_err(|e| format!("it answered something else: {e}"))?;
    match reply.get("error") {
        Some(e) => Ok(Err(serde_json::from_value(e.clone())
            .map_err(|e| format!("it answered an error of another shape: {e}"))?)),
        None => Ok(Ok(reply.get("ok").cloned().unwrap_or(Value::Null))),
    }
}

/// The answer to `asked`, a line that asks for an operation, run on
/// `context`; None when it asks for no operation.
pub fn answer<C>(registry: &Registry<C>, context: &C, asked: &Value) -> Option<Value> {
    let name = asked.get("op")?.as_str()?;
    let input = asked.get("input").cloned().unwrap_or(Value::Null);
    Some(
        match ensemble_core::runtime::blocking(|| {
            registry.run(context, &Caller::Local, name, input)
        }) {
            Ok(output) => json!({ "ok": output }),
            Err(e) => json!({ "error": e }),
        },
    )
}

/// Answer the one line `stream` asks with `answer`, then close it.
pub async fn reply(
    mut stream: UnixStream,
    answer: impl FnOnce(&Value) -> Value,
) -> Result<(), String> {
    let line = read_line(&mut stream).await?;
    let asked: Value = serde_json::from_str(&line).map_err(|e| format!("not JSON: {e}"))?;
    let reply = answer(&asked);
    stream
        .write_all(format!("{reply}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Access, Field, Kind, Spec, operation};

    struct Count(std::sync::Mutex<u64>);

    #[derive(serde::Deserialize)]
    struct By {
        by: u64,
    }

    const ADD: Spec = Spec {
        name: "count.add",
        summary: "Add.",
        access: Access::Local,
        input: &[Field::new("by", Kind::Number, "How much.").required()],
    };

    fn registry() -> Registry<Count> {
        Registry::new(vec![operation(ADD, |c: &Count, _, input: By| {
            if input.by == 0 {
                return Err(Error::invalid("0 adds nothing").on("by"));
            }
            let mut n = c.0.lock().unwrap();
            *n += input.by;
            Ok(json!({ "count": *n }))
        })])
    }

    #[test]
    fn an_operation_runs_in_the_running_process_else_here() {
        let dir = std::env::temp_dir().join(format!("ensemble-local-{}", std::process::id()));
        let socket = dir.join("s.sock");
        let _ = std::fs::remove_dir_all(&dir);
        let registry: &'static Registry<Count> = Box::leak(Box::new(registry()));
        let running: &'static Count = Box::leak(Box::new(Count(std::sync::Mutex::new(10))));
        // Nothing listens: it runs here.
        let route = Route::to(&socket, || Ok(Count(std::sync::Mutex::new(0)))).unwrap();
        assert_eq!(
            route
                .call(registry, "count.add", json!({ "by": 1 }))
                .unwrap()["count"],
            1
        );

        let rt = ensemble_core::runtime::shared();
        let listener = rt.block_on(async { listen(&socket) }).unwrap();
        rt.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(reply(stream, |asked| {
                    answer(registry, running, asked).unwrap()
                }));
            }
        });
        let route = Route::to(&socket, || Err::<Count, _>("not here".into())).unwrap();
        assert!(matches!(route, Route::Running(_)));
        assert_eq!(
            route
                .call(registry, "count.add", json!({ "by": 2 }))
                .unwrap()["count"],
            12
        );
        // Its errors come back as data.
        let err = route
            .call(registry, "count.add", json!({ "by": 0 }))
            .unwrap_err();
        assert_eq!(
            (err.code, err.field.as_deref()),
            (Code::Invalid, Some("by"))
        );
        assert_eq!(*running.0.lock().unwrap(), 12);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
