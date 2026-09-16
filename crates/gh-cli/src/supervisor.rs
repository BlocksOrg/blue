use std::io::{Read, Write};
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use gh_harness::{PtyEvent, PtySession, RawGuard, TerminalModeGuard};
use gh_service::{now_unix, Session};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;
use vte::{Params, Perform};

use crate::commands;

const CONTROL: u8 = 0x1d; // Ctrl-]
const INTERRUPT: u8 = 0x03; // Ctrl-C while the control screen is open
const CONTROL_CSI_U: &[u8] = b"\x1b[93;5u";
const CONTROL_XTERM: &[u8] = b"\x1b[27;5;93~";
const SCROLLBACK_ROWS: usize = 2_000;
// Some terminals emit both a legacy cursor sequence and an enhanced keyboard
// report for one physical arrow press. They can arrive far enough apart that a
// short UI debounce still advances twice, especially while a frame is being
// redrawn. Keep a deliberately conservative window for menu navigation; a
// direction change or any non-navigation key still takes effect immediately.
const NAVIGATION_DEBOUNCE: Duration = Duration::from_millis(150);
const REVISION_AVAILABLE_LABEL: &str = "New Blue policy available";
const SESSION_EXPIRED_LABEL: &str = "Session expired — sign in";
/// How long before the inference JWT expires the countdown starts.
const GATEWAY_EXPIRY_LEAD: Duration = Duration::from_secs(10 * 60);

/// The policy field of the status row, plus whatever needs the user's
/// attention right now. Previously the caller string-matched the label to
/// decide whether to show a banner, which silently coupled the two.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PolicyState {
    label: String,
    /// Right-aligned banner; `None` in the ordinary case.
    notice: Option<String>,
}

impl PolicyState {
    fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            notice: None,
        }
    }
}
const BLUE_ASCII: [&str; 5] = [
    " ____  _",
    "| __ )| |_   _  ___",
    "|  _ \\| | | | |/ _ \\",
    "| |_) | | |_| |  __/",
    "|____/|_|\\__,_|\\___|",
];

struct InputReader {
    receiver: mpsc::Receiver<Vec<u8>>,
    #[cfg(any(unix, windows))]
    stop: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(any(unix, windows))]
    handle: Option<std::thread::JoinHandle<()>>,
}

impl InputReader {
    fn spawn() -> Self {
        #[cfg(unix)]
        {
            Self::spawn_from(std::io::stdin(), libc::STDIN_FILENO)
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            let stdin = std::io::stdin();
            let input_handle = stdin.as_raw_handle() as usize;
            let (sender, receiver) = mpsc::channel();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let thread_stop = stop.clone();
            let handle = std::thread::spawn(move || {
                use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
                use windows_sys::Win32::System::Threading::WaitForSingleObject;
                let mut input = stdin;
                let mut bytes = [0u8; 4096];
                while !thread_stop.load(std::sync::atomic::Ordering::Acquire) {
                    if unsafe { WaitForSingleObject(input_handle as _, 20) } != WAIT_OBJECT_0 {
                        continue;
                    }
                    if !console_read_would_yield(input_handle as _) {
                        continue;
                    }
                    match input.read(&mut bytes) {
                        Ok(0) | Err(_) => break,
                        Ok(count) if sender.send(bytes[..count].to_vec()).is_err() => break,
                        Ok(_) => {}
                    }
                }
            });
            Self {
                receiver,
                stop,
                handle: Some(handle),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                let mut stdin = std::io::stdin();
                let mut bytes = [0u8; 4096];
                loop {
                    match stdin.read(&mut bytes) {
                        Ok(0) | Err(_) => break,
                        Ok(count) if sender.send(bytes[..count].to_vec()).is_err() => break,
                        Ok(_) => {}
                    }
                }
            });
            Self { receiver }
        }
    }

    #[cfg(unix)]
    fn spawn_from<R>(mut input: R, fd: libc::c_int) -> Self
    where
        R: Read + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut bytes = [0u8; 4096];
            while !thread_stop.load(std::sync::atomic::Ordering::Acquire) {
                let mut descriptor = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut descriptor, 1, 20) };
                if ready < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if ready == 0 || descriptor.revents & (libc::POLLIN | libc::POLLHUP) == 0 {
                    continue;
                }
                match input.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) if sender.send(bytes[..count].to_vec()).is_err() => break,
                    Ok(_) => {}
                }
            }
        });
        Self {
            receiver,
            stop,
            handle: Some(handle),
        }
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<Vec<u8>, mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }

    fn try_recv(&self) -> Result<Vec<u8>, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    fn stop(&mut self) {
        #[cfg(any(unix, windows))]
        {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            #[cfg(unix)]
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            // Windows deliberately detaches instead of joining. `stop` runs at
            // every control-key interaction, not only on exit, so a reader that
            // does end up parked in the console must never be able to freeze
            // the UI. The thread sees the flag within one wait timeout and
            // exits on its own.
            #[cfg(windows)]
            drop(self.handle.take());
        }
    }
}

/// Whether a blocking read of `handle` will return promptly.
///
/// A console input handle signals for *any* input record — focus changes, mouse
/// movement, buffer resizes, key releases — but the read only returns once a
/// record translates into bytes, so waiting alone is not a readiness signal and
/// a naive read parks indefinitely. Discard the records that produce nothing
/// and let the caller wait again.
///
/// A handle that is not a console (a pipe or a file) reports ready, because
/// there the wait *is* the readiness signal.
#[cfg(windows)]
fn console_read_would_yield(handle: windows_sys::Win32::Foundation::HANDLE) -> bool {
    use windows_sys::Win32::System::Console::{
        GetNumberOfConsoleInputEvents, PeekConsoleInputW, ReadConsoleInputW, INPUT_RECORD,
    };
    loop {
        let mut pending = 0_u32;
        if unsafe { GetNumberOfConsoleInputEvents(handle, &mut pending) } == 0 {
            return true;
        }
        if pending == 0 {
            return false;
        }
        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        let mut peeked = 0_u32;
        if unsafe { PeekConsoleInputW(handle, &mut record, 1, &mut peeked) } == 0 || peeked == 0 {
            return false;
        }
        if produces_input_bytes(&record) {
            return true;
        }
        let mut discarded = 0_u32;
        if unsafe { ReadConsoleInputW(handle, &mut record, 1, &mut discarded) } == 0
            || discarded == 0
        {
            return false;
        }
    }
}

/// Whether the console will translate `record` into bytes on the input handle.
#[cfg(windows)]
fn produces_input_bytes(record: &windows_sys::Win32::System::Console::INPUT_RECORD) -> bool {
    use windows_sys::Win32::System::Console::KEY_EVENT;
    if u32::from(record.EventType) != KEY_EVENT {
        return false;
    }
    let key = unsafe { record.Event.KeyEvent };
    if key.bKeyDown == 0 {
        return false;
    }
    // Modifier keys pressed on their own. Everything else does produce bytes,
    // including arrows and function keys: they carry `UnicodeChar == 0` but
    // arrive as escape sequences under `ENABLE_VIRTUAL_TERMINAL_INPUT`, so
    // testing the character would swallow them.
    const VK_SHIFT: u16 = 0x10;
    const VK_CONTROL: u16 = 0x11;
    const VK_MENU: u16 = 0x12;
    const VK_CAPITAL: u16 = 0x14;
    const VK_LWIN: u16 = 0x5B;
    const VK_RWIN: u16 = 0x5C;
    const VK_NUMLOCK: u16 = 0x90;
    const VK_SCROLL: u16 = 0x91;
    !matches!(
        key.wVirtualKeyCode,
        VK_SHIFT | VK_CONTROL | VK_MENU | VK_CAPITAL | VK_LWIN | VK_RWIN | VK_NUMLOCK | VK_SCROLL
    )
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stop();
    }
}
#[derive(Clone, Copy)]
struct ControlCommand {
    name: &'static str,
    description: &'static str,
}

const COMMANDS: &[ControlCommand] = &[
    ControlCommand {
        name: "/status",
        description: "Policy and managed files",
    },
    ControlCommand {
        name: "/health",
        description: "Service dependency health",
    },
    ControlCommand {
        name: "/version",
        description: "Metaharness version",
    },
    ControlCommand {
        name: "/gateway",
        description: "Gateway account",
    },
    ControlCommand {
        name: "/direct",
        description: "Toggle personal provider credentials",
    },
    ControlCommand {
        name: "/doctor",
        description: "Agent inventory",
    },
    ControlCommand {
        name: "/agent",
        description: "Show or switch coding agent",
    },
    ControlCommand {
        name: "/apply",
        description: "Reconcile now",
    },
    ControlCommand {
        name: "/help",
        description: "Show command help",
    },
    ControlCommand {
        name: "/resume",
        description: "Resume an owned or shared remote session",
    },
    ControlCommand {
        name: "/login",
        description: "Identity and reauthentication",
    },
    ControlCommand {
        name: "/logout",
        description: "Sign out",
    },
    ControlCommand {
        name: "/reset",
        description: "Disconnect and retain tenant state",
    },
    ControlCommand {
        name: "/quit",
        description: "End the agent and exit",
    },
];

#[derive(Default)]
struct ControlEditor {
    input: Input,
    selected: usize,
    history: Vec<String>,
    history_at: usize,
    navigation: NavigationDebounce,
    gateway_available: bool,
}

#[derive(Default)]
struct NavigationDebounce {
    last: Option<(KeyCode, Instant)>,
}

impl NavigationDebounce {
    fn accept(&mut self, key: KeyEvent) -> bool {
        self.accept_at(key, Instant::now())
    }

