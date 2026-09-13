// SPDX-FileCopyrightText: 2026 Gource contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Bounded local Git history ingestion.
//!
//! Git is intentionally treated as an untrusted local helper. The adapter
//! starts one fixed `git log` invocation with explicit arguments, drains both
//! output streams concurrently, and publishes an index only after the child
//! has exited successfully and the complete machine protocol has validated.

use std::fmt::Write as FmtWrite;
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::parser::{self, NormalizedRecord, ParsedAction, ParsedInput};
use crate::{ErrorCode, IndexedHistory, IngestError, IngestOptions};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_STDERR_BYTES: u64 = 64 * 1024;
const READER_CHUNK_BYTES: usize = 8 * 1024;
const READER_CHANNEL_CAPACITY: usize = 4;
const COMMIT_MARKER: &[u8] = b"GOURCE-COMMIT";
const MAX_REVISION_BYTES: u64 = 4 * 1024;

/// Options controlling the fixed Git subprocess and its wire protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitOptions {
    /// Git executable to invoke. This is passed directly to
    /// [`std::process::Command`] and is never interpreted as shell text.
    pub executable: PathBuf,
    /// Wall-clock deadline for the complete child process and pipe drain.
    pub timeout: Duration,
    /// Maximum diagnostic bytes accepted from stderr. Exceeding this bound
    /// fails the operation rather than truncating diagnostics silently.
    pub max_stderr_bytes: u64,
    /// Select the author timestamp (`%at`) instead of the committer timestamp
    /// (`%ct`).
    pub author_time: bool,
    /// Optional single revision/commit-ish. It is resolved to one full object
    /// id before the log is read and is never interpolated into a command.
    pub revision: Option<String>,
}

impl Default for GitOptions {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("git"),
            timeout: DEFAULT_TIMEOUT,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
            author_time: false,
            revision: None,
        }
    }
}

impl GitOptions {
    #[must_use]
    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = executable.into();
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_max_stderr_bytes(mut self, bytes: u64) -> Self {
        self.max_stderr_bytes = bytes;
        self
    }

    #[must_use]
    pub fn with_author_time(mut self, author_time: bool) -> Self {
        self.author_time = author_time;
        self
    }

    #[must_use]
    pub fn with_revision(mut self, revision: impl Into<String>) -> Self {
        self.revision = Some(revision.into());
        self
    }

    #[must_use]
    pub fn with_ref_name(self, revision: impl Into<String>) -> Self {
        self.with_revision(revision)
    }
}

/// Errors raised while validating, supervising, and decoding a local Git
/// history. A wrapped [`IngestError`] means the Git protocol was complete but
/// a normalized record violated the common index contract.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("repository path is not a directory: {0}")]
    InvalidRepository(PathBuf),
    #[error("Git options are invalid: {0}")]
    InvalidOptions(String),
    #[error("Git process could not be started: {0}")]
    Spawn(#[source] io::Error),
    #[error("Git process I/O failed: {0}")]
    Io(#[source] io::Error),
    #[error("Git process exited unsuccessfully ({status}); stderr: {stderr}")]
    Nonzero { status: String, stderr: String },
    #[error("Git process timed out and was killed; stderr: {stderr}")]
    Timeout { stderr: String },
    #[error("Git process was cancelled and killed; stderr: {stderr}")]
    Cancelled { stderr: String },
    #[error("Git stdout exceeded the configured {limit}-byte limit")]
    StdoutLimit { limit: u64 },
    #[error("Git stderr exceeded the configured {limit}-byte limit")]
    StderrLimit { limit: u64 },
    #[error("malformed Git output at byte {offset}: {message}")]
    MalformedOutput { offset: u64, message: String },
    #[error("Git output is not valid UTF-8 at byte {offset}: {stream}")]
    InvalidUtf8 { stream: String, offset: u64 },
    #[error(
        "Git protocol memory requirement {requested} bytes exceeds configured {limit}-byte limit"
    )]
    WorkingMemoryLimit { requested: u64, limit: u64 },
    #[error("Git protocol event count exceeds configured {limit}")]
    EventCountLimit { limit: u64 },
    #[error("Git record exceeds configured {limit}-byte record limit")]
    RecordTooLarge { limit: u64 },
    #[error("Git ingest failed: {0}")]
    Ingest(#[source] IngestError),
}

impl GitError {
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled { .. })
            || matches!(self, Self::Ingest(error) if error.is_cancelled())
    }
}

/// Ingest a finite local repository through a fixed, non-shell Git command.
///
/// The resulting `IndexedHistory` is built through the same catalog/event
/// builder as custom logs. No partial history is returned on process,
/// protocol, limit, cancellation, or indexing failure.
pub fn ingest_git_repository(
    path: impl AsRef<Path>,
    ingest_options: &IngestOptions,
    git_options: &GitOptions,
) -> Result<IndexedHistory, GitError> {
    let repository = validate_repository_path(path.as_ref())?;
    validate_git_options(git_options)?;
    ingest_options.limits.validate().map_err(GitError::Ingest)?;
    ingest_options
        .check_cancelled(0, 0)
        .map_err(GitError::Ingest)?;

    let parsed = run_git(&repository, ingest_options, git_options)?;
    let catalog = crate::new_catalog(ingest_options);
    crate::build_index(parsed, ingest_options, catalog, crate::CATALOG_MEMORY_BASE)
        .map_err(GitError::Ingest)
}

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RepositoryIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial: u32,
    #[cfg(windows)]
    file_index: u64,
}

