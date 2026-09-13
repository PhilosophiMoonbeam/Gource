// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Ordered, bounded export sinks.  Files are published only after a complete
//! successful stream has been consumed.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::schedule::FrameRate;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

#[cfg(windows)]
type WinHandle = *mut std::ffi::c_void;

#[cfg(windows)]
#[repr(C)]
struct JobBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[cfg(windows)]
#[repr(C)]
struct IoCounters {
    read_operations: u64,
    write_operations: u64,
    other_operations: u64,
    read_bytes: u64,
    write_bytes: u64,
    other_bytes: u64,
}

#[cfg(windows)]
#[repr(C)]
struct JobExtendedLimitInformation {
    basic: JobBasicLimitInformation,
    io: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[cfg(windows)]
#[repr(C)]
struct ThreadEntry32 {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

#[cfg(windows)]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *mut std::ffi::c_void, name: *const u16) -> WinHandle;
    fn SetInformationJobObject(
        job: WinHandle,
        class: u32,
        information: *mut std::ffi::c_void,
        length: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: WinHandle, process: WinHandle) -> i32;
    fn TerminateJobObject(job: WinHandle, exit_code: u32) -> i32;
    fn CloseHandle(handle: WinHandle) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> WinHandle;
    fn Thread32First(snapshot: WinHandle, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: WinHandle, entry: *mut ThreadEntry32) -> i32;
    fn OpenThread(access: u32, inherit_handle: i32, thread_id: u32) -> WinHandle;
    fn ResumeThread(thread: WinHandle) -> u32;
}

#[cfg(windows)]
struct ProcessSupervisor {
    job: WinHandle,
}

#[cfg(not(windows))]
struct ProcessSupervisor;

#[cfg(windows)]
impl Drop for ProcessSupervisor {
    fn drop(&mut self) {
        if !self.job.is_null() {
            unsafe {
                let _ = CloseHandle(self.job);
            }
        }
    }
}

fn prepare_process(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A process group is created by the child before it can execute the
        // encoder.  This lets cancellation terminate every descendant which
        // inherited one of the encoder's pipes.
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // The process must remain suspended until it has been assigned to the
        // Job; otherwise a fast encoder could create an untracked child.
        command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    }
}

fn spawn_command(command: &mut Command) -> io::Result<Child> {
    #[cfg(unix)]
    {
        const MAX_ATTEMPTS: usize = 4;
        const RETRY_DELAY: Duration = Duration::from_millis(5);
        const ETXTBSY: i32 = 26;

        for attempt in 0..MAX_ATTEMPTS {
            match command.spawn() {
                Ok(child) => return Ok(child),
                Err(error)
                    if error.raw_os_error() == Some(ETXTBSY) && attempt + 1 < MAX_ATTEMPTS =>
                {
                    thread::sleep(RETRY_DELAY);
                }
                Err(error) => return Err(error),
            }
        }

        unreachable!("spawn retry loop always returns")
    }
    #[cfg(not(unix))]
    {
        command.spawn()
    }
}

#[cfg(windows)]
fn resume_suspended_process(process_id: u32) -> io::Result<()> {
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
    const THREAD_SUSPEND_RESUME: u32 = 0x0002;
    const INVALID_HANDLE_VALUE: WinHandle = -1isize as WinHandle;

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry: ThreadEntry32 = unsafe { std::mem::zeroed() };
    entry.size = std::mem::size_of::<ThreadEntry32>() as u32;
    let mut result = Err(io::Error::last_os_error());
    if unsafe { Thread32First(snapshot, &mut entry) } != 0 {
        loop {
            if entry.owner_process_id == process_id {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.thread_id) };
                if !thread.is_null() {
                    let resumed = unsafe { ResumeThread(thread) };
                    let error = if resumed == u32::MAX {
                        Some(io::Error::last_os_error())
                    } else {
                        None
                    };
                    unsafe {
                        let _ = CloseHandle(thread);
                    }
                    result = error.map_or(Ok(()), Err);
                    break;
                }
            }
            if unsafe { Thread32Next(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    } else {
        result = Err(io::Error::last_os_error());
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    result
}

fn create_process_supervisor(child: &Child) -> io::Result<ProcessSupervisor> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
        const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

        let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut information: JobExtendedLimitInformation = unsafe { std::mem::zeroed() };
        information.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&mut information as *mut JobExtendedLimitInformation).cast(),
                std::mem::size_of::<JobExtendedLimitInformation>() as u32,
            )
        } != 0;
        if !configured {
            let error = io::Error::last_os_error();
            unsafe {
                let _ = CloseHandle(job);
            }
            return Err(error);
        }
        if unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as WinHandle) } == 0 {
            let error = io::Error::last_os_error();
            unsafe {
                let _ = CloseHandle(job);
            }
            return Err(error);
        }
        if let Err(error) = resume_suspended_process(child.id()) {
            unsafe {
                let _ = CloseHandle(job);
            }
            return Err(error);
        }
        Ok(ProcessSupervisor { job })
    }
    #[cfg(not(windows))]
    {
        let _ = child;
        Ok(ProcessSupervisor)
    }
}

