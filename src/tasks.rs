//! Running Claude Code sessions, shown as project chips beside the usage.
//!
//! Claude Code hooks run `claude-code-usage-monitor.exe --hook`, which records
//! each session's state in `%APPDATA%\ClaudeCodeUsageMonitor\tasks`. The widget
//! reads those files, groups sessions by project, and drops sessions whose
//! Claude Code process has exited.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use windows::core::BOOL;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HWND, LPARAM, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateFontW, DeleteDC, DeleteObject, GetTextExtentPoint32W, SelectObject,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE, FW_MEDIUM,
    OUT_TT_PRECIS,
};
use windows::Win32::System::Console::{AttachConsole, FreeConsole, GetConsoleWindow};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow,
    GA_ROOTOWNER, GW_OWNER, SW_RESTORE,
};

use crate::theme_engine::DataContext;

/// Project chip slots defined by the theme. `attention_chips`,
/// `working_chips` and `done_chips` in settings.json cap each group (default
/// [`DEFAULT_LIMITS`]); extra projects collapse into a `+N` chip per group.
pub const SLOTS: usize = 3 * MAX_PER_GROUP;
pub const MAX_PER_GROUP: usize = 6;
pub const DEFAULT_LIMITS: Limits = Limits {
    attention: 4,
    working: 4,
    done: 3,
};
const CHIP_HEIGHT: f64 = 18.0;
const CHIP_GAP: f64 = 4.0;
const BLOCK_TRAILING_GAP: f64 = 10.0;
const MAX_LABEL_CHARS: usize = 40;
const CHIP_FONT_SIZE: i32 = 11;
/// A session reported as working with no activity for this long was most
/// likely interrupted (Claude Code sends no Stop hook for Esc).
const STALE_WORKING: Duration = Duration::from_secs(30 * 60);
const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Idle,
    Finished,
    Working,
    Waiting,
    Error,
    Permission,
}

impl TaskStatus {
    /// Theme value: 1 idle/finished (grey), 2 working (green), 3 waiting
    /// (amber), 4 permission or error (red).
    fn level(self) -> u8 {
        match self {
            Self::Idle | Self::Finished => 1,
            Self::Working => 2,
            Self::Waiting => 3,
            Self::Permission | Self::Error => 4,
        }
    }

