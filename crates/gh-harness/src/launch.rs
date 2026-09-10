//! Transparent wrap & launch. After gating + config write, we `exec` the native
//! CLI under a PTY, forwarding argv, stdin/stdout, terminal size (SIGWINCH), and
//! the child's exit code — so an interactive TUI agent behaves exactly as if run
//! directly. This is the "use it normally" requirement (plan §6).

use std::collections::{BTreeMap, VecDeque};
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use gh_common::GhError;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug)]
pub enum PtyEvent {
    Output(Vec<u8>),
    Closed,
    Error(String),
}

struct OutputQueue {
    events: VecDeque<PtyEvent>,
    bytes: usize,
}

/// A running agent PTY whose I/O and lifetime can be controlled independently.
/// Reader draining starts immediately, so switching to a control screen cannot
/// block a verbose child on a full PTY buffer.
pub struct PtySession {
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    output: Arc<Mutex<OutputQueue>>,
    #[cfg(unix)]
    process_group: Option<libc::pid_t>,
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self
            .child
            .get_mut()
            .ok()
            .and_then(|child| child.try_wait().ok())
            .flatten()
            .is_none()
        {
            if let Ok(child) = self.child.get_mut() {
                let _ = child.kill();
            }
        }
    }
}

impl PtySession {
    pub fn spawn(
        bin: &Path,
        args: &[String],
        extra_env: &BTreeMap<String, String>,
        rows: u16,
        cols: u16,
    ) -> Result<Self, GhError> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| GhError::other(format!("openpty: {e}")))?;
        let mut cmd = CommandBuilder::new(bin);
        cmd.args(args);
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        for (k, v) in std::env::vars_os() {
            cmd.env(k, v);
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| GhError::other(format!("spawning {}: {e}", bin.display())))?;
        drop(pair.slave);
        #[cfg(unix)]
        let process_group = pair.master.process_group_leader();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| GhError::other(format!("pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| GhError::other(format!("pty writer: {e}")))?;
        let master = Arc::new(Mutex::new(pair.master));
        let output = Arc::new(Mutex::new(OutputQueue {
            events: VecDeque::new(),
            bytes: 0,
        }));
        let output_thread = output.clone();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                let event = match reader.read(&mut buffer) {
                    Ok(0) => PtyEvent::Closed,
                    Ok(n) => PtyEvent::Output(buffer[..n].to_vec()),
                    Err(error) => PtyEvent::Error(error.to_string()),
                };
                let done = !matches!(event, PtyEvent::Output(_));
                let mut queue = output_thread.lock().unwrap();
                if let PtyEvent::Output(bytes) = &event {
                    queue.bytes += bytes.len();
                }
                queue.events.push_back(event);
                while queue.bytes > OUTPUT_LIMIT {
                    match queue.events.pop_front() {
                        Some(PtyEvent::Output(bytes)) => queue.bytes -= bytes.len(),
                        Some(other) => queue.events.push_front(other),
                        None => break,
                    }
                }
                drop(queue);
                if done {
                    break;
                }
            }
        });
        Ok(Self {
            master,
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            output,
            #[cfg(unix)]
            process_group,
        })
    }

    pub fn write_input(&self, bytes: &[u8]) -> Result<(), GhError> {
        let mut writer = self.writer.lock().unwrap();
        writer
            .write_all(bytes)
            .and_then(|_| writer.flush())
            .map_err(|e| GhError::other(format!("pty input: {e}")))
    }

    pub fn drain_events(&self) -> Vec<PtyEvent> {
        let mut queue = self.output.lock().unwrap();
        queue.bytes = 0;
        queue.events.drain(..).collect()
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), GhError> {
        self.master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| GhError::other(format!("resizing pty: {e}")))
    }

    pub fn try_wait(&self) -> Result<Option<i32>, GhError> {
        self.child
            .lock()
            .unwrap()
            .try_wait()
            .map(|status| status.map(|status| status.exit_code() as i32))
            .map_err(|e| GhError::other(format!("polling child: {e}")))
    }

    pub fn wait(&self) -> Result<i32, GhError> {
        self.child
            .lock()
            .unwrap()
            .wait()
            .map(|status| status.exit_code() as i32)
            .map_err(|e| GhError::other(format!("waiting for child: {e}")))
    }

    pub fn terminate(&self) -> Result<(), GhError> {
        #[cfg(unix)]
        if let Some(group) = self.process_group {
            if unsafe { libc::kill(-group, libc::SIGTERM) } == 0 {
                return Ok(());
            }
        }
        self.force_kill()
    }

    pub fn force_kill(&self) -> Result<(), GhError> {
        self.child
            .lock()
            .unwrap()
            .kill()
            .map_err(|e| GhError::other(format!("terminating child: {e}")))
    }
}

