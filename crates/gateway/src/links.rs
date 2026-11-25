//! The links between the containers of a sandbox's pod, which the gateway carries from one's
//! session to another's ([the protocol](schema::sandbox)).
//!
//! Each container's session takes its place in its sandbox's [`Pod`] as it opens, with the links
//! that its owner lets it ask for ([`Pod::place`]), and leaves as it ends. A link that a session
//! asks for and may have, to a container whose session is in the pod, opens at both; the gateway
//! then hands each of its frames from one session to the other as it arrives, through the other's
//! queue, which a thread of that session's writes. So no session's reader waits on another's
//! sandbox, and the windows bound what the queues hold.
//!
//! A link whose other end's session ends has its way to the sandbox ended, its window given back,
//! and what it sends after granted back at once, dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{io, thread};

use schema::ir::{LinkPort, Member};
use schema::sandbox::{FromSandbox, LINK_WINDOW_BYTES, MAX_LINKS, ToSandbox};
use stdx::time::{Clock as _, Machine};

use crate::{Assembler, Bound, Due, Shared, broke, context, lock};

// How long a frame of a link waits for the sandbox to take it, past which the session ends: its
// `sandboxd` reads every frame of a link at once, so one that takes none for this long is wedged.
const FRAME_TIMEOUT: Duration = Duration::from_secs(300);

/// The pod of one sandbox's containers, whose sessions its links join.
#[derive(Clone, Debug, Default)]
pub struct Pod(Arc<Mutex<State>>);

/// A container's place in its pod: which container it is, and the links that it may ask for, each
/// to a container and port.
#[derive(Debug)]
pub struct Place {
    pod: Pod,
    member: Member,
    reaches: BTreeSet<(Member, LinkPort)>,
}

impl Pod {
    /// The place of `member`, whose session may ask for a link to each container and port of
    /// `reaches`, and is asked for any that another's may to it.
    pub fn place(&self, member: Member, reaches: BTreeSet<(Member, LinkPort)>) -> Place {
        Place {
            pod: self.clone(),
            member,
            reaches,
        }
    }
}

/// A frame to a session's sandbox, with its body.
pub(crate) type Frame = (ToSandbox, Vec<u8>);

#[derive(Debug, Default)]
struct State {
    seats: BTreeMap<Member, Seat>,
    // Each open link's end at each session, by the session's container and the link's number
    // there.
    ends: BTreeMap<(Member, u32), End>,
}

#[derive(Debug)]
struct Seat {
    frames: Sender<Frame>,
    // The number of the session's next link: the gateway uses each once.
    next: u32,
    open: usize,
}

#[derive(Debug)]
struct End {
    // The link's other end, while its session lasts.
    peer: Option<(Member, u32)>,
    // The bytes that this end's sandbox may send before more are granted back.
    window: u32,
    // The bytes sent to this end's sandbox that it hasn't granted back.
    owed: u32,
    // Whether this end's sandbox has ended its way.
    ended: bool,
}

impl End {
    fn new(peer: (Member, u32)) -> Self {
        Self {
            peer: Some(peer),
            window: LINK_WINDOW_BYTES,
            owed: 0,
            ended: false,
        }
    }
}

impl State {
    fn send(&self, member: &Member, frame: Frame) {
        // A session that has ended has left, and so has its queue.
        if let Some(seat) = self.seats.get(member) {
            let _ = seat.frames.send(frame);
        }
    }

    // Takes a link out, both its ends at once, once both have ended or one's session has left.
    fn close(&mut self, end: &(Member, u32)) {
        if self.ends.remove(end).is_some()
            && let Some(seat) = self.seats.get_mut(&end.0)
        {
            seat.open -= 1;
        }
    }
}

impl Place {
    /// Seats the session in its pod, and returns the queue of the frames that its links send it.
    pub(crate) fn join(&self) -> Receiver<Frame> {
        let (frames, queue) = mpsc::channel();
        let seat = Seat {
            frames,
            next: 1,
            open: 0,
        };
        let seated = lock(&self.pod.0).seats.insert(self.member.clone(), seat);
        if seated.is_some() {
            unreachable!(
                "`{}`'s session took its place in its pod while another's held it, though a pod is \
                 its sandbox's, whose every container opens one session",
                self.member
            );
        }
        queue
    }

