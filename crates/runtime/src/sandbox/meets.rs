//! How a new sandbox meets the gateway, as its provider's [`Meeting`](provider::Meeting) says.
//!
//! A sandbox's create is given what each of its containers meets the gateway by, and leaves a
//! registration for each ([`Registrations`]). The start then meets every container at once
//! ([`Sandboxes::meet`]). `main` meets on the thread that starts the sandbox. Each companion meets
//! on a thread of its own, which ends before the start returns.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;
use std::{io, iter, panic, thread};

use gateway::{Arrival, Dialer, Listener, Session, Sockets};
use provider::{
    Admits, CompanionDial, Create, Created, Creator, ImageRef, ListenDials, Listened, Listens,
    Provider, SandboxId,
};
use schema::ir::{Companion, CompanionName, Container, Image, Name};
use schema::sandbox::{OpPayloadLimit, PrivateDial, SocketDial};
use schema::workflow::{ByProvider, Placed, RunnerId};
use stdx::{Context, Deadline};

use super::{Hosting, Looks, Sandboxes, Which};
use crate::{Clock, Host, Stopped, interruption, log};

mod gathered;

use gathered::{Failure, gathered};

/// How a provider's sandboxes and the gateway meet ([`Meeting`](provider::Meeting)), with what the
/// run supplies for it.
#[derive(Clone, Copy)]
pub(super) enum Meets<'p> {
    Sockets(&'p dyn Creator<SocketDial>),
    Private(
        &'p dyn Creator<PrivateDial, (Created, Admits)>,
        &'p Listener,
    ),
    Dials(&'p dyn Listens, &'p Dialer),
}

impl Meets<'_> {
    /// Issues what the new sandbox dials, creates it with `main` alone, and returns the
    /// registration of its session, which awaits it by `by`.
    pub(super) fn create(
        self,
        sockets: &Sockets,
        create: &Create,
        by: Deadline,
    ) -> Result<(Created, gateway::Registration), String> {
        let issued = |error: io::Error| error.to_string();
        match self {
            Self::Sockets(creator) => {
                let (dial, registration) = sockets.issue(&create.name).map_err(issued)?;
                Ok((creator.create(create, dial)?, registration))
            }
            Self::Private(creator, listener) => {
                let (dial, unattested) = listener.issue(by);
                let (created, admits) = creator.create(create, dial)?;
                Ok((created, unattested.admitting(admits)))
            }
            Self::Dials(listens, dialer) => {
                let (main, unreached) = dialer.issue().map_err(issued)?;
                let dials = ListenDials {
                    main,
                    companions: Vec::new(),
                };
                // A create that is given no companions is to give no relay, and none is dialed.
                let Listened {
                    created,
                    main,
                    relay: _,
                } = listens.create(create, dials)?;
                Ok((created, unreached.at(main)))
            }
        }
    }
}

/// A runner's companions on the provider whose sandboxes hold them, which listen. Each is as the
/// runner declares it, with its image as that provider published it.
pub(super) struct WithCompanions<'p> {
    listens: &'p dyn Listens,
    dialer: &'p Dialer,
    companions: Vec<(&'p Companion, ImageRef)>,
}

/// The registrations of a new sandbox's sessions, one for each of its containers.
pub(super) struct Registrations {
    pub(super) main: gateway::Registration,
    /// Each companion's, in the order that its runner declares them.
    pub(super) companions: Vec<(CompanionName, gateway::Registration)>,
}

