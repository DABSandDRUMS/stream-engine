//! Local drum-screen control, independent of the engine connection.
//!
//! One worker owns helper execution; egui only exchanges bounded messages. Status is
//! refreshed every two seconds so the global keyboard binding and header stay in sync.

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HELPER: &str = "stream-engine-drum-screen";
const REFRESH: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
enum Request {
    Status,
    Toggle,
}

impl Request {
    fn argument(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Toggle => "toggle",
        }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Desk,
    Tv,
}

struct Failure {
    detail: String,
    unavailable: bool,
}

struct Worker {
    requests: Sender<Request>,
    results: Receiver<Result<Mode, Failure>>,
}

pub struct DrumScreen {
    worker: Option<Worker>,
    mode: Option<Mode>,
    busy: Option<Request>,
    available: bool,
    next_refresh: Instant,
    error: Option<String>,
    notice: Option<String>,
}

impl DrumScreen {
    pub fn new(ctx: egui::Context) -> Self {
        let (requests, incoming) = bounded::<Request>(1);
        let (outgoing, results) = bounded(1);
        let started = std::thread::Builder::new().name("se-drum-screen".into()).spawn(move || {
            while let Ok(request) = incoming.recv() {
                if outgoing.send(run_helper(request)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        let mut state = Self {
            worker: None,
            mode: None,
            busy: None,
            available: false,
            next_refresh: Instant::now(),
            error: None,
            notice: None,
        };
        match started {
            Ok(_) => state.worker = Some(Worker { requests, results }),
            Err(error) => state.fail(Failure {
                detail: format!("Couldn't start drum-screen control: {error}. Restart the UI to try again."),
                unavailable: true,
            }),
        }
        state
    }

    /// Pump results and schedule at most one status request, never waiting on a helper.
    /// A new failure is returned once for the model's toast queue.
    pub fn tick(&mut self, ctx: &egui::Context) -> Option<String> {
        if let Some(worker) = &self.worker {
            match worker.results.try_recv() {
                Ok(result) => {
                    self.busy = None;
                    self.next_refresh = Instant::now() + REFRESH;
                    match result {
                        Ok(mode) => {
                            self.mode = Some(mode);
                            self.available = true;
                            self.error = None;
                        }
                        Err(error) => self.fail(error),
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.worker = None;
                    self.busy = None;
                    self.fail(Failure {
                        detail: "Drum-screen control stopped. Restart the UI to try again.".into(),
                        unavailable: true,
                    });
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.worker.is_some() && self.busy.is_none() {
            let now = Instant::now();
            if now >= self.next_refresh {
                self.request(Request::Status, ctx);
            } else {
                ctx.request_repaint_after(self.next_refresh - now);
            }
        }
        self.notice.take()
    }

    pub fn enabled(&self) -> bool {
        self.available && self.worker.is_some() && self.busy.is_none()
    }

    pub fn on(&self) -> bool {
        matches!(self.mode, Some(Mode::Tv))
    }

    pub fn label(&self) -> &'static str {
        match self.mode {
            Some(Mode::Tv) => "Drum screen on",
            Some(Mode::Desk) => "Drum screen off",
            None if !self.available && self.error.is_some() => "Drum screen unavailable",
            None => "Drum screen unknown",
        }
    }

    pub fn detail(&self) -> &str {
        if let Some(error) = &self.error {
            return error;
        }
        match self.busy {
            Some(Request::Toggle) => "Switching screens. Please wait; this also works when the engine is offline.",
            Some(Request::Status) => "Checking drum-screen mode. Please wait.",
            None => match self.mode {
                Some(Mode::Tv) => "The drums TV is the only active screen. Click to restore the desk screens, workspaces and windows, then park the TV. Works even when the engine is offline.",
                Some(Mode::Desk) => "Click to move the entire desk to the drums TV and turn off the desk screens. Click again to restore the desk. Works even when the engine is offline.",
                None => "Drum-screen mode is not known yet. Waiting for the local helper's status.",
            },
        }
    }

    pub fn toggle(&mut self, ctx: &egui::Context) {
        if self.enabled() {
            self.request(Request::Toggle, ctx);
        }
    }

    fn request(&mut self, request: Request, ctx: &egui::Context) {
        let Some(worker) = &self.worker else { return };
        match worker.requests.try_send(request) {
            Ok(()) => self.busy = Some(request),
            Err(error) => {
                self.worker = None;
                self.fail(Failure {
                    detail: format!("Couldn't request drum-screen {}: {error}. Restart the UI to try again.", request.argument()),
                    unavailable: true,
                });
            }
        }
        ctx.request_repaint();
    }

    fn fail(&mut self, failure: Failure) {
        self.mode = None;
        self.available = !failure.unavailable;
        if self.error.as_deref() != Some(failure.detail.as_str()) {
            self.notice = Some(failure.detail.clone());
        }
        self.error = Some(failure.detail);
    }
}

fn run_helper(request: Request) -> Result<Mode, Failure> {
    let argument = request.argument();
    let preferred = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/bin").join(HELPER));
    let execute = |program: &std::path::Path| Command::new(program).arg(argument).stdin(Stdio::null()).output();
    // A missing local install may be supplied on PATH. Never invoke a shell or pre_exec.
    let output = match preferred {
        Some(path) => match execute(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => execute(std::path::Path::new(HELPER)),
            result => result,
        },
        None => execute(std::path::Path::new(HELPER)),
    }
    .map_err(|error| Failure {
        detail: format!("Couldn't run drum-screen {argument}: {error}. Install the executable helper at ~/.local/bin/{HELPER} (or on PATH)."),
        unavailable: true,
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(Failure {
            detail: if detail.is_empty() {
                format!("Drum-screen {argument} failed ({}). Run {HELPER} {argument} in a terminal for details.", output.status)
            } else {
                format!("Drum-screen {argument} failed: {detail}")
            },
            unavailable: false,
        });
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "desk" => Ok(Mode::Desk),
        "tv" => Ok(Mode::Tv),
        other => Err(Failure {
            detail: format!("Drum-screen {argument} returned an unknown mode ({other:?}). Update ~/.local/bin/{HELPER}; expected desk or tv."),
            unavailable: false,
        }),
    }
}
