//! `shell` tool and user-initiated `!`/`!!` command dispatch.

use std::sync::mpsc;

use tau_proto::{CborValue, Event, Frame, ToolDisplay, ToolDisplayPayload, ToolDisplayStatus};

use crate::argument::{argument_text, optional_argument_int_strict, optional_argument_text};
use crate::config::ShellConfig;
use crate::display::{ToolFailure, ToolOutput, ok_display, text_stats};
use crate::truncate::{MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, truncate_tail, truncate_tail_from_tail};

pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Execute a `shell` tool call.
///
/// **Non-zero exit semantics.** A non-zero exit status is surfaced as
/// `ToolError`, not `ToolResult` — even for benign cases like `grep`
/// finding no matches. Stdout/stderr details are preserved subject to
/// truncation, so the agent still sees what happened. The reason: every
/// model we target treats `ToolError` as a signal worth attending to,
/// and conflating "command ran" with "command failed" pushes
/// shell-script-style exit-code logic into the agent's prompt. Callers
/// that want exit codes returned as data should use a dedicated tool
/// (`grep`, `find`, `ls`) instead of `shell`.
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
                    command: command.clone(),
                    cwd: cwd.clone(),
                    status: None,
                    signal: None,
                    timed_out: false,
                    timeout_secs: Some(timeout_secs),
                    termination_reason: "start_error",
                    stdout: String::new(),
                    stderr: String::new(),
                    stdout_total: None,
                    stderr_total: None,
                }))
        })?;

    let wait = wait_with_timeout(child, timeout);

    let status_code = wait.status_code;
    let signal = wait.signal;
    let success = wait.success;

    // Truncate stdout/stderr from the tail (keep last lines — errors and
    // final results are at the end).
    let stdout_trunc = truncate_tail_from_tail(
        &wait.stdout,
        wait.stdout_total_lines,
        wait.stdout_total_bytes,
    );
    let mut stderr_trunc = truncate_tail_from_tail(
        &wait.stderr,
        wait.stderr_total_lines,
        wait.stderr_total_bytes,
    );
    let stdout_total = stdout_trunc
        .was_truncated
        .then_some((stdout_trunc.total_lines, stdout_trunc.total_bytes));
    let stderr_total = stderr_trunc
        .was_truncated
        .then_some((stderr_trunc.total_lines, stderr_trunc.total_bytes));

    let mut stderr_warnings = Vec::new();
    if let Some(warning) =
        non_utf8_warning(wait.stdout_had_invalid_utf8, wait.stderr_had_invalid_utf8)
    {
        stderr_warnings.push(warning);
    }
    if let Some(warning) =
        truncation_warning(stdout_trunc.was_truncated, stderr_trunc.was_truncated)
    {
        stderr_warnings.push(warning);
    }
    stderr_trunc.content = stderr_with_warnings(&stderr_warnings, stderr_trunc.content);

    let combined = if stdout_trunc.content.is_empty() {
        stderr_trunc.content.clone()
    } else if stderr_trunc.content.is_empty() {
        stdout_trunc.content.clone()
    } else {
        format!("{}\n{}", stdout_trunc.content, stderr_trunc.content)
    };

    let result = command_details_value(CommandDetails {
        command: command.clone(),
        cwd: cwd.clone(),
        status: status_code,
        signal,
        timed_out: wait.timed_out,
        timeout_secs: Some(timeout_secs),
        termination_reason: wait.termination_reason,
        stdout: stdout_trunc.content,
        stderr: stderr_trunc.content,
        stdout_total,
        stderr_total,
    });

    if success {
        let mut display = ok_display(display_args);
        display.payload = display_payload;
        display.stats = text_stats(&combined);
        Ok(ToolOutput { result, display })
    } else {
        let exit_label = status_code
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let status_text = if wait.timed_out {
            "timeout".to_owned()
        } else if let Some(signal) = signal {
            format!("signal {signal}")
        } else {
            exit_label.clone()
        };
        let mut display = ToolDisplay {
            args: display_args,
            status: ToolDisplayStatus::Error,
            status_text,
            payload: display_payload,
            ..Default::default()
        };
        display.stats = text_stats(&combined);
        let message = if wait.timed_out {
            format!("command timed out after {timeout_secs}s")
        } else if let Some(signal) = signal {
            format!("command terminated by signal {signal}")
        } else if status_code.is_some() {
            format!("command exited with status {exit_label}")
        } else {
            "command terminated for unknown reason".to_owned()
        };
        Err(ToolFailure {
            message,
            details: Some(Box::new(result)),
            display: Box::new(display),
        })
    }
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
    let truncated = truncate_tail(&merged);

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

    fn read_available<R: Read>(pipe: &mut Option<R>, capture: &mut CapturedStream) {
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
                Ok(n) => capture.push_bytes(&buf[..n]),
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

    let mut stdout = CapturedStream::default();
    let mut stderr = CapturedStream::default();
    let mut status = None;
    let mut timed_out = false;
    let deadline = std::time::Instant::now() + timeout;

    loop {
        read_available(&mut stdout_pipe, &mut stdout);
        read_available(&mut stderr_pipe, &mut stderr);
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
        read_available(&mut stdout_pipe, &mut stdout);
        read_available(&mut stderr_pipe, &mut stderr);
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

    stdout.finish();
    stderr.finish();
    wait_result_from_parts(status, timed_out, stdout, stderr)
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

    let stdout_handle = std::thread::spawn(move || read_pipe(stdout_pipe));
    let stderr_handle = std::thread::spawn(move || read_pipe(stderr_pipe));

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

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    wait_result_from_parts(status, timed_out, stdout, stderr)
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
    stdout: CapturedStream,
    stderr: CapturedStream,
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

    let stdout_total_lines = stdout.total_lines();
    let stderr_total_lines = stderr.total_lines();

    WaitResult {
        status_code,
        signal,
        success,
        stdout: stdout.tail,
        stderr: stderr.tail,
        stdout_total_lines,
        stdout_total_bytes: stdout.total_bytes,
        stderr_total_lines,
        stderr_total_bytes: stderr.total_bytes,
        stdout_had_invalid_utf8: stdout.had_invalid_utf8,
        stderr_had_invalid_utf8: stderr.had_invalid_utf8,
        timed_out,
        termination_reason,
    }
}

struct WaitResult {
    status_code: Option<i32>,
    signal: Option<i32>,
    success: bool,
    stdout: String,
    stderr: String,
    stdout_total_lines: usize,
    stdout_total_bytes: usize,
    stderr_total_lines: usize,
    stderr_total_bytes: usize,
    stdout_had_invalid_utf8: bool,
    stderr_had_invalid_utf8: bool,
    timed_out: bool,
    termination_reason: &'static str,
}

#[derive(Default)]
struct CapturedStream {
    tail: String,
    pending_utf8: Vec<u8>,
    total_bytes: usize,
    newline_count: usize,
    ends_with_newline: bool,
    had_invalid_utf8: bool,
}

impl CapturedStream {
    const MAX_TAIL_BYTES: usize = MAX_OUTPUT_BYTES + MAX_OUTPUT_LINES + 8192;

    fn push_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

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
                    self.push_str(valid);
                    break;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if 0 < valid_up_to {
                        self.push_str(std::str::from_utf8(&remaining[..valid_up_to]).unwrap_or(""));
                    }
                    if let Some(error_len) = error.error_len() {
                        self.had_invalid_utf8 = true;
                        self.push_str("\u{fffd}");
                        remaining = &remaining[valid_up_to + error_len..];
                    } else {
                        self.pending_utf8 = remaining[valid_up_to..].to_vec();
                        break;
                    }
                }
            }
        }
    }

    fn push_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        self.total_bytes += text.len();
        self.newline_count += text
            .as_bytes()
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count();
        self.ends_with_newline = text.as_bytes().last().is_some_and(|byte| *byte == b'\n');
        self.tail.push_str(text);
        self.trim_tail();
    }

    fn finish(&mut self) {
        if self.pending_utf8.is_empty() {
            return;
        }
        self.had_invalid_utf8 = true;
        self.pending_utf8.clear();
        self.push_str("\u{fffd}");
    }

    fn total_lines(&self) -> usize {
        if self.total_bytes == 0 {
            0
        } else if self.ends_with_newline {
            self.newline_count
        } else {
            self.newline_count + 1
        }
    }

    fn trim_tail(&mut self) {
        if self.tail.len() <= Self::MAX_TAIL_BYTES {
            return;
        }

        let mut start = self.tail.len() - Self::MAX_TAIL_BYTES;
        while start < self.tail.len() && !self.tail.is_char_boundary(start) {
            start += 1;
        }
        self.tail.drain(..start);
    }
}

