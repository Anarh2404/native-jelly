//! A loopback Jellyfin that proves requests overlap: it holds a request's answer until the request
//! it is paired with has arrived too. Requests made one after another miss each other, whichever
//! comes first, so no timing threshold is involved.

use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long a held answer waits. Only a failing test pays it.
const PATIENCE: Duration = Duration::from_secs(2);

pub(crate) struct Rendezvous {
    pub(crate) sid: crate::catalog::ServerId,
    state: Arc<(Mutex<State>, Condvar)>,
}

#[derive(Default)]
struct State {
    seen: Vec<String>,
    missed: Vec<String>,
}

impl Rendezvous {
    /// `pairs` of path fragments ask to overlap, each side holding for the other; `answer` is the
    /// JSON body for a path.
    pub(crate) fn start(pairs: &'static [(&'static str, &'static str)], answer: fn(&str) -> String) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port() as i32;
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let shared = state.clone();
        // Detached: the listener lives as long as the test process, and answers nothing once idle.
        std::thread::spawn(move || {
            for socket in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || serve(socket, &state, pairs, answer));
            }
        });
        let sid = crate::catalog::register_for_test(&format!("rendezvous-{port}"), "127.0.0.1", port, "tok", "rendezvous-client");
        let client = crate::catalog::client_for(sid).expect("registered");
        crate::jf::seat::register_with(client.origin(), crate::jf::seat::Seat {
            user_id: "user-rendezvous".into(),
            ..Default::default()
        });
        Self { sid, state }
    }

    /// Every path that was answered without its partner having arrived.
    pub(crate) fn missed(&self) -> Vec<String> {
        self.state.0.lock().unwrap().missed.clone()
    }

    pub(crate) fn seen(&self) -> Vec<String> {
        self.state.0.lock().unwrap().seen.clone()
    }
}

fn serve(
    mut socket: std::net::TcpStream,
    state: &(Mutex<State>, Condvar),
    pairs: &[(&str, &str)],
    answer: fn(&str) -> String,
) {
    let mut reader = BufReader::new(socket.try_clone().expect("clone"));
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
            break;
        }
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let partners: Vec<&str> = pairs
        .iter()
        .flat_map(|&(a, b)| [(a, b), (b, a)])
        .filter(|(mine, _)| path.contains(mine))
        .map(|(_, partner)| partner)
        .collect();
    let mut st = state.0.lock().unwrap();
    st.seen.push(path.clone());
    state.1.notify_all();
    let deadline = Instant::now() + PATIENCE;
    for partner in partners {
        while !st.seen.iter().any(|p| p.contains(partner)) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                st.missed.push(path.clone());
                break;
            }
            st = state.1.wait_timeout(st, left).unwrap().0;
        }
    }
    drop(st);
    let body = answer(&path);
    let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}