    /// Segoe Fluent Icons glyph, so status reads without relying on colour.
    fn icon(self) -> char {
        match self {
            Self::Idle => '\u{E769}',       // Pause
            Self::Finished => '\u{E73E}',   // CheckMark
            Self::Working => '\u{E895}',    // Sync
            Self::Waiting => '\u{E897}',    // Help (question mark)
            Self::Error => '\u{E7BA}',      // Warning
            Self::Permission => '\u{E72E}', // Lock
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Finished => "Finished",
            Self::Working => "Working",
            Self::Waiting => "Waiting for you",
            Self::Error => "Error",
            Self::Permission => "Permission needed",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TaskRecord {
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub transcript_path: String,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub subagents: u32,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub pid_created: u64,
    /// Hook start time; out-of-order async hooks never roll the status back.
    #[serde(default)]
    pub updated_ms: u64,
}

fn tasks_directory() -> PathBuf {
    crate::app_settings::app_data_directory().join("tasks")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

fn safe_session_id(id: &str) -> Option<&str> {
    (!id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
    .then_some(id)
}

// ---------------------------------------------------------------------------
// Hook mode
// ---------------------------------------------------------------------------

/// Entry point for `--hook`. Never fails loudly: Claude Code must not be
/// disturbed by the monitor.
pub fn run_hook() {
    let started_ms = now_ms();
    let mut input = String::new();
    if std::io::stdin()
        .take(4 * 1024 * 1024)
        .read_to_string(&mut input)
        .is_err()
    {
        return;
    }
    let Ok(event) = serde_json::from_str::<Value>(&input) else {
        return;
    };
    let _ = record_hook_event(&tasks_directory(), &event, started_ms, claude_process);
}

fn text<'a>(event: &'a Value, key: &str) -> &'a str {
    event.get(key).and_then(Value::as_str).unwrap_or("")
}

fn record_hook_event(
    directory: &Path,
    event: &Value,
    started_ms: u64,
    find_process: impl FnOnce() -> Option<(u32, u64)>,
) -> std::io::Result<()> {
    let Some(session_id) = safe_session_id(text(event, "session_id")) else {
        return Ok(());
    };
    std::fs::create_dir_all(directory)?;
    let path = directory.join(format!("{session_id}.json"));
    let lock = std::fs::File::create(directory.join(format!("{session_id}.lock")))?;
    lock.lock()?;
    let name = text(event, "hook_event_name");
    if name == "SessionEnd" {
        let _ = std::fs::remove_file(&path);
        drop(lock);
        let _ = std::fs::remove_file(directory.join(format!("{session_id}.lock")));
        return Ok(());
    }
    let mut record = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<TaskRecord>(&bytes).ok())
        .unwrap_or_else(|| TaskRecord {
            session_id: session_id.to_string(),
            ..TaskRecord::default()
        });
    if record.pid == 0 || name == "SessionStart" {
        if let Some((pid, created)) = find_process() {
            record.pid = pid;
            record.pid_created = created;
        }
    }
    for (key, field) in [
        ("cwd", &mut record.cwd),
        ("transcript_path", &mut record.transcript_path),
    ] {
        let value = text(event, key);
        if !value.is_empty() {
            *field = value.to_string();
        }
    }
    let model = text(event, "model");
    if !model.is_empty() {
        record.model = model.to_string();
    } else if record.model.is_empty() || name == "Stop" {
        if let Some(model) = last_transcript_model(Path::new(&record.transcript_path)) {
            record.model = model;
        }
    }
    match name {
        "SubagentStart" => record.subagents = record.subagents.saturating_add(1),
        "SubagentStop" => record.subagents = record.subagents.saturating_sub(1),
        "SessionStart" if text(event, "source") != "compact" => record.subagents = 0,
        _ => {}
    }
    if started_ms >= record.updated_ms {
        apply_status(&mut record, name, event);
        record.updated_ms = started_ms;
    }
    let json = serde_json::to_vec_pretty(&record).map_err(std::io::Error::other)?;
    let temporary = directory.join(format!(".{session_id}.{}.tmp", std::process::id()));
    std::fs::write(&temporary, json)?;
    std::fs::rename(&temporary, &path)?;
    Ok(())
}

fn apply_status(record: &mut TaskRecord, name: &str, event: &Value) {
    let tool = text(event, "tool_name");
    match name {
        // Auto-compaction restarts the session mid-turn; keep its status.
        "SessionStart" if text(event, "source") == "compact" => {}
        "SessionStart" => {
            record.status = TaskStatus::Idle;
            record.detail.clear();
        }
        "UserPromptSubmit" => {
            record.status = TaskStatus::Working;
            let prompt = first_line(text(event, "prompt"));
            if !prompt.is_empty() {
                record.task = prompt;
            }
            record.detail.clear();
        }
        "PreToolUse" if tool == "AskUserQuestion" => {
            record.status = TaskStatus::Waiting;
            record.detail = "Asked you a question".into();
        }
        "PreToolUse" => {
            record.status = TaskStatus::Working;
            record.detail = tool_summary(tool, event);
        }
        "PostToolUse" | "PostToolUseFailure" => record.status = TaskStatus::Working,
        "PermissionRequest" => {
            record.status = TaskStatus::Permission;
            record.detail = format!("Wants to run {}", tool_summary(tool, event));
        }
        "Notification" => match text(event, "notification_type") {
            "permission_prompt" => {
                record.status = TaskStatus::Permission;
                let message = text(event, "message");
                if !message.is_empty() && record.detail.is_empty() {
                    record.detail = first_line(message);
                }
            }
            "elicitation_dialog" => {
                record.status = TaskStatus::Waiting;
                record.detail = first_line(text(event, "message"));
            }
            _ => {}
        },
        "Stop" => {
            record.status = TaskStatus::Finished;
            record.detail.clear();
        }
        "StopFailure" => {
            record.status = TaskStatus::Error;
            record.detail = first_line(text(event, "error"));
        }
        _ => {}
    }
}

fn tool_summary(tool: &str, event: &Value) -> String {
    let input = event.get("tool_input");
    let argument = ["command", "file_path", "pattern", "description", "url"]
        .iter()
        .find_map(|key| {
            input
                .and_then(|input| input.get(*key))
                .and_then(Value::as_str)
        })
        .map(first_line)
        .unwrap_or_default();
    match (tool.is_empty(), argument.is_empty()) {
        (true, _) => argument,
        (false, true) => tool.to_string(),
        (false, false) => format!("{tool}: {argument}"),
    }
}

fn first_line(value: &str) -> String {
    let line = value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    truncate(line, 70)
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// The last `"model":"…"` in the transcript tail, which is the model of the
/// latest response.
fn last_transcript_model(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    let marker = "\"model\":\"";
    let mut search = tail.as_ref();
    let mut found = None;
    while let Some(index) = search.find(marker) {
        let rest = &search[index + marker.len()..];
        if let Some(end) = rest.find('"') {
            let model = &rest[..end];
            if model.starts_with("claude-") {
                found = Some(model.to_string());
            }
        }
        search = rest;
    }
    found
}

/// `claude-opus-5-5` → `Opus 5.5`; dates and `[1m]` suffixes are dropped.
pub fn model_display_name(model: &str) -> String {
    let base = model.split('[').next().unwrap_or(model);
    let base = base.strip_prefix("claude-").unwrap_or(base);
    let mut parts = base.split('-');
    let Some(family) = parts.next().filter(|family| !family.is_empty()) else {
        return model.to_string();
    };
    let version: Vec<&str> = parts
        .filter(|part| part.len() <= 2 && part.bytes().all(|byte| byte.is_ascii_digit()))
        .collect();
    let mut name = family[..1].to_ascii_uppercase() + &family[1..];
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    name
}

// ---------------------------------------------------------------------------
// Processes and windows
// ---------------------------------------------------------------------------

struct ProcessInfo {
    parent: u32,
    name: String,
}

fn process_table() -> HashMap<u32, ProcessInfo> {
    let mut table = HashMap::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return table;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snapshot, &mut entry).is_ok();
        while ok {
            let length = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            table.insert(
                entry.th32ProcessID,
                ProcessInfo {
                    parent: entry.th32ParentProcessID,
                    name: String::from_utf16_lossy(&entry.szExeFile[..length]).to_ascii_lowercase(),
                },
            );
            ok = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    table
}

fn process_created(pid: u32) -> Option<u64> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let result = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
        let _ = CloseHandle(handle);
        result.ok()?;
        Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}

/// The Claude Code process that ran this hook: the nearest ancestor named
/// `claude*`, else the direct parent.
fn claude_process() -> Option<(u32, u64)> {
    let table = process_table();
    let parent = table.get(&std::process::id())?.parent;
    let mut pid = parent;
    for _ in 0..8 {
        let Some(info) = table.get(&pid) else {
            break;
        };
        if info.name.starts_with("claude") {
            return Some((pid, process_created(pid)?));
        }
        pid = info.parent;
    }
    Some((parent, process_created(parent)?))
}

fn process_alive(pid: u32, created: u64) -> bool {
    pid != 0 && process_created(pid).is_some_and(|actual| created == 0 || actual == created)
}

struct WindowSearch {
    pid: u32,
    windows: Vec<(HWND, String)>,
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = &mut *(lparam.0 as *mut WindowSearch);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == search.pid
        && IsWindowVisible(hwnd).as_bool()
        && GetWindow(hwnd, GW_OWNER).map_or(true, |owner| owner.is_invalid())
    {
        let length = GetWindowTextLengthW(hwnd);
        if length > 0 {
            let mut buffer = vec![0u16; length as usize + 1];
            let copied = GetWindowTextW(hwnd, &mut buffer);
            search
                .windows
                .push((hwnd, String::from_utf16_lossy(&buffer[..copied as usize])));
        }
    }
    BOOL(1)
}

fn top_level_windows(pid: u32) -> Vec<(HWND, String)> {
    let mut search = WindowSearch {
        pid,
        windows: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(collect_window),
            LPARAM(&mut search as *mut WindowSearch as isize),
        );
    }
    search.windows
}

/// The visible window of a console session: the classic console window, or
/// the Windows Terminal window that owns its pseudo-console.
fn console_host_window(pid: u32) -> Option<HWND> {
    unsafe {
        let _ = FreeConsole();
        if AttachConsole(pid).is_err() {
            return None;
        }
        let console = GetConsoleWindow();
        let _ = FreeConsole();
        if console.is_invalid() {
            return None;
        }
        [GetAncestor(console, GA_ROOTOWNER), console]
            .into_iter()
            .find(|window| !window.is_invalid() && IsWindowVisible(*window).as_bool())
    }
}

/// Processes whose windows are never a session's host.
const SHELL_PROCESSES: [&str; 6] = [
    "explorer.exe",
    "svchost.exe",
    "services.exe",
    "wininit.exe",
    "sihost.exe",
    "userinit.exe",
];

fn host_window(record: &TaskRecord) -> Option<HWND> {
    if let Some(window) = console_host_window(record.pid) {
        return Some(window);
    }
    let project = project_name(&record.cwd).to_lowercase();
    let table = process_table();
    let mut pid = record.pid;
    for _ in 0..10 {
        let info = table.get(&pid)?;
        if SHELL_PROCESSES.contains(&info.name.as_str()) {
            return None;
        }
        let windows = top_level_windows(pid);
        let best = windows
            .iter()
            .find(|(_, title)| !project.is_empty() && title.to_lowercase().contains(&project))
            .or_else(|| windows.first());
        if let Some((hwnd, _)) = best {
            return Some(*hwnd);
        }
        if info.parent == 0 || info.parent == pid {
            return None;
        }
        pid = info.parent;
    }
    None
}

/// Bring forward the window hosting a session: its terminal, editor, or the
/// Claude desktop app. Windows Terminal tabs cannot be selected from outside,
/// so a shared terminal window comes forward on whichever tab it shows.
fn focus_session(record: &TaskRecord) -> bool {
    if !process_alive(record.pid, record.pid_created) {
        return false;
    }
    let Some(hwnd) = host_window(record) else {
        return false;
    };
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd).as_bool()
    }
}

