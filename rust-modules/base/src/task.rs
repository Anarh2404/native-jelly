//! The thread seam: where work leaves the main thread ([`spawn`]), and what cannot follow it
//! ([`MainThread`]).
//!
//! `std::thread::spawn` **panics** when the OS refuses the thread — it unwraps `Builder::spawn`,
//! whose `Err` is `pthread_create`'s EAGAIN (glibc returns it when `allocate_stack` cannot `mmap`
//! the stack, or `clone(2)` hits `RLIMIT_NPROC`/`threads-max`). Almost every worker here is
//! started from the SDL loop, so that panic unwinds out of `nj_run` through the C shim and
//! takes the app down. It would also fail at the worst possible moment: the caller has just armed
//! an in-flight flag, so had the app survived, the screen would sit on a spinner that can never
//! resolve.
//!
//! **Measured on the target, not estimated** (`tools/threadprobe.c`, 2026-07-28, run under the
//! app's own uid — the first version of this note guessed and was wrong about which limit binds):
//!
//! | stack | refused at | binding limit |
//! |---|---|---|
//! | 2 MB (the platform default) | **2043 threads** | `RLIMIT_AS` — the full AArch32 4 GB space |
//! | 256 KB (`spawn_small`) | **3745 threads** | `RLIMIT_NPROC`, which is 3746 |
//!
//! Both refusals are EAGAIN, exactly the error `spawn` unwraps. Against that, the app runs **31
//! threads at playback peak** (13 at Home) and peaks at 363 MB of `VmSize` — ~66x and ~11x
//! headroom. So this module is not fixing something that happens; it is refusing to let an
//! unreachable-but-unrecoverable branch exist, for the price of a return value.
//!
//! The table is also why [`SMALL_STACK`] is not a micro-optimisation: which limit you hit depends
//! on the stack size, and the crossover sits between these two values. A 2 MB stack spends address
//! space (the scarcer resource here at 1/2043 per thread); 256 KB spends a thread slot instead.
//!
//! Both entry points below report the refusal instead of panicking. Releasing the armed flag
//! stays with the caller — a latch is a property of the screen, not of the thread.
//!
//! **This is what step 3 of `docs/async-model-decision.md` became.** That step adopted a generic
//! `task::Job<T>` — generation guard, monotone one-slot mailbox, single-flight, cancel — to
//! replace the hand-rolled worker idiom. By the time its turn came, the idiom had already landed
//! by hand at five sites and been device-verified, so `Job` would have been a pure refactor of
//! working code; the re-evaluation declined it and is logged in the decision doc. What that
//! re-evaluation turned up instead is the divergence above: of the five copies of the idiom, only
//! two handled a refused spawn. The piece worth sharing was the spawn, not the mailbox.
//!
//! **This module is also the allowlist boundary of `ci/check-deps.sh`'s `threads` gate**
//! (restructure spec §15.2, phase 12): every `std::thread::spawn` call outside this file, in
//! production code, is a violation — checked file by file across the whole crate, with a
//! `#[cfg(test)] mod` block excluded (a unit test's own mock TCP/HTTP peer stands in for a real
//! peer and is not a worker). Verified 2026-09-10: it is already empty, i.e. every real worker
//! this crate spawns — the demux/media threads, `aq`, `stream`, `imgcache`, `ff`, `http`, `auth`,
//! `curlio`/`route::decision`, the Plex client/transcoder, `browse`, `player`, `nj_machine::present`,
//! `nj_machine::landgate` — already calls [`spawn`]/[`spawn_small`], not `std::thread::spawn` directly.
//! `ci/allow/threads.txt` is that gate's allowlist and it, too, is empty for the same reason; a
//! new production `thread::spawn` outside this file fails CI rather than waiting for a review.

use std::io;
use std::marker::PhantomData;
use std::thread::{Builder, JoinHandle};

mod blocking;
pub mod watchdog;
#[cfg(feature = "threadcheck")]
pub mod runtime_check;
pub use blocking::{allow_blocking, AllowBlocking};
pub use blocking::{assert_may_block, BlockingGuard, BlockingLabel, FrameScope};

