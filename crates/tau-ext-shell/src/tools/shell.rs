//! `shell` tool and user-initiated `!`/`!!` command dispatch.

use std::sync::mpsc;

use tau_proto::{CborValue, Event, Frame, ToolDisplay, ToolDisplayPayload, ToolDisplayStatus};

use crate::argument::{argument_text, optional_argument_int_strict, optional_argument_text};
use crate::config::ShellConfig;
use crate::display::{ToolFailure, ToolOutput, ok_display, text_stats};
use crate::truncate::{MAX_OUTPUT_LINES, mark_line, truncate_line_oriented_lines};

pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub(crate) const SLOW_COMMAND_EXEC_TIME_THRESHOLD_SECS: u64 = 5;

/// Execute a `shell` tool call.
///
/// **Process outcome semantics.** Commands that start successfully always
/// produce `ToolResult`, even when they exit non-zero, time out, or terminate
/// by signal. Those expected process outcomes are represented by structured
/// result fields such as `status`, `timed_out`, `signal`, and
/// `termination_reason`; true invocation/config/start errors remain
/// `ToolError`.
pub(crate) fn run_command(
    arguments: &CborValue,
    shell_config: &ShellConfig,
) -> Result<ToolOutput, ToolFailure> {
    let command = argument_text(arguments, "command").map_err(ToolFailure::from)?;
    let cwd = optional_argument_text(arguments, "cwd");
    let display_args = command_display_args(&command);
    let display_payload = command_display_payload(&command);
    let timeout_secs = parse_timeout_secs(arguments).map_err(|message| {
        ToolFailure::from(message)
            .with_args(display_args.clone())
            .with_payload(display_payload.clone())
    })?;
    let timeout = std::time::Duration::from_secs(timeout_secs);

    let child = shell_config
        .spawn_isolated(&command, cwd.as_deref())
        .map_err(|error| {
            ToolFailure::from(format!("failed to start shell command: {error}"))
                .with_args(display_args.clone())
                .with_payload(display_payload.clone())
                .with_details(command_details_value(CommandDetails {
                    status: None,
                    signal: None,
                    timed_out: false,
                    duration_seconds: None,
                    termination_reason: "start_error",
                    total_lines: None,
                    total_bytes: None,
                    output: String::new(),
                    truncated: false,
                    valid_utf8: true,
                }))
        })?;

    let started = std::time::Instant::now();
    let wait = wait_with_timeout(child, timeout);
    let elapsed = started.elapsed();
    let duration_seconds =
        if std::time::Duration::from_secs(SLOW_COMMAND_EXEC_TIME_THRESHOLD_SECS) < elapsed {
            Some(elapsed.as_secs_f64().ceil() as u64)
        } else {
            None
        };

    let status_code = wait.status_code;
    let signal = wait.signal;
    let success = wait.success;

    let output_trunc = wait.output.truncate();
    let combined = output_trunc.content.clone();

    let result = command_details_value(CommandDetails {
        status: status_code,
        signal,
        timed_out: wait.timed_out,
        duration_seconds,
        termination_reason: wait.termination_reason,
        total_lines: output_trunc
            .was_truncated
            .then_some(output_trunc.total_lines),
        total_bytes: output_trunc
            .was_truncated
            .then_some(output_trunc.total_bytes),
        output: output_trunc.content,
        truncated: output_trunc.was_truncated,
        valid_utf8: !wait.had_invalid_utf8,
    });

    let mut display = if success {
        ok_display(display_args)
    } else {
        let exit_label = status_code
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let status_text = if wait.timed_out {
            "timeout".to_owned()
        } else if let Some(signal) = signal {
            format!("signal {signal}")
        } else {
            exit_label
        };
        ToolDisplay {
            args: display_args,
            status: ToolDisplayStatus::Error,
            status_text,
            ..Default::default()
        }
    };
    display.payload = display_payload;
    display.stats = text_stats(&combined);
    Ok(ToolOutput { result, display })
}