// ---------------------------------------------------------------------------
// Snapshot used by the widget
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ProjectGroup {
    pub name: String,
    pub tasks: Vec<TaskRecord>,
    pub status: TaskStatus,
    pub last_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub groups: Vec<ProjectGroup>,
}

struct Cache {
    loaded: Option<Instant>,
    snapshot: Snapshot,
    signature: Vec<(String, TaskStatus, u32, u64)>,
    limits: Limits,
    /// Theme bindings for the current snapshot. Rendering, hit-testing and
    /// tooltips build a data context many times per refresh, so the layout
    /// is computed once here instead.
    bindings: Option<DataContext>,
    /// settings.json modification time behind `limits`.
    settings_modified: Option<SystemTime>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    loaded: None,
    snapshot: Snapshot { groups: Vec::new() },
    signature: Vec::new(),
    limits: DEFAULT_LIMITS,
    bindings: None,
    settings_modified: None,
});

fn settings_modified() -> Option<SystemTime> {
    std::fs::metadata(crate::app_settings::settings_path())
        .and_then(|meta| meta.modified())
        .ok()
}

fn configured_limits() -> Limits {
    let settings = crate::app_settings::load_settings();
    Limits {
        attention: settings
            .attention_chips
            .unwrap_or(DEFAULT_LIMITS.attention)
            .min(MAX_PER_GROUP),
        working: settings
            .working_chips
            .unwrap_or(DEFAULT_LIMITS.working)
            .min(MAX_PER_GROUP),
        done: settings
            .done_chips
            .unwrap_or(DEFAULT_LIMITS.done)
            .min(MAX_PER_GROUP),
    }
}

