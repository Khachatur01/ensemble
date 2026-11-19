//! Model calls relayed through the hub (`ensemble_link::relay`): between
//! runners, and from the hub to runners. A runner's relay socket says which
//! of its backends it shares and how many calls it takes at once; a call to
//! one goes to that socket, and its answer back to whoever made it, the hub
//! renumbering each call between the two.
//!
//! Who may call a runner's backend: its owner's other runners, and the hub;
//! when it works for the hub, also the runners of every account whose role
//! has `compute`. A call waits for a free place among the runner's
//! `max_shared_calls` for up to [`BUSY_WAIT`], then is answered busy, and the
//! caller tries the next model of its list. The hub never learns a backend's
//! address or key: the runner calls its own.

use ensemble_link::relay::{BUSY_WAIT, Frame, Offer, Open, UNAVAILABLE, body_frames};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Who makes a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Caller {
    /// The hub, for its own work, such as distilling.
    Hub,
    /// A runner of `name`, an account whose role has `compute` or not.
    Account { name: String, compute: bool },
}

/// A connected relay socket, and what its runner shares.
struct Sharer {
    owner: String,
    name: String,
    for_hub: bool,
    shares: Vec<String>,
    max_calls: u32,
    /// The models each shared backend serves, as the runner said.
    models: std::collections::BTreeMap<String, Vec<String>>,
    /// A place for each call it takes at once.
    places: Arc<Semaphore>,
    /// What is sent on its socket.
    outbox: UnboundedSender<Frame>,
}

impl Sharer {
    fn id(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// May `caller` call its backends?
    fn serves(&self, caller: &Caller) -> bool {
        match caller {
            Caller::Hub => true,
            Caller::Account { name, compute } => *name == self.owner || (self.for_hub && *compute),
        }
    }
}

/// Where the answer of a call a runner serves goes.
struct Route {
    /// The socket that serves it.
    server: u64,
    answer: UnboundedSender<Frame>,
    /// The caller's number for the call.
    as_call: u64,
    /// Its place among the serving runner's calls, given back as it ends.
    _place: OwnedSemaphorePermit,
}

#[derive(Default)]
struct Inner {
    sockets: HashMap<u64, Sharer>,
    /// The calls runners serve, by the hub's number for each.
    served: HashMap<u64, Route>,
    /// Where the request of each call a socket makes goes, by the socket and
    /// its number for the call, until the request has ended.
    made: HashMap<(u64, u64), UnboundedSender<Frame>>,
}

/// The relay sockets connected to a hub, and the calls between them.
#[derive(Default)]
pub struct Calls {
    inner: Mutex<Inner>,
    next: AtomicU64,
}

/// A backend a runner offers, as the hub lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Offered {
    pub(crate) runner: String,
    pub(crate) for_hub: bool,
    pub(crate) backend: String,
    pub(crate) max_calls: u32,
    pub(crate) models: Vec<String>,
}