/// Proof that the holder runs on the SDL main thread.
///
/// A ZST whose only content is a `!Send + !Sync` marker, so neither it nor a `&` to it can be
/// captured by anything handed to [`spawn`]. That is the whole mechanism: functions that must
/// not leave the main thread take one, and moving such a call onto a thread stops compiling.
///
/// It gates the two things in `player/` that were main-thread-confined by comment only:
///
/// * **the ACB/Starfish seam** — `player::ffi`'s wrappers. The raw `extern "C"` block is private
///   to that module, so the token is the only way in. Bind order (`setMediaId` → `LOADED` →
///   `setMediaVideoData` → `setDisplayWindow` → `PLAYING`) is a sequence of calls with no
///   locking behind it; a second thread stepping into the middle of it corrupts the sink.
/// * **the native session slot** — `App.adapters.player`. Until phase 9 that was
///   `player::engine::ENGINE`, a `static mut` handed out as `&'static mut` with worker threads
///   holding raw pointers into the boxes it owns: two live `&mut` to it is instant UB, and the
///   token was the only thing standing between the code and one. It is a FIELD now, so the token
///   is CONSUMED into `player::adapter::PlayerAdapter` and `&mut PlayerAdapter` is the
///   proof instead — one the borrow checker keeps rather than one a caller can satisfy twice.
///
/// The one deliberate hole: `assume` is callable, so `unsafe { MainThread::assume() }` inside a
/// worker would defeat this. That is the ceiling of the pattern, not an oversight — what it buys
/// is that the mistake has to be *written*, in an `unsafe` block, instead of happening by
/// forgetting a convention documented in three other files.
pub struct MainThread(PhantomData<*const ()>);

impl MainThread {
    /// Mint the token. `nj_run` calls this once, at the top, and nothing else should.
    ///
    /// # Safety
    /// The caller asserts this is the SDL main thread. It is not a memory-safety obligation in
    /// itself — it is the premise every `&MainThread` downstream is trusted on, including the
    /// one the Player adapter holds, so a false one reintroduces exactly the races this prevents.
    pub unsafe fn assume() -> Self {
        MainThread(PhantomData)
    }
}

/// Stack for the short network workers — one HTTP round trip and a decode. The platform default
/// (2 MB) is pure reserved address space on a 32-bit target, and several of these run at once.
/// Deliberately NOT used for the demux/decode/encode threads: libavformat wants a real stack.
const SMALL_STACK: usize = 256 * 1024;

fn refused(what: &str, e: &io::Error) {
    crate::eventlog::log(&format!(
        "task: spawn '{what}' REFUSED ({e}) — this work is dropped"
    ));
}

fn spawn_with(
    what: &str,
    stack: Option<usize>,
    f: impl FnOnce() + Send + 'static,
) -> Option<JoinHandle<()>> {
    let mut b = Builder::new();
    if let Some(n) = stack {
        b = b.stack_size(n);
    }
    match b.spawn(f) {
        Ok(h) => Some(h),
        Err(e) => {
            refused(what, &e);
            None
        }
    }
}

/// Spawn `f` and keep the handle. `None` means the OS refused the thread and **the worker does
/// not exist** — undo whatever was armed for it. `what` names the work in the failure log.
pub fn spawn(what: &str, f: impl FnOnce() + Send + 'static) -> Option<JoinHandle<()>> {
    spawn_with(what, None, f)
}

/// [`spawn`] on a [`SMALL_STACK`], for the fire-and-forget network workers. `false` = not spawned.
pub fn spawn_small(what: &str, f: impl FnOnce() + Send + 'static) -> bool {
    spawn_with(what, Some(SMALL_STACK), f).is_some()
}

/// [`spawn_small`], but handing back the handle so the caller can [`join`] it later. For work that
/// is fire-and-forget *during* a session yet must still be allowed to finish before the process
/// exits — the end-of-playback scrobble is the only such case, and losing it would lose the
/// server-side resume point.
pub fn spawn_small_keeping(
    what: &str,
    f: impl FnOnce() + Send + 'static,
) -> Option<JoinHandle<()>> {
    spawn_with(what, Some(SMALL_STACK), f)
}