    fn accept_at(&mut self, key: KeyEvent, now: Instant) -> bool {
        if !matches!(key.code, KeyCode::Up | KeyCode::Down) {
            self.last = None;
            return true;
        }
        if self.last.as_ref().is_some_and(|(previous, at)| {
            *previous == key.code && now.duration_since(*at) < NAVIGATION_DEBOUNCE
        }) {
            return false;
        }
        self.last = Some((key.code, now));
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PromptAction {
    Cancel,
    SelectAgent(String),
    KeepSession,
    KeepModeSession(bool),
    ReloadAgent(String),
    ReloadCurrent,
    Apply,
    Quit,
    Logout,
    Reset,
    Login,
}

#[derive(Clone)]
struct PromptOption {
    label: String,
    hint: String,
    action: PromptAction,
}

struct ControlPrompt {
    title: String,
    options: Vec<PromptOption>,
    selected: usize,
    escape_action: PromptAction,
    navigation: NavigationDebounce,
}

impl ControlPrompt {
    fn new(title: impl Into<String>, options: Vec<PromptOption>) -> Self {
        Self {
            title: title.into(),
            options,
            selected: 0,
            escape_action: PromptAction::Cancel,
            navigation: NavigationDebounce::default(),
        }
    }

    fn escape_as(mut self, action: PromptAction) -> Self {
        self.escape_action = action;
        self
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<PromptAction> {
        if !self.navigation.accept(key) {
            return None;
        }
        match key.code {
            KeyCode::Esc => Some(self.escape_action.clone()),
            KeyCode::Enter => self
                .options
                .get(self.selected)
                .map(|option| option.action.clone()),
            KeyCode::Up if !self.options.is_empty() => {
                self.selected = if self.selected == 0 {
                    self.options.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            KeyCode::Down if !self.options.is_empty() => {
                self.selected = (self.selected + 1) % self.options.len();
                None
            }
            KeyCode::PageUp if !self.options.is_empty() => {
                self.selected = self.selected.saturating_sub(8);
                None
            }
            KeyCode::PageDown if !self.options.is_empty() => {
                self.selected = (self.selected + 8).min(self.options.len() - 1);
                None
            }
            KeyCode::Home if !self.options.is_empty() => {
                self.selected = 0;
                None
            }
            KeyCode::End if !self.options.is_empty() => {
                self.selected = self.options.len() - 1;
                None
            }
            _ => None,
        }
    }
}

fn option(label: &str, hint: &str, action: PromptAction) -> PromptOption {
    PromptOption {
        label: label.to_owned(),
        hint: hint.to_owned(),
        action,
    }
}

fn agent_repair_guidance(names: &[String]) -> Option<String> {
    let first = names.first()?;
    Some(format!(
        "Version repair required for: {}. Run `blue agent {first}` outside Blue to install one.",
        names.join(", ")
    ))
}

fn agent_install_guidance(names: &[String]) -> Option<String> {
    let first = names.first()?;
    let action = if cfg!(windows) {
        "Automatic installation is unavailable on Windows; install a policy-supported version manually and add it to PATH.".to_owned()
    } else {
        format!("Run `blue agent {first}` outside Blue to install one.")
    };
    Some(format!("Not installed: {}. {action}", names.join(", ")))
}

fn agent_selector(options: &commands::AgentOptions) -> Option<ControlPrompt> {
    if options.eligible.is_empty() {
        return None;
    }
    let current = options.current.as_deref();
    let mut selector = ControlPrompt::new(
        "Choose your default coding agent",
        options
            .eligible
            .iter()
            .map(|name| {
                option(
                    name,
                    if current == Some(name) {
                        "current default"
                    } else {
                        "installed and allowed"
                    },
                    PromptAction::SelectAgent(name.clone()),
                )
            })
            .collect(),
    );
    selector.selected = options
        .eligible
        .iter()
        .position(|name| current == Some(name.as_str()))
        .unwrap_or(0);
    Some(selector)
}

fn confirmation_prompt(
    title: &str,
    confirm_label: &str,
    confirm_hint: &str,
    action: PromptAction,
) -> ControlPrompt {
    ControlPrompt::new(
        title,
        vec![
            option("Cancel", "leave everything unchanged", PromptAction::Cancel),
            option(confirm_label, confirm_hint, action),
        ],
    )
}

fn mode_reload_prompt(direct: bool) -> ControlPrompt {
    ControlPrompt::new(
        "Quit and reload the agent with the selected mode?",
        vec![
            option(
                "Keep current session",
                "apply after the next Blue restart",
                PromptAction::KeepModeSession(direct),
            ),
            option(
                "Quit and reload now",
                "gracefully stop the current agent",
                PromptAction::ReloadCurrent,
            ),
        ],
    )
    .escape_as(PromptAction::KeepModeSession(direct))
}

fn prompt_popup_height(option_count: usize, terminal_rows: u16) -> u16 {
    // Status row, outer vertical margins, header, minimum transcript, and the
    // help row consume nine rows. The command input is hidden while prompting.
    let available = terminal_rows.saturating_sub(9).max(3);
    ((option_count.saturating_mul(2) + 2) as u16).min(available)
}

enum ResumeStep {
    Sessions,
    Destination,
    CustomPath,
    Loading,
    RepositoryMismatch,
    Confirm,
    Failed,
}

enum ResumeEffect {
    None,
    Cancel,
    Prepare {
        session: Box<commands::RemoteSession>,
        destination: std::path::PathBuf,
    },
    Finish,
}

#[derive(Clone, Copy)]
enum DestinationChoice {
    Current,
    Recorded,
    Custom,
    Cancel,
}

struct ResumeWizard {
    sessions: Vec<commands::RemoteSession>,
    current: std::path::PathBuf,
    step: ResumeStep,
    selected: usize,
    selected_session: usize,
    destination: Option<std::path::PathBuf>,
    path_input: Input,
    error: Option<String>,
    prepared: Option<commands::PreparedRemoteSession>,
    navigation: NavigationDebounce,
}

impl ResumeWizard {
    fn new(sessions: Vec<commands::RemoteSession>) -> Result<Self> {
        Ok(Self {
            sessions,
            current: std::env::current_dir().context("reading current directory")?,
            step: ResumeStep::Sessions,
            selected: 0,
            selected_session: 0,
            destination: None,
            path_input: Input::default(),
            error: None,
            prepared: None,
            navigation: NavigationDebounce::default(),
        })
    }

    fn session(&self) -> &commands::RemoteSession {
        &self.sessions[self.selected_session]
    }

    fn recorded(&self) -> Option<std::path::PathBuf> {
        self.session().cwd.as_deref().map(std::path::PathBuf::from)
    }

    fn destination_choices(&self) -> Vec<DestinationChoice> {
        let mut choices = vec![DestinationChoice::Current];
        if self
            .recorded()
            .is_some_and(|path| path != self.current && path.is_dir())
        {
            choices.push(DestinationChoice::Recorded);
        }
        choices.extend([DestinationChoice::Custom, DestinationChoice::Cancel]);
        choices
    }

    fn prepare(&mut self, destination: std::path::PathBuf) -> ResumeEffect {
        self.destination = Some(destination.clone());
        self.error = None;
        self.step = ResumeStep::Loading;
        ResumeEffect::Prepare {
            session: Box::new(self.session().clone()),
            destination,
        }
    }

    fn prepared(&mut self, prepared: commands::PreparedRemoteSession) {
        let mismatch = prepared.repository_mismatch();
        self.prepared = Some(prepared);
        self.selected = 0;
        self.step = if mismatch {
            ResumeStep::RepositoryMismatch
        } else {
            ResumeStep::Confirm
        };
    }

    fn failed(&mut self, error: anyhow::Error) {
        self.error = Some(format!("{error:#}"));
        self.selected = 0;
        self.step = ResumeStep::Failed;
    }

    fn take_prepared(&mut self) -> Option<commands::PreparedRemoteSession> {
        self.prepared.take()
    }

    fn move_selection(&mut self, key: KeyEvent, count: usize) {
        if count == 0 || !self.navigation.accept(key) {
            return;
        }
        match key.code {
            KeyCode::Up => {
                self.selected = if self.selected == 0 {
                    count - 1
                } else {
                    self.selected - 1
                }
            }
            KeyCode::Down => self.selected = (self.selected + 1) % count,
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(8),
            KeyCode::PageDown => self.selected = (self.selected + 8).min(count - 1),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = count - 1,
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> ResumeEffect {
        match self.step {
            ResumeStep::Sessions => match key.code {
                KeyCode::Esc => ResumeEffect::Cancel,
                KeyCode::Enter => {
                    self.selected_session = self.selected;
                    let recorded = self.recorded();
                    if recorded.is_none() || recorded.as_deref() == Some(self.current.as_path()) {
                        self.prepare(self.current.clone())
                    } else {
                        self.selected = 0;
                        self.step = ResumeStep::Destination;
                        ResumeEffect::None
                    }
                }
                _ => {
                    self.move_selection(key, self.sessions.len());
                    ResumeEffect::None
                }
            },
            ResumeStep::Destination => match key.code {
                KeyCode::Esc => {
                    self.selected = self.selected_session;
                    self.step = ResumeStep::Sessions;
                    ResumeEffect::None
                }
                KeyCode::Enter => match self.destination_choices()[self.selected] {
                    DestinationChoice::Current => self.prepare(self.current.clone()),
                    DestinationChoice::Recorded => self.prepare(
                        self.recorded()
                            .expect("recorded destination option has a path"),
                    ),
                    DestinationChoice::Custom => {
                        self.path_input.reset();
                        self.error = None;
                        self.step = ResumeStep::CustomPath;
                        ResumeEffect::None
                    }
                    DestinationChoice::Cancel => ResumeEffect::Cancel,
                },
                _ => {
                    let count = self.destination_choices().len();
                    self.move_selection(key, count);
                    ResumeEffect::None
                }
            },
            ResumeStep::CustomPath => match key.code {
                KeyCode::Esc => {
                    self.error = None;
                    self.selected = self
                        .destination_choices()
                        .iter()
                        .position(|choice| matches!(choice, DestinationChoice::Custom))
                        .unwrap_or(0);
                    self.step = ResumeStep::Destination;
                    ResumeEffect::None
                }
                KeyCode::Enter => {
                    let path = std::path::PathBuf::from(self.path_input.value().trim());
                    if path.is_dir() {
                        self.prepare(path)
                    } else {
                        self.error = Some(
                            "Destination must be an existing directory. Correct the path and try again."
                                .into(),
                        );
                        ResumeEffect::None
                    }
                }
                _ => {
                    self.path_input.handle_event(&Event::Key(key));
                    self.error = None;
                    ResumeEffect::None
                }
            },
            ResumeStep::Loading => ResumeEffect::None,
            ResumeStep::RepositoryMismatch => match key.code {
                KeyCode::Esc => ResumeEffect::Cancel,
                KeyCode::Enter if self.selected == 0 => ResumeEffect::Cancel,
                KeyCode::Enter => {
                    self.selected = 0;
                    self.step = ResumeStep::Confirm;
                    ResumeEffect::None
                }
                _ => {
                    self.move_selection(key, 2);
                    ResumeEffect::None
                }
            },
            ResumeStep::Confirm => match key.code {
                KeyCode::Esc => ResumeEffect::Cancel,
                KeyCode::Enter if self.selected == 0 => ResumeEffect::Cancel,
                KeyCode::Enter => ResumeEffect::Finish,
                _ => {
                    self.move_selection(key, 2);
                    ResumeEffect::None
                }
            },
            ResumeStep::Failed => match key.code {
                KeyCode::Esc => ResumeEffect::Cancel,
                KeyCode::Enter if self.selected == 0 => {
                    self.selected = 0;
                    self.error = None;
                    self.step = ResumeStep::Destination;
                    ResumeEffect::None
                }
                KeyCode::Enter => ResumeEffect::Cancel,
                _ => {
                    self.move_selection(key, 2);
                    ResumeEffect::None
                }
            },
        }
    }
}

impl ControlEditor {
    fn value(&self) -> &str {
        self.input.value()
    }

    fn set_value(&mut self, value: String) {
        self.input = Input::new(value);
    }

    fn clear(&mut self) {
        self.input.reset();
        self.selected = 0;
        self.history_at = self.history.len();
        self.navigation = NavigationDebounce::default();
    }

    fn suggestions(&self) -> Vec<ControlCommand> {
        command_suggestions(self.value(), self.gateway_available)
    }

    fn selected_suggestion(&self) -> Option<ControlCommand> {
        let suggestions = self.suggestions();
        suggestions
            .get(self.selected.min(suggestions.len().saturating_sub(1)))
            .copied()
    }

    fn complete(&mut self) {
        if let Some(command) = self.selected_suggestion() {
            self.set_value(command.name.to_owned());
        }
    }

    fn submit(&mut self) -> Option<String> {
        let typed = self.value().trim();
        let command = if command_available(typed, self.gateway_available)
            || typed.contains(char::is_whitespace)
        {
            typed.to_owned()
        } else {
            self.selected_suggestion()
                .map(|command| command.name.to_owned())
                .unwrap_or_else(|| typed.to_owned())
        };
        if command.is_empty() {
            return None;
        }
        self.history.push(command.clone());
        self.clear();
        Some(command)
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        if !self.navigation.accept(key) {
            return None;
        }
        let suggestions = self.suggestions();
        match key.code {
            KeyCode::Enter => return self.submit(),
            KeyCode::Tab => self.complete(),
            KeyCode::Up if !suggestions.is_empty() => {
                self.selected = if self.selected == 0 {
                    suggestions.len() - 1
                } else {
                    self.selected - 1
                };
            }
            KeyCode::Down if !suggestions.is_empty() => {
                self.selected = (self.selected + 1) % suggestions.len();
            }
            KeyCode::Up if !self.history.is_empty() => {
                self.history_at = self
                    .history_at
                    .saturating_sub(1)
                    .min(self.history.len() - 1);
                self.set_value(self.history[self.history_at].clone());
            }
            KeyCode::Down if self.history_at < self.history.len() => {
                self.history_at += 1;
                let value = self
                    .history
                    .get(self.history_at)
                    .cloned()
                    .unwrap_or_default();
                self.set_value(value);
            }
            KeyCode::Esc => {
                self.input.reset();
                self.selected = 0;
            }
            _ => {
                self.input.handle_event(&Event::Key(key));
                self.selected = 0;
            }
        }
        None
    }
}

fn command_available(name: &str, gateway_available: bool) -> bool {
    COMMANDS
        .iter()
        .any(|command| command.name == name && (name != "/direct" || gateway_available))
}

fn command_suggestions(input: &str, gateway_available: bool) -> Vec<ControlCommand> {
    let query = input.trim();
    if !query.starts_with('/') || query.contains(char::is_whitespace) {
        return Vec::new();
    }
    let needle = query.trim_start_matches('/').to_ascii_lowercase();
    let mut commands = COMMANDS
        .iter()
        .copied()
        .filter(|command| command.name != "/direct" || gateway_available)
        .filter(|command| command.name[1..].contains(&needle))
        .collect::<Vec<_>>();
    commands.sort_by_key(|command| !command.name[1..].starts_with(&needle));
    commands
}

fn slash_command_with_arguments(command: &str, gateway_available: bool) -> Option<&str> {
    let (name, _) = command.split_once(char::is_whitespace)?;
    COMMANDS
        .iter()
        .any(|item| item.name == name && (item.name != "/direct" || gateway_available))
        .then_some(name)
}

pub enum SupervisorExit {
    Child(i32),
    Restart,
    Switch(String),
    Resume(commands::RestoredSession),
}

fn find_control_chord(input: &[u8]) -> Option<(usize, usize)> {
    let legacy = input
        .iter()
        .position(|byte| *byte == CONTROL)
        .map(|at| (at, 1));
    let encoded = [CONTROL_CSI_U, CONTROL_XTERM]
        .into_iter()
        .filter_map(|sequence| {
            input
                .windows(sequence.len())
                .position(|window| window == sequence)
                .map(|at| (at, sequence.len()))
        })
        .min_by_key(|(at, _)| *at);
    match (legacy, encoded) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(chord), None) | (None, Some(chord)) => Some(chord),
        (None, None) => None,
    }
}

fn trailing_control_prefix(input: &[u8]) -> usize {
    let sequences = [CONTROL_CSI_U, CONTROL_XTERM];
    (1..CONTROL_XTERM.len())
        .filter(|length| {
            input.len() >= *length
                && sequences.iter().any(|sequence| {
                    *length < sequence.len()
                        && input[input.len() - *length..] == sequence[..*length]
                })
        })
        .max()
        .unwrap_or(0)
}

fn decode_kitty_key(body: &[u8]) -> Option<Option<Vec<u8>>> {
    let body = std::str::from_utf8(body).ok()?;
    let mut parameters = body.split(';');
    let code = parameters.next()?.split(':').next()?.parse::<u32>().ok()?;
    let modifier_event = parameters.next().unwrap_or("1");
    let mut modifier_event = modifier_event.split(':');
    let modifiers = modifier_event.next()?.parse::<u16>().ok()?;
    let event = modifier_event
        .next()
        .and_then(|event| event.parse::<u8>().ok())
        .unwrap_or(1);
    if matches!(event, 2 | 3) {
        return Some(None); // key repeat/release
    }
    if code == 93 && modifiers.saturating_sub(1) & 4 != 0 {
        return Some(Some(vec![CONTROL]));
    }
    if modifiers.saturating_sub(1) & 4 != 0 {
        let control = if (b'a' as u32..=b'z' as u32).contains(&code) {
            Some((code as u8) - b'a' + 1)
        } else if (b'A' as u32..=b'Z' as u32).contains(&code) {
            Some((code as u8) - b'A' + 1)
        } else {
            None
        };
        if let Some(control) = control {
            return Some(Some(vec![control]));
        }
    }
    let navigation = match code {
        57349 => Some(b"\x1b[3~".as_slice()),
        57350 => Some(b"\x1b[D".as_slice()),
        57351 => Some(b"\x1b[C".as_slice()),
        57352 => Some(b"\x1b[A".as_slice()),
        57353 => Some(b"\x1b[B".as_slice()),
        57354 => Some(b"\x1b[5~".as_slice()),
        57355 => Some(b"\x1b[6~".as_slice()),
        57356 => Some(b"\x1b[H".as_slice()),
        57357 => Some(b"\x1b[F".as_slice()),
        _ => None,
    };
    if let Some(navigation) = navigation {
        return Some(Some(navigation.to_vec()));
    }
    let character = char::from_u32(code)?;
    let mut encoded = [0u8; 4];
    Some(Some(
        character.encode_utf8(&mut encoded).as_bytes().to_vec(),
    ))
}

/// Convert enhanced keyboard reports left enabled by the child TUI into the
/// ordinary bytes consumed by the control-screen line editor. In particular,
/// release events must disappear rather than leaking their CSI parameters.
fn decode_control_input(pending: &mut Vec<u8>, input: &[u8]) -> Vec<u8> {
    pending.extend_from_slice(input);
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < pending.len() {
        if pending[cursor] != 0x1b {
            output.push(pending[cursor]);
            cursor += 1;
            continue;
        }
        if cursor + 1 >= pending.len() {
            break;
        }
        if pending[cursor + 1] == b'O' {
            if cursor + 2 >= pending.len() {
                break;
            }
            if matches!(pending[cursor + 2], b'A' | b'B' | b'C' | b'D' | b'H' | b'F') {
                output.extend_from_slice(&[0x1b, b'[', pending[cursor + 2]]);
                cursor += 3;
                continue;
            }
        }
        if pending[cursor + 1] != b'[' {
            output.push(pending[cursor]);
            cursor += 1;
            continue;
        }
        let Some(relative_end) = pending[cursor + 2..]
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))
        else {
            break;
        };
        let end = cursor + 2 + relative_end;
        let final_byte = pending[end];
        let body = &pending[cursor + 2..end];
        match final_byte {
            b'u' => {
                if let Some(Some(bytes)) = decode_kitty_key(body) {
                    output.extend(bytes);
                }
            }
            b'A' if body.is_empty() || body.starts_with(b"1;") => {
                output.extend_from_slice(b"\x1b[A")
            }
            b'B' if body.is_empty() || body.starts_with(b"1;") => {
                output.extend_from_slice(b"\x1b[B")
            }
            b'C' if body.is_empty() || body.starts_with(b"1;") => {
                output.extend_from_slice(b"\x1b[C")
            }
            b'D' if body.is_empty() || body.starts_with(b"1;") => {
                output.extend_from_slice(b"\x1b[D")
            }
            b'H' if body.is_empty() || body.starts_with(b"1;") => {
                output.extend_from_slice(b"\x1b[H")
            }
            b'F' if body.is_empty() || body.starts_with(b"1;") => {
                output.extend_from_slice(b"\x1b[F")
            }
            b'~' if body == b"3" => output.extend_from_slice(b"\x1b[3~"),
            b'~' if body == b"5" => output.extend_from_slice(b"\x1b[5~"),
            b'~' if body == b"6" => output.extend_from_slice(b"\x1b[6~"),
            b'~' if body == b"27;5;93" => output.push(CONTROL),
            _ => {} // ignore terminal reports and unsupported navigation keys
        }
        cursor = end + 1;
    }
    pending.drain(..cursor);
    output
}

pub struct StartupView {
    agent: String,
    agent_status: String,
    connection: String,
    identity: String,
    policy: String,
    activity: String,
    drawn: bool,
}

impl StartupView {
    pub fn new(agent: &str) -> Result<Self> {
        let mut view = Self {
            agent: agent.to_owned(),
            agent_status: "checking…".into(),
            connection: "checking…".into(),
            identity: "checking…".into(),
            policy: "checking…".into(),
            activity: "Preparing your workspace…".into(),
            drawn: false,
        };
        view.draw()?;
        Ok(view)
    }

    pub fn connected(&mut self, source: impl Into<String>) -> Result<()> {
        self.connection = format!("connected · {}", source.into());
        self.draw()
    }

    pub fn authenticated(&mut self, identity: impl Into<String>) -> Result<()> {
        self.identity = format!("signed in · {}", identity.into());
        self.draw()
    }

    pub fn policy(&mut self, revision: impl Into<String>, current: bool) -> Result<()> {
        let revision = revision.into();
        let short_revision = revision.chars().take(8).collect::<String>();
        self.policy = format!(
            "{} · {}",
            if current { "current" } else { "reconciling…" },
            short_revision,
        );
        self.draw()
    }

    pub fn ready(&mut self) -> Result<()> {
        self.agent_status = "ready".into();
        self.activity = format!(
            "Starting {}…  Hold Ctrl and press ] for controls.",
            self.agent
        );
        self.draw()
    }

    fn draw(&mut self) -> Result<()> {
        let frame = self.frame(std::env::var_os("NO_COLOR").is_none());
        let mut stdout = std::io::stdout();
        if self.drawn {
            stdout.write_all(b"\x1b[H")?;
        } else {
            stdout.write_all(b"\x1b[2J\x1b[H")?;
            self.drawn = true;
        }
        stdout.write_all(frame.as_bytes())?;
        stdout.flush()?;
        Ok(())
    }

    fn frame(&self, color: bool) -> String {
        let accent = |value: &str| {
            if color {
                format!("\x1b[36m{value}\x1b[0m")
            } else {
                value.to_owned()
            }
        };
        let agent = format!("{} · {}", self.agent, self.agent_status);
        let mut lines = vec![String::new(), String::new()];
        lines.extend(BLUE_ASCII.map(|line| format!("  {}", accent(line))));
        lines.extend([
            format!("  Metaharness v{}", gh_common::blue_version()),
            String::new(),
            format!("  {:<12} {}", "Connection", self.connection),
            format!("  {:<12} {}", "Identity", self.identity),
            format!("  {:<12} {}", "Policy", self.policy),
            format!("  {:<12} {}", "Agent", agent),
            String::new(),
            format!("  {}", accent(&self.activity)),
        ]);
        lines
            .into_iter()
            .map(|line| format!("\r\x1b[2K{line}\r\n"))
            .collect()
    }
}

fn footer(
    agent: &str,
    policy: &PolicyState,
    gateway: &str,
    connectivity: &str,
    width: usize,
) -> String {
    let policy = &policy.label;
    let full = format!(
        " Ctrl-] Control · {agent} · policy {policy} · gateway {gateway} · control {connectivity}"
    );
    if full.chars().count() <= width {
        return full;
    }
    let compact = format!(" Ctrl-] · {agent} · {policy} · {gateway} · {connectivity}");
    if compact.chars().count() <= width {
        return compact;
    }
    compact.chars().take(width.saturating_sub(1)).collect()
}

fn status_row(left: &str, width: usize, notice: Option<&str>) -> String {
    let Some(notice) = notice else {
        return format!("{left:<width$}");
    };
    let notice_width = notice.chars().count();
    if width <= notice_width {
        return notice.chars().take(width).collect();
    }
    let left_width = width - notice_width;
    let left = left
        .chars()
        .take(left_width.saturating_sub(1))
        .collect::<String>();
    format!("{left:<left_width$}{notice}")
}

fn footer_row(
    agent: &str,
    policy: &PolicyState,
    gateway: &str,
    connectivity: &str,
    width: usize,
    color: bool,
) -> String {
    let label = footer(agent, policy, gateway, connectivity, width);
    let label = status_row(&label, width, policy.notice.as_deref());
    if color {
        format!("\x1b[7m{label}\x1b[0m")
    } else {
        label
    }
}

fn draw_footer(
    stdout: &mut impl Write,
    agent: &str,
    policy: &PolicyState,
    gateway: &str,
    connectivity: &str,
    rows: u16,
    cols: u16,
) -> std::io::Result<()> {
    let color = std::env::var_os("NO_COLOR").is_none();
    write!(stdout, "\x1b7\x1b[{};1H\x1b[2K", rows)?;
    write!(
        stdout,
        "{}",
        footer_row(agent, policy, gateway, connectivity, cols as usize, color)
    )?;
    write!(stdout, "\x1b8")?;
    stdout.flush()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ViewportRepair {
    Full,
    Margins { top: u16, bottom: u16 },
}

#[derive(Clone, Copy)]
struct FooterStatus<'a> {
    agent: &'a str,
    policy: &'a PolicyState,
    gateway: &'a str,
    connectivity: &'a str,
    rows: u16,
    cols: u16,
}

impl<'a> FooterStatus<'a> {
    fn new(
        agent: &'a str,
        policy: &'a PolicyState,
        gateway: &'a str,
        connectivity: &'a str,
        size: (u16, u16),
    ) -> Self {
        Self {
            agent,
            policy,
            gateway,
            connectivity,
            rows: size.0,
            cols: size.1,
        }
    }
}

#[derive(Default)]
struct OutputEffects {
    redraw_footer: bool,
    viewport: Option<ViewportRepair>,
}

impl Perform for OutputEffects {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        if action == 'J' && (intermediates.is_empty() || intermediates == b"?") {
            self.redraw_footer = true;
        }
        if matches!(action, 'h' | 'l')
            && intermediates == b"?"
            && params.iter().any(|param| {
                param
                    .first()
                    .is_some_and(|mode| matches!(mode, 47 | 1047 | 1049))
            })
        {
            self.redraw_footer = true;
            self.viewport = Some(ViewportRepair::Full);
        }
        // Full-screen TUIs such as Codex commonly mount and redraw their frame
        // inside a synchronized-output transaction. Repairing after an early
        // clear or alternate-screen switch is not sufficient because the rest
        // of that transaction can still replace the footer. Repaint only when
        // the complete frame is committed so no Blue bytes are inserted into
        // the child's update.
        if action == 'l'
            && intermediates == b"?"
            && params.iter().any(|param| param.first() == Some(&2026))
        {
            self.redraw_footer = true;
            self.viewport = Some(ViewportRepair::Full);
        }
        if action == 'r' && intermediates.is_empty() {
            let mut params = params.iter().filter_map(|param| param.first().copied());
            self.viewport = Some(ViewportRepair::Margins {
                top: params.next().unwrap_or(0),
                bottom: params.next().unwrap_or(0),
            });
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.redraw_footer = true;
            self.viewport = Some(ViewportRepair::Full);
        }
    }
}

struct OutputObserver {
    parser: vte::Parser,
}

impl Default for OutputObserver {
    fn default() -> Self {
        Self {
            parser: vte::Parser::new(),
        }
    }
}

impl OutputObserver {
    fn advance(&mut self, byte: u8) -> OutputEffects {
        let mut effects = OutputEffects::default();
        self.parser.advance(&mut effects, byte);
        effects
    }
}

fn set_agent_viewport(
    stdout: &mut impl Write,
    rows: u16,
    repair: ViewportRepair,
) -> std::io::Result<()> {
    let content_rows = rows.saturating_sub(1).max(1);
    let (top, bottom) = match repair {
        ViewportRepair::Full => (1, content_rows),
        ViewportRepair::Margins { top, bottom } => {
            if bottom != 0 && bottom <= content_rows {
                return Ok(());
            }
            let top = top.max(1).min(content_rows);
            let bottom = if bottom == 0 {
                content_rows
            } else {
                bottom.min(content_rows)
            };
            if top >= bottom && content_rows > 1 {
                (1, content_rows)
            } else {
                (top, bottom)
            }
        }
    };
    write!(stdout, "\x1b7\x1b[{top};{bottom}r\x1b8")
}

fn reset_viewport(stdout: &mut impl Write) -> std::io::Result<()> {
    stdout.write_all(b"\x1b7\x1b[r\x1b8")
}

fn repair_agent_surface(
    stdout: &mut impl Write,
    status: FooterStatus<'_>,
    viewport: Option<ViewportRepair>,
    redraw_footer: bool,
) -> std::io::Result<()> {
    if let Some(viewport) = viewport {
        set_agent_viewport(stdout, status.rows, viewport)?;
    }
    if redraw_footer {
        draw_footer(
            stdout,
            status.agent,
            status.policy,
            status.gateway,
            status.connectivity,
            status.rows,
            status.cols,
        )?;
    }
    Ok(())
}

fn forward_agent_output(
    stdout: &mut impl Write,
    observer: &mut OutputObserver,
    bytes: &[u8],
    status: FooterStatus<'_>,
) -> std::io::Result<()> {
    let mut start = 0;
    for (index, byte) in bytes.iter().copied().enumerate() {
        let effects = observer.advance(byte);
        if effects.redraw_footer || effects.viewport.is_some() {
            stdout.write_all(&bytes[start..=index])?;
            repair_agent_surface(stdout, status, effects.viewport, effects.redraw_footer)?;
            start = index + 1;
        }
    }
    stdout.write_all(&bytes[start..])?;
    stdout.flush()
}

fn session_title(session: &commands::RemoteSession) -> String {
    session
        .title
        .clone()
        .or_else(|| {
            session.summary.as_deref().and_then(|summary| {
                let title = summary.chars().take(72).collect::<String>();
                (!title.trim().is_empty()).then_some(title)
            })
        })
        .unwrap_or_else(|| format!("{} session", session.harness))
}

fn render_resume(
    frame: &mut ratatui::Frame<'_>,
    wizard: &ResumeWizard,
    accent: Style,
    muted: Style,
    selected_style: Style,
    error_style: Style,
) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let status_area = Rect::new(
        area.x,
        area.bottom().saturating_sub(1),
        area.width,
        area.height.min(1),
    );
    let content =
        Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1)).inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .split(content);
    let step = match wizard.step {
        ResumeStep::Sessions => "1 of 3 · Choose session",
        ResumeStep::Destination | ResumeStep::CustomPath => "2 of 3 · Choose destination",
        ResumeStep::Loading => "Preparing resume",
        ResumeStep::RepositoryMismatch => "3 of 4 · Confirm repository",
        ResumeStep::Confirm => {
            if wizard
                .prepared
                .as_ref()
                .is_some_and(commands::PreparedRemoteSession::repository_mismatch)
            {
                "4 of 4 · Confirm active session"
            } else {
                "3 of 3 · Confirm active session"
            }
        }
        ResumeStep::Failed => "Resume unavailable",
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Blue", accent.add_modifier(Modifier::BOLD)),
            Span::raw("  ·  Resume session"),
        ])),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(step).style(muted).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(muted),
        ),
        chunks[1],
    );

    match wizard.step {
        ResumeStep::Sessions => {
            let items = wizard
                .sessions
                .iter()
                .enumerate()
                .map(|(index, session)| {
                    let ownership = if session.shared {
                        format!("shared by {}", session.user_email)
                    } else {
                        "owned by you".into()
                    };
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(
                                if index == wizard.selected {
                                    "▸ "
                                } else {
                                    "  "
                                },
                                accent,
                            ),
                            Span::styled(
                                session_title(session),
                                accent.add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        Line::styled(
                            format!(
                                "    {} · {ownership} · {} · {}",
                                session.harness,
                                session.cwd.as_deref().unwrap_or("unknown directory"),
                                session.updated_at
                            ),
                            muted,
                        ),
                    ])
                })
                .collect::<Vec<_>>();
            let mut state = ListState::default().with_selected(Some(wizard.selected));
            frame.render_stateful_widget(
                List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(format!(
                        " Remote sessions · {}/{} selected ",
                        wizard.selected + 1,
                        wizard.sessions.len(),
                    )))
                    .highlight_style(selected_style),
                chunks[2],
                &mut state,
            );
        }
        ResumeStep::Destination => {
            let recorded = wizard
                .recorded()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "unknown".into());
            let areas =
                Layout::vertical([Constraint::Length(4), Constraint::Min(4)]).split(chunks[2]);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("Recorded: {recorded}")),
                    Line::from(format!("Current:  {}", wizard.current.display())),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Directory context "),
                ),
                areas[0],
            );
            let choices = wizard.destination_choices();
            let items = choices
                .iter()
                .map(|choice| match choice {
                    DestinationChoice::Current => ListItem::new(vec![
                        Line::styled("Use current directory", accent.add_modifier(Modifier::BOLD)),
                        Line::styled(format!("  {}", wizard.current.display()), muted),
                    ]),
                    DestinationChoice::Recorded => ListItem::new(vec![
                        Line::styled(
                            "Use recorded directory",
                            accent.add_modifier(Modifier::BOLD),
                        ),
                        Line::styled(format!("  {recorded}"), muted),
                    ]),
                    DestinationChoice::Custom => ListItem::new(vec![
                        Line::styled(
                            "Choose another directory",
                            accent.add_modifier(Modifier::BOLD),
                        ),
                        Line::styled("  enter an existing destination", muted),
                    ]),
                    DestinationChoice::Cancel => ListItem::new(vec![
                        Line::styled("Cancel", accent.add_modifier(Modifier::BOLD)),
                        Line::styled("  keep the active agent session", muted),
                    ]),
                })
                .collect::<Vec<_>>();
            let mut state = ListState::default().with_selected(Some(wizard.selected));
            frame.render_stateful_widget(
                List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(" Resume in "))
                    .highlight_style(selected_style)
                    .highlight_symbol(" › "),
                areas[1],
                &mut state,
            );
        }
        ResumeStep::CustomPath => {
            let areas = Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(2),
            ])
            .split(chunks[2]);
            frame.render_widget(
                Paragraph::new("Enter an existing destination directory. Relative paths are resolved from the current directory.")
                    .wrap(Wrap { trim: true }),
                areas[0],
            );
            frame.render_widget(
                Paragraph::new(wizard.path_input.value()).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(accent)
                        .title(" Destination directory "),
                ),
                areas[1],
            );
            let cursor = wizard
                .path_input
                .visual_cursor()
                .min(areas[1].width.saturating_sub(3) as usize);
            frame.set_cursor_position((areas[1].x + 1 + cursor as u16, areas[1].y + 1));
            if let Some(error) = wizard.error.as_deref() {
                frame.render_widget(
                    Paragraph::new(error)
                        .style(error_style)
                        .wrap(Wrap { trim: true }),
                    areas[2],
                );
            }
        }
        ResumeStep::Loading => {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled("Preparing remote session…", accent.add_modifier(Modifier::BOLD)),
                    Line::raw(""),
                    Line::raw("Downloading and verifying the bundle, checking compatibility, and detecting local collisions."),
                    Line::raw("The active agent session is still running."),
                ])
                .block(Block::default().borders(Borders::ALL).title(" Please wait "))
                .wrap(Wrap { trim: true }),
                chunks[2],
            );
        }
        ResumeStep::RepositoryMismatch => {
            let prepared = wizard
                .prepared
                .as_ref()
                .expect("repository mismatch step is prepared");
            let recorded_remote = prepared
                .recorded_repository
                .as_ref()
                .and_then(|repository| repository.remote.as_deref())
                .unwrap_or("unknown");
            let destination_remote = prepared
                .destination_repository
                .as_ref()
                .and_then(|repository| repository.remote.as_deref())
                .unwrap_or("no Git origin");
            let areas =
                Layout::vertical([Constraint::Min(7), Constraint::Length(6)]).split(chunks[2]);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        "The selected destination belongs to a different repository.",
                        error_style.add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(""),
                    Line::from(format!("Recorded repository:    {recorded_remote}")),
                    Line::from(format!("Destination repository: {destination_remote}")),
                    Line::raw("Repository files and working-tree changes are not transferred."),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Repository mismatch "),
                )
                .wrap(Wrap { trim: true }),
                areas[0],
            );
            let items = vec![
                ListItem::new(vec![
                    Line::styled("Cancel resume", accent.add_modifier(Modifier::BOLD)),
                    Line::styled("  keep the active agent session", muted),
                ]),
                ListItem::new(vec![
                    Line::styled(
                        "Continue with selected destination",
                        accent.add_modifier(Modifier::BOLD),
                    ),
                    Line::styled("  review the active-session confirmation next", muted),
                ]),
            ];
            let mut state = ListState::default().with_selected(Some(wizard.selected));
            frame.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Resume in different repository? "),
                    )
                    .highlight_style(selected_style)
                    .highlight_symbol(" › "),
                areas[1],
                &mut state,
            );
        }
        ResumeStep::Confirm => {
            let prepared = wizard.prepared.as_ref().expect("confirm step is prepared");
            let recorded_remote = prepared
                .recorded_repository
                .as_ref()
                .and_then(|repository| repository.remote.as_deref())
                .unwrap_or("unknown");
            let destination_remote = prepared
                .destination_repository
                .as_ref()
                .and_then(|repository| repository.remote.as_deref())
                .unwrap_or("no Git origin");
            let areas =
                Layout::vertical([Constraint::Min(6), Constraint::Length(6)]).split(chunks[2]);
            let details = vec![
                Line::from(format!("Session:     {}", session_title(wizard.session()))),
                Line::from(format!("Agent:       {}", wizard.session().harness)),
                Line::from(format!(
                    "Destination: {}",
                    wizard
                        .destination
                        .as_deref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "unknown".into())
                )),
                Line::from(format!("Recorded repository:    {recorded_remote}")),
                Line::from(format!("Destination repository: {destination_remote}")),
            ];
            frame.render_widget(
                Paragraph::new(details)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Resume review "),
                    )
                    .wrap(Wrap { trim: true }),
                areas[0],
            );
            let items = vec![
                ListItem::new(vec![
                    Line::styled("Keep current session", accent.add_modifier(Modifier::BOLD)),
                    Line::styled("  cancel this resume", muted),
                ]),
                ListItem::new(vec![
                    Line::styled(
                        "End current session and resume",
                        error_style.add_modifier(Modifier::BOLD),
                    ),
                    Line::styled(
                        "  stop the active agent, restore, and launch the selected session",
                        muted,
                    ),
                ]),
            ];
            let mut state = ListState::default().with_selected(Some(wizard.selected));
            frame.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" End active agent session? "),
                    )
                    .highlight_style(selected_style)
                    .highlight_symbol(" › "),
                areas[1],
                &mut state,
            );
        }
        ResumeStep::Failed => {
            let areas =
                Layout::vertical([Constraint::Min(4), Constraint::Length(5)]).split(chunks[2]);
            frame.render_widget(
                Paragraph::new(wizard.error.as_deref().unwrap_or("Unknown resume error"))
                    .style(error_style)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Could not prepare session "),
                    )
                    .wrap(Wrap { trim: true }),
                areas[0],
            );
            let items = vec![
                ListItem::new("Choose another destination"),
                ListItem::new("Cancel and keep current session"),
            ];
            let mut state = ListState::default().with_selected(Some(wizard.selected));
            frame.render_stateful_widget(
                List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(" Next step "))
                    .highlight_style(selected_style)
                    .highlight_symbol(" › "),
                areas[1],
                &mut state,
            );
        }
    }

    let help = match wizard.step {
        ResumeStep::CustomPath => "Type a path  ·  Enter continue  ·  Esc back",
        ResumeStep::Loading => "The active agent remains running during preparation",
        ResumeStep::RepositoryMismatch => {
            "↑↓ select  ·  Enter confirm destination  ·  Esc cancel resume"
        }
        ResumeStep::Confirm => "↑↓ select  ·  Enter confirm  ·  Esc keep current session",
        ResumeStep::Failed => "↑↓ select  ·  Enter continue  ·  Esc keep current session",
        _ => "↑↓ select  ·  PgUp/PgDn scroll  ·  Enter continue  ·  Esc back",
    };
    frame.render_widget(Paragraph::new(help).style(muted), chunks[3]);
    let status = status_row(
        " Ctrl+C / Ctrl-] cancel resume and return to agent",
        status_area.width as usize,
        None,
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().add_modifier(Modifier::REVERSED)),
        status_area,
    );
}

