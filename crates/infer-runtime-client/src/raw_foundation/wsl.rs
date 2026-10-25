//! Explicit same-user Windows/WSL adaptation of the Unix lease binding.
//!
//! Only this owner streams samples and unpublished artifacts. It never resolves
//! a caller's file path, puts payloads on HTTP, or changes the lease identity.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Write},
    mem::{size_of, zeroed},
    os::windows::{fs::FileExt, io::AsRawHandle, process::CommandExt},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows_sys::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_TYPE_DISK, GetFileInformationByHandle, GetFileType,
    },
    System::{
        Pipes::PeekNamedPipe, SystemInformation::GetSystemDirectoryW, Threading::CREATE_NO_WINDOW,
        WindowsProgramming::PUBLIC_OBJECT_BASIC_INFORMATION,
    },
};

use super::{RawFoundationExecuteResponse, RawFoundationLeaseGrant};
use crate::{Error, Result, windows_security};

const SCHEMA: &str = "infer.raw-foundation.wsl-stdio@20261025.1";
const MAX_FRAME: usize = 4096;
const MAX_INPUT: u64 = 512 * 1024 * 1024;
const MAX_OUTPUT: u64 = 2 * 1024 * 1024 * 1024;
const STRIPE: usize = 64 * 1024;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryObject(
        handle: HANDLE,
        class: i32,
        information: *mut core::ffi::c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

/// Operator-installed bridge, explicitly tied to one WSL distribution. Paths
/// identify installed code only; source and artifact paths are never accepted.
#[derive(Debug, Clone)]
pub struct RawFoundationWslBridge {
    distribution: String,
    script: String,
}

impl RawFoundationWslBridge {
    pub(crate) fn from_discovery_endpoint(endpoint: &str) -> Result<Self> {
        let (distribution, path) = endpoint
            .strip_prefix("wsl-stdio://")
            .and_then(|value| value.split_once('/'))
            .ok_or(Error::ContractMismatch)?;
        // This dated binding admits literal installation paths. URI escapes,
        // queries and fragments require a different explicit binding version.
        if path.contains(['%', '?', '#']) {
            return Err(Error::ContractMismatch);
        }
        Self::new(distribution, format!("/{path}"))
    }
    pub fn new(
        distribution: impl Into<String>,
        installed_script: impl Into<String>,
    ) -> Result<Self> {
        let distribution = distribution.into();
        let script = installed_script.into();
        if distribution.is_empty()
            || distribution.len() > 128
            || distribution.starts_with('-')
            || !distribution
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            || !script.starts_with('/')
            || script.len() > 1024
            || script.chars().any(char::is_control)
        {
            return Err(failure("invalid WSL bridge installation identity"));
        }
        Ok(Self {
            distribution,
            script,
        })
    }
}

#[derive(Debug)]
pub(crate) struct WslRawFoundationState {
    binding: Option<RawFoundationWslBridge>,
    pending: Arc<Mutex<BTreeMap<String, Session>>>,
    expiration_started: AtomicBool,
}

impl WslRawFoundationState {
    pub(crate) fn new(binding: Option<RawFoundationWslBridge>) -> Self {
        let pending: Arc<Mutex<BTreeMap<String, Session>>> = Arc::new(Mutex::new(BTreeMap::new()));
        Self {
            binding,
            pending,
            expiration_started: AtomicBool::new(false),
        }
    }

    pub(crate) fn configured_binding(&self) -> Option<&RawFoundationWslBridge> {
        self.binding.as_ref()
    }

    fn start_expiration(&self) {
        if !self.expiration_started.swap(true, Ordering::AcqRel) {
            let weak = Arc::downgrade(&self.pending);
            std::thread::spawn(move || {
                loop {
                    {
                        let Some(pending) = weak.upgrade() else {
                            break;
                        };
                        if let Ok(mut pending) = pending.lock() {
                            pending.retain(|_, session| session.expires_at > now_ms());
                        }
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
        }
    }

    pub(crate) fn register(
        &self,
        binding: &RawFoundationWslBridge,
        grant: &RawFoundationLeaseGrant,
        input: &File,
        output: &File,
    ) -> Result<String> {
        self.start_expiration();
        if grant.binding.contract != infer_artifact_lease::ARTIFACT_LEASE_CONTRACT
            || grant.binding.transport != infer_artifact_lease::UNIX_FD_BINDING
            || !grant.binding.endpoint.starts_with('/')
            || grant.expires_at_unix_ms <= now_ms()
            || grant.expires_at_unix_ms > now_ms().saturating_add(60_000)
        {
            return Err(Error::ContractMismatch);
        }
        let input_identity = validate_file(input, true).map_err(io_error)?;
        let output_identity = validate_file(output, false).map_err(io_error)?;
        if input_identity == output_identity {
            return Err(failure("input and output are the same file object"));
        }
        let size = input.metadata().map_err(io_error)?.len();
        if size == 0 || size > MAX_INPUT {
            return Err(failure(
                "WSL RAW sample stream must be between 1 and 512 MiB",
            ));
        }
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| failure("RAW bridge state unavailable"))?;
        pending.retain(|_, session| session.expires_at > now_ms());
        if pending.len() >= 4 {
            return Err(failure("too many pending RAW bridge transfers"));
        }
        let output = output.try_clone().map_err(io_error)?;
        output
            .try_lock()
            .map_err(|_| failure("RAW output cannot be exclusively locked"))?;
        let output = OutputLease {
            file: output,
            complete: false,
        };
        let mut system = [0u16; 32768];
        // SAFETY: system is writable and its length is passed exactly.
        let length =
            unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) } as usize;
        if length == 0 || length >= system.len() {
            return Err(io_error(io::Error::last_os_error()));
        }
        let executable = PathBuf::from(String::from_utf16_lossy(&system[..length])).join("wsl.exe");
        let mut child = Command::new(executable)
            .args([
                "--distribution",
                &binding.distribution,
                "--cd",
                "/",
                "--exec",
                "/usr/bin/python3",
                "-I",
                &binding.script,
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(io_error)?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut session = Session {
            child,
            stdin: Some(stdin),
            stdout,
            output,
            job_id: grant.job_id.clone(),
            expires_at: grant.expires_at_unix_ms,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let _registration_deadline = ProcessDeadline::new(
            &session.child,
            Duration::from_millis(
                grant
                    .expires_at_unix_ms
                    .saturating_sub(now_ms())
                    .min(30_000),
            ),
        )?;
        session.write_frame(&Register {
            schema: SCHEMA,
            endpoint: &grant.binding.endpoint,
            ticket_id: &grant.ticket_id,
            sample_bytes: size,
            expires_at_unix_ms: grant.expires_at_unix_ms,
        })?;
        let mut offset = 0;
        let mut buffer = [0u8; STRIPE];
        while offset < size {
            let count = input
                .seek_read(
                    &mut buffer[..(size - offset).min(STRIPE as u64) as usize],
                    offset,
                )
                .map_err(io_error)?;
            if count == 0 {
                return Err(failure("incomplete RAW input"));
            }
            session
                .stdin
                .as_mut()
                .expect("open stdin")
                .write_all(&buffer[..count])
                .map_err(io_error)?;
            offset += count as u64;
        }
        session
            .stdin
            .as_mut()
            .expect("open stdin")
            .flush()
            .map_err(io_error)?;
        let deadline = Instant::now()
            + Duration::from_millis(
                grant
                    .expires_at_unix_ms
                    .saturating_sub(now_ms())
                    .min(30_000),
            );
        let response: Registered = session.read_frame(deadline)?;
        if response.schema != SCHEMA
            || response.expires_at_unix_ms != grant.expires_at_unix_ms
            || !response.lease_id.starts_with("lease_")
            || response.lease_id.len() > 128
        {
            return Err(Error::ContractMismatch);
        }
        let lease_id = response.lease_id;
        if pending.contains_key(&lease_id) {
            return Err(Error::ContractMismatch);
        }
        pending.insert(lease_id.clone(), session);
        Ok(lease_id)
    }

    pub(crate) fn begin(&self, job_id: &str, lease_id: &str) -> Result<Session> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| failure("RAW bridge state unavailable"))?;
        let session = pending
            .get(lease_id)
            .ok_or_else(|| failure("unknown or already consumed Windows RAW lease"))?;
        if session.job_id != job_id {
            return Err(failure("Windows RAW lease scope mismatch"));
        }
        let mut session = pending.remove(lease_id).expect("checked lease");
        if session.expires_at <= now_ms() {
            return Err(failure("Windows RAW lease expired"));
        }
        validate_file(&session.output.file, false).map_err(io_error)?;
        session.write_frame(&Action {
            schema: SCHEMA,
            operation: "execute",
        })?;
        Ok(session)
    }

    pub(crate) fn discard_job(&self, job_id: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.retain(|_, session| session.job_id != job_id);
        }
    }
}

#[derive(Debug)]
struct OutputLease {
    file: File,
    complete: bool,
}
impl Drop for OutputLease {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.file.set_len(0);
        }
        let _ = self.file.unlock();
    }
}