fn reap_direct_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn kill_and_reap_child(child: &mut Child, supervisor: &ProcessSupervisor) {
    #[cfg(unix)]
    {
        let _ = supervisor;
        const SIGKILL: i32 = 9;
        let pid = child.id();
        if pid > 0 && pid <= i32::MAX as u32 {
            // `prepare_process` places the child in a fresh process group.
            // Killing the group closes inherited pipes held by descendants
            // before the reader and writer threads are joined.
            unsafe {
                let _ = kill(-(pid as i32), SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    unsafe {
        let _ = TerminateJobObject(supervisor.job, 1);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = supervisor;

    // Keep the direct wait as well: it reaps the Child handle even when the
    // group/job was already gone or the process had exited naturally.
    let _ = child.kill();
    let _ = child.wait();
}

/// Result returned after a sink publishes its final artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SinkReport {
    pub frames: u64,
    pub bytes: u64,
    pub output: PathBuf,
}

/// Errors raised by image or process sinks.
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("output path already exists: {0}")]
    OutputExists(PathBuf),
    #[error("output path has no parent: {0}")]
    MissingParent(PathBuf),
    #[error("output path is invalid: {0}")]
    InvalidPath(PathBuf),
    #[error("frame order violation: expected {expected}, got {actual}")]
    FrameOrder { expected: u64, actual: u64 },
    #[error("frame length mismatch: expected {expected}, got {actual}")]
    FrameLength { expected: usize, actual: usize },
    #[error("sink queue capacity must be positive")]
    ZeroQueueCapacity,
    #[error("sink queue byte cap is too small: required {required}, cap {cap}")]
    QueueByteCap { required: u64, cap: u64 },
    #[error("sink cancelled")]
    Cancelled,
    #[error("sink queue disconnected")]
    QueueDisconnected,
    #[error("encoder process could not be started: {0}")]
    Spawn(#[source] io::Error),
    #[error("encoder I/O failed: {0}")]
    Io(#[source] io::Error),
    #[error("encoder exited unsuccessfully ({status}); stderr tail: {stderr}")]
    Nonzero { status: String, stderr: String },
    #[error("encoder timed out and was killed; stderr tail: {stderr}")]
    Timeout { stderr: String },
    #[error("encoder was cancelled; stderr tail: {stderr}")]
    EncoderCancelled { stderr: String },
    #[error("encoder writer failed: {message}; stderr tail: {stderr}")]
    Writer { message: String, stderr: String },
    #[error("failed to publish output: {0}")]
    Publish(#[source] io::Error),
    #[error("PNG encoding failed: {0}")]
    Image(#[source] image::ImageError),
}

/// A sink accepts complete tight RGBA8 frames in monotonically increasing
/// order and applies backpressure rather than dropping frames.
pub trait FrameSink {
    fn push(&mut self, index: u64, rgba8: &[u8]) -> Result<(), SinkError>;
    fn finish(&mut self) -> Result<SinkReport, SinkError>;
    fn cancel(&mut self);
    fn report(&self) -> SinkReport;
}

/// A frame directory sink with deterministic names and atomic directory
/// publication.
pub struct PngFrameSink {
    final_dir: PathBuf,
    staging_dir: PathBuf,
    width: u32,
    height: u32,
    expected_len: usize,
    next_index: u64,
    bytes: u64,
    finalized: bool,
    cancelled: bool,
}

impl PngFrameSink {
    pub fn new(path: impl Into<PathBuf>, width: u32, height: u32) -> Result<Self, SinkError> {
        let final_dir = path.into();
        if final_dir.as_os_str().is_empty() {
            return Err(SinkError::InvalidPath(final_dir));
        }
        if final_dir.exists() {
            return Err(SinkError::OutputExists(final_dir));
        }
        let expected_len = checked_frame_len(width, height)?;
        let staging_dir = make_staging_path(&final_dir, "frames")?;
        fs::create_dir(&staging_dir).map_err(SinkError::Io)?;
        Ok(Self {
            final_dir,
            staging_dir,
            width,
            height,
            expected_len,
            next_index: 0,
            bytes: 0,
            finalized: false,
            cancelled: false,
        })
    }

    #[must_use]
    pub fn output_dir(&self) -> &Path {
        &self.final_dir
    }

    #[must_use]
    pub fn staging_dir(&self) -> &Path {
        &self.staging_dir
    }

    #[must_use]
    pub fn frame_path(&self, index: u64) -> PathBuf {
        self.staging_dir.join(format!("frame_{index:08}.png"))
    }

    /// Write a manifest alongside staged frames before `finish` publishes the
    /// directory.
    pub fn write_manifest(&self, text: &str) -> Result<(), SinkError> {
        let path = self.staging_dir.join("manifest.toml");
        fs::write(path, text).map_err(SinkError::Io)
    }

    fn abort_inner(&mut self) {
        if !self.finalized {
            let _ = fs::remove_dir_all(&self.staging_dir);
        }
    }
}

impl FrameSink for PngFrameSink {
    fn push(&mut self, index: u64, rgba8: &[u8]) -> Result<(), SinkError> {
        if self.cancelled {
            return Err(SinkError::Cancelled);
        }
        if index != self.next_index {
            return Err(SinkError::FrameOrder {
                expected: self.next_index,
                actual: index,
            });
        }
        if rgba8.len() != self.expected_len {
            return Err(SinkError::FrameLength {
                expected: self.expected_len,
                actual: rgba8.len(),
            });
        }
        let path = self.frame_path(index);
        image::save_buffer_with_format(
            &path,
            rgba8,
            self.width,
            self.height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .map_err(SinkError::Image)?;
        self.next_index = self.next_index.saturating_add(1);
        self.bytes = self.bytes.saturating_add(rgba8.len() as u64);
        Ok(())
    }

    fn finish(&mut self) -> Result<SinkReport, SinkError> {
        if self.cancelled {
            self.abort_inner();
            return Err(SinkError::Cancelled);
        }
        if self.finalized {
            return Ok(self.report());
        }
        fs::rename(&self.staging_dir, &self.final_dir).map_err(|error| {
            self.abort_inner();
            SinkError::Publish(error)
        })?;
        self.finalized = true;
        Ok(self.report())
    }

    fn cancel(&mut self) {
        self.cancelled = true;
        self.abort_inner();
    }

    fn report(&self) -> SinkReport {
        SinkReport {
            frames: self.next_index,
            bytes: self.bytes,
            output: self.final_dir.clone(),
        }
    }
}

impl Drop for PngFrameSink {
    fn drop(&mut self) {
        self.abort_inner();
    }
}

/// FFmpeg process configuration.  The command is always launched directly;
/// no shell string is ever assembled.
#[derive(Clone, Debug)]
pub struct FfmpegConfig {
    pub executable: PathBuf,
    pub output: PathBuf,
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
    pub queue_capacity: usize,
    pub max_queue_bytes: u64,
    pub max_stderr_bytes: usize,
    pub timeout: Duration,
    pub color_space: String,
    pub color_primaries: String,
    pub color_transfer: String,
    pub color_range: String,
}

impl FfmpegConfig {
    #[must_use]
    pub fn new(output: impl Into<PathBuf>, width: u32, height: u32, frame_rate: FrameRate) -> Self {
        Self {
            executable: PathBuf::from("ffmpeg"),
            output: output.into(),
            width,
            height,
            frame_rate,
            queue_capacity: 3,
            max_queue_bytes: 3 * checked_frame_len(width, height).unwrap_or(0) as u64,
            max_stderr_bytes: 64 * 1024,
            timeout: Duration::from_secs(30),
            color_space: "bt709".to_owned(),
            color_primaries: "bt709".to_owned(),
            color_transfer: "bt709".to_owned(),
            color_range: "tv".to_owned(),
        }
    }

    #[must_use]
    pub fn argv(&self, output: &Path) -> Vec<OsString> {
        vec![
            OsString::from("-hide_banner"),
            OsString::from("-loglevel"),
            OsString::from("error"),
            OsString::from("-f"),
            OsString::from("rawvideo"),
            OsString::from("-pixel_format"),
            OsString::from("rgba"),
            OsString::from("-video_size"),
            OsString::from(format!("{}x{}", self.width, self.height)),
            OsString::from("-framerate"),
            OsString::from(self.frame_rate.argv_value()),
            OsString::from("-i"),
            OsString::from("-"),
            OsString::from("-an"),
            OsString::from("-c:v"),
            OsString::from("ffv1"),
            OsString::from("-level"),
            OsString::from("3"),
            OsString::from("-g"),
            OsString::from("1"),
            OsString::from("-colorspace"),
            OsString::from(&self.color_space),
            OsString::from("-color_primaries"),
            OsString::from(&self.color_primaries),
            OsString::from("-color_trc"),
            OsString::from(&self.color_transfer),
            OsString::from("-color_range"),
            OsString::from(&self.color_range),
            OsString::from("-f"),
            OsString::from("matroska"),
            output.as_os_str().to_owned(),
        ]
    }
}
#[derive(Debug)]
struct FramePacket {
    index: u64,
    bytes: Vec<u8>,
}

struct WriterCompletion {
    result: Result<(), String>,
}

/// A supervised FFmpeg writer with a bounded frame queue and bounded stderr
/// tail.  The final file is renamed into place only after a clean exit.
pub struct FfmpegSink {
    config: FfmpegConfig,
    staging_path: PathBuf,
    sender: Option<mpsc::SyncSender<FramePacket>>,
    completion: mpsc::Receiver<WriterCompletion>,
    writer: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<()>>,
    child: Child,
    supervisor: ProcessSupervisor,
    cancelled: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
    stderr_done: Arc<AtomicBool>,
    expected_len: usize,
    next_index: u64,
    bytes: u64,
    terminated: bool,
    deadline: Instant,
    finalized: bool,
}

impl FfmpegSink {
    pub fn spawn(config: FfmpegConfig) -> Result<Self, SinkError> {
        Self::spawn_with_cancellation(config, Arc::new(AtomicBool::new(false)))
    }

    /// Spawn an encoder supervised by a caller-owned cancellation token.
    ///
    /// Export jobs pass their own token here so a cancellation requested while
    /// the bounded queue is full interrupts the producer as well as the
    /// renderer.
    pub fn spawn_with_cancellation(
        config: FfmpegConfig,
        cancellation: Arc<AtomicBool>,
    ) -> Result<Self, SinkError> {
        let expected_len = checked_frame_len(config.width, config.height)?;
        if config.queue_capacity == 0 {
            return Err(SinkError::ZeroQueueCapacity);
        }
        let queue_bytes = (expected_len as u64)
            .checked_mul(config.queue_capacity as u64)
            .ok_or(SinkError::QueueByteCap {
                required: u64::MAX,
                cap: config.max_queue_bytes,
            })?;
        if queue_bytes > config.max_queue_bytes {
            return Err(SinkError::QueueByteCap {
                required: queue_bytes,
                cap: config.max_queue_bytes,
            });
        }
        if config.output.as_os_str().is_empty() {
            return Err(SinkError::InvalidPath(config.output));
        }
        if config.output.exists() {
            return Err(SinkError::OutputExists(config.output));
        }
        let deadline = Instant::now()
            .checked_add(config.timeout)
            .unwrap_or_else(Instant::now);
        let staging_path = make_staging_path(&config.output, "video")?;
        let mut command = Command::new(&config.executable);
        command.args(config.argv(&staging_path));
        command.stdin(Stdio::piped());
        command.stdout(Stdio::null());
        command.stderr(Stdio::piped());
        prepare_process(&mut command);
        let mut child = spawn_command(&mut command).map_err(SinkError::Spawn)?;
        let supervisor = match create_process_supervisor(&child) {
            Ok(supervisor) => supervisor,
            Err(error) => {
                reap_direct_child(&mut child);
                let _ = fs::remove_file(&staging_path);
                return Err(SinkError::Spawn(error));
            }
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                kill_and_reap_child(&mut child, &supervisor);
                return Err(SinkError::Spawn(io::Error::other(
                    "ffmpeg stdin was not piped",
                )));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                drop(stdin);
                kill_and_reap_child(&mut child, &supervisor);
                return Err(SinkError::Spawn(io::Error::other(
                    "ffmpeg stderr was not piped",
                )));
            }
        };
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(config.max_stderr_bytes)));
        let stderr_tail_clone = Arc::clone(&stderr_tail);
        let stderr_done = Arc::new(AtomicBool::new(false));
        let stderr_done_reader = Arc::clone(&stderr_done);
        let max_stderr = config.max_stderr_bytes;
        let stderr_reader = match thread::Builder::new()
            .name("gource-ffmpeg-stderr".to_owned())
            .spawn(move || {
                drain_stderr(stderr, stderr_tail_clone, max_stderr);
                stderr_done_reader.store(true, Ordering::Release);
            }) {
            Ok(handle) => handle,
            Err(error) => {
                drop(stdin);
                kill_and_reap_child(&mut child, &supervisor);
                let _ = fs::remove_file(&staging_path);
                return Err(SinkError::Spawn(error));
            }
        };

        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let (completion_sender, completion) = mpsc::channel();
        let cancelled_writer = Arc::clone(&cancellation);
        let writer = match thread::Builder::new()
            .name("gource-ffmpeg-writer".to_owned())
            .spawn(move || {
                let result = write_frames(stdin, receiver, cancelled_writer, expected_len);
                let _ = completion_sender.send(WriterCompletion { result });
            }) {
            Ok(handle) => handle,
            Err(error) => {
                kill_and_reap_child(&mut child, &supervisor);
                let _ = stderr_reader.join();
                let _ = fs::remove_file(&staging_path);
                return Err(SinkError::Spawn(error));
            }
        };
        Ok(Self {
            config,
            staging_path,
            sender: Some(sender),
            completion,
            writer: Some(writer),
            stderr_reader: Some(stderr_reader),
            child,
            supervisor,
            cancelled: cancellation,
            stderr_tail,
            stderr_done,
            expected_len,
            next_index: 0,
            bytes: 0,
            deadline,
            terminated: false,
            finalized: false,
        })
    }

    pub fn new(config: FfmpegConfig) -> Result<Self, SinkError> {
        Self::spawn(config)
    }
    #[must_use]
    pub fn output_path(&self) -> &Path {
        &self.config.output
    }

    #[must_use]
    pub fn staging_path(&self) -> &Path {
        &self.staging_path
    }

    #[must_use]
    pub fn stderr_tail(&self) -> String {
        let mut tail = self.stderr_tail.lock().expect("stderr mutex poisoned");
        String::from_utf8_lossy(tail.make_contiguous()).into_owned()
    }
}

impl FrameSink for FfmpegSink {
    fn push(&mut self, index: u64, rgba8: &[u8]) -> Result<(), SinkError> {
        if self.cancelled.load(Ordering::Acquire) {
            self.abort_and_remove();
            return Err(SinkError::Cancelled);
        }
        if index != self.next_index {
            return Err(SinkError::FrameOrder {
                expected: self.next_index,
                actual: index,
            });
        }
        if rgba8.len() != self.expected_len {
            return Err(SinkError::FrameLength {
                expected: self.expected_len,
                actual: rgba8.len(),
            });
        }
        self.push_packet_wait(FramePacket {
            index,
            bytes: rgba8.to_vec(),
        })
    }
    fn finish(&mut self) -> Result<SinkReport, SinkError> {
        if self.finalized {
            return Ok(self.report());
        }
        if self.cancelled.load(Ordering::Acquire) {
            self.abort_and_remove();
            return Err(SinkError::EncoderCancelled {
                stderr: self.stderr_tail(),
            });
        }
        let status = match self.wait_writer_and_child() {
            Ok(status) => status,
            Err(error) => {
                self.abort_and_remove();
                return Err(error);
            }
        };
        if !status.success() {
            self.abort_and_remove();
            return Err(SinkError::Nonzero {
                status: status.to_string(),
                stderr: self.stderr_tail(),
            });
        }
        if self.cancelled.load(Ordering::Acquire) {
            self.abort_and_remove();
            return Err(SinkError::EncoderCancelled {
                stderr: self.stderr_tail(),
            });
        }
        if Instant::now() >= self.deadline {
            self.abort_and_remove();
            return Err(SinkError::Timeout {
                stderr: self.stderr_tail(),
            });
        }
        fs::rename(&self.staging_path, &self.config.output).map_err(|error| {
            self.abort_and_remove();
            SinkError::Publish(error)
        })?;
        self.finalized = true;
        Ok(self.report())
    }

    fn cancel(&mut self) {
        if self.finalized {
            return;
        }
        self.abort_and_remove();
    }

    fn report(&self) -> SinkReport {
        SinkReport {
            frames: self.next_index,
            bytes: self.bytes,
            output: self.config.output.clone(),
        }
    }
}

impl FfmpegSink {
    fn kill_and_reap(&mut self) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        self.cancelled.store(true, Ordering::Release);
        kill_and_reap_child(&mut self.child, &self.supervisor);
    }

    fn reap_writer(&mut self) {
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
    }

    fn reap_stderr(&mut self) {
        if let Some(handle) = self.stderr_reader.take() {
            let _ = handle.join();
        }
    }

    fn abort_and_remove(&mut self) {
        self.sender.take();
        self.kill_and_reap();
        self.reap_writer();
        self.reap_stderr();
        self.remove_staging();
    }

    fn wait_writer_and_child(&mut self) -> Result<ExitStatus, SinkError> {
        self.sender.take();
        let mut completion = None;
        let mut status = None;
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                self.abort_and_remove();
                return Err(SinkError::EncoderCancelled {
                    stderr: self.stderr_tail(),
                });
            }
            if Instant::now() >= self.deadline {
                self.abort_and_remove();
                return Err(SinkError::Timeout {
                    stderr: self.stderr_tail(),
                });
            }
            if completion.is_none() {
                completion = match self.completion.try_recv() {
                    Ok(done) => Some(done.result),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Some(Err("writer completion channel disconnected".to_owned()))
                    }
                };
            }
            if let Some(result) = completion.as_ref()
                && result.is_err()
            {
                let message = match completion.take() {
                    Some(Err(message)) => message,
                    _ => unreachable!("writer result changed while borrowed"),
                };
                self.abort_and_remove();
                return Err(SinkError::Writer {
                    message,
                    stderr: self.stderr_tail(),
                });
            }
            if status.is_none() {
                status = match self.child.try_wait() {
                    Ok(status) => status,
                    Err(error) => {
                        self.abort_and_remove();
                        return Err(SinkError::Io(error));
                    }
                };
            }
            if let Some(status) = status {
                if !status.success() {
                    let status_text = status.to_string();
                    self.abort_and_remove();
                    return Err(SinkError::Nonzero {
                        status: status_text,
                        stderr: self.stderr_tail(),
                    });
                }
                if completion.is_some() && self.stderr_done.load(Ordering::Acquire) {
                    self.reap_writer();
                    self.reap_stderr();
                    return Ok(status);
                }
            }
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            thread::sleep(std::cmp::min(Duration::from_millis(5), remaining));
        }
    }

    fn push_packet_wait(&mut self, mut packet: FramePacket) -> Result<(), SinkError> {
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                self.abort_and_remove();
                return Err(SinkError::Cancelled);
            }
            if Instant::now() >= self.deadline {
                self.abort_and_remove();
                return Err(SinkError::Timeout {
                    stderr: self.stderr_tail(),
                });
            }
            let completion = match self.completion.try_recv() {
                Ok(done) => Some(done.result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("writer completion channel disconnected".to_owned()))
                }
            };
            if let Some(result) = completion {
                self.abort_and_remove();
                return match result {
                    Ok(()) => Err(SinkError::Writer {
                        message: "writer exited before packet was accepted".to_owned(),
                        stderr: self.stderr_tail(),
                    }),
                    Err(message) => Err(SinkError::Writer {
                        message,
                        stderr: self.stderr_tail(),
                    }),
                };
            }
            let status = match self.child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    self.abort_and_remove();
                    return Err(SinkError::Io(error));
                }
            };
            if let Some(status) = status {
                let status_text = status.to_string();
                let success = status.success();
                self.abort_and_remove();
                return if success {
                    Err(SinkError::Writer {
                        message: "encoder exited before packet was accepted".to_owned(),
                        stderr: self.stderr_tail(),
                    })
                } else {
                    Err(SinkError::Nonzero {
                        status: status_text,
                        stderr: self.stderr_tail(),
                    })
                };
            }
            let send_result = {
                let sender = match self.sender.as_ref() {
                    Some(sender) => sender,
                    None => {
                        self.abort_and_remove();
                        return Err(SinkError::QueueDisconnected);
                    }
                };
                sender.try_send(packet)
            };
            match send_result {
                Ok(()) => {
                    self.next_index = self.next_index.saturating_add(1);
                    self.bytes = self.bytes.saturating_add(self.expected_len as u64);
                    return Ok(());
                }
                Err(mpsc::TrySendError::Full(next)) => {
                    packet = next;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    self.abort_and_remove();
                    return Err(SinkError::QueueDisconnected);
                }
            }
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            thread::sleep(std::cmp::min(Duration::from_millis(2), remaining));
        }
    }

    fn remove_staging(&self) {
        let _ = fs::remove_file(&self.staging_path);
    }
}