fn repository_identity(path: &Path, metadata: &Metadata) -> io::Result<RepositoryIdentity> {
    #[cfg(unix)]
    {
        let _ = path;
        Ok(RepositoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        windows_repository_identity(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, metadata);
        Ok(RepositoryIdentity {})
    }
}

fn same_repository_identity(left: RepositoryIdentity, right: RepositoryIdentity) -> bool {
    left == right
}
#[cfg(windows)]
fn windows_repository_identity(path: &Path) -> io::Result<RepositoryIdentity> {
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const INVALID_HANDLE_VALUE: WinHandle = -1isize as WinHandle;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut information: ByHandleFileInformation = unsafe { std::mem::zeroed() };
    let result = unsafe { GetFileInformationByHandle(handle, &mut information) };
    let error = if result == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok(RepositoryIdentity {
        volume_serial: information.volume_serial_number,
        file_index: (u64::from(information.file_index_high) << 32)
            | u64::from(information.file_index_low),
    })
}

struct ValidatedRepository {
    path: PathBuf,
    identity: RepositoryIdentity,
}

fn validate_repository_path(path: &Path) -> Result<ValidatedRepository, GitError> {
    if path.as_os_str().is_empty() {
        return Err(GitError::InvalidRepository(path.to_owned()));
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|_| GitError::InvalidRepository(path.to_owned()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(GitError::InvalidRepository(path.to_owned()));
    }
    let canonical =
        fs::canonicalize(path).map_err(|_| GitError::InvalidRepository(path.to_owned()))?;
    let canonical_metadata = fs::symlink_metadata(&canonical)
        .map_err(|_| GitError::InvalidRepository(path.to_owned()))?;
    if canonical_metadata.file_type().is_symlink() || !canonical_metadata.is_dir() {
        return Err(GitError::InvalidRepository(path.to_owned()));
    }
    let identity = repository_identity(&canonical, &canonical_metadata)
        .map_err(|_| GitError::InvalidRepository(path.to_owned()))?;
    Ok(ValidatedRepository {
        path: canonical,
        identity,
    })
}

fn revalidate_repository(repository: &ValidatedRepository) -> Result<(), GitError> {
    let metadata = fs::symlink_metadata(&repository.path)
        .map_err(|_| GitError::InvalidRepository(repository.path.clone()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(GitError::InvalidRepository(repository.path.clone()));
    }
    let identity = repository_identity(&repository.path, &metadata)
        .map_err(|_| GitError::InvalidRepository(repository.path.clone()))?;
    if !same_repository_identity(repository.identity, identity) {
        return Err(GitError::InvalidRepository(repository.path.clone()));
    }
    Ok(())
}

fn validate_git_options(options: &GitOptions) -> Result<(), GitError> {
    if options.executable.as_os_str().is_empty() {
        return Err(GitError::InvalidOptions(
            "the Git executable path cannot be empty".to_owned(),
        ));
    }
    if options.timeout.is_zero() {
        return Err(GitError::InvalidOptions(
            "the Git timeout must be positive".to_owned(),
        ));
    }
    if options.max_stderr_bytes == 0 {
        return Err(GitError::InvalidOptions(
            "the Git stderr limit must be positive".to_owned(),
        ));
    }
    if let Some(revision) = options.revision.as_deref() {
        let revision_bytes = u64::try_from(revision.len()).unwrap_or(u64::MAX);
        if revision_bytes > MAX_REVISION_BYTES {
            return Err(GitError::InvalidOptions(
                "the Git revision exceeds the 4096-byte limit".to_owned(),
            ));
        }
        if revision.is_empty()
            || revision.starts_with('-')
            || revision.bytes().any(|byte| byte == 0)
            || revision.contains("..")
            || revision.contains(':')
            || revision.chars().any(char::is_whitespace)
        {
            return Err(GitError::InvalidOptions(
                "the Git revision must be one non-option commit-ish".to_owned(),
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamKind {
    Stdout,
    Stderr,
}

enum ReaderMessage {
    Data(StreamKind, Vec<u8>),
    Eof(StreamKind),
    Error(StreamKind, io::Error),
    Limit(StreamKind, u64),
}

fn spawn_reader<R: Read + Send + 'static>(
    stream: R,
    kind: StreamKind,
    limit: u64,
    sender: SyncSender<ReaderMessage>,
    stop: Arc<AtomicBool>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(match kind {
            StreamKind::Stdout => "gource-git-stdout".to_owned(),
            StreamKind::Stderr => "gource-git-stderr".to_owned(),
        })
        .spawn(move || {
            let mut stream = stream;
            let mut buffer = [0u8; READER_CHUNK_BYTES];
            let mut total = 0u64;
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                match stream.read(&mut buffer) {
                    Ok(0) => {
                        let _ = send_reader_message(&sender, ReaderMessage::Eof(kind), &stop);
                        return;
                    }
                    Ok(read) => {
                        let next_total = total.saturating_add(read as u64);
                        if next_total > limit {
                            let _ = send_reader_message(
                                &sender,
                                ReaderMessage::Limit(kind, limit),
                                &stop,
                            );
                            return;
                        }
                        total = next_total;
                        if !send_reader_message(
                            &sender,
                            ReaderMessage::Data(kind, buffer[..read].to_vec()),
                            &stop,
                        ) {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ =
                            send_reader_message(&sender, ReaderMessage::Error(kind, error), &stop);
                        return;
                    }
                }
            }
        })
}

fn send_reader_message(
    sender: &SyncSender<ReaderMessage>,
    message: ReaderMessage,
    stop: &AtomicBool,
) -> bool {
    let mut message = message;
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match sender.try_send(message) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(returned)) => {
                message = returned;
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn join_reader(handle: &mut Option<JoinHandle<()>>) -> Result<(), GitError> {
    if let Some(handle) = handle.take() {
        handle
            .join()
            .map_err(|_| GitError::Io(io::Error::other("Git output reader thread panicked")))?;
    }
    Ok(())
}

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
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time: [u32; 2],
    last_access_time: [u32; 2],
    last_write_time: [u32; 2],
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
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
    fn CreateFileW(
        name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut std::ffi::c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template: WinHandle,
    ) -> WinHandle;
    fn GetFileInformationByHandle(
        file: WinHandle,
        information: *mut ByHandleFileInformation,
    ) -> i32;
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
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    }
}

fn reap_direct_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
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
                    if let Some(error) = error {
                        result = Err(error);
                    } else {
                        result = Ok(());
                    }
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

fn kill_and_reap_child(child: &mut Child, supervisor: &ProcessSupervisor) {
    #[cfg(unix)]
    let _ = supervisor;
    #[cfg(unix)]
    {
        const SIGKILL: i32 = 9;
        let pid = child.id();
        if pid > 0 && pid <= i32::MAX as u32 {
            // `prepare_process` places the child in a fresh process group.
            // Killing the group also closes inherited pipes held by an
            // accidental descendant before reader threads are joined.
            unsafe {
                let _ = kill(-(pid as i32), SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        unsafe {
            let _ = TerminateJobObject(supervisor.job, 1);
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = supervisor;
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_and_join(
    child: &mut Child,
    supervisor: &ProcessSupervisor,
    stop: &AtomicBool,
    stdout_reader: &mut Option<JoinHandle<()>>,
    stderr_reader: &mut Option<JoinHandle<()>>,
) -> Result<(), GitError> {
    stop.store(true, Ordering::Release);
    kill_and_reap_child(child, supervisor);
    let stdout_result = join_reader(stdout_reader);
    let stderr_result = join_reader(stderr_reader);
    stdout_result?;
    stderr_result
}

fn join_readers(
    stdout_reader: &mut Option<JoinHandle<()>>,
    stderr_reader: &mut Option<JoinHandle<()>>,
) -> Result<(), GitError> {
    let stdout_result = join_reader(stdout_reader);
    let stderr_result = join_reader(stderr_reader);
    stdout_result?;
    stderr_result
}

struct ProcessResult {
    status: ExitStatus,
    stderr: Vec<u8>,
    stdout_bytes: u64,
}

fn supervise_process<F>(
    mut command: Command,
    ingest_options: &IngestOptions,
    deadline: Instant,
    stdout_limit: u64,
    stderr_limit: u64,
    mut on_stdout: F,
) -> Result<ProcessResult, GitError>
where
    F: FnMut(&[u8], u64) -> Result<(), GitError>,
{
    prepare_process(&mut command);
    let mut child = command.spawn().map_err(GitError::Spawn)?;
    let supervisor = match create_process_supervisor(&child) {
        Ok(supervisor) => supervisor,
        Err(error) => {
            reap_direct_child(&mut child);
            return Err(GitError::Spawn(error));
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            kill_and_reap_child(&mut child, &supervisor);
            return Err(GitError::Spawn(io::Error::other(
                "Git stdout was not piped",
            )));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            kill_and_reap_child(&mut child, &supervisor);
            return Err(GitError::Spawn(io::Error::other(
                "Git stderr was not piped",
            )));
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(READER_CHANNEL_CAPACITY);
    let mut stdout_reader = match spawn_reader(
        stdout,
        StreamKind::Stdout,
        stdout_limit,
        sender.clone(),
        Arc::clone(&stop),
    ) {
        Ok(handle) => Some(handle),
        Err(error) => {
            kill_and_reap_child(&mut child, &supervisor);
            return Err(GitError::Spawn(error));
        }
    };
    let mut stderr_reader = match spawn_reader(
        stderr,
        StreamKind::Stderr,
        stderr_limit,
        sender,
        Arc::clone(&stop),
    ) {
        Ok(handle) => Some(handle),
        Err(error) => {
            terminate_and_join(
                &mut child,
                &supervisor,
                &stop,
                &mut stdout_reader,
                &mut None,
            )?;
            return Err(GitError::Spawn(error));
        }
    };

    let mut stderr_bytes = Vec::new();
    let mut stdout_bytes = 0u64;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut channel_closed = false;
    let mut status = None;

    let result = loop {
        if ingest_options.check_cancelled(0, stdout_bytes).is_err() {
            break Err(GitError::Cancelled {
                stderr: bounded_stderr_text(&stderr_bytes),
            });
        }
        if Instant::now() >= deadline {
            break Err(GitError::Timeout {
                stderr: bounded_stderr_text(&stderr_bytes),
            });
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(next) => status = next,
                Err(error) => break Err(GitError::Io(error)),
            }
        }
        if stdout_done && stderr_done && status.is_some() {
            break Ok(());
        }

        if channel_closed {
            if !stdout_done || !stderr_done {
                break Err(GitError::Io(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Git output readers disconnected",
                )));
            }

            // Both reader threads have joined and reported EOF, but the
            // child can still be between closing its descriptors and exiting.
            // Keep supervising it until wait observes a status or the
            // operation deadline fires; a disconnected channel alone is not
            // proof that the process completed successfully.
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(remaining.min(Duration::from_millis(10)));
            continue;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait_for = remaining.min(Duration::from_millis(10));
        match receiver.recv_timeout(wait_for) {
            Ok(ReaderMessage::Data(StreamKind::Stdout, bytes)) => {
                let offset = stdout_bytes;
                let next = stdout_bytes.saturating_add(bytes.len() as u64);
                if next > stdout_limit {
                    break Err(GitError::StdoutLimit {
                        limit: stdout_limit,
                    });
                }
                stdout_bytes = next;
                if let Err(error) = on_stdout(&bytes, offset) {
                    break Err(error);
                }
                ingest_options.report(crate::ProgressUpdate {
                    phase: crate::ProgressPhase::Reading,
                    bytes_read: stdout_bytes,
                    input_bytes: Some(stdout_bytes),
                    records_read: 0,
                });
            }
            Ok(ReaderMessage::Data(StreamKind::Stderr, bytes)) => {
                let next = stderr_bytes.len().saturating_add(bytes.len());
                if next as u64 > stderr_limit {
                    break Err(GitError::StderrLimit {
                        limit: stderr_limit,
                    });
                }
                if stderr_bytes.try_reserve_exact(bytes.len()).is_err() {
                    break Err(GitError::WorkingMemoryLimit {
                        requested: next as u64,
                        limit: stderr_limit,
                    });
                }
                stderr_bytes.extend_from_slice(&bytes);
            }
            Ok(ReaderMessage::Eof(StreamKind::Stdout)) => stdout_done = true,
            Ok(ReaderMessage::Eof(StreamKind::Stderr)) => stderr_done = true,
            Ok(ReaderMessage::Limit(StreamKind::Stdout, limit)) => {
                break Err(GitError::StdoutLimit { limit });
            }
            Ok(ReaderMessage::Limit(StreamKind::Stderr, limit)) => {
                break Err(GitError::StderrLimit { limit });
            }
            Ok(ReaderMessage::Error(StreamKind::Stdout, error)) => {
                break Err(GitError::Io(error));
            }
            Ok(ReaderMessage::Error(StreamKind::Stderr, error)) => {
                break Err(GitError::Io(error));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Sender exhaustion is normal only after both reader threads
                // have completed. Join now so a panic is reported as a typed
                // reader error instead of being mislabeled BrokenPipe.
                if let Err(error) = join_readers(&mut stdout_reader, &mut stderr_reader) {
                    break Err(error);
                }
                channel_closed = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };

    if result.is_err() {
        stop.store(true, Ordering::Release);
        kill_and_reap_child(&mut child, &supervisor);
    } else {
        stop.store(true, Ordering::Release);
    }

    // Always join both readers, including after a process, protocol, limit,
    // cancellation, or timeout error. A reader panic must never be hidden by
    // the error that happened to make the supervisor stop first.
    join_readers(&mut stdout_reader, &mut stderr_reader)?;
    result?;

    let status = status.ok_or_else(|| {
        GitError::Io(io::Error::other(
            "Git process status was unavailable after output readers completed",
        ))
    })?;
    Ok(ProcessResult {
        status,
        stderr: stderr_bytes,
        stdout_bytes,
    })
}

fn run_git(
    repository: &ValidatedRepository,
    ingest_options: &IngestOptions,
    git_options: &GitOptions,
) -> Result<ParsedInput, GitError> {
    let stderr_limit = git_options
        .max_stderr_bytes
        .min(ingest_options.limits.max_input_bytes)
        .min(ingest_options.limits.working_memory_bytes);
    if stderr_limit == 0 {
        return Err(GitError::InvalidOptions(
            "the effective Git stderr limit must be positive".to_owned(),
        ));
    }
    let deadline = Instant::now()
        .checked_add(git_options.timeout)
        .unwrap_or_else(Instant::now);

    // Always resolve a selector before reading history. This snapshots a
    // moving branch/HEAD to one immutable object id. An unborn HEAD is the
    // one intentional exception and is represented by an empty history.
    let requested_revision = git_options.revision.as_deref().unwrap_or("HEAD");
    revalidate_repository(repository)?;
    let revision = resolve_revision(
        repository,
        ingest_options,
        git_options,
        requested_revision,
        git_options.revision.is_none(),
        deadline,
        stderr_limit,
    )?;
    let Some(revision) = revision else {
        return Ok(ParsedInput {
            records: Vec::new(),
            input_bytes: 0,
            input_hash: *blake3::hash(&[]).as_bytes(),
            record_count: 0,
            runs: None,
        });
    };

    revalidate_repository(repository)?;
    let mut command = new_git_command(&repository.path, git_options);
    command
        .arg("log")
        .arg("--reverse")
        .arg("--root")
        .arg("--raw")
        .arg("-z")
        .arg("--no-renames")
        .arg("--no-ext-diff")
        .arg("--no-textconv")
        .arg("--no-color")
        .arg("--no-show-signature")
        .arg("--no-notes")
        .arg("--no-diff-merges")
        .arg("--ignore-submodules=none")
        .arg("-O")
        .arg(null_device_path())
        .arg("--encoding=UTF-8")
        .arg(if git_options.author_time {
            "--pretty=format:GOURCE-COMMIT%x00%at%x00%an%x00"
        } else {
            "--pretty=format:GOURCE-COMMIT%x00%ct%x00%an%x00"
        });
    command.arg("--end-of-options").arg(revision);

    let parser_limit =
        parser::parser_working_memory_bytes(ingest_options.limits.working_memory_bytes);
    let mut protocol = GitProtocolParser::new(ingest_options, parser_limit);
    let mut input_hash = blake3::Hasher::new();
    let result = supervise_process(
        command,
        ingest_options,
        deadline,
        ingest_options.limits.max_input_bytes,
        stderr_limit,
        |bytes, offset| {
            input_hash.update(bytes);
            protocol.feed(bytes, offset)
        },
    )?;
    if !result.status.success() {
        return Err(GitError::Nonzero {
            status: result.status.to_string(),
            stderr: bounded_stderr_text(&result.stderr),
        });
    }
    protocol.finish(result.stdout_bytes, *input_hash.finalize().as_bytes())
}

fn resolve_revision(
    repository: &ValidatedRepository,
    ingest_options: &IngestOptions,
    git_options: &GitOptions,
    revision: &str,
    allow_unborn_head: bool,
    deadline: Instant,
    stderr_limit: u64,
) -> Result<Option<String>, GitError> {
    let parser_limit =
        parser::parser_working_memory_bytes(ingest_options.limits.working_memory_bytes);
    let selector_length = revision
        .len()
        .checked_add(9)
        .and_then(|length| u64::try_from(length).ok())
        .unwrap_or(u64::MAX);
    if selector_length > MAX_REVISION_BYTES || selector_length > parser_limit {
        return Err(GitError::WorkingMemoryLimit {
            requested: selector_length,
            limit: parser_limit,
        });
    }
    let mut selector = String::new();
    selector
        .try_reserve_exact(selector_length as usize)
        .map_err(|_| GitError::WorkingMemoryLimit {
            requested: selector_length,
            limit: parser_limit,
        })?;
    selector.push_str(revision);
    selector.push_str("^{commit}");

    let mut command = new_git_command(&repository.path, git_options);
    command
        .arg("rev-parse")
        .arg("--verify")
        .arg("--end-of-options")
        .arg(selector);
    let mut bytes = Vec::new();
    let output_limit = ingest_options.limits.max_input_bytes.min(4096);
    let result = supervise_process(
        command,
        ingest_options,
        deadline,
        output_limit,
        stderr_limit,
        |chunk, _offset| {
            let requested = bytes
                .len()
                .checked_add(chunk.len())
                .and_then(|length| u64::try_from(length).ok())
                .unwrap_or(u64::MAX);
            bytes
                .try_reserve_exact(chunk.len())
                .map_err(|_| GitError::WorkingMemoryLimit {
                    requested,
                    limit: parser_limit,
                })?;
            bytes.extend_from_slice(chunk);
            Ok(())
        },
    )?;
    if !result.status.success() {
        if allow_unborn_head
            && probe_unborn_head(
                repository,
                ingest_options,
                git_options,
                deadline,
                stderr_limit,
            )?
        {
            return Ok(None);
        }
        return Err(GitError::Nonzero {
            status: result.status.to_string(),
            stderr: bounded_stderr_text(&result.stderr),
        });
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid_utf8("revision", 0))?;
    let oid = text
        .strip_suffix('\n')
        .ok_or_else(|| malformed(bytes.len() as u64, "rev-parse output lacks LF"))?;
    if !(oid.len() == 40 || oid.len() == 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(malformed(
            0,
            "rev-parse did not return exactly one full object id",
        ));
    }
    Ok(Some(oid.to_owned()))
}

fn probe_unborn_head(
    repository: &ValidatedRepository,
    ingest_options: &IngestOptions,
    git_options: &GitOptions,
    deadline: Instant,
    stderr_limit: u64,
) -> Result<bool, GitError> {
    let probe_limit = ingest_options.limits.max_input_bytes.min(4096);
    let mut symbolic = new_git_command(&repository.path, git_options);
    symbolic.arg("symbolic-ref").arg("--quiet").arg("HEAD");
    let mut symbolic_bytes = Vec::new();
    let symbolic_result = supervise_process(
        symbolic,
        ingest_options,
        deadline,
        probe_limit,
        stderr_limit,
        |bytes, _offset| {
            symbolic_bytes.try_reserve_exact(bytes.len()).map_err(|_| {
                GitError::WorkingMemoryLimit {
                    requested: symbolic_bytes.len().saturating_add(bytes.len()) as u64,
                    limit: parser::parser_working_memory_bytes(
                        ingest_options.limits.working_memory_bytes,
                    ),
                }
            })?;
            symbolic_bytes.extend_from_slice(bytes);
            Ok(())
        },
    )?;
    if !symbolic_result.status.success() {
        return Ok(false);
    }
    let symbolic_text =
        std::str::from_utf8(&symbolic_bytes).map_err(|_| invalid_utf8("HEAD", 0))?;
    let Some(reference) = symbolic_text.strip_suffix('\n') else {
        return Ok(false);
    };
    let Some(branch) = reference.strip_prefix("refs/heads/") else {
        return Ok(false);
    };
    if branch.is_empty() {
        return Ok(false);
    }

    // Let Git validate the complete branch grammar instead of maintaining a
    // partial copy of refname rules in this adapter.
    let mut check_ref = new_git_command(&repository.path, git_options);
    check_ref
        .arg("check-ref-format")
        .arg("--branch")
        .arg(branch);
    let check_result = supervise_process(
        check_ref,
        ingest_options,
        deadline,
        probe_limit,
        stderr_limit,
        |_bytes, _offset| Ok(()),
    )?;
    if !check_result.status.success() {
        return Ok(false);
    }

    let mut show_ref = new_git_command(&repository.path, git_options);
    show_ref
        .arg("show-ref")
        .arg("--verify")
        .arg("--quiet")
        .arg("--end-of-options")
        .arg(reference);
    let show_result = supervise_process(
        show_ref,
        ingest_options,
        deadline,
        probe_limit,
        stderr_limit,
        |_bytes, _offset| Ok(()),
    )?;
    if show_result.status.success() {
        return Ok(false);
    }
    if show_result.status.code() != Some(1) {
        return Err(GitError::Nonzero {
            status: show_result.status.to_string(),
            stderr: bounded_stderr_text(&show_result.stderr),
        });
    }

    // A valid symbolic HEAD whose validated branch ref is documented missing
    // is unborn. Dangling or staged objects do not change that conclusion.
    Ok(true)
}

fn new_git_command(repository: &Path, options: &GitOptions) -> Command {
    let mut command = Command::new(&options.executable);
    command
        .arg("--no-pager")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repository)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Do not inherit any Git-controlled environment. PATH is retained only
    // for resolving a bare executable name; all Git config, pager, transport,
    // replacement-object, and tracing variables are reset explicitly.
    let path_env = std::env::var_os("PATH");
    command.env_clear();
    if let Some(path_env) = path_env {
        command.env("PATH", path_env);
    }
    command
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device_path())
        .env("GIT_CONFIG_SYSTEM", null_device_path());
    command
}

#[cfg(unix)]
fn null_device_path() -> &'static str {
    "/dev/null"
}

#[cfg(windows)]
fn null_device_path() -> &'static str {
    "NUL"
}

#[cfg(not(any(unix, windows)))]
fn null_device_path() -> &'static str {
    ""
}

fn bounded_stderr_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProtocolPhase {
    Marker,
    Timestamp,
    Author,
    HeaderLf,
    PostHeader,
    Path,
    PostRecord,
    ExpectMarker,
}

struct GitProtocolParser<'a> {
    options: &'a IngestOptions,
    limit: u64,
    phase: ProtocolPhase,
    timestamp: i64,
    username: String,
    metadata: Vec<u8>,
    pending: Vec<u8>,
    records: Vec<NormalizedRecord>,
    records_bytes: u64,
    sequence: u64,
}

impl<'a> GitProtocolParser<'a> {
    fn new(options: &'a IngestOptions, limit: u64) -> Self {
        Self {
            options,
            limit,
            phase: ProtocolPhase::Marker,
            timestamp: 0,
            username: String::new(),
            metadata: Vec::new(),
            pending: Vec::new(),
            records: Vec::new(),
            records_bytes: 0,
            sequence: 0,
        }
    }

    fn line(&self) -> u64 {
        self.sequence.saturating_add(1)
    }

    fn working_usage(&self) -> u64 {
        self.records_bytes
            .saturating_add(
                (self.records.capacity() as u64)
                    .saturating_mul(std::mem::size_of::<NormalizedRecord>() as u64),
            )
            .saturating_add(self.pending.capacity() as u64)
            .saturating_add(self.username.capacity() as u64)
            .saturating_add(self.metadata.capacity() as u64)
    }

    fn memory_error(&self, requested: u64, offset: u64) -> GitError {
        GitError::Ingest(crate::working_memory_error(
            requested,
            self.limit,
            self.line(),
            offset,
        ))
    }

    fn check_extra(&self, extra: u64, offset: u64) -> Result<(), GitError> {
        let requested = self.working_usage().saturating_add(extra);
        if requested > self.limit {
            Err(self.memory_error(requested, offset))
        } else {
            Ok(())
        }
    }

    fn phase_limit(&self) -> u64 {
        match self.phase {
            ProtocolPhase::Marker | ProtocolPhase::ExpectMarker => COMMIT_MARKER.len() as u64,
            ProtocolPhase::Timestamp => 32,
            ProtocolPhase::Author => self.options.limits.max_contributor_bytes,
            ProtocolPhase::HeaderLf => 0,
            ProtocolPhase::PostHeader | ProtocolPhase::PostRecord => {
                self.options.limits.max_record_bytes
            }
            ProtocolPhase::Path => self.options.limits.max_path_bytes,
        }
    }

    fn field_limit_error(&self, offset: u64) -> GitError {
        match self.phase {
            ProtocolPhase::Author => GitError::Ingest(IngestError::new(
                ErrorCode::ContributorTooLong,
                self.line(),
                offset,
                bounded_context(&self.pending),
                "contributor exceeds configured byte limit",
            )),
            ProtocolPhase::PostHeader | ProtocolPhase::PostRecord => {
                GitError::Ingest(IngestError::new(
                    ErrorCode::RecordTooLarge,
                    self.line(),
                    offset,
                    bounded_context(&self.pending),
                    "Git raw metadata exceeds configured record byte limit",
                ))
            }
            ProtocolPhase::Path => GitError::Ingest(IngestError::new(
                ErrorCode::PathTooLong,
                self.line(),
                offset,
                bounded_context(&self.pending),
                "path exceeds configured byte limit",
            )),
            _ => malformed(offset, "Git protocol field exceeds its fixed bound"),
        }
    }

    fn feed(&mut self, bytes: &[u8], base_offset: u64) -> Result<(), GitError> {
        for (index, &byte) in bytes.iter().enumerate() {
            let offset = base_offset.saturating_add(index as u64);
            if self.phase == ProtocolPhase::HeaderLf {
                match byte {
                    b'\n' => self.phase = ProtocolPhase::PostHeader,
                    0 => self.phase = ProtocolPhase::ExpectMarker,
                    _ => {
                        return Err(malformed(
                            offset,
                            "commit header is not terminated by LF or empty-commit separator",
                        ));
                    }
                }
                continue;
            }
            if byte != 0 {
                let next_length = self.pending.len().saturating_add(1) as u64;
                if next_length > self.phase_limit() {
                    return Err(self.field_limit_error(offset));
                }
                if self.pending.len() == self.pending.capacity() {
                    self.check_extra(1, offset)?;
                    self.pending.try_reserve_exact(1).map_err(|_| {
                        self.memory_error(self.working_usage().saturating_add(1), offset)
                    })?;
                    let requested = self.working_usage();
                    if requested > self.limit {
                        return Err(self.memory_error(requested, offset));
                    }
                }
                self.pending.push(byte);
                continue;
            }
            let token = std::mem::take(&mut self.pending);
            self.consume_token(token, offset)?;
        }
        Ok(())
    }

    fn consume_token(&mut self, token: Vec<u8>, offset: u64) -> Result<(), GitError> {
        match self.phase {
            ProtocolPhase::Marker | ProtocolPhase::ExpectMarker => {
                if token.as_slice() != COMMIT_MARKER {
                    return Err(malformed(offset, "expected GOURCE-COMMIT marker"));
                }
                self.username = String::new();
                self.metadata = Vec::new();
                self.phase = ProtocolPhase::Timestamp;
            }
            ProtocolPhase::Timestamp => {
                let text =
                    std::str::from_utf8(&token).map_err(|_| invalid_utf8("timestamp", offset))?;
                if text.is_empty() {
                    return Err(malformed(offset, "commit timestamp is empty"));
                }
                self.timestamp = text
                    .parse::<i64>()
                    .map_err(|_| malformed(offset, "commit timestamp is not a signed epoch"))?;
                self.phase = ProtocolPhase::Author;
            }
            ProtocolPhase::Author => {
                let text =
                    std::str::from_utf8(&token).map_err(|_| invalid_utf8("contributor", offset))?;
                if text.len() as u64 > self.options.limits.max_contributor_bytes {
                    return Err(GitError::Ingest(IngestError::new(
                        ErrorCode::ContributorTooLong,
                        self.line(),
                        offset,
                        bounded_context(&token),
                        "contributor exceeds configured byte limit",
                    )));
                }
                if token.is_empty() {
                    self.check_extra(7, offset)?;
                    self.username = "Unknown".to_owned();
                } else {
                    self.check_extra(token.capacity() as u64, offset)?;
                    self.username = String::from_utf8(token)
                        .map_err(|_| invalid_utf8("contributor", offset))?;
                }
                self.phase = ProtocolPhase::HeaderLf;
            }
            ProtocolPhase::PostHeader => {
                if token.first() != Some(&b':') {
                    return Err(malformed(
                        offset,
                        "expected raw metadata after commit header LF",
                    ));
                }
                self.metadata = token;
                self.phase = ProtocolPhase::Path;
            }
            ProtocolPhase::Path => {
                self.consume_path(token, offset)?;
                self.phase = ProtocolPhase::PostRecord;
            }
            ProtocolPhase::PostRecord => {
                if token.is_empty() {
                    self.phase = ProtocolPhase::ExpectMarker;
                } else if token.first() == Some(&b':') {
                    self.metadata = token;
                    self.phase = ProtocolPhase::Path;
                } else {
                    return Err(malformed(
                        offset,
                        "expected raw metadata or commit separator",
                    ));
                }
            }
            ProtocolPhase::HeaderLf => {
                return Err(malformed(offset, "unexpected NUL before commit header LF"));
            }
        }
        Ok(())
    }

    fn consume_path(&mut self, path_bytes: Vec<u8>, offset: u64) -> Result<(), GitError> {
        let metadata = std::mem::take(&mut self.metadata);
        let wire_bytes = metadata
            .len()
            .checked_add(1)
            .and_then(|value| value.checked_add(path_bytes.len()))
            .and_then(|value| value.checked_add(1))
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(u64::MAX);
        if wire_bytes > self.options.limits.max_record_bytes {
            return Err(GitError::Ingest(IngestError::new(
                ErrorCode::RecordTooLarge,
                self.line(),
                offset,
                bounded_context(&path_bytes),
                "Git raw record exceeds configured byte limit",
            )));
        }

        let action = parse_action(&metadata, offset)?;
        let path = std::str::from_utf8(&path_bytes).map_err(|_| invalid_utf8("path", offset))?;
        if path.is_empty() {
            return Err(GitError::Ingest(IngestError::new(
                ErrorCode::InvalidPath,
                self.line(),
                offset,
                "",
                "Git path cannot be empty",
            )));
        }
        if path.len() as u64 > self.options.limits.max_path_bytes {
            return Err(GitError::Ingest(IngestError::new(
                ErrorCode::PathTooLong,
                self.line(),
                offset,
                bounded_context(&path_bytes),
                "path exceeds configured byte limit",
            )));
        }
        if path
            .bytes()
            .any(|byte| matches!(byte, b'\0' | b'|' | b'\n' | b'\r'))
        {
            return Err(GitError::Ingest(IngestError::new(
                ErrorCode::InvalidPath,
                self.line(),
                offset,
                bounded_context(&path_bytes),
                "Git path contains a byte forbidden by the common index format",
            )));
        }
        if path.split('/').count() as u64 > self.options.limits.max_path_components {
            return Err(GitError::Ingest(IngestError::new(
                ErrorCode::PathTooDeep,
                self.line(),
                offset,
                bounded_context(&path_bytes),
                "path exceeds configured component limit",
            )));
        }
        if self.sequence >= self.options.limits.max_events {
            return Err(GitError::EventCountLimit {
                limit: self.options.limits.max_events,
            });
        }

        let path = String::from_utf8(path_bytes).map_err(|_| invalid_utf8("path", offset))?;
        let mut username = String::new();
        username
            .try_reserve_exact(self.username.len())
            .map_err(|_| self.memory_error(self.working_usage().saturating_add(1), offset))?;
        username.push_str(&self.username);
        let record_bytes = (std::mem::size_of::<NormalizedRecord>() as u64)
            .saturating_add(username.capacity() as u64)
            .saturating_add(path.capacity() as u64);
        let vector_growth = if self.records.len() == self.records.capacity() {
            std::mem::size_of::<NormalizedRecord>() as u64
        } else {
            0
        };
        let temporary = metadata.capacity() as u64;
        self.check_extra(
            temporary
                .saturating_add(record_bytes)
                .saturating_add(vector_growth),
            offset,
        )?;
        if self.records.len() == self.records.capacity() {
            self.records.try_reserve_exact(1).map_err(|_| {
                self.memory_error(
                    self.working_usage()
                        .saturating_add(temporary)
                        .saturating_add(record_bytes)
                        .saturating_add(vector_growth),
                    offset,
                )
            })?;
            let requested = self
                .working_usage()
                .saturating_add(temporary)
                .saturating_add(record_bytes);
            if requested > self.limit {
                return Err(self.memory_error(requested, offset));
            }
        }
        let record = NormalizedRecord {
            timestamp: self.timestamp,
            username,
            action,
            path,
            is_directory: false,
            colour: None,
            source_sequence: self.sequence,
            line: self.line(),
            byte_offset: offset,
        };
        self.records_bytes = self
            .records_bytes
            .saturating_add(record.username.capacity() as u64)
            .saturating_add(record.path.capacity() as u64);
        self.records.push(record);
        self.sequence = self.sequence.saturating_add(1);
        Ok(())
    }

    fn finish(mut self, input_bytes: u64, input_hash: [u8; 32]) -> Result<ParsedInput, GitError> {
        if !self.pending.is_empty() {
            return Err(malformed(
                input_bytes,
                "Git output ended in an unterminated NUL field",
            ));
        }
        match self.phase {
            ProtocolPhase::HeaderLf | ProtocolPhase::PostRecord => Ok(ParsedInput {
                records: std::mem::take(&mut self.records),
                input_bytes,
                input_hash,
                record_count: self.sequence,
                runs: None,
            }),
            ProtocolPhase::Marker => Err(malformed(
                input_bytes,
                "Git output ended before a commit marker",
            )),
            ProtocolPhase::Timestamp => Err(malformed(
                input_bytes,
                "Git output ended after a commit marker",
            )),
            ProtocolPhase::Author => Err(malformed(
                input_bytes,
                "Git output ended before a contributor field",
            )),
            ProtocolPhase::PostHeader => Err(malformed(
                input_bytes,
                "Git output ended before raw metadata",
            )),
            ProtocolPhase::Path => Err(malformed(
                input_bytes,
                "Git output ended before a raw path field",
            )),
            ProtocolPhase::ExpectMarker => Err(malformed(
                input_bytes,
                "Git output ended after a commit separator",
            )),
        }
    }
}

fn parse_action(metadata: &[u8], offset: u64) -> Result<ParsedAction, GitError> {
    let mut status = None;
    for field in metadata.split(|byte| byte.is_ascii_whitespace()) {
        if !field.is_empty() {
            status = Some(field);
        }
    }
    let status = status.ok_or_else(|| malformed(offset, "raw metadata has no status"))?;
    if status.len() != 1 {
        return Err(malformed(
            offset,
            "renamed or scored raw statuses are unsupported",
        ));
    }
    match status[0] {
        b'A' => Ok(ParsedAction::Add),
        b'M' => Ok(ParsedAction::Modify),
        b'D' => Ok(ParsedAction::Delete),
        _ => Err(malformed(
            offset,
            "raw status must be add, modify, or delete",
        )),
    }
}

fn malformed(offset: u64, message: impl Into<String>) -> GitError {
    GitError::MalformedOutput {
        offset,
        message: message.into(),
    }
}

fn invalid_utf8(stream: &str, offset: u64) -> GitError {
    GitError::InvalidUtf8 {
        stream: stream.to_owned(),
        offset,
    }
}

fn bounded_context(bytes: &[u8]) -> String {
    const MAX_CONTEXT: usize = 96;
    let mut output = String::new();
    for &byte in bytes.iter().take(MAX_CONTEXT) {
        match byte {
            b'\\' => output.push_str("\\\\"),
            b'\n' => output.push_str("\\n"),
            b'\r' => output.push_str("\\r"),
            b'\t' => output.push_str("\\t"),
            0x20..=0x7e => output.push(byte as char),
            _ => {
                let _ = write!(&mut output, "\\x{byte:02x}");
            }
        }
    }
    if bytes.len() > MAX_CONTEXT {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(repository: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git command")
    }

    fn repository() -> TempDir {
        let directory = tempfile::tempdir().expect("temporary repository");
        assert!(git(directory.path(), &["init", "--quiet"]).status.success());
        assert!(
            git(directory.path(), &["config", "user.name", "Test User"])
                .status
                .success()
        );
        assert!(
            git(
                directory.path(),
                &["config", "user.email", "test@example.invalid"]
            )
            .status
            .success()
        );
        directory
    }

    fn commit(repository: &Path, message: &str, timestamp: i64, allow_empty: bool) {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repository)
            .arg("commit")
            .arg("--quiet");
        if allow_empty {
            command.arg("--allow-empty");
        }
        command
            .arg("-m")
            .arg(message)
            .env("GIT_AUTHOR_DATE", format!("@{timestamp} +0000"))
            .env("GIT_COMMITTER_DATE", format!("@{timestamp} +0000"));
        assert!(command.status().expect("git commit").success());
    }

    #[test]
    fn ingests_reverse_chronology_and_spaces_without_record_injection() {
        let directory = repository();
        fs::write(directory.path().join("space name"), b"one").expect("file");
        assert!(
            git(directory.path(), &["add", "--", "space name"])
                .status
                .success()
        );
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(directory.path())
            .args(["commit", "--quiet", "-m", "one"])
            .env("GIT_AUTHOR_DATE", "@100 +0000")
            .env("GIT_COMMITTER_DATE", "@100 +0000");
        assert!(command.status().expect("git commit").success());

        fs::write(directory.path().join("second"), b"two").expect("file");
        assert!(
            git(directory.path(), &["add", "--", "second"])
                .status
                .success()
        );
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(directory.path())
            .args(["commit", "--quiet", "-m", "two"])
            .env("GIT_AUTHOR_DATE", "@200 +0000")
            .env("GIT_COMMITTER_DATE", "@200 +0000");
        assert!(command.status().expect("git commit").success());

        let history = ingest_git_repository(
            directory.path(),
            &IngestOptions::default(),
            &GitOptions::default(),
        )
        .expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(history.events()[0].key.timestamp, 100);
        assert_eq!(history.events()[1].key.timestamp, 200);
        assert!(
            history
                .catalog()
                .paths()
                .iter()
                .any(|path| path.canonical() == "space name")
        );
    }

    #[test]
    fn ignores_empty_commits_between_changed_commits() {
        let directory = repository();

        fs::write(directory.path().join("first"), b"one").expect("first file");
        assert!(
            git(directory.path(), &["add", "--", "first"])
                .status
                .success()
        );
        commit(directory.path(), "first", 100, false);

        commit(directory.path(), "empty one", 150, true);
        commit(directory.path(), "empty two", 175, true);

        fs::write(directory.path().join("second"), b"two").expect("second file");
        assert!(
            git(directory.path(), &["add", "--", "second"])
                .status
                .success()
        );
        commit(directory.path(), "second", 200, false);

        let history = ingest_git_repository(
            directory.path(),
            &IngestOptions::default(),
            &GitOptions::default(),
        )
        .expect("history with empty commits");
        assert_eq!(history.len(), 2);
        assert_eq!(history.events()[0].key.timestamp, 100);
        assert_eq!(history.events()[1].key.timestamp, 200);
        assert!(
            history
                .catalog()
                .paths()
                .iter()
                .any(|path| path.canonical() == "first")
        );
        assert!(
            history
                .catalog()
                .paths()
                .iter()
                .any(|path| path.canonical() == "second")
        );
    }

    #[test]
    fn treats_unborn_head_as_empty_with_staged_objects() {
        let directory = repository();
        fs::write(directory.path().join("staged"), b"content").expect("file");
        assert!(
            git(directory.path(), &["add", "--", "staged"])
                .status
                .success()
        );

        let history = ingest_git_repository(
            directory.path(),
            &IngestOptions::default(),
            &GitOptions::default(),
        )
        .expect("unborn history");
        assert_eq!(history.len(), 0);
    }

    #[cfg(unix)]
    fn supervise_shell(script: &str) -> Result<ProcessResult, GitError> {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        supervise_process(
            command,
            &IngestOptions::default(),
            Instant::now() + Duration::from_secs(2),
            1024,
            1024,
            |_bytes, _offset| Ok(()),
        )
    }

    #[cfg(unix)]
    #[test]
    fn fast_zero_output_process_is_successful() {
        let result = supervise_shell("exit 0").expect("empty process output");
        assert!(result.status.success());
        assert_eq!(result.stdout_bytes, 0);
        assert!(result.stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn stderr_only_process_drains_diagnostics_before_success() {
        let result = supervise_shell("printf diagnostic >&2").expect("stderr-only process");
        assert!(result.status.success());
        assert_eq!(result.stdout_bytes, 0);
        assert_eq!(result.stderr, b"diagnostic");
    }

    #[cfg(unix)]
    #[test]
    fn fast_nonzero_process_preserves_exit_status() {
        let result = supervise_shell("exit 23").expect("nonzero process");
        assert_eq!(result.status.code(), Some(23));
    }

    #[test]
    fn malformed_protocol_is_typed_and_does_not_publish() {
        let options = IngestOptions::default();
        let mut parser = GitProtocolParser::new(
            &options,
            parser::parser_working_memory_bytes(options.limits.working_memory_bytes),
        );
        let error = parser
            .feed(b"GOURCE-COMMIT\0not-a-timestamp\0user\0\n", 0)
            .expect_err("malformed timestamp");
        assert!(matches!(error, GitError::MalformedOutput { .. }));
    }

    #[test]
    fn malformed_header_terminator_is_typed() {
        let options = IngestOptions::default();
        let mut parser = GitProtocolParser::new(
            &options,
            parser::parser_working_memory_bytes(options.limits.working_memory_bytes),
        );
        let data = [b"GOURCE-COMMIT\0".as_slice(), b"100\0user\0x".as_slice()].concat();
        let error = parser
            .feed(&data, 0)
            .expect_err("malformed header terminator");
        assert!(matches!(error, GitError::MalformedOutput { .. }));
    }

    #[test]
    fn empty_commit_framing_survives_every_chunk_boundary() {
        let options = IngestOptions::default();
        let data = [
            b"GOURCE-COMMIT\0".as_slice(),
            b"100\0first\0\n:000000 100644 0000000 1111111 A\0one\0\0".as_slice(),
            b"GOURCE-COMMIT\0".as_slice(),
            b"150\0middle\0\0".as_slice(),
            b"GOURCE-COMMIT\0".as_slice(),
            b"200\0second\0\n:100644 100644 1111111 2222222 M\0two\0\0".as_slice(),
            b"GOURCE-COMMIT\0".as_slice(),
            b"250\0final\0".as_slice(),
        ]
        .concat();
        let input_hash = *blake3::hash(&data).as_bytes();

        for split in 0..=data.len() {
            let mut parser = GitProtocolParser::new(
                &options,
                parser::parser_working_memory_bytes(options.limits.working_memory_bytes),
            );
            parser
                .feed(&data[..split], 0)
                .expect("first protocol chunk");
            parser
                .feed(&data[split..], split as u64)
                .expect("second protocol chunk");
            let parsed = parser
                .finish(data.len() as u64, input_hash)
                .expect("valid protocol at every chunk boundary");

            assert_eq!(parsed.input_bytes, data.len() as u64);
            assert_eq!(parsed.input_hash, input_hash);
            assert_eq!(parsed.records.len(), 2);
            assert_eq!(parsed.records[0].timestamp, 100);
            assert_eq!(parsed.records[0].username, "first");
            assert_eq!(parsed.records[0].path, "one");
            assert_eq!(parsed.records[1].timestamp, 200);
            assert_eq!(parsed.records[1].username, "second");
            assert_eq!(parsed.records[1].path, "two");
            assert_eq!(parsed.records[0].source_sequence, 0);
            assert_eq!(parsed.records[1].source_sequence, 1);
        }
    }

    #[test]
    fn raw_newline_path_is_rejected_without_becoming_records() {
        let options = IngestOptions::default();
        let mut parser = GitProtocolParser::new(
            &options,
            parser::parser_working_memory_bytes(options.limits.working_memory_bytes),
        );
        let data = [
            b"GOURCE-COMMIT\0".as_slice(),
            b"100\0user\0\n:000000 100644 0000000 1111111 A\0".as_slice(),
            b"safe\nname\0".as_slice(),
        ]
        .concat();
        let error = parser
            .feed(&data, 0)
            .expect_err("common path contract rejects newline");
        assert!(matches!(error, GitError::Ingest(_)));
    }

    #[cfg(unix)]
    #[test]
    fn fixture_timeout_is_killed_and_reaped() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("fixture directory");
        let fixture = directory.path().join("git-fixture");
        fs::write(&fixture, b"#!/bin/sh\nsleep 5\n").expect("fixture");
        fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700)).expect("permissions");
        let error = ingest_git_repository(
            directory.path(),
            &IngestOptions::default(),
            &GitOptions::default()
                .with_executable(&fixture)
                .with_timeout(Duration::from_millis(20)),
        )
        .expect_err("timeout");
        assert!(matches!(error, GitError::Timeout { .. }));
    }
}