/// A join this long parked the SDL loop for ~15 frames — the shortest stall a person reads as a
/// freeze rather than a stutter.
const STALL_MS: u64 = 250;

/// Join a worker and report what THIS thread paid for it.
///
/// The counterpart to [`spawn`], and the reason it exists rather than a bare `let _ = h.join();`:
/// the frame loop has an FPS heartbeat, `/tmp/nativejelly-framedrop` catches the frames that blow the
/// budget, and `ui::profile` splits a frame by draw phase — but none of them can see a worker, and
/// every teardown stall this engine has had was the main thread parked in one of these joins with
/// no number left behind. Unconditional: an `Instant` pair around a call whose whole purpose is to
/// block cannot perturb what it measures, and gating it would hide the teardown numbers in exactly
/// the harness runs where they show up.
///
/// The number is the JOINER's wait, not the worker's lifetime. Joining a worker that finished
/// 300 ms ago costs nothing, and two of `engine::teardown`'s three joins are normally of workers
/// that have already exited — reporting their lifetimes would flag every teardown as stalling and
/// make the numbers worthless for finding the one that does.
///
/// After this, a bare `.join()` anywhere outside this module is a stall nobody can see;
/// `grep -rn '\.join()' rust-modules/src` is the enforcement, because there is no other.
///
/// **Baseline measured on device** (2026-07-29, BACK out of a direct-play movie on a healthy LAN,
/// captured by temporarily setting `STALL_MS` to 0): `demux 0ms`, `media 0ms`, `timeline 0ms` —
/// all three joins are free in the ordinary case. That is the number the teardown findings have to
/// be read against: the joins are fault-conditional, not an everyday cost, and it independently
/// confirms the condvar fix that superseded `docs/async-model-review.md` §3b's "every teardown
/// pays 0-1000 ms". It also means a single `THREADJOIN` line in a log is signal, never noise.
pub fn join(what: &str, h: JoinHandle<()>) {
    let t0 = std::time::Instant::now();
    let outcome = h.join();
    let ms = t0.elapsed().as_millis() as u64;
    if outcome.is_err() {
        // Previously swallowed by `let _ = t.join()`. A worker that died holding its socket or an
        // armed in-flight flag is the first thing worth knowing at teardown.
        crate::eventlog::log(&format!(
            "task: worker '{what}' PANICKED (joined after {ms}ms)"
        ));
    }
    if ms >= STALL_MS {
        crate::eventlog::log(&format!("THREADJOIN {what} {ms}ms STALL"));
    }
}

/// What a [`Beside`] still owes: the work (not run yet, or its thread was refused) or its answer.
enum Work<'scope, T> {
    Todo(Box<dyn FnOnce() -> T + Send + 'scope>),
    Done(T),
    Taken,
}

/// A request asked for BESIDE the caller's own, on a scoped sibling thread ([`beside`]).
///
/// Where [`spawn`] starts a detached worker that outlives its caller, this is structured: the
/// thread belongs to a [`std::thread::scope`], so the closure may borrow what the scope's caller
/// borrows, and the scope joins the thread on EVERY exit of the scope body — an early `return`
/// and a panic included — so a request started here cannot be left running behind a result that
/// was built without it. Reading the answer is [`Beside::finish`].
pub struct Beside<'scope, T> {
    thread: Option<std::thread::ScopedJoinHandle<'scope, ()>>,
    work: std::sync::Arc<std::sync::Mutex<Work<'scope, T>>>,
}

/// Ask for `f`'s answer on a scoped sibling thread named `what`, to be read with
/// [`Beside::finish`] once the caller's own work is done. Use it for requests that need nothing
/// from each other, so the caller waits for the slower of them rather than for the sum.
///
/// **A refused thread is not a lost answer**: unlike [`spawn`], which drops the work, the closure
/// is kept and `finish` runs it on the caller's thread (the serial order it had before it was
/// asked beside), and the refusal is logged. **A panic in `f` is the caller's panic**: `finish`
/// re-raises it, as the same call made inline would have; nothing is swallowed into a default.
///
/// Only the borrows of the scope's CALLER can be captured (a thread started inside the scope body
/// cannot borrow the body's own locals) — move owned copies in. Call it as
/// `nj_base::task::beside(..)`.
pub fn beside<'scope, 'env, T, F>(
    s: &'scope std::thread::Scope<'scope, 'env>,
    what: &str,
    f: F,
) -> Beside<'scope, T>
where
    T: Send + 'scope,
    F: FnOnce() -> T + Send + 'scope,
{
    beside_with(s, what, None, f)
}

