//! Transparent wrap & launch. After gating + config write we run the native CLI
//! with its argv, stdio and exit code passed straight through, so the agent
//! behaves exactly as if it had been run directly. This is the "use it
//! normally" requirement (plan §6).
//!
//! The pieces here serve two callers. An interactive terminal is driven by
//! `gh-cli`'s supervisor, which builds a [`PtySession`] and holds the parent
//! terminal in [`RawGuard`]/[`TerminalModeGuard`] while it does. Redirected or
//! piped execution goes to [`launch_inherited`], which needs no pseudo-terminal
//! at all.

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
    #[cfg(windows)]
    job: usize,
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
        #[cfg(windows)]
        if self.job != 0 {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.job as _);
            }
        }
    }
}

#[cfg(not(windows))]
fn pty_command(bin: &Path, args: &[String]) -> Result<CommandBuilder, GhError> {
    let mut cmd = CommandBuilder::new(bin);
    cmd.args(args);
    Ok(cmd)
}

/// A pseudo-console child is started by `CreateProcessW`, which only accepts a
/// PE image as its application: handed a batch file it fails with `%1 is not a
/// valid Win32 application`. Every npm-installed agent on Windows is a
/// `<name>.cmd` wrapper, so run those the way `cmd.exe` itself would —
/// matching what `std::process::Command` does for the non-PTY path.
#[cfg(windows)]
fn pty_command(bin: &Path, args: &[String]) -> Result<CommandBuilder, GhError> {
    if !is_batch_file(bin) {
        let mut cmd = CommandBuilder::new(bin);
        cmd.args(args);
        return Ok(cmd);
    }
    if let Some(argument) = cmd_reserved_argument(args) {
        return Err(GhError::other(format!(
            "cannot pass `{argument}` to `{}`: it is a Windows batch wrapper, and the \
             command interpreter would interpret `{}` instead of forwarding it. Install a \
             native build of the agent, or give this argument inside the agent instead.",
            bin.display(),
            CMD_RESERVED.iter().collect::<String>(),
        )));
    }
    // `/d` skips AutoRun commands, `/e:ON` keeps command extensions on (npm's
    // wrappers need them), and `/v:OFF` leaves `!` literal.
    let comspec =
        std::env::var_os("ComSpec").unwrap_or_else(|| std::ffi::OsString::from("cmd.exe"));
    let mut cmd = CommandBuilder::new(comspec);
    // A quoted batch path as the first token after `/c` triggers cmd's
    // outer-quote stripping once argv contains additional quotes. Prefix with
    // CALL so the path's quotes survive (e.g. an npm prefix containing spaces).
    // CALL reparses its arguments; cmd_reserved_argument above rejects the
    // expansion and control characters that could change on that second pass.
    cmd.args(["/d", "/e:ON", "/v:OFF", "/c", "call"]);
    cmd.arg(bin);
    cmd.args(args);
    Ok(cmd)
}

#[cfg(windows)]
fn is_batch_file(bin: &Path) -> bool {
    bin.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
    })
}

/// What `cmd.exe` acts on rather than forwards. Everything else — spaces,
/// quotes, backslashes — survives the interpreter byte for byte: the wrapper
/// passes `%*` on unchanged, and the agent parses it back with the same rules
/// the arguments were quoted under.
#[cfg(windows)]
const CMD_RESERVED: &[char] = &['%', '&', '|', '<', '>', '^', '(', ')', '\r', '\n'];

#[cfg(windows)]
fn cmd_reserved_argument(args: &[String]) -> Option<&String> {
    args.iter().find(|arg| arg.contains(CMD_RESERVED))
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
        let mut cmd = pty_command(bin, args)?;
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
        // Losing tree-kill is worth a warning, not a dead CLI: without the job
        // object a descendant of the harness can outlive it, but the harness
        // itself is still supervised and still killable.
        #[cfg(windows)]
        let job = match create_kill_on_close_job(child.process_id()) {
            Ok(job) => job,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "continuing without a Windows Job Object; processes the harness spawns will not be terminated as a group"
                );
                0
            }
        };
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
            #[cfg(windows)]
            job,
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
        #[cfg(windows)]
        if self.job != 0
            && unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job as _, 1)
            } != 0
        {
            return Ok(());
        }
        self.child
            .lock()
            .unwrap()
            .kill()
            .map_err(|e| GhError::other(format!("terminating child: {e}")))
    }
}

