use interprocess::local_socket::{prelude::*, GenericFilePath, Stream};
use serde_json::json;
use std::{
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

/// The IPC socket state used to send mpv commands to the running instance.
struct IpcState {
    socket: Option<Stream>,
    pending: Vec<String>,
}

/// A thin wrapper around mpv playback.
/// It owns the active child process, prepares IPC commands, and tracks playback state.
pub struct Player {
    ipc: Arc<Mutex<IpcState>>,
    current_child: Option<Child>,
    generation: u64,
    volume: u8,
    started_at: Option<Instant>,
    paused_at: Option<Instant>,
    paused: bool,
}

impl Player {
    /// Creates a fresh player with default volume and no active mpv process.
    pub fn new() -> Self {
        Self {
            ipc: Arc::new(Mutex::new(IpcState { socket: None, pending: Vec::new() })),
            current_child: None,
            generation: 0,
            volume: 85,
            started_at: None,
            paused_at: None,
            paused: false,
        }
    }

    /// Returns the IPC socket path that mpv will listen on for control commands.
    fn pipe_path(&self) -> String {
        let id = std::process::id();
        #[cfg(windows)]
        { format!(r"\\.\pipe\songbird-{id}-{}", self.generation) }
        #[cfg(not(windows))]
        { format!("/tmp/songbird-{id}-{}", self.generation) }
    }

    /// Sends a raw mpv IPC command such as pause, volume, or seek.
    fn send_command(&self, command: serde_json::Value) {
        let line = format!("{}\n", json!({ "command": command }));
        let mut state = self.ipc.lock().unwrap();
        if let Some(socket) = state.socket.as_mut() {
            if socket.write_all(line.as_bytes()).is_ok() {
                let _ = socket.flush();
                return;
            }
            state.socket = None;
        }
        state.pending.push(line);
    }

    /// Connects to the IPC socket for the current mpv process.
    fn connect_ipc(&self, pipe_path: String) {
        let ipc = Arc::clone(&self.ipc);
        thread::spawn(move || {
            for _ in 0..30 {
                let Ok(name) = pipe_path.clone().to_fs_name::<GenericFilePath>() else { return; };
                if let Ok(mut socket) = Stream::connect(name) {
                    let mut state = ipc.lock().unwrap();
                    for line in state.pending.drain(..) {
                        let _ = socket.write_all(line.as_bytes());
                    }
                    let _ = socket.flush();
                    state.socket = Some(socket);
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
    }

    /// Starts playback of a file.
    /// The previous player process is stopped first to avoid duplicated audio.
    pub fn play<P: AsRef<Path>>(&mut self, file: P) -> bool {
        self.stop();
        self.generation = self.generation.wrapping_add(1);
        self.started_at = Some(Instant::now());
        self.paused_at = None;
        self.paused = false;

        let pipe_path = self.pipe_path();
        let volume = self.volume;
        let spawned = if cfg!(windows) {
            Command::new("mpv.exe")
                .args(["--no-video", "--really-quiet", "--force-window=no", &format!("--input-ipc-server={pipe_path}"), &format!("--volume={volume}")])
                .arg(file.as_ref())
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
                .spawn()
        } else {
            Command::new("mpv")
                .args(["--no-video", "--really-quiet", "--force-window=no", &format!("--input-ipc-server={pipe_path}"), &format!("--volume={volume}")])
                .arg(file.as_ref())
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
                .spawn()
        };

        let Ok(child) = spawned else {
            self.started_at = None;
            return false;
        };

        self.current_child = Some(child);
        self.connect_ipc(pipe_path);
        true
    }

    /// Toggles pause state by sending a `cycle pause` IPC command to mpv.
    pub fn pause(&mut self) {
        if self.started_at.is_none() { return; }
        self.send_command(json!(["cycle", "pause"]));
        if self.paused {
            if let Some(paused_at) = self.paused_at.take() {
                if let Some(started) = self.started_at.as_mut() {
                    *started += paused_at.elapsed();
                }
            }
        } else {
            self.paused_at = Some(Instant::now());
        }
        self.paused = !self.paused;
    }

    /// Stops the currently playing track and clears the IPC socket state.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.current_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let mut state = self.ipc.lock().unwrap();
        state.socket = None;
        state.pending.clear();
        self.started_at = None;
        self.paused_at = None;
        self.paused = false;
    }

    /// Updates the mpv volume and returns the clamped value.
    pub fn set_volume(&mut self, value: i16) -> u8 {
        self.volume = value.clamp(0, 130) as u8;
        self.send_command(json!(["set_property", "volume", self.volume]));
        self.volume
    }

    pub fn volume(&self) -> u8 { self.volume }
    pub fn paused(&self) -> bool { self.paused }

    /// Returns the elapsed playback time used by the now-playing progress bar.
    pub fn elapsed_seconds(&self) -> f64 {
        let Some(started) = self.started_at else { return 0.0; };
        if self.paused {
            self.paused_at.map(|p| (p - started).as_secs_f64()).unwrap_or(0.0)
        } else {
            started.elapsed().as_secs_f64()
        }
    }

    /// Checks whether the active mpv child has exited.
    /// When it does, the UI can move to the next track automatically.
    pub fn poll_track_end(&mut self) -> bool {
        let Some(child) = self.current_child.as_mut() else { return false; };
        match child.try_wait() {
            Ok(Some(_)) => {
                self.started_at = None;
                self.paused_at = None;
                self.paused = false;
                self.current_child = None;
                let mut state = self.ipc.lock().unwrap();
                state.socket = None;
                state.pending.clear();
                true
            }
            Ok(None) => false,
            Err(_) => {
                self.started_at = None;
                self.paused_at = None;
                self.paused = false;
                self.current_child = None;
                let mut state = self.ipc.lock().unwrap();
                state.socket = None;
                state.pending.clear();
                false
            }
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) { self.stop(); }
}