fn parse_timeout_secs(arguments: &CborValue) -> Result<u64, String> {
    let Some(timeout) = optional_argument_int_strict(arguments, "timeout")? else {
        return Ok(DEFAULT_TIMEOUT_SECS);
    };
    if timeout < 0 {
        return Err("argument `timeout` must be non-negative".to_owned());
    }
    Ok(timeout as u64)
}

/// Run a user-initiated `!`/`!!` shell command, streaming stdout and
/// stderr back as `ShellCommandProgress` chunks while they arrive and
/// emitting `ShellCommandFinished` with the full (truncated-tail)
/// output when the child exits.
pub(crate) fn dispatch_user_shell_command(
    cmd: tau_proto::UiShellCommand,
    shell_config: ShellConfig,
    tx: &mpsc::Sender<Frame>,
) {
    use std::io::Read;

    let mut child = match shell_config.spawn_isolated(&cmd.command, None) {
        Ok(child) => child,
        Err(err) => {
            let _ = tx.send(Frame::Event(Event::ShellCommandFinished(
                tau_proto::ShellCommandFinished {
                    command_id: cmd.command_id,
                    session_id: cmd.session_id,
                    command: cmd.command,
                    include_in_context: cmd.include_in_context,
                    output: format!("failed to start shell command: {err}"),
                    exit_code: None,
                    cancelled: false,
                },
            )));
            return;
        }
    };

    // Read each pipe on a dedicated thread. Each read chunk is both
    // emitted as a `ShellCommandProgress` event (for live UI
    // rendering) and accumulated into a buffer that we later truncate
    // and send in `ShellCommandFinished` (for session-history
    // injection when `include_in_context`).

    fn pump<R: Read + Send + 'static>(
        mut pipe: R,
        stream: tau_proto::ShellStream,
        command_id: tau_proto::ShellCommandId,
        tx: mpsc::Sender<Frame>,
    ) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut captured = String::new();
            let mut buf = [0u8; 4096];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                        captured.push_str(&chunk);
                        let _ = tx.send(Frame::Event(Event::ShellCommandProgress(
                            tau_proto::ShellCommandProgress {
                                command_id: command_id.clone(),
                                stream,
                                chunk,
                            },
                        )));
                    }
                }
            }
            captured
        })
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = stdout_pipe.map(|p| {
        pump(
            p,
            tau_proto::ShellStream::Stdout,
            cmd.command_id.clone(),
            tx.clone(),
        )
    });
    let stderr_handle = stderr_pipe.map(|p| {
        pump(
            p,
            tau_proto::ShellStream::Stderr,
            cmd.command_id.clone(),
            tx.clone(),
        )
    });

    // Bound the user `!`/`!!` runtime. Without this a hung command
    // would block the worker thread (and its semaphore permit)
    // forever. The default is generous (1h) so legitimate long
    // builds aren't cut short.
    let timeout = std::time::Duration::from_secs(shell_config.user_command_timeout_secs);
    let pid = child.id();
    let (done_tx, done_rx) = mpsc::channel::<Option<std::process::ExitStatus>>();
    let waiter = std::thread::spawn(move || {
        let status = child.wait().ok();
        let _ = done_tx.send(status);
    });

    let (exit_code, status_note, cancelled) = match done_rx.recv_timeout(timeout) {
        Ok(Some(status)) => (status.code(), None, false),
        Ok(None) => (None, Some("wait failed".to_owned()), false),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Kill the whole process group. `setsid` in
            // `apply_command_isolation` made the child a session
            // leader, so its pgid equals its pid — sending a signal
            // to `-pid` reaches the leader and every descendant it
            // hasn't detached.
            #[cfg(unix)]
            #[allow(unsafe_code)]
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            // Wait for the waiter to observe the death so the child
            // is reaped rather than left as a zombie.
            let status = done_rx.recv().ok().flatten();
            let _ = waiter.join();
            (
                status.and_then(|s| s.code()),
                Some(format!(
                    "command killed after {}s timeout",
                    shell_config.user_command_timeout_secs
                )),
                true,
            )
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            (None, Some("waiter thread vanished".to_owned()), false)
        }
    };

    let stdout = stdout_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();

    // Interleave stdout + stderr in the final output the way the
    // `shell` tool does: stderr follows stdout under a separator.
    let mut merged = stdout;
    if !stderr.is_empty() {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str("[stderr]\n");
        merged.push_str(&stderr);
    }
    if let Some(note) = status_note {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(&note);
    }
    let truncated = crate::truncate::truncate_tail(&merged);

    let _ = tx.send(Frame::Event(Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: cmd.command_id,
            session_id: cmd.session_id,
            command: cmd.command,
            include_in_context: cmd.include_in_context,
            output: truncated.content,
            exit_code,
            cancelled,
        },
    )));
}