/// Project folder name; worktrees under `.claude\worktrees` count as their
/// repository.
pub fn project_name(cwd: &str) -> String {
    let normalized = cwd.replace('/', "\\");
    let lower = normalized.to_lowercase();
    let root = match lower.find("\\.claude\\worktrees\\") {
        Some(index) => &normalized[..index],
        None => normalized.trim_end_matches('\\'),
    };
    root.rsplit('\\')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("Claude Code")
        .to_string()
}

fn effective_status(record: &TaskRecord, now: u64) -> TaskStatus {
    if record.status != TaskStatus::Working {
        return record.status;
    }
    let transcript_ms = std::fs::metadata(&record.transcript_path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0);
    let last = record.updated_ms.max(transcript_ms);
    if now.saturating_sub(last) > STALE_WORKING.as_millis() as u64 {
        TaskStatus::Idle
    } else {
        TaskStatus::Working
    }
}

fn load_snapshot(directory: &Path, alive: impl Fn(&TaskRecord) -> bool) -> Snapshot {
    let now = now_ms();
    let mut groups: Vec<ProjectGroup> = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Snapshot::default();
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(mut record) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<TaskRecord>(&bytes).ok())
        else {
            continue;
        };
        if !alive(&record) {
            // The session's Claude Code process is gone without a SessionEnd.
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(path.with_extension("lock"));
            continue;
        }
        record.status = effective_status(&record, now);
        let name = project_name(&record.cwd);
        match groups.iter_mut().find(|group| group.name == name) {
            Some(group) => group.tasks.push(record),
            None => groups.push(ProjectGroup {
                name,
                tasks: vec![record],
                status: TaskStatus::Idle,
                last_ms: 0,
            }),
        }
    }
    for group in &mut groups {
        group.tasks.sort_by(|a, b| {
            b.status
                .level()
                .cmp(&a.status.level())
                .then(b.updated_ms.cmp(&a.updated_ms))
        });
        group.status = group
            .tasks
            .iter()
            .map(|task| task.status)
            .max()
            .unwrap_or_default();
        group.last_ms = group
            .tasks
            .iter()
            .map(|task| task.updated_ms)
            .max()
            .unwrap_or(0);
    }
    groups.sort_by(|a, b| {
        b.status
            .level()
            .cmp(&a.status.level())
            .then(b.last_ms.cmp(&a.last_ms))
            .then(a.name.cmp(&b.name))
    });
    Snapshot { groups }
}

fn signature(snapshot: &Snapshot) -> Vec<(String, TaskStatus, u32, u64)> {
    snapshot
        .groups
        .iter()
        .map(|group| {
            (
                group.name.clone(),
                group.status,
                group.tasks.len() as u32,
                group.last_ms,
            )
        })
        .collect()
}

/// Reload the session files. Returns whether anything the widget draws
/// changed.
pub fn refresh() -> bool {
    let snapshot = load_snapshot(&tasks_directory(), |record| {
        process_alive(record.pid, record.pid_created)
    });
    let modified = settings_modified();
    let reload_settings = {
        let cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
        cache.loaded.is_none() || cache.settings_modified != modified
    };
    let limits = if reload_settings {
        configured_limits()
    } else {
        limits()
    };
    let bindings = bindings(&snapshot, limits);
    let mut cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    cache.settings_modified = modified;
    cache.bindings = Some(bindings);
    let signature = signature(&snapshot);
    let changed = cache.loaded.is_none() || cache.signature != signature;
    let limit_changed = cache.limits != limits;
    cache.limits = limits;
    let drawn_changed = limit_changed
        || changed
            && (cache.signature.len() != signature.len()
                || cache
                    .signature
                    .iter()
                    .zip(&signature)
                    .any(|(a, b)| a.0 != b.0 || a.1.level() != b.1.level() || a.2 != b.2));
    cache.snapshot = snapshot;
    cache.signature = signature;
    cache.loaded = Some(Instant::now());
    drawn_changed
}

pub fn refresh_interval_ms() -> u32 {
    REFRESH_INTERVAL.as_millis() as u32
}

