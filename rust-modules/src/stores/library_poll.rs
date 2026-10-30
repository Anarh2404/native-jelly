//! **Noticing that a server's library changed**, by asking it now and then.
//!
//! Upstream gets this from a Plex notification socket; this app's libcurl 7.61 has no WebSocket, so
//! it polls instead, slowly: one small request per server every [`POLL_MS`], and once on the way back
//! to the foreground, never while the player is on screen.
//!
//! The question is [`Signature`], the listing's total and its newest item. Measured on Jellyfin 12.0:
//! about 20 ms, unmoved by a playback report or a mark-played. It is compared with the last answer
//! rather than asking "what changed since T", because `MinDateLastSaved` would read the television's
//! clock against the server's, and would not see a deletion at all.
//!
//! [`Machine`] is pure: it reads no clock, starts nothing and holds no channel.

use crate::catalog::ServerId;

/// How often a server is asked while the app is on screen.
pub(crate) const POLL_MS: u32 = 5 * 60 * 1000;
/// A return to the foreground asks at once, unless it asked this recently.
const FOREGROUND_SPACING_MS: u32 = 30 * 1000;

/// What a server's library looked like when last asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Signature {
    pub(crate) total: i64,
    pub(crate) newest: String,
}

/// `a` is at or after `b`, on the app's wrapping millisecond clock.
fn reached(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) < 0x8000_0000
}

#[derive(Default)]
pub(crate) struct Machine {
    /// When to ask next. `None` until the first tick starts the clock.
    next_at: Option<u32>,
    last_asked: Option<u32>,
    in_flight: bool,
    known: Vec<(ServerId, Signature)>,
}

impl Machine {
    /// One frame: `true` when a round of questions should start now.
    pub(crate) fn tick(&mut self, now: u32, on_player: bool) -> bool {
        let next_at = *self.next_at.get_or_insert(now.wrapping_add(POLL_MS));
        if self.in_flight || on_player || !reached(now, next_at) {
            return false;
        }
        self.in_flight = true;
        self.last_asked = Some(now);
        self.next_at = Some(now.wrapping_add(POLL_MS));
        true
    }

    /// The app came back to the foreground: ask on the next frame that may.
    pub(crate) fn foregrounded(&mut self, now: u32) {
        if self.last_asked.is_none_or(|at| reached(now, at.wrapping_add(FOREGROUND_SPACING_MS))) {
            self.next_at = Some(now);
        }
    }

    /// The round ended. A server that did not answer keeps its last signature, so a blip is neither
    /// a change nor a lost baseline. Returns the servers whose library changed; a server's first
    /// answer is its baseline, not a change.
    pub(crate) fn landed(&mut self, answers: Vec<(ServerId, Option<Signature>)>) -> Vec<ServerId> {
        self.in_flight = false;
        let mut changed = Vec::new();
        for (sid, answer) in answers {
            let Some(signature) = answer else { continue };
            match self.known.iter_mut().find(|(known, _)| *known == sid) {
                Some((_, last)) if *last == signature => {}
                Some((_, last)) => {
                    *last = signature;
                    changed.push(sid);
                }
                None => self.known.push((sid, signature)),
            }
        }
        changed
    }
}

/// The machine and the one worker it may have out.
#[derive(Default)]
pub(crate) struct LibraryPoll {
    machine: Machine,
    rx: Option<std::sync::mpsc::Receiver<Vec<(ServerId, Option<Signature>)>>>,
}

impl LibraryPoll {
    pub(crate) fn foregrounded(&mut self, now: u32) {
        self.machine.foregrounded(now);
    }

    /// One frame, on the main thread. Never blocks. Returns the servers whose library changed.
    pub(crate) fn tick(&mut self, now: u32, on_player: bool) -> Vec<ServerId> {
        if self.machine.tick(now, on_player) {
            let sids: Vec<ServerId> = crate::catalog::server_ids().collect();
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = nj_base::task::spawn_small("library-poll", move || {
                let answers = sids.into_iter().map(|sid| (sid, signature_of(sid))).collect();
                let _ = tx.send(answers);
            });
            if spawned {
                self.rx = Some(rx);
            } else {
                let _ = self.machine.landed(Vec::new());
            }
        }
        let Some(rx) = &self.rx else { return Vec::new() };
        match rx.try_recv() {
            Ok(answers) => {
                self.rx = None;
                self.machine.landed(answers)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Vec::new(),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                self.machine.landed(Vec::new())
            }
        }
    }
}

fn signature_of(sid: ServerId) -> Option<Signature> {
    let (total, newest) = crate::catalog::client_for(sid)?.jf()?.library_signature()?;
    Some(Signature { total, newest })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: fn() -> ServerId = || ServerId::from_raw(0);

    fn sig(total: i64, newest: &str) -> Option<Signature> {
        Some(Signature { total, newest: newest.into() })
    }

    #[test]
    fn it_asks_on_the_interval_never_on_the_first_frame_or_twice_at_once() {
        let mut m = Machine::default();
        assert!(!m.tick(1_000, false), "the first frame only starts the clock");
        assert!(!m.tick(1_000 + POLL_MS - 1, false));
        assert!(m.tick(1_000 + POLL_MS, false));
        assert!(!m.tick(1_000 + 2 * POLL_MS, false), "one round at a time");
        let _ = m.landed(Vec::new());
        assert!(m.tick(1_000 + 2 * POLL_MS, false));
    }

    #[test]
    fn the_player_holds_the_question_until_it_leaves() {
        let mut m = Machine::default();
        let _ = m.tick(0, false);
        assert!(!m.tick(POLL_MS, true));
        assert!(!m.tick(POLL_MS * 3, true));
        assert!(m.tick(POLL_MS * 3 + 1, false));
    }

    #[test]
    fn the_foreground_asks_at_once_unless_it_just_asked() {
        let mut m = Machine::default();
        let _ = m.tick(0, false);
        m.foregrounded(1_000);
        assert!(m.tick(1_000, false), "nothing asked yet");
        let _ = m.landed(Vec::new());
        m.foregrounded(1_000 + FOREGROUND_SPACING_MS - 1);
        assert!(!m.tick(1_000 + FOREGROUND_SPACING_MS - 1, false), "asked moments ago");
        m.foregrounded(1_000 + FOREGROUND_SPACING_MS);
        assert!(m.tick(1_000 + FOREGROUND_SPACING_MS, false));
    }

    #[test]
    fn only_a_moved_signature_is_a_change() {
        let mut m = Machine::default();
        assert!(m.landed(vec![(SID(), sig(5, "a"))]).is_empty(), "the first answer is the baseline");
        assert!(m.landed(vec![(SID(), sig(5, "a"))]).is_empty());
        assert_eq!(m.landed(vec![(SID(), sig(6, "b"))]), vec![SID()], "an addition");
        assert_eq!(m.landed(vec![(SID(), sig(5, "b"))]), vec![SID()], "a deletion moves only the total");
        assert_eq!(m.landed(vec![(SID(), sig(5, "c"))]), vec![SID()], "one out, one in moves only the newest");
    }

    #[test]
    fn a_server_that_did_not_answer_keeps_its_baseline() {
        let mut m = Machine::default();
        let _ = m.landed(vec![(SID(), sig(5, "a"))]);
        assert!(m.landed(vec![(SID(), None)]).is_empty());
        assert!(m.landed(vec![(SID(), sig(5, "a"))]).is_empty(), "the blip was neither a change nor a reset");
    }
}