impl WithCompanions<'_> {
    /// Issues what each container of the new sandbox is given, and creates it with the
    /// companions. Returns the registrations of its sessions, or why it can't be met, though it
    /// was created.
    pub(super) fn create(
        &self,
        create: &Create,
    ) -> Result<(Created, Result<Registrations, String>), String> {
        let Self {
            listens,
            dialer,
            companions,
        } = self;
        let issued = |error: io::Error| error.to_string();
        let (main, unreached) = dialer.issue().map_err(issued)?;
        let (mut dials, mut unrelayed) = (Vec::new(), Vec::new());
        for (companion, image) in companions {
            let Companion {
                name,
                image: _,
                user,
                cwd: _,
                env: _,
            } = *companion;
            let (relayed, unreached) = dialer.relayed(name).map_err(issued)?;
            dials.push(CompanionDial {
                name: name.clone(),
                image: image.clone(),
                user: *user,
                relayed,
            });
            unrelayed.push((name.clone(), unreached));
        }
        let dials = ListenDials {
            main,
            companions: dials,
        };
        let Listened {
            created,
            main,
            relay,
        } = listens.create(create, dials)?;
        let registrations = match relay {
            // Each companion listens behind the relay, and `main`'s endpoint leads to `main` alone.
            Some(relay) => Ok(Registrations {
                main: unreached.at(main),
                companions: (unrelayed.into_iter())
                    .map(|(name, unreached)| (name, unreached.at(relay.clone())))
                    .collect(),
            }),
            None => Err(format!(
                "its create gave no relay, though it was given {} companions: expected the \
                 endpoint where its relay listens",
                companions.len()
            )),
        };
        Ok((created, registrations))
    }
}

/// Publishes the images of each leased runner's companions, a runner's in one batch, on the
/// provider that the runner is placed on. Returns each runner that declares companions, with them.
///
/// Fails for a runner that declares companions where the provider's sandboxes don't listen, as
/// only a sandbox that listens holds any. Stops as interrupted before each batch once a signal has
/// come.
pub(super) fn publish_companions<'p>(
    placed: &'p Placed,
    providers: &ByProvider<Hosting<'p>>,
) -> Result<BTreeMap<RunnerId, WithCompanions<'p>>, Stopped> {
    let workflow = placed.workflow();
    let leased: BTreeSet<RunnerId> = (workflow.sessions())
        .map(|(_, session)| session.runner)
        .collect();
    let mut published = BTreeMap::new();
    for runner in leased {
        let declared = &workflow.runner(runner).companions;
        if declared.is_empty() {
            continue;
        }
        let role = &workflow.sandbox(runner.role()).name;
        let on = &placed.placement(runner.role()).provider;
        let hosting = providers.get(on);
        let (listens, dialer) = match hosting.meets {
            Meets::Dials(listens, dialer) => (listens, dialer),
            Meets::Sockets(_) | Meets::Private(..) => {
                let refused = format!(
                    "role `{role}` declares companions, and the sandboxes of provider `{on}` \
                     hold none, as they don't listen: declare none, or run it on a provider \
                     whose sandboxes listen"
                );
                return Err(refused.into());
            }
        };
        interruption::check()?;
        let images: Vec<Image> = (declared.iter())
            .map(|companion| companion.image.clone())
            .collect();
        let references = listens.publish_companion_images(&images, &hosting.sandboxd);
        let references = references.context(format_args!(
            "publishing the images of role `{role}`'s companions on provider `{on}` failed"
        ))?;
        if references.len() != declared.len() {
            let miscounted = format!(
                "provider `{on}` published {} images for role `{role}`'s {} companions: expected \
                 one for each",
                references.len(),
                declared.len()
            );
            return Err(miscounted.into());
        }
        let with = WithCompanions {
            listens,
            dialer,
            companions: declared.iter().zip(references).collect(),
        };
        published.insert(runner, with);
    }
    Ok(published)
}

/// The sessions of a sandbox's containers: `main`'s, and that of each companion that its role
/// declares, by the companion's name.
pub(crate) struct Sessions<C> {
    pub(crate) main: Session<C>,
    companions: BTreeMap<CompanionName, Session<C>>,
}

impl<C> Sessions<C> {
    /// The session of the companion `name`, if the sandbox's role declares it.
    pub(crate) fn companion(&mut self, name: &CompanionName) -> Option<&mut Session<C>> {
        self.companions.get_mut(name)
    }
}

/// How the meeting of one container went.
struct Opened<C> {
    /// When it met the gateway, if it did.
    met: Option<Duration>,
    /// Its session, set up, or why it has none.
    session: Result<Session<C>, Failure>,
}