/// Wait for a child process with a timeout, preserving bounded tail output.
///
/// On Unix the shell tool must not wait for stdout/stderr EOF: background or
/// detached descendants can inherit those pipe write ends long after the
/// foreground shell exits or is killed. The main thread therefore polls
/// nonblocking pipes and an internal child-exit wake pipe together, then
/// returns after foreground exit or timeout with only a brief nonblocking
/// drain.
#[cfg(unix)]
fn wait_with_timeout(mut child: std::process::Child, timeout: std::time::Duration) -> WaitResult {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::mpsc::TryRecvError;

    const READ_CHUNK_BYTES: usize = 8192;
    const DRAIN_AFTER_DONE: std::time::Duration = std::time::Duration::from_millis(50);

    fn set_nonblocking(fd: RawFd) {
        #[allow(unsafe_code)]
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if 0 <= flags {
                let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
    }

    fn read_available<R: Read>(
        pipe: &mut Option<R>,
        stream: OutputStream,
        capture: &mut CapturedOutput,
    ) {
        let Some(pipe_ref) = pipe.as_mut() else {
            return;
        };

        let mut close_pipe = false;
        let mut buf = [0u8; READ_CHUNK_BYTES];
        loop {
            match pipe_ref.read(&mut buf) {
                Ok(0) => {
                    close_pipe = true;
                    break;
                }
                Ok(n) => capture.push_bytes(stream, &buf[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    close_pipe = true;
                    break;
                }
            }
        }
        if close_pipe {
            *pipe = None;
        }
    }

    fn collect_status(
        status_rx: &mpsc::Receiver<Option<std::process::ExitStatus>>,
        status: &mut Option<std::process::ExitStatus>,
    ) -> bool {
        if status.is_some() {
            return true;
        }
        match status_rx.try_recv() {
            Ok(received) => {
                *status = received;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => true,
        }
    }

    fn poll_timeout_ms(deadline: std::time::Instant) -> i32 {
        let now = std::time::Instant::now();
        if deadline <= now {
            return 0;
        }
        let remaining = deadline - now;
        i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX)
    }

    fn drain_wake_fd(wake_read: &OwnedFd) {
        let mut buf = [0u8; 16];
        loop {
            #[allow(unsafe_code)]
            let n = unsafe {
                libc::read(
                    wake_read.as_raw_fd(),
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if 0 < n {
                continue;
            }
            break;
        }
    }

    let pid = child.id();
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    if let Some(pipe) = stdout_pipe.as_ref() {
        set_nonblocking(pipe.as_raw_fd());
    }
    if let Some(pipe) = stderr_pipe.as_ref() {
        set_nonblocking(pipe.as_raw_fd());
    }

    let mut wake_fds = [0; 2];
    #[allow(unsafe_code)]
    let wake_pipe_ok = unsafe { libc::pipe(wake_fds.as_mut_ptr()) == 0 };
    let (wake_read, wake_write) = if wake_pipe_ok {
        #[allow(unsafe_code)]
        unsafe {
            (
                Some(OwnedFd::from_raw_fd(wake_fds[0])),
                Some(OwnedFd::from_raw_fd(wake_fds[1])),
            )
        }
    } else {
        (None, None)
    };
    if let Some(wake_read) = wake_read.as_ref() {
        set_nonblocking(wake_read.as_raw_fd());
    }
    let waiter_wake_read = wake_read.as_ref().and_then(|wake_read| {
        #[allow(unsafe_code)]
        let fd = unsafe { libc::dup(wake_read.as_raw_fd()) };
        if 0 <= fd {
            #[allow(unsafe_code)]
            unsafe {
                Some(OwnedFd::from_raw_fd(fd))
            }
        } else {
            None
        }
    });

    let (status_tx, status_rx) = mpsc::channel::<Option<std::process::ExitStatus>>();
    let _waiter = std::thread::spawn(move || {
        let _wake_read_guard = waiter_wake_read;
        let status = child.wait().ok();
        let _ = status_tx.send(status);
        if let Some(wake_write) = wake_write {
            let byte = [1u8];
            #[allow(unsafe_code)]
            unsafe {
                let _ = libc::write(
                    wake_write.as_raw_fd(),
                    byte.as_ptr().cast::<libc::c_void>(),
                    byte.len(),
                );
            }
        }
    });

    let mut output = CapturedOutput::default();
    let mut status = None;
    let mut timed_out = false;
    let deadline = std::time::Instant::now() + timeout;

    loop {
        read_available(&mut stdout_pipe, OutputStream::Stdout, &mut output);
        read_available(&mut stderr_pipe, OutputStream::Stderr, &mut output);
        if collect_status(&status_rx, &mut status) {
            break;
        }

        let now = std::time::Instant::now();
        if deadline <= now {
            timed_out = true;
            kill_process_group_by_pid(pid);
            break;
        }

        let mut poll_fds = Vec::new();
        if let Some(pipe) = stdout_pipe.as_ref() {
            poll_fds.push(libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }
        if let Some(pipe) = stderr_pipe.as_ref() {
            poll_fds.push(libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }
        if let Some(wake_read) = wake_read.as_ref() {
            poll_fds.push(libc::pollfd {
                fd: wake_read.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }

        if poll_fds.is_empty() {
            let sleep_for = (deadline - now).min(std::time::Duration::from_millis(25));
            std::thread::sleep(sleep_for);
            continue;
        }

        #[allow(unsafe_code)]
        unsafe {
            let _ = libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as libc::nfds_t,
                poll_timeout_ms(deadline),
            );
        }
        if let Some(wake_read) = wake_read.as_ref() {
            drain_wake_fd(wake_read);
        }
    }

    let drain_deadline = std::time::Instant::now() + DRAIN_AFTER_DONE;
    loop {
        read_available(&mut stdout_pipe, OutputStream::Stdout, &mut output);
        read_available(&mut stderr_pipe, OutputStream::Stderr, &mut output);
        let _ = collect_status(&status_rx, &mut status);
        if stdout_pipe.is_none() && stderr_pipe.is_none() {
            break;
        }
        if drain_deadline <= std::time::Instant::now() {
            break;
        }

        let mut poll_fds = Vec::new();
        if let Some(pipe) = stdout_pipe.as_ref() {
            poll_fds.push(libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }
        if let Some(pipe) = stderr_pipe.as_ref() {
            poll_fds.push(libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }
        if poll_fds.is_empty() {
            break;
        }
        #[allow(unsafe_code)]
        unsafe {
            let _ = libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as libc::nfds_t,
                poll_timeout_ms(drain_deadline),
            );
        }
    }

    output.finish();
    wait_result_from_parts(status, timed_out, output)
}

/// Wait for a child process with a timeout, preserving output even when
/// the timeout is reached.
///
/// Non-Unix keeps the older blocking-pipe fallback. The crate's process-group
/// isolation is Unix-only, so the hard timeout/read-loop guarantees are
/// provided by the Unix implementation above.
#[cfg(not(unix))]
fn wait_with_timeout(mut child: std::process::Child, timeout: std::time::Duration) -> WaitResult {
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_handle = std::thread::spawn(move || read_pipe(stdout_pipe, OutputStream::Stdout));
    let stderr_handle = std::thread::spawn(move || read_pipe(stderr_pipe, OutputStream::Stderr));

    let deadline = std::time::Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }

        let now = std::time::Instant::now();
        if deadline <= now {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }

        std::thread::sleep((deadline - now).min(std::time::Duration::from_millis(25)));
    };

    let mut output = CapturedOutput::default();
    let stdout_output = stdout_handle.join().unwrap_or_default();
    output.mark_invalid_utf8(OutputStream::Stdout, stdout_output.had_invalid_utf8);
    for line in stdout_output.lines {
        output.push_line(line.stream, line.content);
    }
    let stderr_output = stderr_handle.join().unwrap_or_default();
    output.mark_invalid_utf8(OutputStream::Stderr, stderr_output.had_invalid_utf8);
    for line in stderr_output.lines {
        output.push_line(line.stream, line.content);
    }
    wait_result_from_parts(status, timed_out, output)
}

#[cfg(unix)]
fn exit_status_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;

    status.signal()
}

#[cfg(not(unix))]
fn exit_status_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(unix)]
fn kill_process_group_by_pid(pid: u32) {
    #[allow(unsafe_code)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn wait_result_from_parts(
    status: Option<std::process::ExitStatus>,
    timed_out: bool,
    output: CapturedOutput,
) -> WaitResult {
    let status_code = status.as_ref().and_then(|status| status.code());
    let signal = status.as_ref().and_then(exit_status_signal);
    let success = !timed_out && status.as_ref().is_some_and(|status| status.success());
    let termination_reason = if timed_out {
        "timeout"
    } else if signal.is_some() {
        "signal"
    } else if status.is_some() {
        "exit"
    } else {
        "unknown"
    };

    let had_invalid_utf8 = output.stdout.had_invalid_utf8 || output.stderr.had_invalid_utf8;
    WaitResult {
        status_code,
        signal,
        success,
        output,
        had_invalid_utf8,
        timed_out,
        termination_reason,
    }
}

struct WaitResult {
    status_code: Option<i32>,
    signal: Option<i32>,
    success: bool,
    output: CapturedOutput,
    had_invalid_utf8: bool,
    timed_out: bool,
    termination_reason: &'static str,
}

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    fn prefix(self) -> &'static str {
        match self {
            Self::Stdout => "out",
            Self::Stderr => "err",
        }
    }
}

struct OutputLine {
    stream: OutputStream,
    content: OutputContent,
}

#[derive(Clone)]
enum OutputContent {
    Text { text: String, no_nl: bool },
    InvalidUtf8 { no_nl: bool },
}

#[derive(Default)]
struct CapturedOutput {
    stdout: StreamDecoder,
    stderr: StreamDecoder,
    head_lines: Vec<OutputLine>,
    tail_lines: Vec<OutputLine>,
    total_lines: usize,
    total_bytes: usize,
}

impl CapturedOutput {
    fn push_bytes(&mut self, stream: OutputStream, bytes: &[u8]) {
        let decoder = match stream {
            OutputStream::Stdout => &mut self.stdout,
            OutputStream::Stderr => &mut self.stderr,
        };
        for line in decoder.push_bytes(bytes) {
            self.push_line(stream, line);
        }
    }

    fn push_line(&mut self, stream: OutputStream, content: OutputContent) {
        let separator_bytes = usize::from(self.total_lines != 0);
        self.total_bytes += separator_bytes + formatted_output_line_len(stream, &content);
        let line = OutputLine { stream, content };
        if self.total_lines < MAX_OUTPUT_LINES / 2 {
            self.head_lines.push(line);
        } else {
            self.tail_lines.push(line);
            if MAX_OUTPUT_LINES / 2 < self.tail_lines.len() {
                self.tail_lines.remove(0);
            }
        }
        self.total_lines += 1;
    }

    fn finish(&mut self) {
        for line in self.stdout.finish() {
            self.push_line(OutputStream::Stdout, line);
        }
        for line in self.stderr.finish() {
            self.push_line(OutputStream::Stderr, line);
        }
    }

    #[cfg(not(unix))]
    fn mark_invalid_utf8(&mut self, stream: OutputStream, had_invalid_utf8: bool) {
        if !had_invalid_utf8 {
            return;
        }
        match stream {
            OutputStream::Stdout => self.stdout.had_invalid_utf8 = true,
            OutputStream::Stderr => self.stderr.had_invalid_utf8 = true,
        }
    }

    fn truncate(&self) -> crate::truncate::Truncated {
        let mut rendered = self
            .head_lines
            .iter()
            .map(render_output_line)
            .collect::<Vec<_>>();
        rendered.extend(self.tail_lines.iter().map(render_output_line));
        let rendered_refs = rendered.iter().map(String::as_str).collect::<Vec<_>>();
        truncate_line_oriented_lines(
            rendered_refs.iter().copied(),
            self.total_lines,
            self.total_bytes,
        )
    }
}

#[derive(Default)]
struct StreamDecoder {
    pending_utf8: Vec<u8>,
    pending_line: String,
    pending_line_invalid: bool,
    had_invalid_utf8: bool,
}

impl StreamDecoder {
    fn push_bytes(&mut self, bytes: &[u8]) -> Vec<OutputContent> {
        if bytes.is_empty() {
            return Vec::new();
        }

        let mut lines = Vec::new();
        let mut merged;
        let mut remaining = if self.pending_utf8.is_empty() {
            bytes
        } else {
            merged = std::mem::take(&mut self.pending_utf8);
            merged.extend_from_slice(bytes);
            &merged
        };

        loop {
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    self.push_str(valid, &mut lines);
                    break;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if 0 < valid_up_to {
                        self.push_str(
                            std::str::from_utf8(&remaining[..valid_up_to]).unwrap_or(""),
                            &mut lines,
                        );
                    }
                    if let Some(error_len) = error.error_len() {
                        self.had_invalid_utf8 = true;
                        self.pending_line_invalid = true;
                        remaining = &remaining[valid_up_to + error_len..];
                    } else {
                        self.pending_utf8 = remaining[valid_up_to..].to_vec();
                        break;
                    }
                }
            }
        }
        lines
    }

    fn push_str(&mut self, text: &str, lines: &mut Vec<OutputContent>) {
        for segment in text.split_inclusive('\n') {
            if let Some(line) = segment.strip_suffix('\n') {
                if !self.pending_line_invalid {
                    self.pending_line.push_str(line);
                }
                lines.push(self.take_pending_line(false));
            } else if !self.pending_line_invalid {
                self.pending_line.push_str(segment);
            }
        }
    }

    fn finish(&mut self) -> Vec<OutputContent> {
        if !self.pending_utf8.is_empty() {
            self.had_invalid_utf8 = true;
            self.pending_utf8.clear();
            self.pending_line_invalid = true;
        }
        if self.pending_line.is_empty() && !self.pending_line_invalid {
            Vec::new()
        } else {
            vec![self.take_pending_line(true)]
        }
    }

    fn take_pending_line(&mut self, no_nl: bool) -> OutputContent {
        if std::mem::take(&mut self.pending_line_invalid) {
            self.pending_line.clear();
            OutputContent::InvalidUtf8 { no_nl }
        } else {
            OutputContent::Text {
                text: std::mem::take(&mut self.pending_line),
                no_nl,
            }
        }
    }
}