#[cfg(not(unix))]
fn read_pipe(pipe: Option<impl std::io::Read>) -> CapturedStream {
    let Some(mut pipe) = pipe else {
        return CapturedStream::default();
    };
    let mut output = CapturedStream::default();
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => output.push_bytes(&buf[..n]),
        }
    }
    output.finish();
    output
}

fn non_utf8_warning(stdout_invalid: bool, stderr_invalid: bool) -> Option<&'static str> {
    match (stdout_invalid, stderr_invalid) {
        (true, true) => Some(
            "[tau-shell] warning: stdout and stderr contained non-UTF-8 bytes; invalid bytes were replaced",
        ),
        (true, false) => Some(
            "[tau-shell] warning: stdout contained non-UTF-8 bytes; invalid bytes were replaced",
        ),
        (false, true) => Some(
            "[tau-shell] warning: stderr contained non-UTF-8 bytes; invalid bytes were replaced",
        ),
        (false, false) => None,
    }
}

fn truncation_warning(stdout_truncated: bool, stderr_truncated: bool) -> Option<&'static str> {
    match (stdout_truncated, stderr_truncated) {
        (true, true) => {
            Some("[tau-shell] warning: stdout and stderr were truncated; see truncation markers")
        }
        (true, false) => {
            Some("[tau-shell] warning: stdout was truncated; see truncation marker in stdout")
        }
        (false, true) => {
            Some("[tau-shell] warning: stderr was truncated; see truncation marker in stderr")
        }
        (false, false) => None,
    }
}