/// The meeting of a companion: its thread, or how it went, once the thread has ended.
type Meeting<'scope, C> = Result<thread::ScopedJoinHandle<'scope, Opened<C>>, Opened<C>>;

// How long a start sleeps between its looks at a sandbox whose companions still wait, as the
// gateway's accept sleeps between its own.
const WATCH: Duration = Duration::from_millis(10);

/// What a container's session opens with. That is the limit on an op's payload, if it takes ops,
/// whether it takes model requests, and where its arrivals go, as what.
type Opens<T, W> = (Option<OpPayloadLimit>, bool, Sender<T>, W);

impl<H: Host> Sandboxes<'_, H> {
    /// Meets each container of sandbox `id` of `provider` at its registration, and sets its
    /// session up, all by `by`, a time on `clock`. `main`'s session takes ops as `ops` says, and
    /// model requests if `model`. A companion's takes neither. `arrivals` and `wrap` are as
    /// [`start`](Self::start)'s.
    ///
    /// The containers meet at once, each companion on a thread that has ended when this returns.
    /// The first failure stops the others' waits, and is the one returned, by its container's
    /// name where the sandbox holds companions. Then every session that opened has been dropped.
    /// The sandbox's end, and that of a session that opened, are such failures until all have met.
    pub(super) fn meet<T: Send + 'static>(
        &self,
        (which, id, provider): (Which, &SandboxId, &dyn Provider),
        registrations: Registrations,
        (clock, by): (&H::Clock, Duration),
        (ops, model): (Option<OpPayloadLimit>, bool),
        arrivals: Sender<T>,
        wrap: impl Fn(Container, Arrival) -> T + Clone + Send + 'static,
    ) -> Result<Sessions<H::Clock>, String> {
        let Registrations { main, companions } = registrations;
        let (sandbox, alone) = (which.name(), companions.is_empty());
        // Shared by the containers' threads. The first whose meeting fails sets it. Each one's
        // wait for its container reads it, and ends once it is set. So the first failure ends the
        // rest and is the one returned, as the first error of an errgroup's goroutines is:
        // https://github.com/golang/sync/blob/7fc1605a5dde7535a0fc1770ca44238629ff29ac/errgroup/errgroup.go#L64-L83
        let stop = &AtomicBool::new(false);
        // One thread for each companion, so at most as many as the runner's class holds
        // (`Companions::Reserved::most`), which placing checked.
        let (main, companions, ended) = thread::scope(|scope| {
            let meeting: Vec<_> = (companions.into_iter())
                .map(|(name, registration)| {
                    let container = Container::Companion(name.clone());
                    // A companion's session is passive, and serves no model.
                    let opens = (None, false, arrivals.clone(), of(wrap.clone(), container));
                    let clock = clock.clone();
                    let meet = move || {
                        let stopped = || stopped(stop);
                        let accepted = accepted(registration, (&clock, by), opens, stopped);
                        set_up(accepted.map_err(|error| error.to_string()), by, stop)
                    };
                    let thread = thread::Builder::new().name(format!("{sandbox}-{name}"));
                    let spawned = thread.spawn_scoped(scope, meet).map_err(|error| {
                        let unmet = format!("the thread that meets it didn't start: {error}");
                        set_up(Err(unmet), by, stop)
                    });
                    (name, spawned)
                })
                .collect();
            let opens = (ops, model, arrivals, of(wrap, Container::Main));
            let (looks, sandbox) = (Looks::new(clock), (&sandbox, id, provider, alone));
            let main = self.meet_main(sandbox, main, (clock, by), opens, (&looks, stop));
            self.watch((&looks, provider, id), main, meeting, stop)
        });
        if !alone {
            let companions = (companions.iter())
                .map(|(name, opened)| (Container::Companion(name.clone()), opened.met));
            let met = iter::once((Container::Main, main.met)).chain(companions);
            self.send(met_entries(&sandbox, met.collect()));
        }
        // The sandbox's own end fails the start, unless a container's failure came first.
        if let Some(Failure {
            reason,
            first: true,
        }) = ended
        {
            return Err(reason);
        }
        let sessions = gathered(main, companions).map_err(|(container, failure)| {
            if alone {
                failure.reason
            } else {
                format!("its container `{container}`: {}", failure.reason)
            }
        })?;
        match sessions.main.gpus() {
            Some(gpus) => self.saw(which, id, gpus).map(|()| sessions),
            None => Ok(sessions),
        }
    }

    /// Meets `main` of `sandbox`, the sandbox `id` of `provider`, at `registration`, and sets its
    /// session up, as [`meet`](Self::meet) says. Tells the log as `main` meets, if the sandbox
    /// holds it `alone`.
    fn meet_main<T: Send + 'static>(
        &self,
        (sandbox, id, provider, alone): (&Name, &SandboxId, &dyn Provider, bool),
        registration: gateway::Registration,
        (clock, by): (&H::Clock, Duration),
        opens: Opens<T, impl Fn(Arrival) -> T + Send + 'static>,
        (looks, stop): (&Looks<'_, H::Clock>, &AtomicBool),
    ) -> Opened<H::Clock> {
        // The wait ends at once for a signal and for a companion's failure. It ends too for a
        // sandbox that its provider lists as ended, which will never connect.
        let ended =
            || stopped(stop) || looks.ended(self.between_looks, || self.ended(provider, id));
        let accepted = accepted(registration, (clock, by), opens, ended);
        // A sandbox that holds `main` alone has met once `main` has.
        if alone && let Ok((_, at)) = &accepted {
            let (sandbox, at) = (sandbox.clone(), *at);
            self.send([log::Entry::Met { sandbox, at }]);
        }
        let accepted = accepted.map_err(|error| match looks.found() {
            (true, _) => "it ended before it met the gateway: expected its `sandboxd` to run \
                          until it connects"
                .to_owned(),
            (false, Some(failed)) if error.kind() == io::ErrorKind::TimedOut => {
                format!("{error}; the last list of its provider failed: {failed}")
            }
            (false, _) => error.to_string(),
        });
        set_up(accepted, by, stop)
    }

    /// Waits for each of `meeting`, the meetings of a sandbox's companions, to end, and returns
    /// how each went, after `main`'s. While one still waits, it watches sandbox `id` of `provider`
    /// and each session that opened. A session that ended fails its container's meeting. A
    /// sandbox that its provider lists as ended fails the start, as the failure returned last.
    /// Either sets `stop`.
    fn watch<'scope>(
        &self,
        (looks, provider, id): (&Looks<'_, H::Clock>, &dyn Provider, &SandboxId),
        mut main: Opened<H::Clock>,
        mut meeting: Vec<(CompanionName, Meeting<'scope, H::Clock>)>,
        stop: &AtomicBool,
    ) -> (Opened<H::Clock>, Companions<H::Clock>, Option<Failure>) {
        let mut ended = None;
        loop {
            let settled = meeting.into_iter().map(|(name, meeting)| match meeting {
                Ok(thread) if thread.is_finished() => (name, Err(joined(thread))),
                meeting => (name, meeting),
            });
            meeting = settled.collect();
            // A failure, or a signal, ends each wait by itself.
            if meeting.iter().all(|(_, meeting)| meeting.is_err()) || stopped(stop) {
                break;
            }
            if looks.ended(self.between_looks, || self.ended(provider, id)) {
                let reason = "it ended before its companions met the gateway: expected it to \
                              run until each of them connects";
                ended = Some(failed(reason.to_owned(), stop));
                break;
            }
            let opened = meeting
                .iter_mut()
                .filter_map(|(_, meeting)| meeting.as_mut().err());
            for opened in iter::once(&mut main).chain(opened) {
                let ended = opened.session.as_ref().ok().and_then(Session::ended);
                if let Some(reason) = ended.map(|reason| format!("its session ended: {reason}")) {
                    opened.session = Err(failed(reason, stop));
                }
            }
            thread::sleep(WATCH);
        }
        // A thread that still runs ends once its wait has read `stop`, or its set-up is answered.
        let joined = |meeting: Meeting<'scope, _>| meeting.map_or_else(|opened| opened, joined);
        let companions = meeting
            .into_iter()
            .map(|(name, meeting)| (name, joined(meeting)));
        (main, companions.collect(), ended)
    }
}

