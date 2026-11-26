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

    /// Ask the UI for a password. `None` when canceled.
    pub fn request_password(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        if state.canceled {
            return None;
        }
        state.password_pending = true;
        state.password = None;
        drop(state);
        self.send(&WorkerEvent::PasswordRequest);
        let mut state = self.state.lock().unwrap();
        while state.password.is_none() && !state.canceled {
            state = self.changed.wait(state).unwrap();
        }
        state.password_pending = false;
        if state.canceled {
            if let Some(mut p) = state.password.take() {
                wipe(&mut p);
            }
            return None;
        }
        state.password.take()
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

/// Wait for the `start` line written by the UI before touching anything.
pub fn wait_for_start() -> bool {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok()
        && matches!(serde_json::from_str::<WorkerCommand>(line.trim()), Ok(WorkerCommand::Start))
}

/// One password per archive attempt, shared by the open and extract callbacks.
#[derive(Default)]
pub struct ArchivePassword {
    inner: Mutex<(bool, Option<Vec<u16>>)>,
}

impl ArchivePassword {
    pub fn requested(&self) -> bool {
        self.inner.lock().unwrap().0
    }

    /// The UTF-16 password, asking once. `None` when canceled.
    pub fn get(&self) -> Option<Vec<u16>> {
        let mut inner = self.inner.lock().unwrap();
        if inner.1.is_none() {
            inner.0 = true;
            let mut text = control().request_password()?;
            inner.1 = Some(text.encode_utf16().collect());
            wipe(&mut text);
        }
        inner.1.clone()
    }
}

impl Drop for ArchivePassword {
    fn drop(&mut self) {
        if let Some(p) = self.inner.lock().unwrap().1.as_mut() {
            p.iter_mut().for_each(|c| unsafe { std::ptr::write_volatile(c, 0) });
        }
    }
}