fn limits() -> Limits {
    CACHE
        .lock()
        .map(|cache| cache.limits)
        .unwrap_or(DEFAULT_LIMITS)
}
fn snapshot() -> Snapshot {
    refresh_if_stale();
    CACHE
        .lock()
        .map(|cache| cache.snapshot.clone())
        .unwrap_or_default()
}

fn refresh_if_stale() {
    let stale = CACHE
        .lock()
        .map(|cache| {
            cache
                .loaded
                .is_none_or(|time| time.elapsed() > REFRESH_INTERVAL * 4)
        })
        .unwrap_or(true);
    if stale {
        refresh();
    }
}

thread_local! {
    static TEXT_WIDTHS: std::cell::RefCell<HashMap<String, f64>> =
        std::cell::RefCell::new(HashMap::new());
}

/// [`measure_text`], remembered per label.
fn text_width(label: &str) -> f64 {
    if let Some(width) = TEXT_WIDTHS.with(|cache| cache.borrow().get(label).copied()) {
        return width;
    }
    let width = measure_text(label);
    TEXT_WIDTHS.with(|cache| {
        let mut cache = cache.borrow_mut();
        // Labels come from project names; keep the table small.
        if cache.len() > 256 {
            cache.clear();
        }
        cache.insert(label.to_string(), width);
    });
    width
}

/// Width of `label` in the chip font at 96 DPI, measured with GDI like the
/// theme renderer draws it. Medium weight covers the bolder alert text.
fn measure_text(label: &str) -> f64 {
    let fallback = label.chars().count() as f64 * 7.0;
    if label.is_empty() {
        return 0.0;
    }
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return fallback;
        }
        let family: Vec<u16> = "Segoe UI".encode_utf16().chain(Some(0)).collect();
        let font = CreateFontW(
            -CHIP_FONT_SIZE,
            0,
            0,
            0,
            FW_MEDIUM.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(family.as_ptr()),
        );
        let previous = SelectObject(dc, font.into());
        let wide: Vec<u16> = label.encode_utf16().collect();
        let mut size = SIZE::default();
        let measured = GetTextExtentPoint32W(dc, &wide, &mut size).as_bool();
        SelectObject(dc, previous);
        let _ = DeleteObject(font.into());
        let _ = DeleteDC(dc);
        if measured {
            f64::from(size.cx)
        } else {
            fallback
        }
    }
}

fn chip_width(label: &str, with_dot: bool) -> f64 {
    // Text plus padding (and the status dot); a few pixels of slack keep
    // DrawText from ellipsizing at fractional DPI scales.
    text_width(label).ceil() + 4.0 + if with_dot { 24.0 } else { 14.0 }
}

struct ChipLayout {
    label: String,
    level: u8,
    icon: char,
    x: f64,
    y: f64,
    width: f64,
}

fn chip_label(group: &ProjectGroup) -> String {
    let name = truncate(&group.name, MAX_LABEL_CHARS);
    if group.tasks.len() > 1 {
        format!("{name} {}", group.tasks.len())
    } else {
        name
    }
}

/// A chip the user can hover or double-click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipTarget {
    /// The Nth project chip: attention, then working, then done chips.
    Slot(usize),
    /// Projects needing attention beyond the attention limit.
    More,
    /// Working projects beyond the working limit.
    Working,
    /// Finished or idle projects beyond the done limit.
    Done,
}

/// How many project chips each group may show before a `+N` chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub attention: usize,
    pub working: usize,
    pub done: usize,
}

/// Attention first: projects waiting on the user, then working, then finished
/// projects, each up to its limit with the rest in a `+N` chip.
struct Partition<'a> {
    attention: Vec<&'a ProjectGroup>,
    more: Vec<&'a ProjectGroup>,
    working: Vec<&'a ProjectGroup>,
    working_more: Vec<&'a ProjectGroup>,
    done: Vec<&'a ProjectGroup>,
    done_more: Vec<&'a ProjectGroup>,
}

fn partition(snapshot: &Snapshot, limits: Limits) -> Partition<'_> {
    let by_level = |keep: fn(u8) -> bool| {
        snapshot
            .groups
            .iter()
            .filter(|group| keep(group.status.level()))
            .collect::<Vec<_>>()
    };
    let mut attention = by_level(|level| level >= 3);
    let more = attention.split_off(attention.len().min(limits.attention));
    let mut working = by_level(|level| level == 2);
    let working_more = working.split_off(working.len().min(limits.working));
    let mut done = by_level(|level| level <= 1);
    let done_more = done.split_off(done.len().min(limits.done));
    Partition {
        attention,
        more,
        working,
        working_more,
        done,
        done_more,
    }
}

impl<'a> Partition<'a> {
    /// Projects with their own chip, in slot order.
    fn chips(&self) -> impl Iterator<Item = &&'a ProjectGroup> {
        self.attention.iter().chain(&self.working).chain(&self.done)
    }

    fn groups(&self, target: ChipTarget) -> Vec<&'a ProjectGroup> {
        match target {
            ChipTarget::Slot(slot) => self.chips().nth(slot).copied().into_iter().collect(),
            ChipTarget::More => self.more.clone(),
            ChipTarget::Working => self.working_more.clone(),
            ChipTarget::Done => self.done_more.clone(),
        }
    }
}

