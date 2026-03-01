// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use is_terminal::IsTerminal;
use parking_lot::Mutex;

use crate::types::AvailableUpdate;

// ── ANSI color codes ─────────────────────────────────────────────────────────

const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";

// ── Spinner ───────────────────────────────────────────────────────────────────

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn spinner_frame(elapsed: Duration) -> char {
    let index = (elapsed.as_millis() / 100) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[index]
}

// ── Progress bar ──────────────────────────────────────────────────────────────

const BAR_WIDTH: usize = 4;
const BAR_FILL: char = '⣿';

fn progress_bar(stage: u8) -> String {
    let filled = stage.min(BAR_WIDTH as u8) as usize;
    let empty = BAR_WIDTH - filled;
    format!(
        "[{GREEN}{}{RESET}{}]",
        BAR_FILL.to_string().repeat(filled),
        " ".repeat(empty),
    )
}

// ── Stage labels ──────────────────────────────────────────────────────────────

fn stage_label(stage: u8) -> &'static str {
    match stage {
        0 => "Backing up",
        1 => "Downloading",
        2 => "Extracting",
        _ => "Installing",
    }
}

// ── Terminal width ────────────────────────────────────────────────────────────

fn terminal_width() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
}

// ── Status ────────────────────────────────────────────────────────────────────

enum TaskStatus {
    InProgress,
    Succeeded,
    Failed,
}

struct TaskState {
    name: String,
    stage: u8,
    status: TaskStatus,
    start: Instant,
}

impl TaskState {
    fn new(name: String) -> Self {
        Self {
            name,
            stage: 0,
            status: TaskStatus::InProgress,
            start: Instant::now(),
        }
    }

    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    fn is_complete(&self) -> bool {
        matches!(self.status, TaskStatus::Succeeded | TaskStatus::Failed)
    }
}

// ── Row rendering ─────────────────────────────────────────────────────────────

fn render_row(state: &TaskState, width: usize) -> String {
    let elapsed = state.elapsed();
    let time_str = format!("{:.1}s", elapsed.as_secs_f64());

    if state.is_complete() {
        render_complete_row(state, &time_str, width)
    } else {
        render_progress_row(state, elapsed, &time_str, width)
    }
}

fn render_complete_row(state: &TaskState, time_str: &str, width: usize) -> String {
    let (icon_color, icon, status_color, status_label) = match state.status {
        TaskStatus::Succeeded => (GREEN, '✓', GREEN, "Updated"),
        _ => (RED, '✗', RED, "Failed"),
    };

    // Visible text: "{icon} {name} {status}"
    let visible_left = format!("{icon} {} {}", state.name, status_label);
    let padding = padding_between(visible_left.len(), time_str.len(), width);

    format!(
        "{icon_color}{icon}{RESET} {} {status_color}{status_label}{RESET}{padding}{CYAN}{time_str}{RESET}",
        state.name,
    )
}

fn render_progress_row(
    state: &TaskState,
    elapsed: Duration,
    time_str: &str,
    width: usize,
) -> String {
    let spinner = spinner_frame(elapsed);
    let bar = progress_bar(state.stage);
    let label = stage_label(state.stage);

    // Visible text: "⠋ {name} [⣿⣿  ] {label}"
    // bar visible width = BAR_WIDTH + 2 brackets
    let visible_left = format!(
        "{spinner} {} [{}] {label}",
        state.name,
        " ".repeat(BAR_WIDTH)
    );
    let padding = padding_between(visible_left.len(), time_str.len(), width);

    format!(
        "{YELLOW}{spinner}{RESET} {} {bar} {label}{padding}{CYAN}{time_str}{RESET}",
        state.name,
    )
}

/// Calculates the number of spaces needed to push the time field to the right edge.
fn padding_between(left_visible_len: usize, right_len: usize, width: usize) -> String {
    let used = left_visible_len + 1 + right_len; // +1 for the space before time
    if used >= width {
        " ".to_string()
    } else {
        " ".repeat(width - used)
    }
}

// ── Render loop ───────────────────────────────────────────────────────────────

fn render_all(states: &[TaskState], width: usize) {
    let mut out = String::new();
    for state in states {
        out.push_str(&format!("\r{}\x1b[K\n", render_row(state, width)));
    }
    print!("{out}");
    std::io::stdout().flush().ok();
}

fn run_render_loop(states: Arc<Mutex<Vec<TaskState>>>, stop: Arc<AtomicBool>) {
    loop {
        let width = terminal_width();
        {
            let locked = states.lock();
            let n = locked.len();
            // Move cursor up to start of our block, then redraw every row.
            print!("\x1b[{n}A");
            render_all(&locked, width);
        }

        if stop.load(Ordering::Relaxed) {
            break;
        }

        thread::sleep(Duration::from_millis(100));
    }
}

// ── UpdateUi ──────────────────────────────────────────────────────────────────

pub(crate) struct UpdateUi {
    states: Arc<Mutex<Vec<TaskState>>>,
    stop: Arc<AtomicBool>,
    render_thread: Option<JoinHandle<()>>,
    is_tty: bool,
}

impl UpdateUi {
    pub(crate) fn new(updates: &[&AvailableUpdate]) -> Self {
        let is_tty = std::io::stdout().is_terminal();

        let task_states: Vec<TaskState> = updates
            .iter()
            .map(|u| TaskState::new(u.installed.name.clone()))
            .collect();

        let states = Arc::new(Mutex::new(task_states));
        let stop = Arc::new(AtomicBool::new(false));

        if !is_tty {
            return Self {
                states,
                stop,
                render_thread: None,
                is_tty,
            };
        }

        // Reserve lines in the terminal — the render loop will overwrite them.
        let n = updates.len();
        for _ in 0..n {
            println!();
        }

        let states_clone = Arc::clone(&states);
        let stop_clone = Arc::clone(&stop);
        let render_thread = thread::spawn(move || run_render_loop(states_clone, stop_clone));

        Self {
            states,
            stop,
            render_thread: Some(render_thread),
            is_tty,
        }
    }

    /// Returns a reporter closure that advances the named task through stages.
    pub(crate) fn reporter(&self, index: usize) -> impl Fn(u8) {
        let states = Arc::clone(&self.states);
        move |stage: u8| {
            let mut locked = states.lock();
            if let Some(task) = locked.get_mut(index) {
                task.stage = stage;
            }
        }
    }

    /// Marks a task as complete with a success or failure status.
    pub(crate) fn complete_task(&self, index: usize, succeeded: bool) {
        if self.is_tty {
            let mut locked = self.states.lock();
            if let Some(task) = locked.get_mut(index) {
                task.status = if succeeded {
                    TaskStatus::Succeeded
                } else {
                    TaskStatus::Failed
                };
            }
        } else {
            let locked = self.states.lock();
            if let Some(task) = locked.get(index) {
                if succeeded {
                    println!("  \u{2713} {} (updated)", task.name);
                } else {
                    println!("  \u{2717} {} (failed)", task.name);
                }
            }
        }
    }

    /// Stops the render thread and performs a final render pass.
    pub(crate) fn finish(mut self) {
        if let Some(thread) = self.render_thread.take() {
            self.stop.store(true, Ordering::Release);
            thread.join().ok();

            // Final render pass so the terminal shows the completed state.
            let locked = self.states.lock();
            let width = terminal_width();
            let n = locked.len();
            print!("\x1b[{n}A");
            render_all(&locked, width);
        }
    }
}
