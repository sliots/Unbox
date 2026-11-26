//! One window for many archives: the first Unbox window of a session owns a named
//! pipe, and every Unbox started after it (several files opened at once, or one more
//! double-click while it works) hands its archive over and exits. The owner extracts
//! them one after another.

use crate::win::wide;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// One archive to extract.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub archive: PathBuf,
    /// Put the content straight into the archive's folder ("解压到当前文件夹").
    #[serde(default)]
    pub here: bool,
    /// Extract into this folder instead of the archive's own.
    #[serde(default)]
    pub to: Option<PathBuf>,
}

const ACCEPTED: u8 = 1;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_PIPE_CONNECTED: u32 = 535;
/// How long the owner may take to take a job on before the sender runs it itself.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

fn pipe_name() -> String {
    let mut session = 0u32;
    unsafe {
        windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(std::process::id(), &mut session);
    }
    let user: String = std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_alphanumeric()).collect();
    format!(r"\\.\pipe\Unbox.Queue.{session}.{user}")
}

pub enum HandOff {
    /// A running Unbox took the job.
    Accepted,
    /// No Unbox owns the queue.
    NoOwner,
    /// The owner is closing or did not answer: run the job here.
    Declined,
}

/// Give `job` to the Unbox that owns the queue.
pub fn hand_off(job: &Job) -> HandOff {
    let name = pipe_name();
    let mut pipe = None;
    for _ in 0..20 {
        match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
            Ok(file) => {
                pipe = Some(file);
                break;
            }
            Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => return HandOff::NoOwner,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => unsafe {
                WaitNamedPipeW(wide(&name).as_ptr(), 500);
            },
            Err(_) => return HandOff::Declined,
        }
    }
    let Some(mut pipe) = pipe else { return HandOff::Declined };
    let Ok(mut line) = serde_json::to_vec(job) else { return HandOff::Declined };
    line.push(b'\n');
    let mut answer = [0u8; 1];
    if pipe.write_all(&line).is_ok() && pipe.flush().is_ok() && pipe.read_exact(&mut answer).is_ok() && answer[0] == ACCEPTED {
        HandOff::Accepted
    } else {
        HandOff::Declined
    }
}

/// Offer of one received job to the UI thread; whichever side moves first wins, so a
/// job is never both taken by the owner and run by its sender.
pub struct Offer {
    state: Mutex<Option<bool>>,
    decided: Condvar,
}

impl Offer {
    /// Called on the UI thread: `take` decides (and enqueues) unless the offer expired.
    pub fn answer(&self, take: impl FnOnce() -> bool) {
        let mut state = self.state.lock().unwrap();
        if state.is_none() {
            *state = Some(take());
            self.decided.notify_all();
        }
    }
}

/// Become the owner of the queue. `false` when another Unbox already owns it.
/// `deliver` hands each received job to the UI thread together with its offer.
pub fn serve(deliver: impl Fn(Job, Arc<Offer>) + Send + 'static) -> bool {
    let name = wide(pipe_name());
    // One instance, reused for every sender; the first-instance flag makes ownership
    // exclusive even when several Unbox processes start at the same moment.
    let pipe: HANDLE = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    if pipe == INVALID_HANDLE_VALUE {
        return false;
    }
    let pipe = pipe as usize;
    std::thread::spawn(move || {
        let pipe = pipe as HANDLE;
        loop {
            let connected =
                unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) } != 0 || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if connected {
                serve_one(pipe, &deliver);
            }
            unsafe {
                DisconnectNamedPipe(pipe);
            }
            if !connected {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
    true
}

fn serve_one(pipe: HANDLE, deliver: &impl Fn(Job, Arc<Offer>)) {
    // Borrow the handle as a file without taking ownership of it.
    let mut file = ManuallyDrop::new(unsafe { File::from_raw_handle(pipe as _) });
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 64 * 1024 {
        match file.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) => line.push(byte[0]),
            _ => return,
        }
    }
    let accepted = match serde_json::from_slice::<Job>(&line) {
        Ok(job) => {
            let offer = Arc::new(Offer { state: Mutex::new(None), decided: Condvar::new() });
            deliver(job, offer.clone());
            let state = offer.state.lock().unwrap();
            let (mut state, _) = offer.decided.wait_timeout_while(state, ACCEPT_TIMEOUT, |s| s.is_none()).unwrap();
            // Expire the offer so a late answer cannot take the job as well.
            *state.get_or_insert(false)
        }
        Err(_) => false,
    };
    let _ = file.write_all(&[if accepted { ACCEPTED } else { 0 }]);
    let _ = file.flush();
}