struct Layout {
    chips: Vec<ChipLayout>,
    more: Option<ChipLayout>,
    working: Option<ChipLayout>,
    done: Option<ChipLayout>,
    width: f64,
}

/// Chips in order (attention, working, done, each followed by its `+N`) go on
/// the shorter of two rows. One or two items sit on a single centred row.
fn lay_out(snapshot: &Snapshot, limits: Limits) -> Layout {
    let parts = partition(snapshot, limits);
    let extra = |groups: &[&ProjectGroup], status: TaskStatus| {
        (!groups.is_empty()).then(|| (format!("+{}", groups.len()), status))
    };
    let more = extra(&parts.more, TaskStatus::Waiting);
    let working_more = extra(&parts.working_more, TaskStatus::Working);
    let done_more = extra(&parts.done_more, TaskStatus::Finished);
    let items = parts.chips().count()
        + more.is_some() as usize
        + working_more.is_some() as usize
        + done_more.is_some() as usize;
    let two_rows = items > 2;
    let row_y = |row: usize| -> f64 {
        if two_rows {
            4.0 + row as f64 * (CHIP_HEIGHT + 4.0)
        } else {
            14.0
        }
    };

    let mut cursor = [0.0f64; 2];
    let mut place = |label: String, level: u8, icon: char, with_icon: bool| {
        let width = chip_width(&label, with_icon);
        let row = if two_rows && cursor[1] < cursor[0] {
            1
        } else {
            0
        };
        let chip = ChipLayout {
            label,
            level,
            icon,
            x: cursor[row],
            y: row_y(row),
            width,
        };
        cursor[row] += width + CHIP_GAP;
        chip
    };
    let mut chips = Vec::new();
    for group in &parts.attention {
        chips.push(place(
            chip_label(group),
            group.status.level(),
            group.status.icon(),
            true,
        ));
    }
    // The attention `+N` has no icon; the working one keeps the sync icon.
    let more = more.map(|(label, _)| place(label, 1, ' ', false));
    for group in &parts.working {
        chips.push(place(
            chip_label(group),
            group.status.level(),
            group.status.icon(),
            true,
        ));
    }
    let working =
        working_more.map(|(label, status)| place(label, status.level(), status.icon(), true));
    for group in &parts.done {
        chips.push(place(
            chip_label(group),
            group.status.level(),
            group.status.icon(),
            true,
        ));
    }
    let done = done_more.map(|(label, status)| place(label, status.level(), status.icon(), true));

    let width = if snapshot.groups.is_empty() {
        0.0
    } else {
        cursor[0].max(cursor[1]) - CHIP_GAP + BLOCK_TRAILING_GAP
    };
    Layout {
        chips,
        more,
        working,
        done,
        width,
    }
}

fn insert_chip(context: &mut DataContext, prefix: &str, chip: Option<&ChipLayout>) {
    context.insert(&format!("{prefix}.visible"), chip.is_some() as u8 as f64);
    context.insert(
        &format!("{prefix}.status"),
        chip.map_or(0.0, |chip| f64::from(chip.level)),
    );
    context.insert(
        &format!("{prefix}.alert"),
        chip.is_some_and(|chip| chip.level == 4) as u8 as f64,
    );
    context.insert_string(
        &format!("{prefix}.icon"),
        chip.map(|chip| chip.icon.to_string()).unwrap_or_default(),
    );
    context.insert(&format!("{prefix}.x"), chip.map_or(0.0, |chip| chip.x));
    context.insert(&format!("{prefix}.y"), chip.map_or(0.0, |chip| chip.y));
    context.insert(
        &format!("{prefix}.width"),
        chip.map_or(0.0, |chip| chip.width),
    );
    context.insert(&format!("{prefix}.height"), CHIP_HEIGHT);
    context.insert_string(
        &format!("{prefix}.label"),
        chip.map(|chip| chip.label.clone()).unwrap_or_default(),
    );
}

/// Theme bindings: `tasks.count`, `tasks.width`, and
/// `{visible,status,alert,icon,x,y,width,height,label}` for each
/// `tasks.slotN`, `tasks.more`, `tasks.working` (working `+N`) and
/// `tasks.done` (done `+N`).
pub fn insert_context(context: &mut DataContext) {
    refresh_if_stale();
    if let Ok(cache) = CACHE.lock() {
        if let Some(bindings) = &cache.bindings {
            context.extend(bindings);
        }
    }
}