fn beside_with<'scope, 'env, T, F>(
    s: &'scope std::thread::Scope<'scope, 'env>,
    what: &str,
    stack: Option<usize>,
    f: F,
) -> Beside<'scope, T>
where
    T: Send + 'scope,
    F: FnOnce() -> T + Send + 'scope,
{
    use std::sync::{Arc, Mutex};
    // The lock is never held while `f` runs, so a panic in `f` cannot poison it.
    fn lock<'a, 'scope, T>(work: &'a Mutex<Work<'scope, T>>) -> std::sync::MutexGuard<'a, Work<'scope, T>> {
        work.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn run<T>(work: &Mutex<Work<'_, T>>) {
        let todo = match std::mem::replace(&mut *lock(work), Work::Taken) {
            Work::Todo(f) => f,
            other => {
                *lock(work) = other;
                return;
            }
        };
        let answer = todo();
        *lock(work) = Work::Done(answer);
    }
    let work = Arc::new(Mutex::new(Work::Todo(Box::new(f) as Box<dyn FnOnce() -> T + Send + 'scope>)));
    let mut b = Builder::new().name(what.to_string());
    if let Some(n) = stack {
        b = b.stack_size(n);
    }
    let worker = Arc::clone(&work);
    let thread = match b.spawn_scoped(s, move || run(&worker)) {
        Ok(h) => Some(h),
        Err(e) => {
            crate::eventlog::log(&format!(
                "task: spawn '{what}' REFUSED ({e}) — asking for it on the caller's thread instead"
            ));
            None
        }
    };
    Beside { thread, work }
}

impl<T> Beside<'_, T> {
    /// The answer: waits for the sibling thread, or — when its thread was refused — asks here.
    /// Re-raises a panic of the work.
    pub fn finish(self) -> T {
        if let Some(h) = self.thread {
            if let Err(panic) = h.join() {
                std::panic::resume_unwind(panic);
            }
        }
        let work = std::mem::replace(&mut *self.work.lock().unwrap_or_else(|e| e.into_inner()), Work::Taken);
        match work {
            Work::Done(answer) => answer,
            Work::Todo(f) => f(),
            Work::Taken => unreachable!("a Beside is finished once, and its work ran once"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    /// The whole point of the module: a refused spawn is a return value, not a panic. Forced with
    /// an unsatisfiable stack size, since exhausting the real thread limit is not something a
    /// host test can arrange politely.
    ///
    /// **Not `usize::MAX`**, which is the obvious spelling of "unsatisfiable" and the one this
    /// test used until 2026-08-02. `std`'s unix `Thread::new` rounds the requested stack up to a
    /// page — `stack_size + page_size - 1` — and on `usize::MAX` that addition overflows, so a
    /// debug-assertions build panics *inside std* before `pthread_create` is ever reached. The
    /// test then fails claiming the very thing it exists to prove. It is toolchain-dependent
    /// (green on 1.96 stable, red on 1.98-nightly) and this crate is nightly-only for
    /// `-Z build-std`, so `make check` was the one that saw it.
    ///
    /// Half the address space is just as unsatisfiable — no allocator anywhere will find 8 EiB —
    /// while leaving room for the page rounding to land, and it stays correct on the 32-bit
    /// target, which a literal like `1 << 60` would not.
    #[test]
    fn a_refused_spawn_reports_instead_of_panicking() {
        let h = super::spawn_with("unsatisfiable", Some(usize::MAX / 2), || unreachable!());
        assert!(
            h.is_none(),
            "a spawn that cannot succeed must report, not hand back a handle"
        );
    }

    /// [`super::MainThread`] earns its keep entirely by NOT being `Send` — that absence is what
    /// makes a closure which captured one impossible to hand to `spawn`. An absent impl is
    /// invisible to ordinary code, so detect it: the inherent const applies only when `T: Send`,
    /// and resolution falls through to the blanket trait when it doesn't. The `i32` line is there
    /// so a probe that silently answered "never Send" would fail rather than pass everything.
    #[test]
    fn the_main_thread_token_cannot_cross_a_spawn() {
        struct Probe<T>(std::marker::PhantomData<T>);
        trait NotSend {
            const SEND: bool = false;
        }
        impl<T> NotSend for Probe<T> {}
        impl<T: Send> Probe<T> {
            const SEND: bool = true;
        }

        assert!(Probe::<i32>::SEND, "the probe must see a Send type as Send");
        assert!(
            !Probe::<super::MainThread>::SEND,
            "MainThread must not be Send"
        );
        assert!(
            !Probe::<&super::MainThread>::SEND,
            "nor may a borrow of it — that is the form it is passed in"
        );
    }

    #[test]
    fn a_spawned_worker_actually_runs() {
        let (tx, rx) = mpsc::channel();
        assert!(super::spawn_small("probe", move || tx.send(7).unwrap()));
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(7));
    }

    /// A sibling asked for beside the caller's work is running while the caller works: the caller
    /// parks on a channel only the sibling can feed, so a `beside` that ran the work later (or
    /// inline, serially) would never get the answer.
    #[test]
    fn a_beside_request_runs_while_the_caller_works() {
        let (tx, rx) = mpsc::channel();
        let answer = std::thread::scope(|s| {
            let sibling = super::beside(s, "beside-probe", move || {
                tx.send(7).unwrap();
                "asked"
            });
            assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(7), "the sibling never ran beside us");
            sibling.finish()
        });
        assert_eq!(answer, "asked");
    }

    /// The closure may borrow what the scope's caller borrows, and the thread carries its label.
    #[test]
    fn a_beside_request_borrows_the_callers_data_and_carries_its_name() {
        let owned = String::from("borrowed");
        let (len, name) = std::thread::scope(|s| {
            super::beside(s, "beside-name", || (owned.len(), std::thread::current().name().map(str::to_owned))).finish()
        });
        assert_eq!((len, name.as_deref()), (8, Some("beside-name")));
    }

    /// The OS-refused arm: the thread cannot start, and the work is NOT dropped (unlike `spawn`) —
    /// `finish` runs it, here, exactly once.
    #[test]
    fn a_refused_beside_thread_is_asked_on_the_callers_thread() {
        let ran = std::sync::atomic::AtomicU32::new(0);
        let caller = std::thread::current().id();
        let (answer, on_caller) = std::thread::scope(|s| {
            let sibling = super::beside_with(s, "unsatisfiable", Some(usize::MAX / 2), || {
                ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (9, std::thread::current().id())
            });
            let (answer, ran_on) = sibling.finish();
            (answer, ran_on == caller)
        });
        assert_eq!(answer, 9);
        assert!(on_caller, "a refused thread asks on the caller's thread");
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1, "and exactly once");
    }

    /// A panic in the sibling's work is the caller's panic at `finish`, as the same call made
    /// inline would have been, and not a default that hides it.
    #[test]
    fn a_beside_panic_is_reraised_at_finish() {
        let outcome = std::panic::catch_unwind(|| {
            std::thread::scope(|s| {
                let sibling = super::beside(s, "beside-panic", || -> u8 { panic!("sibling work") });
                sibling.finish()
            })
        });
        let panic = outcome.expect_err("finish must re-raise");
        assert_eq!(panic.downcast_ref::<&str>().copied(), Some("sibling work"));
    }

    /// The scope joins a sibling its owner never `finish`ed: an early return out of the scope body
    /// cannot leave the request running behind the result.
    #[test]
    fn a_beside_request_never_finished_is_still_joined_by_the_scope() {
        let done = std::sync::atomic::AtomicBool::new(false);
        let early: Option<()> = std::thread::scope(|s| {
            let _dropped = super::beside(s, "beside-slow", || {
                std::thread::sleep(std::time::Duration::from_millis(150));
                done.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            None
        });
        assert!(early.is_none());
        assert!(done.load(std::sync::atomic::Ordering::SeqCst), "the scope returned before the sibling finished");
    }
}