impl Calls {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fresh(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// A relay socket of `owner`'s that offered `offer`: its number, its
    /// runner as the hub knows it, and what is to be sent on it; or why it
    /// is refused. One that works for the hub needs `host`.
    pub(crate) fn connect(
        &self,
        owner: &str,
        offer: &Offer,
        may_host: bool,
    ) -> Result<(u64, String, UnboundedReceiver<Frame>), String> {
        let name = offer.name.trim().to_string();
        if !ensemble_config::runner::valid_name(&name) {
            return Err(format!(
                "relay: '{name}' is not a runner name: letters, digits, '-', '_' or '.'"
            ));
        }
        let for_hub = match offer.works_for.as_str() {
            "hub" => true,
            "owner" => false,
            other => {
                return Err(format!(
                    "relay: `works_for` must be owner or hub, not '{other}'"
                ));
            }
        };
        if for_hub && !may_host {
            return Err(
                "relay: a runner that works for the hub needs the `host` permission in your role; say `works_for: owner`, or ask an admin"
                    .into(),
            );
        }
        if let Some(bad) = offer
            .shares
            .iter()
            .find(|b| !ensemble_config::runner::valid_name(b))
        {
            return Err(format!("relay: '{bad}' is not a backend's name"));
        }
        let max_calls = offer.max_shared_calls.max(1);
        let (outbox, frames) = unbounded_channel();
        let sharer = Sharer {
            owner: owner.to_string(),
            name,
            for_hub,
            shares: offer.shares.clone(),
            max_calls,
            models: offer.models.clone(),
            places: Arc::new(Semaphore::new(max_calls as usize)),
            outbox,
        };
        let id = sharer.id();
        let mut inner = self.lock();
        if inner.sockets.values().any(|s| s.id() == id) {
            return Err(format!(
                "relay: {id} is connected already, from another process; give this one another name"
            ));
        }
        let socket = self.fresh();
        inner.sockets.insert(socket, sharer);
        Ok((socket, id, frames))
    }

    /// Socket `socket` is gone: the calls it served are answered so, and
    /// those it made are given up.
    pub(crate) fn disconnect(&self, socket: u64) {
        let mut inner = self.lock();
        let Some(gone) = inner.sockets.remove(&socket) else {
            return;
        };
        // Calls waiting for a place there wait no longer.
        gone.places.close();
        let ended: Vec<u64> = inner
            .served
            .iter()
            .filter(|(_, r)| r.server == socket)
            .map(|(id, _)| *id)
            .collect();
        for route in ended.iter().filter_map(|id| inner.served.remove(id)) {
            let _ = route.answer.send(Frame::error(
                route.as_call,
                UNAVAILABLE,
                format!("runner {} went away before it answered", gone.id()),
            ));
        }
        inner.made.retain(|(s, _), _| *s != socket);
    }

    /// Every backend the connected runners share, by runner.
    pub(crate) fn offered(&self) -> Vec<Offered> {
        let inner = self.lock();
        let mut out: Vec<Offered> = inner
            .sockets
            .values()
            .flat_map(|s| {
                s.shares.iter().map(|b| Offered {
                    runner: s.id(),
                    for_hub: s.for_hub,
                    backend: b.clone(),
                    max_calls: s.max_calls,
                    models: s.models.get(b).cloned().unwrap_or_default(),
                })
            })
            .collect();
        out.sort_by(|a, b| (&a.runner, &a.backend).cmp(&(&b.runner, &b.backend)));
        out
    }

    /// A frame that came on socket `socket`, whose runner is `caller`'s: the
    /// start or more of a call it makes, or the answer to one it serves.
    pub(crate) fn frame(self: &Arc<Self>, socket: u64, caller: &Caller, frame: Frame) {
        if frame.to_server() {
            let call = frame.call();
            if let Frame::Open { open, .. } = frame {
                let (up, request) = unbounded_channel();
                let answer = match self.lock().sockets.get(&socket) {
                    Some(s) => s.outbox.clone(),
                    None => return,
                };
                self.lock().made.insert((socket, call), up);
                let calls = self.clone();
                let caller = caller.clone();
                tokio::spawn(async move { calls.call(caller, open, answer, call, request).await });
                return;
            }
            let mut inner = self.lock();
            let last = matches!(frame, Frame::End { .. } | Frame::Cancel { .. });
            if let Some(up) = inner.made.get(&(socket, call)) {
                let _ = up.send(frame);
            }
            if last {
                inner.made.remove(&(socket, call));
            }
            return;
        }
        let call = frame.call();
        let mut inner = self.lock();
        let Some(route) = inner.served.get(&call).filter(|r| r.server == socket) else {
            return;
        };
        let ends = frame.ends_answer();
        let gone = route.answer.send(frame.renumbered(route.as_call)).is_err();
        if gone && let Some(server) = inner.sockets.get(&socket) {
            // Whoever asked went away: the runner may stop.
            let _ = server.outbox.send(Frame::Cancel { call });
        }
        if gone || ends {
            inner.served.remove(&call);
        }
    }

    /// A call the hub makes itself: `open`, with the request's `body`. The
    /// answer's frames come on what it returns; dropping that gives it up.
    pub(crate) fn open_as_hub(
        self: &Arc<Self>,
        open: Open,
        body: Vec<u8>,
    ) -> UnboundedReceiver<Frame> {
        let (answer, answers) = unbounded_channel();
        let (up, request) = unbounded_channel();
        for frame in body_frames(0, &body) {
            let _ = up.send(frame);
        }
        let calls = self.clone();
        tokio::spawn(async move { calls.call(Caller::Hub, open, answer, 0, request).await });
        answers
    }

    /// Carry one call of `caller`'s, which numbers it `as_call`: find the
    /// runner that serves it, wait for a place there, then pass its request
    /// on as it comes. Its answer comes back through [`Calls::frame`].
    async fn call(
        self: Arc<Self>,
        caller: Caller,
        open: Open,
        answer: UnboundedSender<Frame>,
        as_call: u64,
        mut request: UnboundedReceiver<Frame>,
    ) {
        let refuse = |why: String| {
            let _ = answer.send(Frame::error(as_call, UNAVAILABLE, why));
        };
        let found = {
            let inner = self.lock();
            inner
                .sockets
                .iter()
                .find(|(_, s)| s.id() == open.runner)
                .map(|(socket, s)| {
                    (
                        *socket,
                        s.shares.contains(&open.backend),
                        s.serves(&caller),
                        s.places.clone(),
                        s.max_calls,
                    )
                })
        };
        let (server, places) = match found {
            None => return refuse(format!("runner {} is not connected", open.runner)),
            Some((_, false, ..)) => {
                return refuse(format!(
                    "runner {} shares no backend '{}'",
                    open.runner, open.backend
                ));
            }
            Some((_, _, false, ..)) => {
                return refuse(format!(
                    "runner {} does not share its backends with you: it works for its owner",
                    open.runner
                ));
            }
            Some((server, true, true, places, max)) => {
                match tokio::time::timeout(BUSY_WAIT, places.acquire_owned()).await {
                    Ok(Ok(place)) => (server, place),
                    Ok(Err(_)) => return refuse(format!("runner {} went away", open.runner)),
                    Err(_) => {
                        return refuse(format!(
                            "runner {} is busy: it takes {max} call(s) at once, and none ended within {}s",
                            open.runner,
                            BUSY_WAIT.as_secs()
                        ));
                    }
                }
            }
        };
        let id = self.fresh();
        let outbox = {
            let mut inner = self.lock();
            let Some(outbox) = inner.sockets.get(&server).map(|s| s.outbox.clone()) else {
                drop(inner);
                return refuse(format!("runner {} went away", open.runner));
            };
            inner.served.insert(
                id,
                Route {
                    server,
                    answer: answer.clone(),
                    as_call,
                    _place: places,
                },
            );
            outbox
        };
        let _ = outbox.send(Frame::Open { call: id, open });
        loop {
            let Some(frame) = request.recv().await else {
                // Whoever asked went away before its request ended.
                let _ = outbox.send(Frame::Cancel { call: id });
                self.lock().served.remove(&id);
                return;
            };
            let cancel = matches!(frame, Frame::Cancel { .. });
            let last = cancel || matches!(frame, Frame::End { .. });
            if outbox.send(frame.renumbered(id)).is_err() || cancel {
                self.lock().served.remove(&id);
                return;
            }
            if last {
                return;
            }
        }
    }
}

/// The hub's own calls to runners' backends, for its loopback endpoint
/// (`ensemble_link::loopback`), which its distiller and embedding model use.
pub struct HubOpener(pub Arc<Calls>);

impl ensemble_link::loopback::Opener for HubOpener {
    fn open(&self, open: Open, body: Vec<u8>) -> UnboundedReceiver<Frame> {
        self.0.open_as_hub(open, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn offer(name: &str, works_for: &str, shares: &[&str], max: u32) -> Offer {
        Offer {
            name: name.into(),
            works_for: works_for.into(),
            shares: shares.iter().map(|s| s.to_string()).collect(),
            max_shared_calls: max,
            models: Default::default(),
        }
    }

    fn open(runner: &str, backend: &str) -> Open {
        Open {
            runner: runner.into(),
            backend: backend.into(),
            method: "POST".into(),
            path: "/chat/completions".into(),
        }
    }

    async fn next(rx: &mut UnboundedReceiver<Frame>) -> Frame {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a frame in time")
            .expect("the channel is open")
    }

    #[tokio::test]
    async fn a_call_reaches_the_runner_that_shares_the_backend_and_its_answer_comes_back() {
        let calls = Arc::new(Calls::default());
        let (gpu, id, mut served) = calls
            .connect("ana", &offer("gpu-box", "owner", &["local"], 1), false)
            .unwrap();
        assert_eq!(id, "ana/gpu-box");
        assert!(
            calls
                .connect("ana", &offer("gpu-box", "owner", &[], 1), false)
                .unwrap_err()
                .contains("connected already")
        );
        assert!(
            calls
                .connect("bob", &offer("lab", "hub", &[], 1), false)
                .unwrap_err()
                .contains("`host`")
        );
        // ana's laptop calls her gpu box.
        let (laptop, _, mut laptop_frames) = calls
            .connect("ana", &offer("laptop", "owner", &[], 1), false)
            .unwrap();
        let ana = Caller::Account {
            name: "ana".into(),
            compute: false,
        };
        calls.frame(
            laptop,
            &ana,
            Frame::Open {
                call: 5,
                open: open("ana/gpu-box", "local"),
            },
        );
        for f in body_frames(5, b"{\"model\": \"q\"}") {
            calls.frame(laptop, &ana, f);
        }
        let Frame::Open { call, open: asked } = next(&mut served).await else {
            panic!("an open first")
        };
        assert_eq!(asked.backend, "local");
        assert_eq!(
            next(&mut served).await,
            Frame::Body {
                call,
                bytes: b"{\"model\": \"q\"}".to_vec()
            }
        );
        assert_eq!(next(&mut served).await, Frame::End { call });
        for f in [
            Frame::Head {
                call,
                status: 200,
                content_type: "application/json".into(),
            },
            Frame::Reply {
                call,
                bytes: b"{}".to_vec(),
            },
            Frame::Done { call },
        ] {
            calls.frame(gpu, &ana, f);
        }
        assert!(matches!(
            next(&mut laptop_frames).await,
            Frame::Head {
                call: 5,
                status: 200,
                ..
            }
        ));
        assert_eq!(
            next(&mut laptop_frames).await,
            Frame::Reply {
                call: 5,
                bytes: b"{}".to_vec()
            }
        );
        assert_eq!(next(&mut laptop_frames).await, Frame::Done { call: 5 });
        assert!(calls.lock().served.is_empty(), "its place is given back");

        // bob may not call a runner that works for its owner, compute or not.
        let bob = Caller::Account {
            name: "bob".into(),
            compute: true,
        };
        let (bobs, _, mut bobs_frames) = calls
            .connect("bob", &offer("desk", "owner", &[], 1), false)
            .unwrap();
        calls.frame(
            bobs,
            &bob,
            Frame::Open {
                call: 1,
                open: open("ana/gpu-box", "local"),
            },
        );
        let Frame::Error {
            status, message, ..
        } = next(&mut bobs_frames).await
        else {
            panic!("refused")
        };
        assert_eq!(status, UNAVAILABLE);
        assert!(message.contains("works for its owner"), "{message}");
        // The hub may; a backend it does not share, or a runner not there, is
        // unavailable.
        let mut answer = calls.open_as_hub(open("ana/gpu-box", "big"), vec![]);
        assert!(
            matches!(next(&mut answer).await, Frame::Error { message, .. } if message.contains("shares no backend 'big'"))
        );
        let mut answer = calls.open_as_hub(open("ana/elsewhere", "local"), vec![]);
        assert!(
            matches!(next(&mut answer).await, Frame::Error { message, .. } if message.contains("not connected"))
        );
        let hubs = calls.open_as_hub(open("ana/gpu-box", "local"), vec![]);
        assert!(matches!(next(&mut served).await, Frame::Open { .. }));
        assert_eq!(calls.offered()[0].runner, "ana/gpu-box");

        // The runner goes away while it serves, and while another call
        // waits for its one place: both callers hear so at once.
        let mut waiting = calls.open_as_hub(open("ana/gpu-box", "local"), vec![]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        calls.disconnect(gpu);
        let Frame::Error { message, .. } = next(&mut waiting).await else {
            panic!("told")
        };
        assert!(message.contains("went away"), "{message}");
        let Frame::Error { message, .. } = next(&mut { hubs }).await else {
            panic!("told")
        };
        assert!(
            message.contains("went away before it answered"),
            "{message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_to_a_runner_with_no_free_place_waits_then_is_answered_busy() {
        let calls = Arc::new(Calls::default());
        let (_gpu, _, mut served) = calls
            .connect("ana", &offer("gpu-box", "owner", &["local"], 1), false)
            .unwrap();
        let _first = calls.open_as_hub(open("ana/gpu-box", "local"), vec![]);
        assert!(matches!(next(&mut served).await, Frame::Open { .. }));
        let mut second = calls.open_as_hub(open("ana/gpu-box", "local"), vec![]);
        // Paused, the clock moves on to whatever is waited for first.
        let busy = tokio::time::timeout(BUSY_WAIT * 2, second.recv()).await;
        let Ok(Some(Frame::Error {
            status, message, ..
        })) = busy
        else {
            panic!("busy")
        };
        assert_eq!(status, UNAVAILABLE);
        assert!(message.contains("busy"), "{message}");
    }
}