fn stderr_with_warnings(warnings: &[&str], stderr: String) -> String {
    if warnings.is_empty() {
        return stderr;
    }

    let warning_text = warnings.join("\n");
    if stderr.is_empty() {
        return warning_text;
    }
    format!("{warning_text}\n{stderr}")
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
    pub(crate) command: String,
    pub(crate) cwd: Option<String>,
    pub(crate) status: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) timeout_secs: Option<u64>,
    pub(crate) termination_reason: &'static str,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) stdout_total: Option<(usize, usize)>,
    pub(crate) stderr_total: Option<(usize, usize)>,
}

pub(crate) fn command_details_value(details: CommandDetails) -> CborValue {
    let CommandDetails {
        command,
        cwd,
        status,
        signal,
        timed_out,
        timeout_secs,
        termination_reason,
        stdout,
        stderr,
        stdout_total,
        stderr_total,
    } = details;
    let mut entries = vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text(command),
        ),
        (
            CborValue::Text("stdout".to_owned()),
            CborValue::Text(stdout.clone()),
        ),
        (
            CborValue::Text("stderr".to_owned()),
            CborValue::Text(stderr.clone()),
        ),
        (
            CborValue::Text("stdout_lines".to_owned()),
            CborValue::Integer((stdout.lines().count() as i64).into()),
        ),
        (
            CborValue::Text("stdout_bytes".to_owned()),
            CborValue::Integer((stdout.len() as i64).into()),
        ),
        (
            CborValue::Text("stderr_lines".to_owned()),
            CborValue::Integer((stderr.lines().count() as i64).into()),
        ),
        (
            CborValue::Text("stderr_bytes".to_owned()),
            CborValue::Integer((stderr.len() as i64).into()),
        ),
        (
            CborValue::Text("timed_out".to_owned()),
            CborValue::Bool(timed_out),
        ),
        (
            CborValue::Text("termination_reason".to_owned()),
            CborValue::Text(termination_reason.to_owned()),
        ),
    ];
    if let Some(cwd) = cwd {
        entries.push((CborValue::Text("cwd".to_owned()), CborValue::Text(cwd)));
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
    if let Some(timeout_secs) = timeout_secs {
        entries.push((
            CborValue::Text("timeout_secs".to_owned()),
            CborValue::Integer((timeout_secs as i64).into()),
        ));
    }
    if let Some((total_lines, total_bytes)) = stdout_total {
        entries.push((
            CborValue::Text("stdout_total_lines".to_owned()),
            CborValue::Integer((total_lines as i64).into()),
        ));
        entries.push((
            CborValue::Text("stdout_total_bytes".to_owned()),
            CborValue::Integer((total_bytes as i64).into()),
        ));
        entries.push((
            CborValue::Text("stdout_truncated".to_owned()),
            CborValue::Bool(true),
        ));
    }
    if let Some((total_lines, total_bytes)) = stderr_total {
        entries.push((
            CborValue::Text("stderr_total_lines".to_owned()),
            CborValue::Integer((total_lines as i64).into()),
        ));
        entries.push((
            CborValue::Text("stderr_total_bytes".to_owned()),
            CborValue::Integer((total_bytes as i64).into()),
        ));
        entries.push((
            CborValue::Text("stderr_truncated".to_owned()),
            CborValue::Bool(true),
        ));
    }
    CborValue::Map(entries)
}
