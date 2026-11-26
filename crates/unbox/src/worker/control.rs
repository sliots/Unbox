//! Worker side of the IPC channel: pause / resume / cancel and the answers to
//! password, file-name and encoding prompts.

use std::io::{BufRead, Write};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;
use unbox_core::ipc::{to_line, InvalidCharsAction, WorkerCommand, WorkerEvent};

#[derive(Default)]
struct State {
    paused: bool,
    canceled: bool,
    password_pending: bool,
    password: Option<String>,
    password_skipped: bool,
    invalid_action: Option<InvalidCharsAction>,
    encoding: Option<u32>,
}

pub struct Control {
    state: Mutex<State>,
    changed: Condvar,
    out: Mutex<std::io::Stdout>,
}

pub fn control() -> &'static Control {
    static CONTROL: OnceLock<Control> = OnceLock::new();
    CONTROL.get_or_init(|| Control { state: Mutex::default(), changed: Condvar::new(), out: Mutex::new(std::io::stdout()) })
}

fn wipe(text: &mut String) {
    // Best effort: overwrite the secret before the allocation is released.
    unsafe {
        for b in text.as_bytes_mut() {
            std::ptr::write_volatile(b, 0);
        }
    }
    text.clear();
}

impl Control {
    pub fn send(&self, event: &WorkerEvent) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{}", to_line(event));
        let _ = out.flush();
    }

    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        state.canceled = true;
        state.paused = false;
        self.changed.notify_all();
    }

    pub fn canceled(&self) -> bool {
        self.state.lock().unwrap().canceled
    }

    /// Block while paused. Returns `false` once canceled.
    pub fn check(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        while state.paused && !state.canceled {
            state = self.changed.wait(state).unwrap();
        }
        !state.canceled
    }

    fn handle(&self, command: WorkerCommand) {
        match command {
            WorkerCommand::Pause => {
                self.state.lock().unwrap().paused = true;
                self.send(&WorkerEvent::PausedAck);
            }
            WorkerCommand::Resume => {
                self.state.lock().unwrap().paused = false;
                self.changed.notify_all();
                self.send(&WorkerEvent::ResumedAck);
            }
            WorkerCommand::Cancel => self.cancel(),
            WorkerCommand::PasswordResponse { mut password } => {
                let mut state = self.state.lock().unwrap();
                if state.password_pending && state.password.is_none() {
                    state.password = Some(std::mem::take(&mut password));
                    self.changed.notify_all();
                }
                wipe(&mut password);
            }
            WorkerCommand::PasswordSkip => {
                let mut state = self.state.lock().unwrap();
                if state.password_pending {
                    state.password_skipped = true;
                    self.changed.notify_all();
                }
            }
            WorkerCommand::InvalidCharsResponse { action } => {
                self.state.lock().unwrap().invalid_action = Some(action);
                self.changed.notify_all();
            }
            WorkerCommand::EncodingResponse { cp } => {
                self.state.lock().unwrap().encoding = Some(cp);
                self.changed.notify_all();
            }
            WorkerCommand::Start => {}
        }
    }

    /// Read commands from stdin until EOF. A managed worker cancels itself when its
    /// owner disappears.
    pub fn listen(&'static self, managed: bool) {
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            let mut reader = stdin.lock();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(command) = serde_json::from_str::<WorkerCommand>(line.trim()) {
                            self.handle(command);
                        }
                        wipe(&mut line);
                    }
                }
            }
            if managed {
                self.cancel();
            }
        });
    }

    /// Ask the UI for a password.
    pub fn request_password(&self, request: WorkerEvent) -> Answer {
        let mut state = self.state.lock().unwrap();
        if state.canceled {
            return Answer::Canceled;
        }
        state.password_pending = true;
        state.password = None;
        state.password_skipped = false;
        drop(state);
        self.send(&request);
        let mut state = self.state.lock().unwrap();
        while state.password.is_none() && !state.password_skipped && !state.canceled {
            state = self.changed.wait(state).unwrap();
        }
        state.password_pending = false;
        if state.canceled || state.password_skipped {
            if let Some(mut p) = state.password.take() {
                wipe(&mut p);
            }
            return if state.canceled { Answer::Canceled } else { Answer::Skip };
        }
        state.password.take().map_or(Answer::Canceled, Answer::Password)
    }

    fn wait_answer<T>(&self, event: WorkerEvent, take: impl Fn(&mut State) -> Option<T>, reset: impl Fn(&mut State)) -> Result<T, bool> {
        reset(&mut self.state.lock().unwrap());
        self.send(&event);
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        let mut state = self.state.lock().unwrap();
        loop {
            if state.canceled {
                return Err(true);
            }
            if let Some(value) = take(&mut state) {
                return Ok(value);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(false);
            }
            state = self.changed.wait_timeout(state, deadline - now).unwrap().0;
        }
    }

    /// `Err(true)` when canceled, `Err(false)` on timeout.
    pub fn ask_invalid_chars(&self, event: WorkerEvent) -> Result<InvalidCharsAction, bool> {
        self.wait_answer(event, |s| s.invalid_action.take(), |s| s.invalid_action = None)
    }

    pub fn ask_encoding(&self, event: WorkerEvent) -> Result<u32, bool> {
        self.wait_answer(event, |s| s.encoding.take(), |s| s.encoding = None)
    }
}