impl Drop for FfmpegSink {
    fn drop(&mut self) {
        if !self.finalized {
            self.cancel();
        }
    }
}

fn write_frames(
    mut stdin: ChildStdin,
    receiver: mpsc::Receiver<FramePacket>,
    cancelled: Arc<AtomicBool>,
    expected_len: usize,
) -> Result<(), String> {
    let mut expected = 0u64;
    for packet in receiver {
        if cancelled.load(Ordering::Acquire) {
            return Err("cancelled".to_owned());
        }
        if packet.index != expected {
            return Err(format!(
                "frame order violation: expected {expected}, got {}",
                packet.index
            ));
        }
        if packet.bytes.len() != expected_len {
            return Err(format!(
                "frame length mismatch: expected {expected_len}, got {}",
                packet.bytes.len()
            ));
        }
        stdin
            .write_all(&packet.bytes)
            .map_err(|error| error.to_string())?;
        expected = expected.saturating_add(1);
    }
    stdin.flush().map_err(|error| error.to_string())
}

fn drain_stderr(mut stderr: impl Read, tail: Arc<Mutex<VecDeque<u8>>>, cap: usize) {
    let mut buffer = [0u8; 4096];
    loop {
        let read = match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        if cap == 0 {
            continue;
        }
        let mut target = tail.lock().expect("stderr mutex poisoned");
        target.extend(&buffer[..read]);
        while target.len() > cap {
            target.pop_front();
        }
    }
}