    /// Takes the session out of its pod, ending each of its links' ways to the other ends.
    pub(crate) fn leave(&self) {
        let mut state = lock(&self.pod.0);
        state.seats.remove(&self.member);
        let own = (self.member.clone(), 0)..=(self.member.clone(), u32::MAX);
        let keys: Vec<(Member, u32)> = state.ends.range(own).map(|(key, _)| key.clone()).collect();
        for key in keys {
            let Some(end) = state.ends.remove(&key) else {
                continue;
            };
            let Some(peer) = end.peer else {
                continue;
            };
            let Some(other) = state.ends.get_mut(&peer) else {
                continue;
            };
            other.peer = None;
            if other.ended {
                state.close(&peer);
                continue;
            }
            // What it sent that this end won't grant back, given back here.
            let bytes = LINK_WINDOW_BYTES - other.window;
            other.window = LINK_WINDOW_BYTES;
            if bytes > 0 {
                state.send(
                    &peer.0,
                    (
                        ToSandbox::LinkCredit {
                            link: peer.1,
                            bytes,
                        },
                        Vec::new(),
                    ),
                );
            }
            if !end.ended {
                state.send(&peer.0, (ToSandbox::LinkEnd { link: peer.1 }, Vec::new()));
            }
        }
    }

    /// Opens a link to `to`'s service at `port`, if this session may ask for it and `to`'s session
    /// is in the pod, for the sandbox's link request `request`; or refuses it.
    pub(crate) fn open(&self, request: u32, to: &Member, port: LinkPort) {
        let mut state = lock(&self.pod.0);
        let refused = |message: String| (ToSandbox::LinkRefused { request, message }, Vec::new());
        let me = &self.member;
        if !self.reaches.contains(&(to.clone(), port)) {
            let refusal = format!("no link of the pod joins `{me}` to `{to}` at port {port}");
            state.send(me, refused(refusal));
            return;
        }
        let (Some(own), Some(theirs)) = (state.seats.get(me), state.seats.get(to)) else {
            let refusal = format!(
                "`{to}`'s session isn't open: expected a link to a container while its session \
                 lasts"
            );
            state.send(me, refused(refusal));
            return;
        };
        if own.open >= MAX_LINKS || theirs.open >= MAX_LINKS {
            let refusal = format!(
                "the link would take `{me}` or `{to}` past {MAX_LINKS} links open: expected fewer \
                 open at once"
            );
            state.send(me, refused(refusal));
            return;
        }
        let ours = take_number(&mut state, me);
        let link = take_number(&mut state, to);
        state
            .ends
            .insert((me.clone(), ours), End::new((to.clone(), link)));
        state
            .ends
            .insert((to.clone(), link), End::new((me.clone(), ours)));
        // The connect goes first, so the other end's queue holds it before any of the link's bytes.
        state.send(to, (ToSandbox::LinkConnect { link, port }, Vec::new()));
        state.send(
            me,
            (
                ToSandbox::LinkOpened {
                    request,
                    link: ours,
                },
                Vec::new(),
            ),
        );
    }

    /// Hands bytes of link `link` to its other end, or grants them back at once if that end's
    /// session has left.
    pub(crate) fn data(&self, link: u32, body: Vec<u8>) -> Result<(), String> {
        let mut state = lock(&self.pod.0);
        let key = (self.member.clone(), link);
        let Some(end) = state.ends.get_mut(&key) else {
            return Err(broke(&format!(
                "it sent bytes of link {link}, which is not open"
            )));
        };
        if end.ended {
            return Err(broke(&format!(
                "it sent bytes of link {link} after it ended its way"
            )));
        }
        let Ok(bytes) = u32::try_from(body.len()) else {
            unreachable!(
                "a frame's body of {} bytes overflows a u32, though it is at most \
                 `frame::MAX_BODY_BYTES`, 1 MiB",
                body.len()
            )
        };
        let Some(window) = end.window.checked_sub(bytes) else {
            return Err(broke(&format!(
                "it sent {bytes} bytes of link {link}, past its window, of which {} bytes were \
                 left",
                end.window
            )));
        };
        let Some(peer) = end.peer.clone() else {
            state.send(&key.0, (ToSandbox::LinkCredit { link, bytes }, Vec::new()));
            return Ok(());
        };
        end.window = window;
        if let Some(other) = state.ends.get_mut(&peer) {
            other.owed += bytes;
        }
        state.send(&peer.0, (ToSandbox::LinkData { link: peer.1 }, body));
        Ok(())
    }