/// How the meetings of a sandbox's companions went, in the order declared.
type Companions<C> = Vec<(CompanionName, Opened<C>)>;

/// How the meeting that `thread` ran went, once the thread has ended.
fn joined<C>(thread: thread::ScopedJoinHandle<'_, Opened<C>>) -> Opened<C> {
    let opened = thread.join();
    opened.unwrap_or_else(|panic| panic::resume_unwind(panic))
}

/// A failure for `reason`, which sets `stop`, and is the first if nothing had set it.
fn failed(reason: String, stop: &AtomicBool) -> Failure {
    let first = !stop.swap(true, Ordering::Relaxed);
    Failure { reason, first }
}

/// Whether a container's wait to meet the gateway ends now. It ends for a signal, and once the
/// meeting of another container of its sandbox has failed, which set `stop`.
fn stopped(stop: &AtomicBool) -> bool {
    interruption::signal().is_some() || stop.load(Ordering::Relaxed)
}

/// `wrap`, for the arrivals on the session of `container`.
fn of<T>(
    wrap: impl Fn(Container, Arrival) -> T + Send + 'static,
    container: Container,
) -> impl Fn(Arrival) -> T + Send + 'static {
    move |arrival| wrap(container.clone(), arrival)
}

/// Waits for a container to meet the gateway at `registration` by `by`, a time on `clock`, and
/// returns its session with when it met. The session takes ops and model requests as `opens`
/// says, and hands its arrivals over through `opens`' sender and wrap.
fn accepted<C: Clock, T: Send + 'static>(
    registration: gateway::Registration,
    (clock, by): (&C, Duration),
    (ops, model, arrivals, wrap): (
        Option<OpPayloadLimit>,
        bool,
        Sender<T>,
        impl Fn(Arrival) -> T + Send + 'static,
    ),
    stopped: impl Fn() -> bool,
) -> io::Result<(Session<C>, Duration)> {
    let session = registration.accept((clock, by), ops, model, arrivals, wrap, stopped)?;
    Ok((session, clock.now()))
}