fn render_output_line(line: &OutputLine) -> String {
    let prefix = line.stream.prefix();
    match &line.content {
        OutputContent::Text { text, no_nl } => {
            let marker = no_nl.then_some("no_nl");
            format_output_line(&prefix, marker, text)
        }
        OutputContent::InvalidUtf8 { no_nl } => {
            let marker = if *no_nl {
                "invalid-utf8,no_nl"
            } else {
                "invalid-utf8"
            };
            mark_line(&format_output_line(prefix, None, ""), marker)
        }
    }
}

fn format_output_line(prefix: &str, marker: Option<&str>, content: &str) -> String {
    match marker {
        Some(marker) => format!("{prefix}({marker}) {content}"),
        None => format!("{prefix} {content}"),
    }
}

fn formatted_output_line_len(stream: OutputStream, content: &OutputContent) -> usize {
    render_output_line(&OutputLine {
        stream,
        content: content.clone(),
    })
    .len()
}

#[cfg(not(unix))]
#[derive(Default)]
struct PipeOutput {
    lines: Vec<OutputLine>,
    had_invalid_utf8: bool,
}

#[cfg(not(unix))]
fn read_pipe(pipe: Option<impl std::io::Read>, stream: OutputStream) -> PipeOutput {
    let Some(mut pipe) = pipe else {
        return PipeOutput::default();
    };
    let mut decoder = StreamDecoder::default();
    let mut lines = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                lines.extend(decoder.push_bytes(&buf[..n]));
            }
        }
    }
    lines.extend(decoder.finish());
    PipeOutput {
        lines: lines
            .into_iter()
            .map(|content| OutputLine { stream, content })
            .collect(),
        had_invalid_utf8: decoder.had_invalid_utf8,
    }
}