fn checked_frame_len(width: u32, height: u32) -> Result<usize, SinkError> {
    if width == 0 || height == 0 {
        return Err(SinkError::InvalidPath(PathBuf::from("zero frame extent")));
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|value| value.checked_mul(4))
        .ok_or(SinkError::InvalidPath(PathBuf::from(
            "frame extent overflow",
        )))
}

fn make_staging_path(final_path: &Path, kind: &str) -> Result<PathBuf, SinkError> {
    let parent = final_path
        .parent()
        .ok_or_else(|| SinkError::MissingParent(final_path.to_owned()))?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    fs::create_dir_all(parent).map_err(SinkError::Io)?;
    let name = final_path
        .file_name()
        .ok_or_else(|| SinkError::InvalidPath(final_path.to_owned()))?
        .to_string_lossy();
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{name}.{kind}.partial-{}-{sequence}",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(SinkError::InvalidPath(final_path.to_owned()))
}

/// A small bounded in-memory queue useful for sinks and deterministic tests.
#[derive(Debug)]
pub struct BoundedFrameQueue {
    capacity: usize,
    max_bytes: u64,
    bytes: u64,
    queue: VecDeque<FramePacket>,
}

impl BoundedFrameQueue {
    pub fn new(capacity: usize, max_bytes: u64) -> Result<Self, SinkError> {
        if capacity == 0 {
            return Err(SinkError::ZeroQueueCapacity);
        }
        Ok(Self {
            capacity,
            max_bytes,
            bytes: 0,
            queue: VecDeque::with_capacity(capacity),
        })
    }