fn bindings(snapshot: &Snapshot, limits: Limits) -> DataContext {
    let mut bindings = DataContext::default();
    let context = &mut bindings;
    let layout = lay_out(snapshot, limits);
    context.insert("tasks.count", snapshot.groups.len() as f64);
    context.insert("tasks.width", layout.width);
    context.insert(
        "tasks.sessions",
        snapshot
            .groups
            .iter()
            .map(|group| group.tasks.len())
            .sum::<usize>() as f64,
    );
    for slot in 0..SLOTS {
        insert_chip(
            context,
            &format!("tasks.slot{slot}"),
            layout.chips.get(slot),
        );
    }
    insert_chip(context, "tasks.more", layout.more.as_ref());
    insert_chip(context, "tasks.working", layout.working.as_ref());
    insert_chip(context, "tasks.done", layout.done.as_ref());
    bindings
}

fn ago(ms: u64, now: u64) -> String {
    let seconds = now.saturating_sub(ms) / 1000;
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h {}m ago", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn task_lines(task: &TaskRecord, now: u64, lines: &mut Vec<String>) {
    let title = if task.task.is_empty() {
        "New session"
    } else {
        task.task.as_str()
    };
    lines.push(format!("● {} · {title}", task.status.label()));
    let mut facts = Vec::new();
    if !task.model.is_empty() {
        facts.push(model_display_name(&task.model));
    }
    match task.subagents {
        0 => {}
        1 => facts.push("1 subagent".into()),
        n => facts.push(format!("{n} subagents")),
    }
    facts.push(ago(task.updated_ms, now));
    lines.push(format!("    {}", facts.join(" · ")));
    if !task.detail.is_empty() {
        lines.push(format!("    {}", task.detail));
    }
}

/// Hover text listing the projects behind a chip.
pub fn tooltip_text(target: ChipTarget) -> Option<String> {
    let snapshot = snapshot();
    let parts = partition(&snapshot, limits());
    let groups = parts.groups(target);
    if groups.is_empty() {
        return None;
    }
    let now = now_ms();
    let mut lines = Vec::new();
    for group in groups {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(group.name.clone());
        for task in &group.tasks {
            task_lines(task, now, &mut lines);
        }
    }
    lines.push(String::new());
    lines.push("Double-click to open".into());
    Some(lines.join("\n"))
}