pub enum Answer {
    Password(String),
    /// Leave this (inner) archive as it is.
    Skip,
    Canceled,
}

/// Wait for the `start` line written by the UI before touching anything.
pub fn wait_for_start() -> bool {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok()
        && matches!(serde_json::from_str::<WorkerCommand>(line.trim()), Ok(WorkerCommand::Start))
}

/// The password of one archive: automatic guesses first, then the user, who is asked
/// again after every rejected answer. Shared by the open and extract callbacks.
#[derive(Default)]
pub struct ArchivePassword {
    inner: Mutex<PasswordState>,
}

#[derive(Default)]
struct PasswordState {
    /// A password was needed at some point.
    requested: bool,
    /// A password was needed during the current open attempt.
    asked: bool,
    current: Option<Vec<u16>>,
    current_from_user: bool,
    user_rejected: bool,
    guesses: std::collections::VecDeque<String>,
    guess_total: u32,
    guess_tried: u32,
    /// Wrong answers from the user, and whether any of them was a definite verdict.
    user_failures: u32,
    definite_failure: bool,
    /// Found inside the extracted content: the user may leave it as it is.
    inner: bool,
    skipped: bool,
}

fn wipe_wide(text: &mut [u16]) {
    text.iter_mut().for_each(|c| unsafe { std::ptr::write_volatile(c, 0) });
}

impl ArchivePassword {
    pub fn with_guesses(guesses: Vec<String>) -> Self {
        let state = PasswordState { guess_total: guesses.len() as u32, guesses: guesses.into(), ..Default::default() };
        ArchivePassword { inner: Mutex::new(state) }
    }

    /// An archive found inside the extracted content.
    pub fn for_inner_archive(self) -> Self {
        self.inner.lock().unwrap().inner = true;
        self
    }

    /// The user chose to leave this archive as it is.
    pub fn skipped(&self) -> bool {
        self.inner.lock().unwrap().skipped
    }

    /// The candidate in use was typed by the user (guesses are probed more cheaply).
    pub fn current_from_user(&self) -> bool {
        let state = self.inner.lock().unwrap();
        state.current.is_some() && state.current_from_user
    }

    pub fn requested(&self) -> bool {
        self.inner.lock().unwrap().requested
    }

    /// Start an open attempt; `asked` then tells whether this attempt needed a password.
    pub fn begin_attempt(&self) {
        self.inner.lock().unwrap().asked = false;
    }

    pub fn asked(&self) -> bool {
        self.inner.lock().unwrap().asked
    }

    /// A candidate is in use (it opened the archive or passed verification).
    pub fn has_current(&self) -> bool {
        self.inner.lock().unwrap().current.is_some()
    }

    /// The plain-text candidate in use, used to seed the guesses of an inner archive.
    pub fn current_text(&self) -> Option<String> {
        self.inner.lock().unwrap().current.as_ref().map(|p| String::from_utf16_lossy(p))
    }

    /// Never guess again: used when a guess could not be verified before extraction.
    pub fn drop_guesses(&self) {
        let mut state = self.inner.lock().unwrap();
        state.guesses.iter_mut().for_each(wipe);
        state.guesses.clear();
    }

    /// The UTF-16 candidate: the current one, else the next guess, else the user's
    /// answer. `None` when canceled.
    pub fn get(&self) -> Option<Vec<u16>> {
        let mut state = self.inner.lock().unwrap();
        state.requested = true;
        state.asked = true;
        if state.current.is_none() {
            if let Some(mut guess) = state.guesses.pop_front() {
                state.guess_tried += 1;
                control().send(&WorkerEvent::PasswordProbing { tried: state.guess_tried, total: state.guess_total });
                state.current = Some(guess.encode_utf16().collect());
                state.current_from_user = false;
                wipe(&mut guess);
            } else {
                let request = WorkerEvent::PasswordRequest {
                    retry: state.user_rejected,
                    failures: state.user_failures,
                    maybe_damaged: state.user_failures >= 3 && !state.definite_failure,
                    inner: state.inner,
                };
                drop(state);
                let answer = control().request_password(request);
                state = self.inner.lock().unwrap();
                let mut text = match answer {
                    Answer::Password(text) => text,
                    Answer::Skip => {
                        state.skipped = true;
                        return None;
                    }
                    Answer::Canceled => return None,
                };
                state.current = Some(text.encode_utf16().collect());
                state.current_from_user = true;
                wipe(&mut text);
            }
        }
        state.current.clone()
    }

    /// The candidate in use is wrong: the next `get` moves on to the next guess or asks
    /// the user again (telling them the previous answer was wrong). `definite` when the
    /// format said "wrong password" rather than reporting undecodable data.
    pub fn reject(&self, definite: bool) {
        let mut state = self.inner.lock().unwrap();
        if let Some(mut p) = state.current.take() {
            wipe_wide(&mut p);
            if state.current_from_user {
                state.user_rejected = true;
                state.user_failures += 1;
                state.definite_failure |= definite;
            }
        }
    }
}

impl Drop for ArchivePassword {
    fn drop(&mut self) {
        let state = self.inner.get_mut().unwrap();
        if let Some(p) = state.current.as_mut() {
            wipe_wide(p);
        }
        state.guesses.iter_mut().for_each(wipe);
    }
}