fn command_display_args(command: &str) -> String {
    shorten_command_line(command.lines().next().unwrap_or_default())
}

fn shorten_command_line(line: &str) -> String {
    const EDGE_CHARS: usize = 20;
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= EDGE_CHARS * 2 {
        return line.to_owned();
    }

    let head: String = chars.iter().take(EDGE_CHARS).copied().collect();
    let tail: String = chars
        .iter()
        .skip(chars.len() - EDGE_CHARS)
        .copied()
        .collect();
    format!("{head}┄{tail}")
}

fn command_display_payload(command: &str) -> Option<ToolDisplayPayload> {
    if command.lines().count() < 2 {
        return None;
    }
    Some(ToolDisplayPayload::Text {
        text: command.to_owned(),
    })
}

pub(crate) struct CommandDetails {
    pub(crate) status: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) duration_seconds: Option<u64>,
    pub(crate) termination_reason: &'static str,
    pub(crate) total_lines: Option<usize>,
    pub(crate) total_bytes: Option<usize>,
    pub(crate) output: String,
    pub(crate) truncated: bool,
    pub(crate) valid_utf8: bool,
}

pub(crate) fn command_details_value(details: CommandDetails) -> CborValue {
    let CommandDetails {
        status,
        signal,
        timed_out,
        duration_seconds,
        termination_reason,
        total_lines,
        total_bytes,
        output,
        truncated,
        valid_utf8,
    } = details;
    let mut entries = vec![(
        CborValue::Text("output".to_owned()),
        CborValue::Text(output),
    )];
    if !valid_utf8 {
        entries.push((
            CborValue::Text("valid_utf8".to_owned()),
            CborValue::Bool(false),
        ));
    }
    if timed_out {
        entries.push((
            CborValue::Text("timed_out".to_owned()),
            CborValue::Bool(true),
        ));
    }
    if timed_out || signal.is_some() || status != Some(0) || termination_reason != "exit" {
        entries.push((
            CborValue::Text("termination_reason".to_owned()),
            CborValue::Text(termination_reason.to_owned()),
        ));
    }
    if truncated {
        entries.push((
            CborValue::Text("truncated".to_owned()),
            CborValue::Bool(true),
        ));
        if let Some(total_lines) = total_lines {
            entries.push((
                CborValue::Text("total_lines".to_owned()),
                CborValue::Integer((total_lines as i64).into()),
            ));
        }
        if let Some(total_bytes) = total_bytes {
            entries.push((
                CborValue::Text("total_bytes".to_owned()),
                CborValue::Integer((total_bytes as i64).into()),
            ));
        }
    }
    if let Some(status) = status {
        entries.push((
            CborValue::Text("status".to_owned()),
            CborValue::Integer(status.into()),
        ));
    }
    if let Some(signal) = signal {
        entries.push((
            CborValue::Text("signal".to_owned()),
            CborValue::Integer(signal.into()),
        ));
    }
    if let Some(duration_seconds) = duration_seconds {
        entries.push((
            CborValue::Text("duration_seconds".to_owned()),
            CborValue::Integer((duration_seconds as i64).into()),
        ));
    }
    CborValue::Map(entries)
}