fn draw_control<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    agent: &str,
    interaction: (
        &ControlEditor,
        Option<&ControlPrompt>,
        Option<&ResumeWizard>,
    ),
    transcript: &[String],
    policy: &PolicyState,
    gateway: &str,
    connection: (&str, bool),
) -> std::io::Result<()> {
    let (editor, prompt, resume) = interaction;
    let (connectivity, signed_out) = connection;
    let color = std::env::var_os("NO_COLOR").is_none();
    let accent = if color {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let muted = if color {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    let selected = if color {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::REVERSED)
    };
    let error = if color {
        Style::default().fg(Color::Red)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let suggestions = if prompt.is_some() {
        Vec::new()
    } else {
        editor.suggestions()
    };
    terminal.draw(|frame| {
        if let Some(wizard) = resume {
            render_resume(frame, wizard, accent, muted, selected, error);
            return;
        }
        let frame_area = frame.area();
        // Compute this after ratatui has refreshed the terminal dimensions for
        // the frame. Reading Terminal::size before draw can observe the stale
        // one-row viewport created while a PTY is being handed back to Blue.
        let popup_height = if let Some(prompt) = prompt {
            prompt_popup_height(prompt.options.len(), frame_area.height)
        } else if suggestions.is_empty() {
            0
        } else {
            (suggestions.len().min(6) + 2) as u16
        };
        let status_area = Rect::new(
            frame_area.x,
            frame_area.bottom().saturating_sub(1),
            frame_area.width,
            frame_area.height.min(1),
        );
        let content_area = Rect::new(
            frame_area.x,
            frame_area.y,
            frame_area.width,
            frame_area.height.saturating_sub(1),
        )
        .inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        frame.render_widget(Clear, frame_area);
        let chunks = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(popup_height),
            Constraint::Length(if prompt.is_some() { 0 } else { 3 }),
            Constraint::Length(1),
        ])
        .split(content_area);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Blue", accent.add_modifier(Modifier::BOLD)),
                Span::raw(format!(" control  ·  {agent}")),
            ])),
            chunks[0],
        );

        let transcript_lines = transcript
            .iter()
            .map(|line| {
                let style = if line.starts_with("Error:") {
                    error
                } else if line.starts_with("› ") {
                    accent.add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::styled(line.clone(), style)
            })
            .collect::<Vec<_>>();
        let scroll = transcript_lines
            .len()
            .saturating_sub(chunks[1].height as usize);
        frame.render_widget(
            Paragraph::new(transcript_lines).scroll((scroll as u16, 0)),
            chunks[1],
        );

        if let Some(prompt) = prompt {
            let items = prompt
                .options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(if index == prompt.selected { "▸ " } else { "  " }, accent),
                            Span::styled(&option.label, accent.add_modifier(Modifier::BOLD)),
                        ]),
                        Line::from(vec![Span::raw("    "), Span::styled(&option.hint, muted)]),
                    ])
                })
                .collect::<Vec<_>>();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(if prompt.options.len().saturating_mul(2) + 2 > popup_height as usize {
                            format!(" {} · {}/{} ", prompt.title, prompt.selected + 1, prompt.options.len())
                        } else { format!(" {} ", prompt.title) }),
                )
                .highlight_style(selected);
            let mut state = ListState::default().with_selected(Some(prompt.selected));
            frame.render_stateful_widget(list, chunks[2], &mut state);
        } else if !suggestions.is_empty() {
            let items = suggestions
                .iter()
                .map(|command| {
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{:<11}", command.name), accent),
                        Span::raw(command.description),
                    ]))
                })
                .collect::<Vec<_>>();
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(" Commands "))
                .highlight_style(selected)
                .highlight_symbol(" › ");
            let mut state = ListState::default().with_selected(Some(
                editor.selected.min(suggestions.len().saturating_sub(1)),
            ));
            frame.render_stateful_widget(list, chunks[2], &mut state);
        }

        if prompt.is_none() {
            let input = Paragraph::new(editor.value()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(accent)
                    .title(" Command "),
            );
            frame.render_widget(input, chunks[3]);
            let cursor = editor
                .input
                .visual_cursor()
                .min(chunks[3].width.saturating_sub(3) as usize);
            frame.set_cursor_position((chunks[3].x + 1 + cursor as u16, chunks[3].y + 1));
        }
        let help = if prompt.is_some() {
            "↑↓ select  ·  PgUp/PgDn scroll  ·  Enter confirm  ·  Esc cancel"
        } else {
            "Type / for commands  ·  ↑↓ select  ·  Tab complete  ·  Enter run"
        };
        frame.render_widget(Paragraph::new(help).style(muted), chunks[4]);

        let identity = if signed_out { "signed out" } else { agent };
        let status = format!(
            " Ctrl+C / Ctrl-] agent  ·  {identity}  ·  policy {}  ·  gateway {gateway}  ·  control {connectivity}",
            policy.label
        );
        let status = status
            .chars()
            .take(status_area.width as usize)
            .collect::<String>();
        let status = status_row(&status, status_area.width as usize, policy.notice.as_deref());
        frame.render_widget(
            Paragraph::new(status)
            .style(Style::default().add_modifier(Modifier::REVERSED)),
            status_area,
        );
    })?;
    Ok(())
}