/// Launch `bin` with `args`, injecting `extra_env` (e.g. the Codex inference JWT).
/// Blocks until the child exits and returns its exit code.
pub fn launch(
    bin: &Path,
    args: &[String],
    extra_env: &BTreeMap<String, String>,
) -> Result<i32, GhError> {
    let (rows, cols) = terminal_size();
    let session = Arc::new(PtySession::spawn(bin, args, extra_env, rows, cols)?);

    // Put the *parent* terminal in raw mode so keystrokes (incl. Ctrl-C/Z)
    // pass through as bytes to the child's line discipline. Restored on drop.
    let _raw = RawGuard::enter();
    let _terminal_modes = TerminalModeGuard::enter();
    install_winch_handler();
    spawn_resize_thread(session.clone());

    // stdin → child (detached; the process exits when we return).
    let input_session = session.clone();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if input_session.write_input(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // child → stdout (main thread). Ends at EOF when the child closes the pty.
    let mut stdout = std::io::stdout();
    let mut output_closed = false;
    let mut exit_code = None;
    loop {
        for event in session.drain_events() {
            match event {
                PtyEvent::Output(bytes) => {
                    if stdout.write_all(&bytes).is_err() || stdout.flush().is_err() {
                        break;
                    }
                }
                PtyEvent::Closed | PtyEvent::Error(_) => output_closed = true,
            }
        }
        if exit_code.is_none() {
            exit_code = session.try_wait()?;
        }
        if output_closed {
            if let Some(code) = exit_code {
                return Ok(code);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// ---- terminal helpers (unix) ----

#[cfg(unix)]
pub fn terminal_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 {
            (ws.ws_row, ws.ws_col)
        } else {
            (24, 80)
        }
    }
}

#[cfg(not(unix))]
pub fn terminal_size() -> (u16, u16) {
    (24, 80)
}

#[cfg(unix)]
static RESIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
static RAW_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
static TERMINAL_MODES_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Return the parent terminal to conservative shell-friendly modes. Full-screen
/// children normally undo these themselves, but a crash or forced termination
/// can otherwise leave mouse movement and focus changes arriving as input.
pub const TERMINAL_MODE_RESET: &[u8] = b"\x1b[r\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[<u\x1b[=0u\x1b[>4;0m\x1b[?1l\x1b>\x1b[?1049l\x1b[r\x1b[?25h";

#[cfg(unix)]
static mut SAVED_TERMIOS: std::mem::MaybeUninit<libc::termios> = std::mem::MaybeUninit::uninit();

#[cfg(unix)]
extern "C" fn restore_on_signal(signal: libc::c_int) {
    if TERMINAL_MODES_ACTIVE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                TERMINAL_MODE_RESET.as_ptr().cast(),
                TERMINAL_MODE_RESET.len(),
            );
        }
    }
    if RAW_ACTIVE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        unsafe {
            let saved = std::ptr::addr_of!(SAVED_TERMIOS).cast::<libc::termios>();
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
        }
    }
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Restores terminal-emulator modes independently of termios raw mode.
pub struct TerminalModeGuard {
    active: bool,
}

impl TerminalModeGuard {
    pub fn enter() -> Option<Self> {
        if !std::io::stdout().is_terminal() {
            return None;
        }
        #[cfg(unix)]
        TERMINAL_MODES_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        Some(Self { active: true })
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        #[cfg(unix)]
        TERMINAL_MODES_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(TERMINAL_MODE_RESET);
        let _ = stdout.flush();
        self.active = false;
    }
}

#[cfg(unix)]
fn install_restore_handlers() {
    unsafe {
        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
            libc::signal(signal, restore_on_signal as *const () as libc::sighandler_t);
        }
    }
}

#[cfg(unix)]
extern "C" fn on_winch(_sig: libc::c_int) {
    RESIZED.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(unix)]
fn install_winch_handler() {
    unsafe {
        libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
fn install_winch_handler() {}

#[cfg(unix)]
fn spawn_resize_thread(session: Arc<PtySession>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(150));
        if RESIZED.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let (rows, cols) = terminal_size();
            let _ = session.resize(rows, cols);
        }
    });
}

#[cfg(not(unix))]
fn spawn_resize_thread(_session: Arc<PtySession>) {}

/// RAII raw-mode guard for the parent terminal.
#[cfg(unix)]
pub struct RawGuard {
    fd: libc::c_int,
    orig: libc::termios,
}

#[cfg(unix)]
impl RawGuard {
    pub fn enter() -> Option<Self> {
        unsafe {
            let fd = libc::STDIN_FILENO;
            if libc::isatty(fd) == 0 {
                return None; // not a tty (piped) — leave modes alone
            }
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut orig) != 0 {
                return None;
            }
            let mut raw = orig;
            libc::cfmakeraw(&mut raw);
            std::ptr::write(std::ptr::addr_of_mut!(SAVED_TERMIOS).cast(), orig);
            install_restore_handlers();
            RAW_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                RAW_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
                return None;
            }
            Some(RawGuard { fd, orig })
        }
    }
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        RAW_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
        }
    }
}

#[cfg(not(unix))]
pub struct RawGuard;

#[cfg(not(unix))]
impl RawGuard {
    pub fn enter() -> Option<Self> {
        None
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn terminal_reset_unwinds_enhanced_keyboard_mode() {
        let pop = TERMINAL_MODE_RESET
            .windows(b"\x1b[<u".len())
            .position(|window| window == b"\x1b[<u")
            .expect("terminal reset should pop Kitty keyboard mode");
        let clear = TERMINAL_MODE_RESET
            .windows(b"\x1b[=0u".len())
            .position(|window| window == b"\x1b[=0u")
            .expect("terminal reset should clear enhanced keyboard flags");
        assert!(pop < clear);
    }

    #[test]
    fn controllable_session_forwards_input_output_resize_and_exit() {
        let args = vec![
            "-c".to_owned(),
            "printf ready; IFS= read -r value; printf ':%s' \"$value\"".to_owned(),
        ];
        let session =
            PtySession::spawn(Path::new("/bin/sh"), &args, &BTreeMap::new(), 20, 60).unwrap();
        session.resize(19, 72).unwrap();
        session.write_input(b"hello\n").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut output = Vec::new();
        let mut code = None;
        while std::time::Instant::now() < deadline {
            for event in session.drain_events() {
                if let PtyEvent::Output(bytes) = event {
                    output.extend(bytes);
                }
            }
            code = code.or(session.try_wait().unwrap());
            if code.is_some() && String::from_utf8_lossy(&output).contains(":hello") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(code, Some(0));
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("ready"));
        assert!(output.contains(":hello"));
    }
}