/// Double-click on a chip: focus the most urgent (then most recent) session
/// behind it.
pub fn open(target: ChipTarget) {
    let snapshot = snapshot();
    let parts = partition(&snapshot, limits());
    let mut groups = parts.groups(target);
    if matches!(target, ChipTarget::Working | ChipTarget::Done) {
        groups.sort_by_key(|group| std::cmp::Reverse(group.last_ms));
    }
    for group in &groups {
        for task in &group.tasks {
            if focus_session(task) {
                return;
            }
        }
    }
    crate::diagnose::log(format!("no window found for task chip {target:?}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hook(directory: &Path, event: Value, at: u64) {
        record_hook_event(directory, &event, at, || Some((1234, 5678))).unwrap();
    }

    fn read(directory: &Path, id: &str) -> TaskRecord {
        serde_json::from_slice(&std::fs::read(directory.join(format!("{id}.json"))).unwrap())
            .unwrap()
    }

    #[test]
    fn hook_events_drive_status_and_ignore_late_async_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        hook(
            path,
            json!({"session_id":"s1","hook_event_name":"SessionStart","cwd":"D:\\coding\\wordwarden","model":"claude-opus-5-5"}),
            10,
        );
        assert_eq!(read(path, "s1").status, TaskStatus::Idle);
        assert_eq!(read(path, "s1").pid, 1234);
        hook(
            path,
            json!({"session_id":"s1","hook_event_name":"UserPromptSubmit","prompt":"\n  Fix the hover card\nmore"}),
            20,
        );
        let record = read(path, "s1");
        assert_eq!(record.status, TaskStatus::Working);
        assert_eq!(record.task, "Fix the hover card");
        hook(
            path,
            json!({"session_id":"s1","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"npm run build"}}),
            30,
        );
        let record = read(path, "s1");
        assert_eq!(record.status, TaskStatus::Permission);
        assert_eq!(record.detail, "Wants to run Bash: npm run build");
        // An async PreToolUse that started earlier finishes later.
        hook(
            path,
            json!({"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"Read"}),
            25,
        );
        assert_eq!(read(path, "s1").status, TaskStatus::Permission);
        hook(
            path,
            json!({"session_id":"s1","hook_event_name":"SubagentStart"}),
            40,
        );
        hook(
            path,
            json!({"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion"}),
            50,
        );
        let record = read(path, "s1");
        assert_eq!(record.status, TaskStatus::Waiting);
        assert_eq!(record.subagents, 1);
        hook(
            path,
            json!({"session_id":"s1","hook_event_name":"Stop"}),
            60,
        );
        assert_eq!(read(path, "s1").status, TaskStatus::Finished);
        hook(
            path,
            json!({"session_id":"s1","hook_event_name":"SessionEnd"}),
            70,
        );
        assert!(!path.join("s1.json").exists());
    }

    #[test]
    fn unsafe_session_ids_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        hook(
            dir.path(),
            json!({"session_id":"..\\evil","hook_event_name":"Stop"}),
            1,
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn snapshot_groups_by_project_and_prunes_dead_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let now = now_ms();
        hook(
            path,
            json!({"session_id":"a","hook_event_name":"Stop","cwd":"D:\\coding\\wordwarden"}),
            now,
        );
        hook(
            path,
            json!({"session_id":"b","hook_event_name":"PermissionRequest","tool_name":"Bash","cwd":"D:\\coding\\wordwarden\\.claude\\worktrees\\x"}),
            now,
        );
        hook(
            path,
            json!({"session_id":"c","hook_event_name":"UserPromptSubmit","prompt":"hi","cwd":"D:\\coding\\obs"}),
            now,
        );
        hook(
            path,
            json!({"session_id":"dead","hook_event_name":"Stop","cwd":"D:\\coding\\gone"}),
            now,
        );
        let snapshot = load_snapshot(path, |record| record.session_id != "dead");
        let names: Vec<_> = snapshot
            .groups
            .iter()
            .map(|group| group.name.as_str())
            .collect();
        assert_eq!(names, ["wordwarden", "obs"]);
        assert_eq!(snapshot.groups[0].tasks.len(), 2);
        assert_eq!(snapshot.groups[0].status, TaskStatus::Permission);
        assert!(!path.join("dead.json").exists());
        // wordwarden needs the user, obs is working: one chip each.
        let layout = lay_out(&snapshot, DEFAULT_LIMITS);
        let labels: Vec<_> = layout
            .chips
            .iter()
            .map(|chip| chip.label.as_str())
            .collect();
        assert_eq!(labels, ["wordwarden 2", "obs"]);
        assert_eq!(layout.chips[0].level, 4);
        assert_eq!(layout.chips[1].level, 2);
        assert!(layout.more.is_none() && layout.working.is_none() && layout.done.is_none());
        assert_eq!(layout.chips[0].y, layout.chips[1].y);
        assert!(layout.width > layout.chips[0].width + layout.chips[1].width);
    }

    fn groups(statuses: &[TaskStatus]) -> Snapshot {
        Snapshot {
            groups: statuses
                .iter()
                .enumerate()
                .map(|(index, status)| ProjectGroup {
                    name: format!("p{index}"),
                    tasks: vec![TaskRecord::default()],
                    status: *status,
                    last_ms: 0,
                })
                .collect(),
        }
    }

    fn limits(attention: usize, working: usize, done: usize) -> Limits {
        Limits {
            attention,
            working,
            done,
        }
    }

    #[test]
    fn each_group_shows_its_limit_then_a_plus_chip() {
        use TaskStatus::*;
        // Five need the user, five are working, three are done; a limit of
        // four shows four chips plus "+1" for each of the first two groups.
        let snapshot = groups(&[
            Permission, Waiting, Waiting, Waiting, Waiting, Working, Working, Working, Working,
            Working, Finished, Idle, Finished,
        ]);
        let layout = lay_out(&snapshot, limits(4, 4, 2));
        assert_eq!(layout.chips.len(), 10);
        assert!(layout.chips[..4].iter().all(|chip| chip.level >= 3));
        assert!(layout.chips[4..8].iter().all(|chip| chip.level == 2));
        assert!(layout.chips[8..].iter().all(|chip| chip.level == 1));
        assert_eq!(layout.more.as_ref().unwrap().label, "+1");
        assert_eq!(layout.working.as_ref().unwrap().label, "+1");
        assert_eq!(layout.done.as_ref().unwrap().label, "+1");
        // At or under the limit there is no plus chip.
        let layout = lay_out(&snapshot, limits(5, 5, 3));
        assert_eq!(layout.chips.len(), 13);
        assert!(layout.more.is_none() && layout.working.is_none() && layout.done.is_none());
        // A limit of zero hides a group behind its plus chip.
        let layout = lay_out(&snapshot, limits(4, 0, 0));
        assert_eq!(layout.chips.len(), 4);
        assert_eq!(layout.working.unwrap().label, "+5");
        assert_eq!(layout.done.unwrap().label, "+3");
    }

    #[test]
    fn chips_balance_rows_and_small_sets_stay_on_one_row() {
        use TaskStatus::*;
        let layout = lay_out(&groups(&[Waiting; 6]), limits(6, 4, 3));
        let rows: Vec<f64> = layout.chips.iter().map(|chip| chip.y).collect();
        assert_eq!(rows.iter().filter(|y| **y == rows[0]).count(), 3);
        let quiet = lay_out(&groups(&[Working, Finished]), DEFAULT_LIMITS);
        assert_eq!(quiet.chips.len(), 2);
        assert!(quiet.chips.iter().all(|chip| chip.y == 14.0));
        assert!(quiet.done.is_none());
    }

    #[test]
    fn names_are_readable() {
        assert_eq!(model_display_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_display_name("claude-fable-5-1[1m]"), "Fable 5.1");
        assert_eq!(
            model_display_name("claude-sonnet-4-5-20250929"),
            "Sonnet 4.5"
        );
        assert_eq!(project_name("D:/coding/mywebsite/"), "mywebsite");
        assert_eq!(project_name("D:\\coding\\x\\.claude\\worktrees\\feat"), "x");
    }
}