fn replay_agent(
    stdout: &mut impl Write,
    terminal: &vt100::Parser,
    status: FooterStatus<'_>,
) -> std::io::Result<()> {
    if terminal.screen().alternate_screen() {
        stdout.write_all(b"\x1b[?1049h")?;
    }
    write!(stdout, "\x1b[2J\x1b[H")?;
    stdout.write_all(&terminal.screen().state_formatted())?;
    set_agent_viewport(stdout, status.rows, ViewportRepair::Full)?;
    draw_footer(
        stdout,
        status.agent,
        status.policy,
        status.gateway,
        status.connectivity,
        status.rows,
        status.cols,
    )
}

/// Redraws whichever surface currently owns the terminal.
#[allow(clippy::too_many_arguments)]
fn redraw_surface<W: Write>(
    active: bool,
    stdout: &mut impl Write,
    control_terminal: Option<&mut Terminal<CrosstermBackend<W>>>,
    agent: &str,
    interaction: (
        &ControlEditor,
        Option<&ControlPrompt>,
        Option<&ResumeWizard>,
    ),
    transcript: &[String],
    policy: &PolicyState,
    gateway: &str,
    connection: (&str, bool),
    size: (u16, u16),
) -> std::io::Result<()> {
    if active {
        draw_footer(stdout, agent, policy, gateway, connection.0, size.0, size.1)
    } else {
        draw_control(
            control_terminal.expect("control terminal is open"),
            agent,
            interaction,
            transcript,
            policy,
            gateway,
            connection,
        )
    }
}