    pub fn push(&mut self, index: u64, bytes: Vec<u8>) -> Result<(), SinkError> {
        let next = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or(SinkError::QueueByteCap {
                required: u64::MAX,
                cap: self.max_bytes,
            })?;
        if self.queue.len() >= self.capacity || next > self.max_bytes {
            return Err(SinkError::QueueByteCap {
                required: next,
                cap: self.max_bytes,
            });
        }
        self.bytes = next;
        self.queue.push_back(FramePacket { index, bytes });
        Ok(())
    }

    pub fn pop(&mut self) -> Option<(u64, Vec<u8>)> {
        let packet = self.queue.pop_front()?;
        self.bytes = self.bytes.saturating_sub(packet.bytes.len() as u64);
        Some((packet.index, packet.bytes))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::sync::Barrier;
    use tempfile::tempdir;

    #[cfg(unix)]
    fn fixture(body: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempdir().unwrap();
        let path = directory.path().join("encoder-fixture");
        let script = format!("#!/bin/sh\n{body}\n");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).unwrap();
        (directory, path)
    }

    #[cfg(unix)]
    fn make_fifo(path: &Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo failed for {}", path.display());
    }

    #[cfg(unix)]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    #[test]
    fn png_sink_rejects_out_of_order_and_publishes_atomically() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("frames");
        let mut sink = PngFrameSink::new(&output, 1, 1).unwrap();
        assert!(matches!(
            sink.push(1, &[0, 0, 0, 255]),
            Err(SinkError::FrameOrder { .. })
        ));
        let expected = [1, 2, 3, 255];
        sink.push(0, &expected).unwrap();
        assert!(!output.exists(), "final directory published before finish");
        assert!(sink.staging_dir().is_dir());