    /// Grants back `bytes` of link `link` to its other end.
    pub(crate) fn credit(&self, link: u32, bytes: u32) -> Result<(), String> {
        let mut state = lock(&self.pod.0);
        let key = (self.member.clone(), link);
        let Some(end) = state.ends.get_mut(&key) else {
            let opened = state.seats.get(&key.0).is_some_and(|seat| link < seat.next);
            // A grant of the last bytes of a link that has since closed is an expected race.
            if opened {
                return Ok(());
            }
            return Err(broke(&format!(
                "it granted back bytes of link {link}, which it was never sent"
            )));
        };
        let Some(owed) = end.owed.checked_sub(bytes) else {
            return Err(broke(&format!(
                "it granted back {bytes} bytes of link {link}, more than the {} it was sent and \
                 hadn't granted back",
                end.owed
            )));
        };
        end.owed = owed;
        let Some(peer) = end.peer.clone() else {
            return Ok(());
        };
        if let Some(other) = state.ends.get_mut(&peer) {
            other.window += bytes;
        }
        let credit = ToSandbox::LinkCredit {
            link: peer.1,
            bytes,
        };
        state.send(&peer.0, (credit, Vec::new()));
        Ok(())
    }

    /// Ends link `link`'s way from this session to its other end, and takes the link out once both
    /// ways have ended.
    pub(crate) fn end(&self, link: u32) -> Result<(), String> {
        let mut state = lock(&self.pod.0);
        let key = (self.member.clone(), link);
        let Some(end) = state.ends.get_mut(&key) else {
            return Err(broke(&format!("it ended link {link}, which is not open")));
        };
        if end.ended {
            return Err(broke(&format!("it ended link {link}'s way twice")));
        }
        end.ended = true;
        let Some(peer) = end.peer.clone() else {
            state.close(&key);
            return Ok(());
        };
        state.send(&peer.0, (ToSandbox::LinkEnd { link: peer.1 }, Vec::new()));
        if state.ends.get(&peer).is_some_and(|other| other.ended) {
            state.close(&key);
            state.close(&peer);
        }
        Ok(())
    }
}

fn take_number(state: &mut State, member: &Member) -> u32 {
    let Some(seat) = state.seats.get_mut(member) else {
        unreachable!("`{member}` has no seat in its pod, though the caller found it there")
    };
    let number = seat.next;
    seat.next = number.wrapping_add(1);
    seat.open += 1;
    number
}

/// Starts the thread that writes the frames of the session's links to its sandbox, in their order,
/// until the session ends; or ends the session, if no thread starts.
pub(crate) fn write(shared: &Arc<Shared>, frames: Receiver<Frame>) -> io::Result<()> {
    let writing = Arc::clone(shared);
    let spawned = thread::Builder::new()
        .name("links".to_owned())
        .spawn(move || {
            // The queue's sender goes as the session leaves its pod, once it has ended.
            for (message, body) in frames {
                let by = Bound {
                    clock: &Machine,
                    by: Machine.now().saturating_add(FRAME_TIMEOUT),
                };
                let writer = lock(&writing.writer);
                let sent = (writing.can_send(by, Due::Link))
                    .and_then(|()| writing.write(&writer, &message, &body, by, Due::Link));
                if sent.is_err() {
                    return;
                }
            }
        });
    spawned.map(drop).map_err(|error| {
        let reason = "the session's links' writer thread didn't start";
        shared.end(reason.to_owned());
        context(reason, &error)
    })
}

impl Assembler {
    /// Acts on a frame of a link, which only a session with a place in its pod takes.
    pub(crate) fn link(&self, message: FromSandbox, body: Vec<u8>) -> Result<(), String> {
        let Some(place) = &self.shared.links else {
            return Err(broke(
                "it sent a frame of a link, but the session carries no links: send one only on a \
                 session accepted with a place in its pod",
            ));
        };
        match message {
            FromSandbox::LinkOpen { request, to, port } => {
                place.open(request, &to, port);
                Ok(())
            }
            FromSandbox::LinkData { link } => place.data(link, body),
            FromSandbox::LinkCredit { link, bytes } => place.credit(link, bytes),
            FromSandbox::LinkEnd { link } => place.end(link),
            other => unreachable!(
                "`Assembler::link` was handed {other:?}, though `route` hands it a link's frames \
                 alone"
            ),
        }
    }
}