fn stop_child(session: &PtySession) -> Result<i32> {
    session.terminate()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(code) = session.try_wait()? {
            return Ok(code);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    session.force_kill()?;
    session.wait().map_err(Into::into)
}

fn active_identity() -> Result<Option<String>> {
    let Some(mut session) = Session::load()? else {
        return Ok(None);
    };
    if session.refresh_if_needed(now_unix()).is_err() {
        return Ok(None);
    }
    Ok(Some(match (&session.email, &session.org_id) {
        (Some(email), Some(org)) => format!("Signed in as {email} (org {org})."),
        (Some(email), None) => format!("Signed in as {email}."),
        _ => "Signed in.".to_owned(),
    }))
}

fn run_plain_command(command: &str) -> Result<String> {
    match command {
        "/status" => commands::status_text(false),
        "/health" => commands::health_text(),
        "/version" => Ok(commands::version_text()),
        "/gateway" => commands::gateway_text(),
        "/doctor" => commands::doctor_text(),
        "/apply yes" => commands::apply_text(),
        _ => Ok(String::new()),
    }
}

fn append_transcript(transcript: &mut Vec<String>, text: impl AsRef<str>) {
    transcript.extend(text.as_ref().lines().map(str::to_owned));
    const MAX_TRANSCRIPT_LINES: usize = 2_000;
    if transcript.len() > MAX_TRANSCRIPT_LINES {
        transcript.drain(..transcript.len() - MAX_TRANSCRIPT_LINES);
    }
}

fn begin_command_output(transcript: &mut Vec<String>, command: &str) {
    transcript.clear();
    append_transcript(transcript, format!("› {command}"));
}

type ControlTerminal = Terminal<CrosstermBackend<std::io::Stdout>>;

fn open_control_terminal() -> std::io::Result<ControlTerminal> {
    let (rows, cols) = gh_harness::terminal_size();
    Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows.max(1))),
        },
    )
}

const CONTROL_SURFACE_HANDOFF: &[u8] = b"\x1b[?2026l\x1b[?1049l\x1b[r\x1b[2J\x1b[H\x1b[?25h";

fn close_control_surface(
    terminal: &mut Option<ControlTerminal>,
    stdout: &mut impl Write,
) -> std::io::Result<()> {
    // Drop ratatui's cached surface before handing the terminal to another
    // full-screen process. The next harness must start from a clean primary
    // screen even when the previous harness or Blue used an alternate screen.
    terminal.take();
    stdout.write_all(CONTROL_SURFACE_HANDOFF)?;
    stdout.flush()
}

fn start_connectivity_probe(
    health_url: Option<String>,
) -> (mpsc::Receiver<String>, Option<mpsc::Sender<()>>, String) {
    let (status_tx, status_rx) = mpsc::channel();
    let Some(health_url) = health_url else {
        return (status_rx, None, "local".to_owned());
    };
    let (stop_tx, stop_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(3))
            .build()
        {
            Ok(client) => client,
            Err(_) => return,
        };
        loop {
            let status = match client.get(&health_url).send() {
                Ok(response) if response.status().is_success() => "connected",
                _ => "offline",
            };
            if status_tx.send(status.to_owned()).is_err() {
                return;
            }
            match stop_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    });
    (status_rx, Some(stop_tx), "checking".to_owned())
}

pub struct SupervisorRuntime {
    pub connectivity_health_url: Option<String>,
    pub revision_notice: Option<Arc<Mutex<Option<String>>>>,
    /// Set by the background watcher when the control service answers 401.
    /// Driven by the /governance-config response, never by the token refresh:
    /// @better-auth/oauth-provider still mints an access token after the
    /// backing session is gone, so the refresh succeeding proves nothing.
    pub auth_notice: Option<Arc<Mutex<Option<String>>>>,
    /// `exp` of the inference JWT this agent was launched with, when there is
    /// one. The token is fixed in the child's environment, so the only honest
    /// remedy is to restart with a fresh one.
    pub gateway_token_expires_at: Option<i64>,
    pub gateway_available: bool,
    pub direct_mode: bool,
}