/// Sets up, by `by`, the session of a container that was accepted at a time, or keeps why it
/// wasn't. A failure sets `stop`, and is the first if nothing had set it.
fn set_up<C: Clock>(
    accepted: Result<(Session<C>, Duration), String>,
    by: Duration,
    stop: &AtomicBool,
) -> Opened<C> {
    let met = accepted.as_ref().ok().map(|(_, at)| *at);
    let session = accepted.and_then(|(mut session, _)| session.set_up(by).map(|()| session));
    let session = session.map_err(|reason| failed(reason, stop));
    Opened { met, session }
}

/// What the log is told of the meetings of `sandbox`'s containers, each with when it met, if it
/// did, in the order declared. That is a `container_met` for each that met, in the order of their
/// times, and of their declaration where two met at one time. The sandbox's `sandbox_met`
/// follows, at the last of those times, if every container met.
fn met_entries(sandbox: &Name, containers: Vec<(Container, Option<Duration>)>) -> Vec<log::Entry> {
    let count = containers.len();
    let mut met: Vec<(Container, Duration)> = (containers.into_iter())
        .filter_map(|(container, at)| Some((container, at?)))
        .collect();
    // Stable, so that containers that met at one time keep their declared order.
    met.sort_by_key(|(_, at)| *at);
    let last = met.last().filter(|_| met.len() == count).map(|(_, at)| *at);
    let each = met
        .into_iter()
        .map(|(container, at)| log::Entry::ContainerMet {
            sandbox: sandbox.clone(),
            container,
            at,
        });
    let sandbox_met = last.map(|at| log::Entry::Met {
        sandbox: sandbox.clone(),
        at,
    });
    each.chain(sandbox_met).collect()
}