        sink.finish().unwrap();
        let decoded = image::open(output.join("frame_00000000.png"))
            .unwrap()
            .into_rgba8()
            .into_raw();
        assert_eq!(decoded, expected);
        assert!(!sink.staging_dir().exists());
    }

    #[test]
    fn bounded_queue_enforces_count_and_preserves_order() {
        let mut queue = BoundedFrameQueue::new(2, 8).unwrap();
        queue.push(0, vec![1; 4]).unwrap();
        queue.push(1, vec![2; 4]).unwrap();
        assert!(queue.push(2, vec![3]).is_err());
        assert_eq!(queue.pop(), Some((0, vec![1; 4])));
        assert_eq!(queue.pop(), Some((1, vec![2; 4])));
    }

    #[test]
    fn ffmpeg_missing_executable_preserves_spawn_error() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("missing.mkv");
        let mut config = FfmpegConfig::new(&output, 1, 1, FrameRate::integer(30).unwrap());
        config.executable = directory.path().join("missing-encoder");
        assert!(matches!(
            FfmpegSink::spawn(config),
            Err(SinkError::Spawn(error)) if error.kind() == io::ErrorKind::NotFound
        ));
    }
    #[cfg(unix)]
    #[test]
    fn ffmpeg_blocked_queue_honors_job_deadline_and_reaps() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("blocked.mkv");
        let (_fixture_directory, executable) = fixture("exec sleep 30");
        let mut config = FfmpegConfig::new(&output, 256, 256, FrameRate::integer(30).unwrap());
        config.executable = executable;
        config.queue_capacity = 1;
        config.max_queue_bytes = 256 * 256 * 4;
        config.timeout = Duration::from_millis(75);
        let frame = vec![0u8; 256 * 256 * 4];
        let mut sink = FfmpegSink::spawn(config).unwrap();
        let staging = sink.staging_path().to_owned();
        let started = Instant::now();
        let mut result = Ok(());
        for index in 0..32 {
            match sink.push(index, &frame) {
                Ok(()) => {}
                error => {
                    result = error;
                    break;
                }
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "blocked encoder exceeded its deadline"
        );
        assert!(matches!(result, Err(SinkError::Timeout { .. })));
        assert!(!staging.exists());
        assert!(!output.exists());
    }

    #[cfg(unix)]
    #[test]
    fn ffmpeg_blocked_queue_honors_external_cancellation_and_reaps() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("blocked-cancelled.mkv");
        let ready = directory.path().join("ready");
        let control = directory.path().join("control");
        make_fifo(&ready);
        make_fifo(&control);
        let (_fixture_directory, executable) = fixture(&format!(
            "out=\"\"; for arg in \"$@\"; do out=\"$arg\"; done; printf ready > {}; cat < {} >/dev/null; exec sleep 30",
            shell_quote(&ready),
            shell_quote(&control),
        ));
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut config = FfmpegConfig::new(&output, 1024, 1024, FrameRate::integer(30).unwrap());
        config.executable = executable;
        config.queue_capacity = 1;
        config.max_queue_bytes = 1024 * 1024 * 4;
        config.timeout = Duration::from_secs(2);
        let frame = vec![0u8; 1024 * 1024 * 4];
        let mut sink =
            FfmpegSink::spawn_with_cancellation(config, Arc::clone(&cancellation)).unwrap();
        let staging = sink.staging_path().to_owned();

        // Opening the readiness FIFO blocks until the fixture has started.
        // The control FIFO then keeps its descendant alive without reading the
        // encoder stdin, so the bounded producer is guaranteed to fill.
        let mut readiness = fs::OpenOptions::new().read(true).open(&ready).unwrap();
        let mut marker = [0u8; 5];
        readiness.read_exact(&mut marker).unwrap();
        assert_eq!(&marker, b"ready");
        let control_writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&control)
            .unwrap();

        sink.push(0, &frame).unwrap();
        sink.push(1, &frame).unwrap();
        let gate = Arc::new(Barrier::new(2));
        let canceller = {
            let gate = Arc::clone(&gate);
            let cancellation = Arc::clone(&cancellation);
            thread::spawn(move || {
                gate.wait();
                cancellation.store(true, Ordering::Release);
            })
        };
        gate.wait();
        let result = sink.push(2, &frame);
        canceller.join().unwrap();
        drop(control_writer);

        assert!(matches!(result, Err(SinkError::Cancelled)));
        assert!(!staging.exists());
        assert!(!output.exists());
    }
    #[cfg(unix)]
    #[test]
    fn ffmpeg_success_fixture_publishes_only_after_clean_exit() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("out.mkv");
        let (_fixture_directory, executable) =
            fixture("out=\"\"; for arg in \"$@\"; do out=\"$arg\"; done; cat > \"$out\"");
        let mut config = FfmpegConfig::new(&output, 1, 1, FrameRate::integer(30).unwrap());
        config.executable = executable;
        config.queue_capacity = 1;
        config.max_queue_bytes = 4;
        config.timeout = Duration::from_secs(2);
        let mut sink = FfmpegSink::spawn(config).unwrap();
        let first = [1, 2, 3, 4];
        let second = [5, 6, 7, 8];
        sink.push(0, &first).unwrap();
        sink.push(1, &second).unwrap();
        assert!(!output.exists(), "final output published before finish");

        let report = sink.finish().unwrap();
        assert_eq!(report.frames, 2);
        assert_eq!(report.bytes, 8);
        assert_eq!(fs::read(&output).unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(!sink.staging_path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn ffmpeg_nonzero_fixture_reaps_and_removes_partial_output() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("failed.mkv");
        let ready = directory.path().join("frame-ready");
        let release = directory.path().join("release");
        make_fifo(&ready);
        make_fifo(&release);
        let (_fixture_directory, executable) = fixture(&format!(
            "out=\"\"; for arg in \"$@\"; do out=\"$arg\"; done; dd if=/dev/stdin bs=4 count=1 of=\"$out\" 2>/dev/null; printf frame > {}; cat < {} >/dev/null; dd if=/dev/zero bs=1024 count=1 >&2; echo fixture-failure >&2; exit 7",
            shell_quote(&ready),
            shell_quote(&release),
        ));
        const STDERR_CAP: usize = 64;
        let mut config = FfmpegConfig::new(&output, 1, 1, FrameRate::integer(30).unwrap());
        config.executable = executable;
        config.queue_capacity = 1;
        config.max_queue_bytes = 4;
        config.max_stderr_bytes = STDERR_CAP;
        config.timeout = Duration::from_secs(2);
        let mut sink = FfmpegSink::spawn(config).unwrap();
        let frame = [1, 2, 3, 4];
        sink.push(0, &frame).unwrap();

        // The readiness marker is written only after the fixture consumed the
        // first frame. Release the fixture, then observe its actual exit
        // status before exercising the producer's failure path.
        let mut readiness = fs::OpenOptions::new().read(true).open(&ready).unwrap();
        let mut marker = [0u8; 5];
        readiness.read_exact(&mut marker).unwrap();
        assert_eq!(&marker, b"frame");
        assert!(sink.staging_path().is_file());
        let mut release_writer = fs::OpenOptions::new().write(true).open(&release).unwrap();
        release_writer.write_all(b"exit").unwrap();
        drop(release_writer);
        let status = sink.child.wait().unwrap();
        assert_eq!(status.code(), Some(7));

        let failure = sink.push(1, &frame).unwrap_err();
        let stderr = match &failure {
            SinkError::Nonzero { stderr, .. } | SinkError::Writer { stderr, .. } => stderr,
            error => panic!("unexpected typed failure: {error:?}"),
        };
        assert!(stderr.contains("fixture-failure"));
        assert!(stderr.len() <= STDERR_CAP);
        assert!(!sink.staging_path().exists());
        assert!(!output.exists());
    }

    #[cfg(unix)]
    #[test]
    fn ffmpeg_hung_fixture_times_out_and_is_reaped() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("hung.mkv");
        let (_fixture_directory, executable) = fixture("exec sleep 30");
        let mut config = FfmpegConfig::new(&output, 1, 1, FrameRate::integer(30).unwrap());
        config.executable = executable;
        config.queue_capacity = 1;
        config.max_queue_bytes = 4;
        config.timeout = Duration::from_millis(50);
        let mut sink = FfmpegSink::spawn(config).unwrap();
        sink.push(0, &[1, 2, 3, 4]).unwrap();
        assert!(matches!(sink.finish(), Err(SinkError::Timeout { .. })));
        assert!(!output.exists());
    }

    #[cfg(unix)]
    #[test]
    fn ffmpeg_cancellation_removes_partial_output() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("cancelled.mkv");
        let (_fixture_directory, executable) = fixture("exec sleep 30");
        let mut config = FfmpegConfig::new(&output, 1, 1, FrameRate::integer(30).unwrap());
        config.executable = executable;
        config.queue_capacity = 1;
        config.max_queue_bytes = 4;
        config.timeout = Duration::from_secs(2);
        let mut sink = FfmpegSink::spawn(config).unwrap();
        sink.push(0, &[1, 2, 3, 4]).unwrap();
        sink.cancel();
        assert!(matches!(
            sink.finish(),
            Err(SinkError::EncoderCancelled { .. })
        ));
        assert!(!output.exists());
    }
}