#[derive(Debug)]
pub(crate) struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    output: OutputLease,
    job_id: String,
    expires_at: u64,
    cancelled: Arc<AtomicBool>,
}

impl Session {
    fn write_frame(&mut self, value: &impl Serialize) -> Result<()> {
        let mut frame =
            serde_json::to_vec(value).map_err(|_| failure("invalid WSL bridge frame"))?;
        frame.push(b'\n');
        if frame.len() > MAX_FRAME {
            return Err(failure("WSL bridge frame too large"));
        }
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| failure("WSL bridge closed"))?;
        stdin
            .write_all(&frame)
            .and_then(|_| stdin.flush())
            .map_err(io_error)
    }

    fn read_some(&mut self, bytes: &mut [u8], deadline: Instant) -> Result<usize> {
        loop {
            if self.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(failure("WSL bridge cancelled or timed out"));
            }
            let mut available = 0;
            // SAFETY: stdout owns this live pipe; only the availability count is queried.
            let ok = unsafe {
                PeekNamedPipe(
                    self.stdout.as_raw_handle(),
                    core::ptr::null_mut(),
                    0,
                    core::ptr::null_mut(),
                    &mut available,
                    core::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(failure("WSL bridge closed without a complete result"));
            }
            if available > 0 {
                let length = bytes.len().min(available as usize);
                return self.stdout.read(&mut bytes[..length]).map_err(io_error);
            }
            if self.child.try_wait().map_err(io_error)?.is_some() {
                return Err(failure("WSL bridge exited without a complete result"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn read_frame<T: for<'de> Deserialize<'de>>(&mut self, deadline: Instant) -> Result<T> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            if self.read_some(&mut byte, deadline)? != 1 {
                return Err(failure("truncated WSL bridge frame"));
            }
            if byte[0] == b'\n' {
                break;
            }
            bytes.push(byte[0]);
            if bytes.len() >= MAX_FRAME {
                return Err(failure("WSL bridge frame too large"));
            }
        }
        serde_json::from_slice(&bytes).map_err(|_| failure("malformed WSL bridge response"))
    }

    pub(crate) async fn finish(self, response: &RawFoundationExecuteResponse) -> Result<()> {
        if response.id != self.job_id
            || response.status != "completed"
            || response.artifact.artifact_file_bytes == 0
            || response.artifact.artifact_file_bytes > MAX_OUTPUT
        {
            return Err(Error::ContractMismatch);
        }
        let size = response.artifact.artifact_file_bytes;
        let digest = response.artifact.artifact_file_sha256.clone();
        let mut cancellation = CancelOnDrop {
            cancelled: Arc::clone(&self.cancelled),
            armed: true,
        };
        let result = tokio::task::spawn_blocking(move || self.collect(size, &digest))
            .await
            .map_err(|_| failure("WSL bridge collection worker failed"))?;
        cancellation.armed = false;
        result
    }

    fn collect(mut self, size: u64, digest: &str) -> Result<()> {
        validate_file(&self.output.file, false).map_err(io_error)?;
        let _collection_deadline = ProcessDeadline::new(&self.child, Duration::from_secs(120))?;
        self.write_frame(&Collect {
            schema: SCHEMA,
            operation: "collect",
            artifact_file_bytes: size,
            artifact_file_sha256: digest,
        })?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let response: Collected = self.read_frame(deadline)?;
        if response.schema != SCHEMA || response.artifact_file_bytes != size {
            return Err(Error::ContractMismatch);
        }
        let mut buffer = [0u8; STRIPE];
        let mut actual = Sha256::new();
        let mut offset = 0;
        while offset < size {
            let count = self.read_some(
                &mut buffer[..(size - offset).min(STRIPE as u64) as usize],
                deadline,
            )?;
            if count == 0 {
                return Err(failure("truncated WSL artifact"));
            }
            actual.update(&buffer[..count]);
            let mut written = 0;
            while written < count {
                let count = self
                    .output
                    .file
                    .seek_write(&buffer[written..count], offset + written as u64)
                    .map_err(io_error)?;
                if count == 0 {
                    return Err(failure("RAW output write made no progress"));
                }
                written += count;
            }
            offset += count as u64;
        }
        if format!("{:x}", actual.finalize()) != digest {
            return Err(failure("RAW artifact digest mismatch"));
        }
        self.stdin.take();
        let exit_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait().map_err(io_error)? {
                if !status.success() {
                    return Err(failure("WSL bridge cleanup failed"));
                }
                break;
            }
            if self.cancelled.load(Ordering::Acquire) || Instant::now() >= exit_deadline {
                return Err(failure("WSL bridge cleanup timed out"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.output.file.sync_all().map_err(io_error)?;
        self.output.complete = true;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Bound even a stalled WSL startup/upload by holding this exact child process
/// object. No PID is looked up again and no unrelated process can be killed.
struct ProcessDeadline(std::sync::mpsc::Sender<()>);
impl ProcessDeadline {
    fn new(child: &Child, duration: Duration) -> Result<Self> {
        use std::os::windows::io::AsHandle;
        let process = child.as_handle().try_clone_to_owned().map_err(io_error)?;
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if matches!(
                receive.recv_timeout(duration),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                // SAFETY: this owned handle refers only to the bridge we spawned.
                unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(
                        process.as_raw_handle(),
                        2,
                    );
                }
            }
        });
        Ok(Self(send))
    }
}
impl Drop for ProcessDeadline {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

struct CancelOnDrop {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

fn validate_file(file: &File, input: bool) -> io::Result<(u32, u32, u32)> {
    windows_security::validate_security(file)?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    // SAFETY: the borrowed file remains open and info has the documented layout.
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK
        || unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || info.nNumberOfLinks != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "RAW handle must be a private regular file with one link",
        ));
    }
    let mut basic: PUBLIC_OBJECT_BASIC_INFORMATION = unsafe { zeroed() };
    // SAFETY: ObjectBasicInformation (0) fills the documented public structure.
    if unsafe {
        NtQueryObject(
            file.as_raw_handle(),
            0,
            (&mut basic as *mut PUBLIC_OBJECT_BASIC_INFORMATION).cast(),
            size_of::<PUBLIC_OBJECT_BASIC_INFORMATION>() as u32,
            core::ptr::null_mut(),
        )
    } < 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot inspect RAW handle access",
        ));
    }
    let access = basic.GrantedAccess;
    if (input && (access & 1 == 0 || access & 6 != 0))
        // GENERIC_WRITE includes FILE_APPEND_DATA on Windows, but explicit
        // offset writes remain supported when FILE_WRITE_DATA is present.
        // Append-only handles lack FILE_WRITE_DATA and are rejected here.
        || (!input && (access & 2 == 0 || access & 1 != 0 || file.metadata()?.len() != 0))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "RAW input must be read-only and output empty write-only non-append",
        ));
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |v| v.as_millis() as u64)
}
fn failure(message: &str) -> Error {
    Error::Input(format!("RAW WSL bridge: {message}"))
}
fn io_error(error: io::Error) -> Error {
    failure(&format!("I/O failure ({:?})", error.kind()))
}

#[derive(Serialize)]
struct Register<'a> {
    schema: &'a str,
    endpoint: &'a str,
    ticket_id: &'a str,
    sample_bytes: u64,
    expires_at_unix_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registered {
    schema: String,
    lease_id: String,
    expires_at_unix_ms: u64,
}
#[derive(Serialize)]
struct Action<'a> {
    schema: &'a str,
    operation: &'a str,
}
#[derive(Serialize)]
struct Collect<'a> {
    schema: &'a str,
    operation: &'a str,
    artifact_file_bytes: u64,
    artifact_file_sha256: &'a str,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Collected {
    schema: String,
    artifact_file_bytes: u64,
}

#[cfg(test)]
mod tests;