/// Attach the ConPTY child to a kill-on-close job object, so everything it
/// spawns dies with it.
///
/// The job is necessarily created after `spawn_command`, because that is when
/// the process id first exists: a descendant spawned inside that window escapes
/// it. Closing the gap needs `PROC_THREAD_ATTRIBUTE_JOB_LIST` on the child's
/// startup attributes, which `portable-pty` does not expose.
#[cfg(windows)]
fn create_kill_on_close_job(process_id: Option<u32>) -> Result<usize, GhError> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };
    let process_id =
        process_id.ok_or_else(|| GhError::other("ConPTY child did not expose a process id"))?;
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(GhError::other(format!(
            "creating Windows Job Object: {}",
            std::io::Error::last_os_error()
        )));
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    };
    let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, process_id) };
    let assigned = !process.is_null() && unsafe { AssignProcessToJobObject(job, process) } != 0;
    if !process.is_null() {
        unsafe { CloseHandle(process) };
    }
    if configured == 0 || !assigned {
        unsafe { CloseHandle(job) };
        return Err(GhError::other(format!(
            "attaching ConPTY child to Windows Job Object: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(job as usize)
}

/// Run `bin` with `args`, injecting `extra_env` (e.g. the Codex inference JWT),
/// on the parent's own stdio. Blocks until the child exits and returns its exit
/// code.
///
/// No pseudo-terminal is allocated: the only caller is the redirected or piped
/// path, which has no terminal to mirror. An interactive terminal goes to the
/// supervisor instead, which drives a [`PtySession`] directly.
pub fn launch_inherited(
    bin: &Path,
    args: &[String],
    extra_env: &BTreeMap<String, String>,
) -> Result<i32, GhError> {
    let status = std::process::Command::new(bin)
        .args(args)
        .envs(extra_env)
        .status()
        .map_err(|e| GhError::other(format!("spawning {}: {e}", bin.display())))?;
    Ok(status.code().unwrap_or(1))
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

#[cfg(windows)]
pub fn terminal_size() -> (u16, u16) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::GetConsoleScreenBufferInfo;
    let handle = std::io::stdout().as_raw_handle();
    let mut info = unsafe { std::mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } != 0 {
        let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16;
        let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16;
        (rows, cols)
    } else {
        (24, 80)
    }
}

#[cfg(not(any(unix, windows)))]
pub fn terminal_size() -> (u16, u16) {
    (24, 80)
}

#[cfg(unix)]
static RAW_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
static TERMINAL_MODES_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Return the parent terminal to conservative shell-friendly modes. Full-screen
/// children normally undo these themselves, but a crash or forced termination
/// can otherwise leave mouse movement and focus changes arriving as input.
pub const TERMINAL_MODE_RESET: &[u8] = b"\x1b[r\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[<u\x1b[=0u\x1b[>4;0m\x1b[?1l\x1b>\x1b[?1049l\x1b[r\x1b[?25h";

/// ConPTY can leave a Git Bash terminal displaying the child's last frame
/// after the alternate screen is restored. Clear the restored primary screen
/// so the next shell prompt cannot be painted over that stale frame.
#[cfg(windows)]
const WINDOWS_TERMINAL_SURFACE_RESET: &[u8] = b"\x1b[r\x1b[2J\x1b[H\x1b[?25h";

fn reset_terminal_modes(stdout: &mut impl Write) -> std::io::Result<()> {
    stdout.write_all(TERMINAL_MODE_RESET)?;
    #[cfg(windows)]
    stdout.write_all(WINDOWS_TERMINAL_SURFACE_RESET)?;
    stdout.flush()
}

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
        let _ = reset_terminal_modes(&mut stdout);
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

#[cfg(windows)]
pub struct RawGuard {
    input: windows_sys::Win32::Foundation::HANDLE,
    input_mode: u32,
    output: windows_sys::Win32::Foundation::HANDLE,
    output_mode: u32,
}

#[cfg(windows)]
impl RawGuard {
    pub fn enter() -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, SetConsoleMode, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
            ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT,
        };
        let input = std::io::stdin().as_raw_handle();
        let output = std::io::stdout().as_raw_handle();
        let (mut input_mode, mut output_mode) = (0, 0);
        if unsafe { GetConsoleMode(input, &mut input_mode) } == 0
            || unsafe { GetConsoleMode(output, &mut output_mode) } == 0
        {
            return None;
        }
        // Mouse and buffer-resize records are left enabled by default, and each
        // one wakes a waiter on the input handle without ever producing a byte
        // to read. Nothing here consumes them — resizes are polled — so turn
        // them off, matching what `cfmakeraw` gives us on Unix.
        let raw = (input_mode
            & !(ENABLE_ECHO_INPUT
                | ENABLE_LINE_INPUT
                | ENABLE_PROCESSED_INPUT
                | ENABLE_MOUSE_INPUT
                | ENABLE_WINDOW_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        if unsafe { SetConsoleMode(input, raw) } == 0 {
            return None;
        }
        let _ = unsafe { SetConsoleMode(output, output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) };
        Some(Self {
            input,
            input_mode,
            output,
            output_mode,
        })
    }
}

#[cfg(windows)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::SetConsoleMode;
        unsafe {
            SetConsoleMode(self.input, self.input_mode);
            SetConsoleMode(self.output, self.output_mode);
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub struct RawGuard;

#[cfg(not(any(unix, windows)))]
impl RawGuard {
    pub fn enter() -> Option<Self> {
        None
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The piped path is the whole of `launch_inherited`: the child writes to
    /// the parent's own handles and its exit code comes back untranslated.
    #[test]
    fn inherited_launch_passes_through_stdio_and_the_exit_code() {
        let script = "printf out; exit 7";
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::inherited_launch_child_helper",
                "--nocapture",
            ])
            .env("BLUE_INHERITED_LAUNCH_SCRIPT", script)
            .output()
            .unwrap();
        assert!(output.status.success(), "the helper itself should pass");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("out"),
            "the child should have written to the inherited stdout"
        );
    }

    /// Runs inside the child spawned by
    /// `inherited_launch_passes_through_stdio_and_the_exit_code`, whose piped
    /// stdio it inherits in turn.
    #[test]
    fn inherited_launch_child_helper() {
        let Some(script) = std::env::var_os("BLUE_INHERITED_LAUNCH_SCRIPT") else {
            return;
        };
        let code = launch_inherited(
            Path::new("/bin/sh"),
            &["-c".into(), script.to_string_lossy().into_owned()],
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(code, 7);
    }

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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn terminal_reset_clears_the_restored_primary_screen() {
        let mut output = Vec::new();
        reset_terminal_modes(&mut output).unwrap();
        assert!(output.ends_with(b"\x1b[?1049l\x1b[r\x1b[?25h\x1b[r\x1b[2J\x1b[H\x1b[?25h"));
    }

    /// npm installs every agent on Windows as a `.cmd` wrapper, and ConPTY
    /// starts its child through `CreateProcessW`, which rejects one outright.
    #[test]
    fn windows_runs_a_batch_agent_through_the_command_interpreter() {
        let native = pty_command(Path::new(r"C:\agents\codex.exe"), &["--help".into()]).unwrap();
        assert_eq!(
            native.get_argv(),
            &[
                std::ffi::OsString::from(r"C:\agents\codex.exe"),
                std::ffi::OsString::from("--help"),
            ]
        );

        let batch = pty_command(
            Path::new(r"C:\Users\dev\AppData\Roaming\npm\codex.CMD"),
            &["--model".into(), "gpt-5".into()],
        )
        .unwrap();
        let argv = batch.get_argv();
        assert!(Path::new(&argv[0])
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("cmd.exe")));
        assert_eq!(
            &argv[1..],
            &[
                std::ffi::OsString::from("/d"),
                std::ffi::OsString::from("/e:ON"),
                std::ffi::OsString::from("/v:OFF"),
                std::ffi::OsString::from("/c"),
                std::ffi::OsString::from("call"),
                std::ffi::OsString::from(r"C:\Users\dev\AppData\Roaming\npm\codex.CMD"),
                std::ffi::OsString::from("--model"),
                std::ffi::OsString::from("gpt-5"),
            ]
        );
    }

    /// Quotes and spaces survive `cmd.exe` unchanged, so they stay on the
    /// batch path; what it would interpret is refused instead of mangled.
    #[test]
    fn only_arguments_the_interpreter_would_reinterpret_are_refused() {
        let batch = Path::new(r"C:\npm\claude.cmd");
        assert!(pty_command(batch, &[r#"fix the "a b" module"#.into()]).is_ok());
        assert!(pty_command(batch, &["--dir".into(), r"C:\code\app".into()]).is_ok());
        for reserved in ["a&b", "%USERPROFILE%", "a|b", "a>b", "a^b", "(a)", "a\r\nb"] {
            assert!(
                pty_command(batch, &[reserved.into()]).is_err(),
                "{reserved} would not reach the agent intact"
            );
            // A native agent takes the same argument unchanged.
            assert!(pty_command(Path::new(r"C:\npm\claude.exe"), &[reserved.into()]).is_ok());
        }
    }
}