pub fn supervise(
    bin: &Path,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
    agent: &str,
    policy: &str,
    gateway: &str,
    runtime: SupervisorRuntime,
) -> Result<SupervisorExit> {
    let SupervisorRuntime {
        connectivity_health_url,
        revision_notice,
        auth_notice,
        gateway_token_expires_at,
        gateway_available,
        direct_mode,
    } = runtime;
    let (rows, cols) = gh_harness::terminal_size();
    let session = Arc::new(PtySession::spawn(
        bin,
        args,
        env,
        rows.saturating_sub(1).max(1),
        cols,
    )?);
    let mut raw = RawGuard::enter();
    let _terminal_modes = TerminalModeGuard::enter();
    // This reader must stop before a restart/switch/resume recursively launches
    // another supervisor. Otherwise the abandoned reader can steal terminal
    // query replies intended for the next child and discard them.
    let mut input_reader = InputReader::spawn();
    let mut stdout = std::io::stdout();
    let mut control_terminal: Option<ControlTerminal> = None;
    let mut active = true;
    let mut signed_out = false;
    let mut editor = ControlEditor {
        gateway_available,
        ..ControlEditor::default()
    };
    let mut configured_direct_mode = direct_mode;
    let mut prompt: Option<ControlPrompt> = None;
    let mut resume: Option<ResumeWizard> = None;
    let mut escape = Vec::new();
    let mut transcript = vec![
        "Agent output remains buffered while Blue control is open.".into(),
        "Type / to browse commands.".into(),
    ];
    let mut policy_state = PolicyState::new(policy);
    let (connectivity_rx, _connectivity_stop, mut connectivity_state) =
        start_connectivity_probe(connectivity_health_url);
    let mut last_notice = None;
    let mut last_auth_notice = None;
    let mut gateway_expiry_warned = false;
    let mut pending_input = Vec::new();
    let mut control_input = Vec::new();
    let mut terminal = vt100::Parser::new(rows.saturating_sub(1).max(1), cols, SCROLLBACK_ROWS);
    let mut output_observer = OutputObserver::default();
    let mut size = (rows, cols);
    repair_agent_surface(
        &mut stdout,
        FooterStatus::new(agent, &policy_state, gateway, &connectivity_state, size),
        Some(ViewportRepair::Full),
        true,
    )?;

    loop {
        if let Ok(next) = connectivity_rx.try_recv() {
            if next != connectivity_state {
                connectivity_state = next;
                if active {
                    draw_footer(
                        &mut stdout,
                        agent,
                        &policy_state,
                        gateway,
                        &connectivity_state,
                        size.0,
                        size.1,
                    )?;
                } else {
                    draw_control(
                        control_terminal.as_mut().expect("control terminal is open"),
                        agent,
                        (&editor, prompt.as_ref(), resume.as_ref()),
                        &transcript,
                        &policy_state,
                        gateway,
                        (&connectivity_state, signed_out),
                    )?;
                }
            }
        }
        let notice = revision_notice
            .as_ref()
            .and_then(|notice| notice.lock().ok().and_then(|notice| notice.clone()));
        if notice != last_notice {
            if let Some(notice) = notice.as_ref() {
                policy_state = PolicyState {
                    label: "update available".into(),
                    notice: Some(REVISION_AVAILABLE_LABEL.into()),
                };
                append_transcript(&mut transcript, format!("Policy update: {notice}"));
                if active {
                    draw_footer(
                        &mut stdout,
                        agent,
                        &policy_state,
                        gateway,
                        &connectivity_state,
                        size.0,
                        size.1,
                    )?;
                } else {
                    draw_control(
                        control_terminal.as_mut().expect("control terminal is open"),
                        agent,
                        (&editor, prompt.as_ref(), resume.as_ref()),
                        &transcript,
                        &policy_state,
                        gateway,
                        (&connectivity_state, signed_out),
                    )?;
                }
            }
            last_notice = notice;
        }

        // A dead session is not a policy update: it needs the user to act, and
        // the only way back is to stop the child and sign in.
        let auth = auth_notice
            .as_ref()
            .and_then(|notice| notice.lock().ok().and_then(|notice| notice.clone()));
        if auth.is_some() && auth != last_auth_notice {
            let message = auth.clone().unwrap_or_default();
            policy_state.notice = Some(SESSION_EXPIRED_LABEL.into());
            append_transcript(&mut transcript, message);
            // Only raise the prompt when nothing else owns the surface, and
            // only once per distinct notice — otherwise a cancelled prompt
            // comes straight back on the next poll. The banner stays lit.
            if prompt.is_none() && resume.is_none() {
                prompt = Some(confirmation_prompt(
                    "Login expired. Stop the agent and sign in?",
                    "Stop agent and sign in",
                    "the current agent session must end first",
                    PromptAction::Login,
                ));
            }
            redraw_surface(
                active,
                &mut stdout,
                control_terminal.as_mut(),
                agent,
                (&editor, prompt.as_ref(), resume.as_ref()),
                &transcript,
                &policy_state,
                gateway,
                (&connectivity_state, signed_out),
                size,
            )?;
            last_auth_notice = auth;
        }

        // The inference JWT is baked into the child's environment at spawn and
        // is never rotated in flight, so warn before it dies mid-turn rather
        // than letting the agent start failing against the proxy. Never
        // auto-restart: that would kill an in-flight turn.
        if let Some(expires_at) = gateway_token_expires_at {
            let remaining = expires_at.saturating_sub(now_unix());
            if remaining > 0 && remaining <= GATEWAY_EXPIRY_LEAD.as_secs() as i64 {
                let minutes = (remaining + 59) / 60;
                let banner = format!("Gateway access expires in {minutes}m");
                if policy_state.notice.as_deref() != Some(banner.as_str())
                    && last_auth_notice.is_none()
                {
                    policy_state.notice = Some(banner);
                    if !gateway_expiry_warned {
                        gateway_expiry_warned = true;
                        append_transcript(
                            &mut transcript,
                            "Gateway access expires soon. Reload the agent to mint a fresh token.",
                        );
                        if prompt.is_none() && resume.is_none() {
                            prompt = Some(confirmation_prompt(
                                "Gateway access expires soon. Quit and reload the agent?",
                                "Quit and reload now",
                                "gracefully stop the current agent",
                                PromptAction::ReloadCurrent,
                            ));
                        }
                    }
                    redraw_surface(
                        active,
                        &mut stdout,
                        control_terminal.as_mut(),
                        agent,
                        (&editor, prompt.as_ref(), resume.as_ref()),
                        &transcript,
                        &policy_state,
                        gateway,
                        (&connectivity_state, signed_out),
                        size,
                    )?;
                }
            }
        }
        for event in session.drain_events() {
            match event {
                PtyEvent::Output(bytes) => {
                    terminal.process(&bytes);
                    if active {
                        forward_agent_output(
                            &mut stdout,
                            &mut output_observer,
                            &bytes,
                            FooterStatus::new(
                                agent,
                                &policy_state,
                                gateway,
                                &connectivity_state,
                                size,
                            ),
                        )?;
                    } else {
                        for byte in bytes {
                            let _ = output_observer.advance(byte);
                        }
                    }
                }
                PtyEvent::Error(error) if !error.contains("Input/output error") => {
                    append_transcript(&mut transcript, format!("Error: agent output: {error}"));
                }
                _ => {}
            }
        }
        let now_size = gh_harness::terminal_size();
        if now_size != size {
            size = now_size;
            session.resize(size.0.saturating_sub(1).max(1), size.1)?;
            terminal.set_size(size.0.saturating_sub(1).max(1), size.1);
            if active {
                repair_agent_surface(
                    &mut stdout,
                    FooterStatus::new(agent, &policy_state, gateway, &connectivity_state, size),
                    Some(ViewportRepair::Full),
                    true,
                )?;
            } else {
                control_terminal = Some(open_control_terminal()?);
                draw_control(
                    control_terminal.as_mut().expect("control terminal is open"),
                    agent,
                    (&editor, prompt.as_ref(), resume.as_ref()),
                    &transcript,
                    &policy_state,
                    gateway,
                    (&connectivity_state, signed_out),
                )?;
            }
        }
        if !signed_out {
            if let Some(code) = session.try_wait()? {
                // Let the dedicated reader publish the final PTY bytes before
                // tearing down the frame.
                std::thread::sleep(Duration::from_millis(20));
                for event in session.drain_events() {
                    if let PtyEvent::Output(bytes) = event {
                        terminal.process(&bytes);
                        if active {
                            stdout.write_all(&bytes)?;
                        }
                    }
                }
                write!(stdout, "\x1b[{};1H\x1b[2K\r\n", size.0)?;
                stdout.flush()?;
                return Ok(SupervisorExit::Child(code));
            }
        }

        let mut input = match input_reader.recv_timeout(Duration::from_millis(15)) {
            Ok(bytes) => {
                pending_input.extend(bytes);
                if find_control_chord(&pending_input).is_none() {
                    let retained = trailing_control_prefix(&pending_input);
                    if retained > 0 {
                        let split = pending_input.len() - retained;
                        if split == 0 {
                            continue;
                        }
                        pending_input.drain(..split).collect::<Vec<_>>()
                    } else {
                        std::mem::take(&mut pending_input)
                    }
                } else {
                    std::mem::take(&mut pending_input)
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) if !pending_input.is_empty() => {
                std::mem::take(&mut pending_input)
            }
            Err(_) => continue,
        };
        {
            if active {
                if let Some((index, chord_len)) = find_control_chord(&input) {
                    if index > 0 {
                        session.write_input(&input[..index])?;
                    }
                    input = input[index + chord_len..].to_vec();
                    active = false;
                    editor.clear();
                    reset_viewport(&mut stdout)?;
                    if terminal.screen().alternate_screen() {
                        stdout.write_all(b"\x1b[?1049l")?;
                        stdout.flush()?;
                    }
                    reset_viewport(&mut stdout)?;
                    control_terminal = Some(open_control_terminal()?);
                    control_terminal
                        .as_mut()
                        .expect("control terminal is open")
                        .clear()?;
                    draw_control(
                        control_terminal.as_mut().expect("control terminal is open"),
                        agent,
                        (&editor, prompt.as_ref(), resume.as_ref()),
                        &transcript,
                        &policy_state,
                        gateway,
                        (&connectivity_state, signed_out),
                    )?;
                } else {
                    session.write_input(&input)?;
                    continue;
                }
            } else if let Some((0, chord_len)) = find_control_chord(&input) {
                active = true;
                prompt = None;
                resume = None;
                control_terminal.take();
                replay_agent(
                    &mut stdout,
                    &terminal,
                    FooterStatus::new(agent, &policy_state, gateway, &connectivity_state, size),
                )?;
                if chord_len < input.len() {
                    session.write_input(&input[chord_len..])?;
                }
                continue;
            }

            input = decode_control_input(&mut control_input, &input);

            let lone_escape = input.as_slice() == b"\x1b";
            for byte in input {
                let mut key = None;
                if lone_escape {
                    key = Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                } else if !escape.is_empty() || byte == 0x1b {
                    escape.push(byte);
                    if escape.len() == 3
                        && escape[0..2] == [0x1b, b'[']
                        && !matches!(escape[2], b'3' | b'5' | b'6')
                    {
                        key = match escape[2] {
                            b'A' => Some(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
                            b'B' => Some(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                            b'C' => Some(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
                            b'D' => Some(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
                            b'H' => Some(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
                            b'F' => Some(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
                            _ => None,
                        };
                        escape.clear();
                    } else if escape.len() == 4 && escape[0..2] == [0x1b, b'['] && escape[3] == b'~'
                    {
                        key = match escape[2] {
                            b'3' => Some(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
                            b'5' => Some(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
                            b'6' => Some(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
                            _ => None,
                        };
                        escape.clear();
                    } else if escape.len() >= 4 {
                        escape.clear();
                    }
                    if key.is_none() {
                        continue;
                    }
                } else {
                    key = match byte {
                        b'\r' | b'\n' => Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                        b'\t' => Some(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
                        0x7f | 0x08 => Some(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
                        CONTROL | INTERRUPT if !signed_out => {
                            active = true;
                            prompt = None;
                            resume = None;
                            replay_agent(
                                &mut stdout,
                                &terminal,
                                FooterStatus::new(
                                    agent,
                                    &policy_state,
                                    gateway,
                                    &connectivity_state,
                                    size,
                                ),
                            )?;
                            continue;
                        }
                        byte if byte.is_ascii_graphic() || byte == b' ' => Some(KeyEvent::new(
                            KeyCode::Char(byte as char),
                            KeyModifiers::NONE,
                        )),
                        _ => None,
                    };
                }

                if resume.is_some() {
                    let effect = key
                        .map(|key| {
                            resume
                                .as_mut()
                                .expect("resume wizard is open")
                                .handle_key(key)
                        })
                        .unwrap_or(ResumeEffect::None);
                    match effect {
                        ResumeEffect::None => {}
                        ResumeEffect::Cancel => {
                            resume = None;
                            append_transcript(
                                &mut transcript,
                                "Remote resume cancelled; the active agent session is still running.",
                            );
                        }
                        ResumeEffect::Prepare {
                            session: selected,
                            destination,
                        } => {
                            draw_control(
                                control_terminal.as_mut().expect("control terminal is open"),
                                agent,
                                (&editor, prompt.as_ref(), resume.as_ref()),
                                &transcript,
                                &policy_state,
                                gateway,
                                (&connectivity_state, signed_out),
                            )?;
                            let result = commands::prepare_remote_session(&selected, &destination);
                            // Keystrokes entered while the blocking download/preflight was
                            // running must never spill into the destructive confirmation.
                            while input_reader.try_recv().is_ok() {}
                            pending_input.clear();
                            control_input.clear();
                            escape.clear();
                            match result {
                                Ok(prepared) => resume
                                    .as_mut()
                                    .expect("resume wizard is open")
                                    .prepared(prepared),
                                Err(error) => resume
                                    .as_mut()
                                    .expect("resume wizard is open")
                                    .failed(error),
                            }
                        }
                        ResumeEffect::Finish => {
                            let prepared = resume
                                .as_mut()
                                .and_then(ResumeWizard::take_prepared)
                                .expect("confirmed resume is prepared");
                            let _ = stop_child(&session)?;
                            let result = commands::finish_remote_resume(prepared);
                            close_control_surface(&mut control_terminal, &mut stdout)?;
                            input_reader.stop();
                            raw.take();
                            return Ok(SupervisorExit::Resume(result?));
                        }
                    }
                    if !active {
                        draw_control(
                            control_terminal.as_mut().expect("control terminal is open"),
                            agent,
                            (&editor, prompt.as_ref(), resume.as_ref()),
                            &transcript,
                            &policy_state,
                            gateway,
                            (&connectivity_state, signed_out),
                        )?;
                    }
                    continue;
                }

                let prompt_action =
                    if let (Some(key), Some(current_prompt)) = (key, prompt.as_mut()) {
                        current_prompt.handle_key(key)
                    } else {
                        None
                    };
                if let Some(action) = prompt_action {
                    prompt = None;
                    match action {
                        PromptAction::Cancel => {
                            append_transcript(&mut transcript, "Cancelled; no action was taken.")
                        }
                        PromptAction::SelectAgent(name) => {
                            match commands::set_preferred_agent(&name) {
                                Ok(selected) if selected == agent => append_transcript(
                                    &mut transcript,
                                    format!("{selected} is already the active and default agent."),
                                ),
                                Ok(selected) => {
                                    append_transcript(
                                        &mut transcript,
                                        format!(
                                            "Default agent updated to {selected}. It will apply on next restart."
                                        ),
                                    );
                                    prompt = Some(
                                        ControlPrompt::new(
                                            "Reload the agent now?",
                                            vec![
                                                option(
                                                    "Keep current session",
                                                    "use the new default next time",
                                                    PromptAction::KeepSession,
                                                ),
                                                option(
                                                    "Quit and reload now",
                                                    "gracefully stop the current agent",
                                                    PromptAction::ReloadAgent(selected),
                                                ),
                                            ],
                                        )
                                        .escape_as(PromptAction::KeepSession),
                                    );
                                }
                                Err(error) => append_transcript(
                                    &mut transcript,
                                    format!("Error: {error:#}"),
                                ),
                            }
                        }
                        PromptAction::KeepSession => append_transcript(
                            &mut transcript,
                            "Current agent session is still running; the new default will be used next time.",
                        ),
                        PromptAction::KeepModeSession(direct) => append_transcript(
                            &mut transcript,
                            if direct {
                                "Current agent session is still using the organization gateway. Direct mode will be used after the next Blue restart."
                            } else {
                                "Current agent session is still using direct credentials. The organization gateway will be used after the next Blue restart."
                            },
                        ),
                        PromptAction::ReloadAgent(selected) => {
                            append_transcript(
                                &mut transcript,
                                format!("Stopping the current agent and reloading {selected}…"),
                            );
                            draw_control(
                                control_terminal.as_mut().expect("control terminal is open"),
                                agent,
                                (&editor, prompt.as_ref(), resume.as_ref()),
                                &transcript,
                                &policy_state,
                                gateway,
                                (&connectivity_state, signed_out),
                            )?;
                            if !signed_out {
                                let _ = stop_child(&session)?;
                            }
                            close_control_surface(&mut control_terminal, &mut stdout)?;
                            input_reader.stop();
                            raw.take();
                            return Ok(SupervisorExit::Switch(selected));
                        }
                        PromptAction::ReloadCurrent => {
                            append_transcript(
                                &mut transcript,
                                format!("Stopping and reloading {agent} with the selected inference mode…"),
                            );
                            draw_control(
                                control_terminal.as_mut().expect("control terminal is open"),
                                agent,
                                (&editor, prompt.as_ref(), resume.as_ref()),
                                &transcript,
                                &policy_state,
                                gateway,
                                (&connectivity_state, signed_out),
                            )?;
                            let _ = stop_child(&session)?;
                            close_control_surface(&mut control_terminal, &mut stdout)?;
                            input_reader.stop();
                            raw.take();
                            return Ok(SupervisorExit::Restart);
                        }
                        PromptAction::Apply => {
                            append_transcript(&mut transcript, "Running apply…");
                            draw_control(
                                control_terminal.as_mut().expect("control terminal is open"),
                                agent,
                                (&editor, prompt.as_ref(), resume.as_ref()),
                                &transcript,
                                &policy_state,
                                gateway,
                                (&connectivity_state, signed_out),
                            )?;
                            let result = run_plain_command("/apply yes")
                                .unwrap_or_else(|error| format!("Error: {error:#}"));
                            transcript.pop();
                            append_transcript(&mut transcript, result);
                        }
                        PromptAction::Quit => {
                            let code = if signed_out { 0 } else { stop_child(&session)? };
                            write!(stdout, "\x1b[2J\x1b[H")?;
                            return Ok(SupervisorExit::Child(code));
                        }
                        PromptAction::Logout => {
                            if !signed_out {
                                let _ = stop_child(&session)?;
                            }
                            input_reader.stop();
                            raw.take();
                            commands::logout()?;
                            raw = RawGuard::enter();
                            // Unix readers are interruptible and have been
                            // joined above. The non-Unix fallback blocks in
                            // stdin.read(), so replacing it would leave the old
                            // thread competing with the new one for input.
                            #[cfg(unix)]
                            {
                                input_reader = InputReader::spawn();
                            }
                            control_terminal = Some(open_control_terminal()?);
                            signed_out = true;
                            append_transcript(
                                &mut transcript,
                                "Signed out. Use /login to authenticate again, or /quit.",
                            );
                        }
                        PromptAction::Reset => {
                            if !signed_out {
                                let _ = stop_child(&session)?;
                            }
                            input_reader.stop();
                            raw.take();
                            commands::reset(true)?;
                            return Ok(SupervisorExit::Child(0));
                        }
                        PromptAction::Login => {
                            if !signed_out {
                                let _ = stop_child(&session)?;
                            }
                            close_control_surface(&mut control_terminal, &mut stdout)?;
                            input_reader.stop();
                            raw.take();
                            commands::login(false)?;
                            return Ok(SupervisorExit::Restart);
                        }
                    }
                } else if prompt.is_none() {
                    if let Some(command) = key.and_then(|key| editor.handle_key(key)) {
                        begin_command_output(&mut transcript, &command);
                        match command.as_str() {
                        "/resume" if !signed_out => {
                            match commands::remote_sessions() {
                                Ok(sessions) if sessions.is_empty() => append_transcript(&mut transcript, "No owned or shared resumable sessions are available."),
                                Ok(sessions) => match ResumeWizard::new(sessions) {
                                    Ok(wizard) => resume = Some(wizard),
                                    Err(error) => append_transcript(&mut transcript, format!("Remote resume unavailable: {error:#}")),
                                },
                                Err(error) => append_transcript(&mut transcript, format!("Remote resume unavailable: {error:#}")),
                            }
                        }
                        "/resume" => append_transcript(
                            &mut transcript,
                            "No agent is running. Use /login or /quit.",
                        ),
                        "/help" => append_transcript(
                            &mut transcript,
                            format!(
                                "Commands: {}",
                                COMMANDS
                                    .iter()
                                    .filter(|command| command.name != "/direct" || gateway_available)
                                    .map(|command| command.name)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        ),
                        "/direct" => {
                            let next_direct_mode = !configured_direct_mode;
                            let active_harness = if signed_out {
                                None
                            } else {
                                Some(agent.parse().expect("supervisor agent is a known harness"))
                            };
                            match commands::set_direct_mode(next_direct_mode, active_harness) {
                                Ok(()) => {
                                    configured_direct_mode = next_direct_mode;
                                    if signed_out {
                                        append_transcript(
                                            &mut transcript,
                                            if next_direct_mode {
                                                "Direct mode selected. It will apply on the next Blue restart."
                                            } else {
                                                "Organization gateway mode selected. It will apply on the next Blue restart."
                                            },
                                        );
                                    } else {
                                        append_transcript(
                                            &mut transcript,
                                            if next_direct_mode {
                                                "Gateway wiring was removed from the managed profile."
                                            } else {
                                                "Organization gateway wiring was restored to the managed profile."
                                            },
                                        );
                                        prompt = Some(mode_reload_prompt(next_direct_mode));
                                    }
                                }
                                Err(error) => append_transcript(
                                    &mut transcript,
                                    format!("Error: {error:#}"),
                                ),
                            }
                        }
                        "/agent" => {
                            match commands::agent_options() {
                                Ok(options) => {
                                    // Not selectable here — the installer the
                                    // repair runs would draw over the TUI — but
                                    // named, so they are not silently missing.
                                    if let Some(guidance) =
                                        agent_repair_guidance(&options.needs_repair)
                                    {
                                        append_transcript(
                                            &mut transcript,
                                            guidance,
                                        );
                                    }
                                    if let Some(guidance) = agent_install_guidance(&options.needs_install) {
                                        append_transcript(&mut transcript, guidance);
                                    }
                                    prompt = agent_selector(&options);
                                }
                                Err(error) => append_transcript(
                                    &mut transcript,
                                    format!("Error: {error:#}"),
                                ),
                            }
                        }
                        "/apply" => prompt = Some(confirmation_prompt(
                            "Apply configuration now?",
                            "Apply now",
                            "the running agent may require a restart",
                            PromptAction::Apply,
                        )),
                        "/quit" => prompt = Some(confirmation_prompt(
                            "End the agent and exit Blue?",
                            "Quit Blue",
                            "graceful stop, then force after five seconds",
                            PromptAction::Quit,
                        )),
                        "/logout" => prompt = Some(confirmation_prompt(
                            "Sign out and end the agent?",
                            "Sign out",
                            "stop the agent and remove credentials",
                            PromptAction::Logout,
                        )),
                        "/reset" => prompt = Some(confirmation_prompt(
                            "Reset Blue and end the agent?",
                            "Reset Blue",
                            "disconnect and retain non-secret tenant state",
                            PromptAction::Reset,
                        )),
                        "/login" => match active_identity()? {
                            Some(identity) => append_transcript(&mut transcript, identity),
                            None if signed_out => {
                                close_control_surface(&mut control_terminal, &mut stdout)?;
                                input_reader.stop();
                                raw.take();
                                commands::login(false)?;
                                return Ok(SupervisorExit::Restart);
                            }
                            None => {
                                prompt = Some(confirmation_prompt(
                                    "Login expired. Stop the agent and sign in?",
                                    "Stop agent and sign in",
                                    "the current agent session must end first",
                                    PromptAction::Login,
                                ));
                            }
                        },
                        command if slash_command_with_arguments(command, gateway_available).is_some() => append_transcript(
                            &mut transcript,
                            format!(
                                "Slash-command arguments are not supported. Run `{}` and choose from the options.",
                                slash_command_with_arguments(command, gateway_available).unwrap_or(command)
                            ),
                        ),
                        command if command_available(command, gateway_available) => {
                            append_transcript(
                                &mut transcript,
                                format!("Running {}…", command.trim_start_matches('/')),
                            );
                            draw_control(
                                control_terminal.as_mut().expect("control terminal is open"),
                                agent,
                                    (&editor, prompt.as_ref(), resume.as_ref()),
                                &transcript,
                                &policy_state,
                                gateway,
                                (&connectivity_state, signed_out),
                            )?;
                            let result = run_plain_command(command)
                                .unwrap_or_else(|error| format!("Error: {error:#}"));
                            transcript.pop();
                            append_transcript(&mut transcript, result);
                        }
                        _ => append_transcript(
                            &mut transcript,
                            format!("Unknown command `{command}`. Use /help."),
                        ),
                        }
                    }
                }
                if !active {
                    draw_control(
                        control_terminal.as_mut().expect("control terminal is open"),
                        agent,
                        (&editor, prompt.as_ref(), resume.as_ref()),
                        &transcript,
                        &policy_state,
                        gateway,
                        (&connectivity_state, signed_out),
                    )?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn input_reader_stops_before_the_next_supervisor_starts() {
        use std::os::fd::FromRawFd;

        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let input = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        let reader = InputReader::spawn_from(input, fds[0]);
        let started = Instant::now();
        drop(reader);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(writer);
    }

    #[test]
    fn control_surface_handoff_resets_and_clears_the_primary_screen() {
        assert_eq!(
            CONTROL_SURFACE_HANDOFF,
            b"\x1b[?2026l\x1b[?1049l\x1b[r\x1b[2J\x1b[H\x1b[?25h"
        );
    }

    fn remote_session(cwd: Option<String>) -> commands::RemoteSession {
        commands::RemoteSession {
            id: "remote-1".into(),
            harness: "codex".into(),
            native_session_id: "native-1".into(),
            title: Some("Investigate terminal rendering".into()),
            summary: Some("Resume flow regression".into()),
            cwd,
            user_email: "dev@example.com".into(),
            updated_at: "2026-09-08T12:00:00Z".into(),
            shared: false,
        }
    }

    fn screen_row(parser: &vt100::Parser, row: u16, cols: u16) -> String {
        (0..cols)
            .filter_map(|col| parser.screen().cell(row, col))
            .map(|cell| cell.contents())
            .collect()
    }

    fn revision_available() -> PolicyState {
        PolicyState {
            label: "update available".into(),
            notice: Some(REVISION_AVAILABLE_LABEL.into()),
        }
    }

    #[test]
    fn footer_compacts_without_overflow() {
        let current = PolicyState::new("current");
        assert!(
            footer("opencode", &current, "managed", "connected", 80)
                .chars()
                .count()
                <= 80
        );
        assert!(
            footer("opencode", &current, "managed", "offline", 20)
                .chars()
                .count()
                <= 20
        );
        assert_eq!(
            footer("codex", &current, "managed", "connected", 80),
            " Ctrl-] Control · codex · policy current · gateway managed · control connected"
        );
        let plain = footer_row("codex", &current, "managed", "connected", 60, false);
        assert_eq!(plain.chars().count(), 60);
        assert!(!plain.contains("\x1b["));
        assert!(
            footer_row("codex", &current, "managed", "connected", 60, true).contains("\x1b[7m")
        );
        let update = footer_row(
            "codex",
            &revision_available(),
            "managed",
            "connected",
            80,
            false,
        );
        assert_eq!(update.chars().count(), 80);
        assert!(update.ends_with(REVISION_AVAILABLE_LABEL));
        assert!(update.starts_with(" Ctrl-] · codex"));

        let compact_update = footer_row(
            "opencode",
            &revision_available(),
            "managed",
            "connected",
            35,
            false,
        );
        assert_eq!(compact_update.chars().count(), 35);
        assert!(compact_update.ends_with(REVISION_AVAILABLE_LABEL));

        let narrow_update = footer_row(
            "codex",
            &revision_available(),
            "managed",
            "connected",
            12,
            false,
        );
        assert_eq!(narrow_update, "New Blue pol");
    }

    #[test]
    fn the_status_banner_is_independent_of_the_policy_label() {
        // The banner used to be inferred from the policy label reading exactly
        // "update available", so any other attention state was unrenderable.
        let expired = PolicyState {
            label: "current".into(),
            notice: Some(SESSION_EXPIRED_LABEL.into()),
        };
        let row = footer_row("codex", &expired, "managed", "connected", 80, false);
        assert_eq!(row.chars().count(), 80);
        assert!(row.ends_with(SESSION_EXPIRED_LABEL));
        assert!(row.contains("policy current"));

        let countdown = PolicyState {
            label: "current".into(),
            notice: Some("Gateway access expires in 7m".into()),
        };
        assert!(
            footer_row("codex", &countdown, "managed", "connected", 80, false)
                .ends_with("Gateway access expires in 7m")
        );

        // No notice, no banner.
        assert_eq!(
            footer_row(
                "codex",
                &PolicyState::new("current"),
                "managed",
                "connected",
                80,
                false
            )
            .trim_end(),
            footer(
                "codex",
                &PolicyState::new("current"),
                "managed",
                "connected",
                80
            )
            .trim_end()
        );
    }

    #[test]
    fn fragmented_ansi_is_forwarded_without_footer_bytes_inside_it() {
        let mut output = Vec::new();
        let mut observer = OutputObserver::default();
        forward_agent_output(
            &mut output,
            &mut observer,
            b"\x1b[38",
            FooterStatus::new(
                "codex",
                &PolicyState::new("current"),
                "managed",
                "connected",
                (24, 80),
            ),
        )
        .unwrap();
        forward_agent_output(
            &mut output,
            &mut observer,
            b";2;1;2;3mBLUE_ANSI_OK\x1b[0m",
            FooterStatus::new(
                "codex",
                &PolicyState::new("current"),
                "managed",
                "connected",
                (24, 80),
            ),
        )
        .unwrap();

        assert_eq!(output, b"\x1b[38;2;1;2;3mBLUE_ANSI_OK\x1b[0m");
    }

    #[test]
    fn destructive_terminal_sequences_restore_the_footer() {
        const ROWS: u16 = 8;
        const COLS: u16 = 80;
        let mut output = Vec::new();
        let mut observer = OutputObserver::default();
        repair_agent_surface(
            &mut output,
            FooterStatus::new(
                "codex",
                &PolicyState::new("current"),
                "managed",
                "connected",
                (ROWS, COLS),
            ),
            Some(ViewportRepair::Full),
            true,
        )
        .unwrap();

        for bytes in [
            b"\x1b[2".as_slice(),
            b"J\x1b[Hagent".as_slice(),
            b"\x1b[?1049h\x1b[2J\x1b[Halternate".as_slice(),
            b"\x1bcreset".as_slice(),
        ] {
            forward_agent_output(
                &mut output,
                &mut observer,
                bytes,
                FooterStatus::new(
                    "codex",
                    &PolicyState::new("current"),
                    "managed",
                    "connected",
                    (ROWS, COLS),
                ),
            )
            .unwrap();
        }

        let mut terminal = vt100::Parser::new(ROWS, COLS, 0);
        terminal.process(&output);
        assert!(screen_row(&terminal, ROWS - 1, COLS).contains("Ctrl-] Control"));
    }

    #[test]
    fn scroll_region_reset_cannot_scroll_over_the_footer() {
        const ROWS: u16 = 8;
        const COLS: u16 = 80;
        let mut output = Vec::new();
        let mut observer = OutputObserver::default();
        repair_agent_surface(
            &mut output,
            FooterStatus::new(
                "codex",
                &PolicyState::new("current"),
                "managed",
                "connected",
                (ROWS, COLS),
            ),
            Some(ViewportRepair::Full),
            true,
        )
        .unwrap();
        forward_agent_output(
            &mut output,
            &mut observer,
            b"\x1b[r\x1b[1;1H1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\r\n10\r\n",
            FooterStatus::new(
                "codex",
                &PolicyState::new("current"),
                "managed",
                "connected",
                (ROWS, COLS),
            ),
        )
        .unwrap();

        let mut terminal = vt100::Parser::new(ROWS, COLS, 0);
        terminal.process(&output);
        assert!(screen_row(&terminal, ROWS - 1, COLS).contains("Ctrl-] Control"));
    }

    #[test]
    fn synchronized_tui_mount_repairs_footer_after_frame_commit() {
        const ROWS: u16 = 8;
        const COLS: u16 = 80;
        let mut output = Vec::new();
        let mut observer = OutputObserver::default();
        repair_agent_surface(
            &mut output,
            FooterStatus::new(
                "codex",
                &PolicyState::new("current"),
                "managed",
                "connected",
                (ROWS, COLS),
            ),
            Some(ViewportRepair::Full),
            true,
        )
        .unwrap();

        forward_agent_output(
            &mut output,
            &mut observer,
            b"\x1b[?2026h\x1b[?1049h\x1b[2J\x1b[8;1HCODEX FRAME\x1b[?2026l",
            FooterStatus::new(
                "codex",
                &PolicyState::new("current"),
                "managed",
                "connected",
                (ROWS, COLS),
            ),
        )
        .unwrap();

        let mut terminal = vt100::Parser::new(ROWS, COLS, 0);
        terminal.process(&output);
        assert!(screen_row(&terminal, ROWS - 1, COLS).contains("Ctrl-] Control"));
    }

    #[test]
    fn recognizes_legacy_and_enhanced_control_chords() {
        assert_eq!(find_control_chord(b"a\x1db"), Some((1, 1)));
        assert_eq!(find_control_chord(b"a\x1b[93;5ub"), Some((1, 7)));
        assert_eq!(find_control_chord(b"\x1b[27;5;93~"), Some((0, 10)));
        assert_eq!(find_control_chord(b"plain input"), None);
        assert_eq!(trailing_control_prefix(b"abc\x1b[93;"), 5);
        assert_eq!(trailing_control_prefix(b"plain input"), 0);
    }

    #[test]
    fn control_input_decodes_presses_and_discards_kitty_releases() {
        let mut pending = Vec::new();
        let decoded = decode_control_input(
            &mut pending,
            b"/\x1b[47;1:3us\x1b[115;1:3u\x1b[115;1u\x1b[13;1u",
        );
        assert_eq!(decoded, b"/ss\r");
        assert!(pending.is_empty());

        assert!(decode_control_input(&mut pending, b"\x1b[11").is_empty());
        assert_eq!(decode_control_input(&mut pending, b"5;1us"), b"ss");
        assert_eq!(decode_control_input(&mut pending, b"\x1b[93;5u"), [CONTROL]);
        assert!(decode_control_input(&mut pending, b"\x1b[93;5:3u").is_empty());
        assert_eq!(
            decode_control_input(&mut pending, b"\x1b[57352;1u"),
            b"\x1b[A"
        );
        assert_eq!(decode_control_input(&mut pending, b"\x1b[1;1B"), b"\x1b[B");
        assert_eq!(
            decode_control_input(&mut pending, b"\x1b[57354;1u"),
            b"\x1b[5~"
        );
        assert_eq!(decode_control_input(&mut pending, b"\x1b[6~"), b"\x1b[6~");
        assert_eq!(decode_control_input(&mut pending, b"\x1bOA"), b"\x1b[A");
        assert_eq!(decode_control_input(&mut pending, b"\x1bOB"), b"\x1b[B");
        assert!(decode_control_input(&mut pending, b"\x1bO").is_empty());
        assert_eq!(decode_control_input(&mut pending, b"C"), b"\x1b[C");
        assert_eq!(decode_control_input(&mut pending, b"\x03"), [INTERRUPT]);
        assert_eq!(
            decode_control_input(&mut pending, b"\x1b[99;5u"),
            [INTERRUPT]
        );
        assert!(decode_control_input(&mut pending, b"\x1b[57353;1:2u").is_empty());
        assert!(decode_control_input(&mut pending, b"\x1b[115;1:2u").is_empty());
        assert!(decode_control_input(&mut pending, b"\x1b[99;5:3u").is_empty());
    }

    #[test]
    fn command_palette_filters_and_prioritizes_prefixes() {
        let all = command_suggestions("/", false);
        assert_eq!(all.len(), COMMANDS.len() - 1);
        assert_eq!(command_suggestions("/sta", false)[0].name, "/status");
        assert_eq!(command_suggestions("/way", false)[0].name, "/gateway");
        assert!(command_suggestions("status", false).is_empty());
        assert!(command_suggestions("/quit yes", false).is_empty());
        assert!(command_suggestions("/dir", false).is_empty());
        assert_eq!(command_suggestions("/dir", true)[0].name, "/direct");
    }

    #[test]
    fn slash_command_arguments_are_rejected_as_feedback_shortcuts() {
        assert_eq!(
            slash_command_with_arguments("/agent codex", false),
            Some("/agent")
        );
        assert_eq!(
            slash_command_with_arguments("/quit yes", false),
            Some("/quit")
        );
        assert_eq!(
            slash_command_with_arguments("/status extra", false),
            Some("/status")
        );
        assert_eq!(slash_command_with_arguments("/unknown value", false), None);
        assert_eq!(slash_command_with_arguments("/agent", false), None);
        assert_eq!(slash_command_with_arguments("/direct now", false), None);
        assert_eq!(
            slash_command_with_arguments("/direct now", true),
            Some("/direct")
        );
    }

    #[test]
    fn confirmation_prompts_default_to_cancel_and_support_keyboard_navigation() {
        let mut prompt =
            confirmation_prompt("Quit?", "Quit Blue", "end the agent", PromptAction::Quit);
        assert_eq!(
            prompt.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(PromptAction::Cancel)
        );

        assert_eq!(
            prompt.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            None
        );
        assert_eq!(prompt.selected, 1);
        assert_eq!(
            prompt.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(PromptAction::Quit)
        );
        assert_eq!(
            prompt.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(PromptAction::Cancel)
        );
    }

    #[test]
    fn missing_agents_get_guidance_but_no_tui_selection() {
        let options = commands::AgentOptions {
            eligible: vec![],
            needs_repair: vec!["claude".into()],
            needs_install: vec!["codex".into()],
            current: None,
        };
        assert!(agent_selector(&options).is_none());
        assert!(agent_install_guidance(&[]).is_none());
        let guidance = agent_install_guidance(&options.needs_install).unwrap();
        assert!(guidance.contains("Not installed: codex"));
        assert!(guidance.contains(if cfg!(windows) {
            "unavailable on Windows"
        } else {
            "blue agent codex"
        }));
        assert!(agent_repair_guidance(&options.needs_repair)
            .unwrap()
            .contains("claude"));
    }

    #[test]
    fn agent_repair_guidance_handles_one_or_many_agents() {
        assert_eq!(agent_repair_guidance(&[]), None);
        assert_eq!(
            agent_repair_guidance(&["codex".into()]).as_deref(),
            Some(
                "Version repair required for: codex. Run `blue agent codex` outside Blue to install one."
            )
        );
        assert_eq!(
            agent_repair_guidance(&["codex".into(), "claude".into()]).as_deref(),
            Some(
                "Version repair required for: codex, claude. Run `blue agent codex` outside Blue to install one."
            )
        );
    }

    #[test]
    fn agent_selector_is_absent_when_only_repairs_are_available() {
        let repair_only = commands::AgentOptions {
            eligible: Vec::new(),
            needs_repair: vec!["codex".into()],
            needs_install: vec!["kimi".into()],
            current: Some("codex".into()),
        };
        assert!(agent_selector(&repair_only).is_none());

        let mixed = commands::AgentOptions {
            eligible: vec!["claude".into()],
            needs_repair: vec!["codex".into()],
            needs_install: vec!["kimi".into()],
            current: Some("claude".into()),
        };
        let selector = agent_selector(&mixed).unwrap();
        assert_eq!(selector.options.len(), 1);
        assert_eq!(selector.options[0].label, "claude");
        assert_eq!(selector.selected, 0);
    }

    #[test]
    fn remote_resume_session_list_is_bounded_and_scrollable() {
        let sessions = (0..100)
            .map(|index| commands::RemoteSession {
                id: format!("id-{index}"),
                harness: "codex".into(),
                native_session_id: format!("native-{index}"),
                title: Some(format!("Session {index}")),
                summary: Some("preview".into()),
                cwd: Some("/workspace".into()),
                user_email: "owner@example.com".into(),
                updated_at: "2026-09-05T00:00:00Z".into(),
                shared: false,
            })
            .collect();
        let mut wizard = ResumeWizard::new(sessions).unwrap();
        assert_eq!(wizard.sessions.len(), 100);
        wizard.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(wizard.selected, 1);
        wizard.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(wizard.selected, 9);
        wizard.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(wizard.selected, 99);
        wizard.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(wizard.selected, 0);
        assert_eq!(
            COMMANDS
                .iter()
                .find(|command| command.name == "/resume")
                .unwrap()
                .description,
            "Resume an owned or shared remote session"
        );
    }

    #[test]
    fn agent_reload_prompt_keeps_the_running_session_by_default() {
        let mut prompt = ControlPrompt::new(
            "Reload the agent now?",
            vec![
                option(
                    "Keep current session",
                    "use the new default next time",
                    PromptAction::KeepSession,
                ),
                option(
                    "Quit and reload now",
                    "stop the current agent",
                    PromptAction::ReloadAgent("codex".into()),
                ),
            ],
        )
        .escape_as(PromptAction::KeepSession);

        assert_eq!(
            prompt.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(PromptAction::KeepSession)
        );
        assert_eq!(
            prompt.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(PromptAction::KeepSession)
        );
        prompt.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            prompt.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(PromptAction::ReloadAgent("codex".into()))
        );
    }

    #[test]
    fn mode_reload_prompt_keeps_the_running_session_by_default() {
        let mut prompt = mode_reload_prompt(true);

        assert_eq!(
            prompt.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(PromptAction::KeepModeSession(true))
        );
        assert_eq!(
            prompt.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(PromptAction::KeepModeSession(true))
        );
        prompt.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            prompt.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(PromptAction::ReloadCurrent)
        );
    }

    #[test]
    fn control_editor_completes_selected_command_and_keeps_history() {
        let mut editor = ControlEditor::default();
        for character in "/sta".chars() {
            assert!(editor
                .handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .is_none());
        }
        editor.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(editor.value(), "/status");
        assert_eq!(
            editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some("/status".to_owned())
        );
        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(editor.value(), "/status");
    }

    #[test]
    fn control_navigation_debounces_duplicate_arrow_reports() {
        let mut debounce = NavigationDebounce::default();
        let started = Instant::now();
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);

        assert!(debounce.accept_at(down, started));
        assert!(!debounce.accept_at(down, started + Duration::from_millis(20)));
        assert!(debounce.accept_at(down, started + NAVIGATION_DEBOUNCE));
    }

    #[test]
    fn every_navigable_control_debounces_its_own_arrow_input() {
        let mut editor = ControlEditor::default();
        let mut prompt =
            confirmation_prompt("Quit?", "Quit Blue", "end the agent", PromptAction::Quit);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);

        editor.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        editor.handle_key(down);
        editor.handle_key(down);
        assert_eq!(editor.selected, 1);

        prompt.handle_key(down);
        prompt.handle_key(down);
        assert_eq!(prompt.selected, 1);
    }

    #[test]
    fn resume_destination_offers_current_recorded_custom_and_cancel() {
        let root = std::env::temp_dir().join(format!("blue-resume-ui-{}", uuid::Uuid::new_v4()));
        let current = root.join("current");
        let recorded = root.join("recorded");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::create_dir_all(&recorded).unwrap();
        let mut wizard =
            ResumeWizard::new(vec![remote_session(Some(recorded.display().to_string()))]).unwrap();
        wizard.current = current;

        assert!(matches!(
            wizard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ResumeEffect::None
        ));
        assert!(matches!(wizard.step, ResumeStep::Destination));
        assert!(matches!(
            wizard.destination_choices().as_slice(),
            [
                DestinationChoice::Current,
                DestinationChoice::Recorded,
                DestinationChoice::Custom,
                DestinationChoice::Cancel
            ]
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resume_confirmation_keeps_the_active_session_by_default() {
        let mut wizard = ResumeWizard::new(vec![remote_session(None)]).unwrap();
        wizard.step = ResumeStep::Confirm;

        assert!(matches!(
            wizard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ResumeEffect::Cancel
        ));
        wizard.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(
            wizard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ResumeEffect::Finish
        ));
    }

    #[test]
    fn invalid_custom_resume_path_stays_editable() {
        let mut wizard = ResumeWizard::new(vec![remote_session(None)]).unwrap();
        wizard.step = ResumeStep::CustomPath;
        wizard.path_input = Input::new("/a/directory/that/does/not/exist".into());

        assert!(matches!(
            wizard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ResumeEffect::None
        ));
        assert!(matches!(wizard.step, ResumeStep::CustomPath));
        assert!(wizard
            .error
            .as_deref()
            .unwrap()
            .contains("existing directory"));
    }

    #[test]
    fn resume_session_picker_replaces_the_control_surface() {
        #[derive(Clone)]
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let output = Arc::new(Mutex::new(Vec::new()));
        let backend = CrosstermBackend::new(SharedWriter(output.clone()));
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 100, 24)),
            },
        )
        .unwrap();
        let editor = ControlEditor::default();
        let mut second = remote_session(None);
        second.id = "remote-2".into();
        second.native_session_id = "native-2".into();
        second.title = Some("Second resumable session".into());
        let mut wizard = ResumeWizard::new(vec![remote_session(None), second]).unwrap();

        draw_control(
            &mut terminal,
            "codex",
            (&editor, None, Some(&wizard)),
            &["OLD CONTROL TRANSCRIPT".into()],
            &PolicyState::new("current"),
            "managed",
            ("connected", false),
        )
        .unwrap();
        let mut screen = vt100::Parser::new(24, 100, 0);
        screen.process(&output.lock().unwrap());
        let contents = screen.screen().contents();
        assert!(contents.contains("Resume session"));
        assert!(contents.contains("Remote sessions"));
        assert!(!contents.contains("OLD CONTROL TRANSCRIPT"));
        assert!(!contents.contains(" Command "));
        assert!(contents.contains("▸ Investigate terminal rendering"));
        assert!(!contents.contains("Investigate terminal rendering · codex"));
        assert!(contents.contains("codex · owned by you · unknown directory"));

        wizard.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        draw_control(
            &mut terminal,
            "codex",
            (&editor, None, Some(&wizard)),
            &["OLD CONTROL TRANSCRIPT".into()],
            &PolicyState::new("current"),
            "managed",
            ("connected", false),
        )
        .unwrap();
        let mut moved = vt100::Parser::new(24, 100, 0);
        moved.process(&output.lock().unwrap());
        let moved_contents = moved.screen().contents();
        assert_eq!(wizard.selected, 1);
        assert!(moved_contents.contains("2/2 selected"));
        assert!(moved_contents.contains("▸ Second resumable session"));
        assert!(!moved_contents.contains("Second resumable session · codex"));
    }

    #[test]
    fn resume_review_requires_the_explicit_end_session_action() {
        #[derive(Clone)]
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let output = Arc::new(Mutex::new(Vec::new()));
        let backend = CrosstermBackend::new(SharedWriter(output.clone()));
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 100, 24)),
            },
        )
        .unwrap();
        let editor = ControlEditor::default();
        let mut wizard = ResumeWizard::new(vec![remote_session(None)]).unwrap();
        wizard.destination = Some(wizard.current.clone());
        wizard.prepared(commands::PreparedRemoteSession {
            bundle: gh_config::session_bundle::VerifiedBundle {
                manifest: gh_config::session_bundle::BundleManifest {
                    schema_version: 1,
                    artifact_format: gh_config::session_bundle::ARTIFACT_FORMAT.into(),
                    harness: "codex".into(),
                    compatibility_profile: "codex-v0_145_0".into(),
                    native_session_id: "native-1".into(),
                    captured_at_unix_ms: 0,
                    cwd: None,
                    repository: None,
                    title: None,
                    summary: None,
                    files: Vec::new(),
                },
                files: std::collections::BTreeMap::new(),
            },
            destination: wizard.current.clone(),
            recorded_repository: Some(gh_config::session_bundle::RepositoryIdentity {
                root: Some("/recorded".into()),
                remote: Some("git@example.com:team/recorded.git".into()),
            }),
            destination_repository: Some(gh_config::session_bundle::RepositoryIdentity {
                root: Some("/destination".into()),
                remote: Some("git@example.com:team/destination.git".into()),
            }),
        });

        assert!(matches!(wizard.step, ResumeStep::RepositoryMismatch));

        draw_control(
            &mut terminal,
            "codex",
            (&editor, None, Some(&wizard)),
            &[],
            &PolicyState::new("current"),
            "managed",
            ("connected", false),
        )
        .unwrap();
        let mut screen = vt100::Parser::new(24, 100, 0);
        screen.process(&output.lock().unwrap());
        let contents = screen.screen().contents();
        assert!(contents.contains("Resume in different repository?"));
        assert!(contents.contains("Continue with selected destination"));
        assert!(contents.contains("Repository mismatch"));
        assert!(!contents.contains("End active agent session?"));

        wizard.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        wizard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(wizard.step, ResumeStep::Confirm));
        draw_control(
            &mut terminal,
            "codex",
            (&editor, None, Some(&wizard)),
            &[],
            &PolicyState::new("current"),
            "managed",
            ("connected", false),
        )
        .unwrap();
        let mut confirmation = vt100::Parser::new(24, 100, 0);
        confirmation.process(&output.lock().unwrap());
        let contents = confirmation.screen().contents();
        assert!(contents.contains("End active agent session?"));
        assert!(contents.contains("Keep current session"));
        assert!(contents.contains("End current session and resume"));
        assert!(!contents.contains("resume in different repository"));
        assert_eq!(wizard.selected, 0);
    }

    #[test]
    fn command_palette_navigation_wraps_at_both_ends() {
        let mut editor = ControlEditor::default();
        editor.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));

        editor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(editor.selected, COMMANDS.len() - 2);

        editor.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(editor.selected, 0);
    }

    #[test]
    fn new_command_replaces_previous_command_output() {
        let mut output = vec!["› /status".to_owned(), "Overall: up to date".to_owned()];
        begin_command_output(&mut output, "/doctor");
        append_transcript(&mut output, "blue doctor");

        assert_eq!(output, ["› /doctor", "blue doctor"]);
    }

    #[test]
    fn unchanged_control_frames_are_updated_in_place() {
        #[derive(Clone)]
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let output = Arc::new(Mutex::new(Vec::new()));
        let backend = CrosstermBackend::new(SharedWriter(output.clone()));
        let mut terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 100, 24)),
            },
        )
        .unwrap();
        let editor = ControlEditor::default();
        let transcript = vec!["Agent output is buffered.".to_owned()];

        draw_control(
            &mut terminal,
            "codex",
            (&editor, None, None),
            &transcript,
            &PolicyState::new("current"),
            "managed",
            ("connected", false),
        )
        .unwrap();
        let first_frame_bytes = output.lock().unwrap().len();
        let first_frame = output.lock().unwrap().clone();
        let mut screen = vt100::Parser::new(24, 100, 0);
        screen.process(&first_frame);
        let contents = screen.screen().contents();
        assert!(contents.contains("Agent output is buffered."));
        assert!(contents.contains("Ctrl+C / Ctrl-] agent"));
        assert!(contents.contains("policy current"));
        draw_control(
            &mut terminal,
            "codex",
            (&editor, None, None),
            &transcript,
            &PolicyState::new("current"),
            "managed",
            ("connected", false),
        )
        .unwrap();
        let second_frame_bytes = output.lock().unwrap().len() - first_frame_bytes;

        assert!(first_frame_bytes > 500);
        // Crossterm's cursor/style bookkeeping varies slightly by platform.
        // An unchanged frame should still be only a small fraction of the
        // initial render rather than depending on an exact ANSI byte count.
        assert!(
            second_frame_bytes < first_frame_bytes / 10,
            "unchanged redraw wrote {second_frame_bytes} bytes after a {first_frame_bytes}-byte initial frame"
        );
    }

    #[test]
    fn startup_frame_is_padded_and_respects_no_color() {
        let view = StartupView {
            agent: "codex".into(),
            agent_status: "ready".into(),
            connection: "connected · https://control.example".into(),
            identity: "signed in · dev@example.com".into(),
            policy: "current · revision".into(),
            activity: "Starting codex…  Hold Ctrl and press ] for controls.".into(),
            drawn: false,
        };
        let plain = view.frame(false);
        assert!(plain.starts_with("\r\x1b[2K\r\n\r\x1b[2K\r\n\r\x1b[2K   ____  _\r\n"));
        assert!(plain.contains(&format!("Metaharness v{}", gh_common::blue_version())));
        assert!(plain.contains("Connection"));
        assert!(plain.contains("Starting codex…  Hold Ctrl and press ] for controls."));
        assert!(!plain.contains("\x1b[36m"));
        assert!(view.frame(true).contains("\x1b[36m ____  _\x1b[0m"));
    }

    #[test]
    fn startup_updates_replace_a_single_screen_region() {
        let mut terminal = vt100::Parser::new(24, 120, 100);
        let mut view = StartupView {
            agent: "codex".into(),
            agent_status: "checking…".into(),
            connection: "checking…".into(),
            identity: "checking…".into(),
            policy: "checking…".into(),
            activity: "Preparing your workspace…".into(),
            drawn: false,
        };
        terminal.process(b"\x1b[2J\x1b[H");
        terminal.process(view.frame(false).as_bytes());
        view.connection = "connected · http://127.0.0.1:8080".into();
        view.identity = "signed in · dev@example.com".into();
        view.policy = "current · d20a39e0".into();
        view.agent_status = "ready".into();
        view.activity = "Starting codex…  Hold Ctrl and press ] for controls.".into();
        terminal.process(b"\x1b[H");
        terminal.process(view.frame(false).as_bytes());

        let screen = terminal.screen().contents();
        assert_eq!(screen.matches("Metaharness v").count(), 1);
        assert!(!screen.contains("checking…"));
        assert!(screen.contains("current · d20a39e0"));
    }
}
