use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::registry::{OpenObjectIdentity, PendingDescriptors};
use crate::{ArtifactLeaseError, ArtifactLeaseIdentity, ArtifactLeaseRegistry};

const REQUEST_SCHEMA: &str = "infer.artifact-lease.register";
const RESPONSE_SCHEMA: &str = "infer.artifact-lease.registered";
const SCHEMA_VERSION: &str = "20260811.1";
const MAX_FRAME_BYTES: usize = 4096;
const FIXTURE_STRIPE_BYTES: usize = 64 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum UnixLeaseProtocolError {
    #[error(transparent)]
    Lease(#[from] ArtifactLeaseError),
    #[error("artifact lease Unix protocol I/O failed")]
    Io(#[from] std::io::Error),
    #[error("artifact lease Unix request is malformed")]
    MalformedRequest,
    #[error("artifact lease Unix response is malformed")]
    MalformedResponse,
    #[error("artifact lease Unix peer does not match the daemon owner")]
    PeerMismatch,
    #[error("artifact lease Unix socket parent must be an owner-only directory")]
    UnsafeSocketParent,
    #[error("artifact lease Unix socket path is already occupied")]
    SocketOccupied,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterRequest {
    schema: String,
    schema_version: String,
    operation: String,
    ticket_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterResponse {
    schema: String,
    schema_version: String,
    lease_id: String,
    expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FixtureCopyReceipt {
    pub bytes_written: u64,
    pub sha256: String,
    pub maximum_explicit_buffer_bytes: usize,
    pub explicit_full_payload_buffers: usize,
}

/// Owner of one private Unix socket. It deliberately accepts only lease
/// registration; Job submission remains on the authenticated HTTP control
/// plane and is not duplicated here.
#[derive(Debug)]
pub struct ArtifactLeaseUnixServer {
    listener: UnixListener,
    socket_path: PathBuf,
    socket_device: u64,
    socket_inode: u64,
    registry: Arc<ArtifactLeaseRegistry>,
}

impl ArtifactLeaseUnixServer {
    pub fn bind(
        socket_path: impl Into<PathBuf>,
        registry: Arc<ArtifactLeaseRegistry>,
    ) -> Result<Self, UnixLeaseProtocolError> {
        let socket_path = socket_path.into();
        validate_socket_parent(&socket_path, registry.expected_uid())?;
        if fs::symlink_metadata(&socket_path).is_ok() {
            return Err(UnixLeaseProtocolError::SocketOccupied);
        }
        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        let metadata = fs::symlink_metadata(&socket_path)?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != registry.expected_uid()
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            let _ = fs::remove_file(&socket_path);
            return Err(UnixLeaseProtocolError::SocketOccupied);
        }
        Ok(Self {
            listener,
            socket_path,
            socket_device: metadata.dev(),
            socket_inode: metadata.ino(),
            registry,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<(), UnixLeaseProtocolError> {
        self.listener.set_nonblocking(nonblocking)?;
        Ok(())
    }

    pub fn accept_once(
        &self,
        now_unix_ms: u64,
    ) -> Result<ArtifactLeaseIdentity, UnixLeaseProtocolError> {
        let (mut stream, _) = self.listener.accept()?;
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let identity = receive_and_register(&stream, &self.registry, now_unix_ms)?;
        let response = RegisterResponse {
            schema: RESPONSE_SCHEMA.into(),
            schema_version: SCHEMA_VERSION.into(),
            lease_id: identity.lease_id.clone(),
            expires_at_unix_ms: identity.expires_at_unix_ms,
        };
        write_json_line(&mut stream, &response)?;
        Ok(identity)
    }
}

/// Process-lifetime owner for the lease registration socket. The blocking
/// descriptor handshake stays off Tokio workers and the socket is removed by
/// the server's inode-checked Drop implementation.
pub struct ArtifactLeaseUnixService {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ArtifactLeaseUnixService {
    pub fn start(
        socket_path: impl Into<PathBuf>,
        registry: Arc<ArtifactLeaseRegistry>,
    ) -> Result<Self, UnixLeaseProtocolError> {
        let server = ArtifactLeaseUnixServer::bind(socket_path, registry)?;
        server.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("infer-artifact-lease".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match server.accept_once(unix_time_ms()) {
                        Ok(_) => {}
                        Err(UnixLeaseProtocolError::Io(error))
                            if error.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => {}
                    }
                }
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ArtifactLeaseUnixService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

impl Drop for ArtifactLeaseUnixServer {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.socket_path) else {
            return;
        };
        if metadata.dev() == self.socket_device && metadata.ino() == self.socket_inode {
            let _ = fs::remove_file(&self.socket_path);
        }
    }
}

/// Send one ticket and exactly two already-open descriptors. The first must be
/// a read-only input; the second must be a new, empty, writable output.
pub fn register_unix_handles(
    socket_path: &Path,
    ticket_id: &str,
    input: RawFd,
    output: RawFd,
) -> Result<(String, u64), UnixLeaseProtocolError> {
    validate_client_socket(socket_path)?;
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let request = RegisterRequest {
        schema: REQUEST_SCHEMA.into(),
        schema_version: SCHEMA_VERSION.into(),
        operation: "register".into(),
        ticket_id: ticket_id.into(),
    };
    let mut frame =
        serde_json::to_vec(&request).map_err(|_| UnixLeaseProtocolError::MalformedRequest)?;
    frame.push(b'\n');
    if frame.len() > MAX_FRAME_BYTES {
        return Err(UnixLeaseProtocolError::MalformedRequest);
    }
    send_frame_with_fds(&stream, &frame, [input, output])?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let response: RegisterResponse = read_json_line(&mut stream, MAX_FRAME_BYTES)
        .map_err(|_| UnixLeaseProtocolError::MalformedResponse)?;
    if response.schema != RESPONSE_SCHEMA || response.schema_version != SCHEMA_VERSION {
        return Err(UnixLeaseProtocolError::MalformedResponse);
    }
    Ok((response.lease_id, response.expires_at_unix_ms))
}

/// Deterministic Phase-1 acceptance fixture. It streams through one bounded
/// stripe buffer and reports the allocation ceiling explicitly; it never
/// materializes the complete input or output in memory.
pub fn copy_striped_and_hash(
    lease: crate::ConsumedArtifactLease,
) -> Result<FixtureCopyReceipt, ArtifactLeaseError> {
    copy_striped_and_hash_with_cancel(lease, || false)
}

pub fn copy_striped_and_hash_with_cancel(
    lease: crate::ConsumedArtifactLease,
    mut cancelled: impl FnMut() -> bool,
) -> Result<FixtureCopyReceipt, ArtifactLeaseError> {
    let expected_input_size = lease.identity.input_size_bytes;
    let mut input = File::from(lease.input);
    let mut output = File::from(lease.output);
    input.seek(SeekFrom::Start(0))?;
    output.seek(SeekFrom::Start(0))?;
    let mut buffer = vec![0_u8; FIXTURE_STRIPE_BYTES];
    let mut hasher = Sha256::new();
    let mut bytes_written = 0_u64;
    loop {
        if cancelled() {
            return Err(ArtifactLeaseError::Cancelled);
        }
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        bytes_written = bytes_written.saturating_add(read as u64);
    }
    output.sync_all()?;
    if bytes_written != expected_input_size {
        return Err(ArtifactLeaseError::InvalidDescriptor(
            "input size changed while reading",
        ));
    }
    Ok(FixtureCopyReceipt {
        bytes_written,
        sha256: format!("{:x}", hasher.finalize()),
        maximum_explicit_buffer_bytes: buffer.len(),
        explicit_full_payload_buffers: 0,
    })
}

fn receive_and_register(
    stream: &UnixStream,
    registry: &ArtifactLeaseRegistry,
    now_unix_ms: u64,
) -> Result<ArtifactLeaseIdentity, UnixLeaseProtocolError> {
    if peer_effective_uid(stream)? != registry.expected_uid() {
        return Err(UnixLeaseProtocolError::PeerMismatch);
    }
    let (frame, [input, output]) = receive_frame_with_fds(stream)?;
    let request: RegisterRequest = serde_json::from_slice(
        frame
            .strip_suffix(b"\n")
            .ok_or(UnixLeaseProtocolError::MalformedRequest)?,
    )
    .map_err(|_| UnixLeaseProtocolError::MalformedRequest)?;
    if request.schema != REQUEST_SCHEMA
        || request.schema_version != SCHEMA_VERSION
        || request.operation != "register"
    {
        return Err(UnixLeaseProtocolError::MalformedRequest);
    }
    let input_stat = validate_input_fd(&input, registry.expected_uid())?;
    let output_stat = validate_output_fd(&output, registry.expected_uid())?;
    if input_stat.st_dev == output_stat.st_dev && input_stat.st_ino == output_stat.st_ino {
        return Err(ArtifactLeaseError::InvalidDescriptor(
            "input and output refer to the same file",
        )
        .into());
    }
    registry
        .register(
            &request.ticket_id,
            PendingDescriptors { input, output },
            OpenObjectIdentity {
                input_size_bytes: input_stat
                    .st_size
                    .try_into()
                    .map_err(|_| ArtifactLeaseError::InvalidDescriptor("input size is negative"))?,
                input_device: input_stat.st_dev.try_into().map_err(|_| {
                    ArtifactLeaseError::InvalidDescriptor("input device identity is invalid")
                })?,
                input_inode: input_stat.st_ino,
                output_device: output_stat.st_dev.try_into().map_err(|_| {
                    ArtifactLeaseError::InvalidDescriptor("output device identity is invalid")
                })?,
                output_inode: output_stat.st_ino,
            },
            now_unix_ms,
        )
        .map_err(Into::into)
}

fn validate_input_fd(fd: &OwnedFd, expected_uid: u32) -> Result<libc::stat, ArtifactLeaseError> {
    let flags = descriptor_flags(fd)?;
    if flags & libc::O_ACCMODE != libc::O_RDONLY {
        return Err(ArtifactLeaseError::InvalidDescriptor(
            "input is not read-only",
        ));
    }
    let stat = descriptor_stat(fd)?;
    validate_regular_owner_link(&stat, expected_uid, "input")?;
    set_close_on_exec(fd)?;
    Ok(stat)
}

fn validate_output_fd(fd: &OwnedFd, expected_uid: u32) -> Result<libc::stat, ArtifactLeaseError> {
    let flags = descriptor_flags(fd)?;
    if flags & libc::O_ACCMODE != libc::O_WRONLY || flags & libc::O_APPEND != 0 {
        return Err(ArtifactLeaseError::InvalidDescriptor(
            "output is not exclusive non-append writable",
        ));
    }
    let stat = descriptor_stat(fd)?;
    validate_regular_owner_link(&stat, expected_uid, "output")?;
    if stat.st_size != 0 {
        return Err(ArtifactLeaseError::InvalidDescriptor("output is not empty"));
    }
    // The lock is held by the received open-file description for the lease
    // lifetime. It protects against cooperating writers without pretending to
    // be a cross-process immutable-file primitive.
    let result = unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return Err(ArtifactLeaseError::InvalidDescriptor(
            "output cannot be exclusively locked",
        ));
    }
    set_close_on_exec(fd)?;
    Ok(stat)
}

fn validate_regular_owner_link(
    stat: &libc::stat,
    expected_uid: u32,
    label: &'static str,
) -> Result<(), ArtifactLeaseError> {
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(ArtifactLeaseError::InvalidDescriptor(match label {
            "input" => "input is not a regular file",
            _ => "output is not a regular file",
        }));
    }
    if stat.st_uid != expected_uid {
        return Err(ArtifactLeaseError::InvalidDescriptor(match label {
            "input" => "input owner does not match",
            _ => "output owner does not match",
        }));
    }
    if stat.st_mode & 0o077 != 0 {
        return Err(ArtifactLeaseError::InvalidDescriptor(match label {
            "input" => "input permissions are not owner-only",
            _ => "output permissions are not owner-only",
        }));
    }
    if stat.st_nlink != 1 {
        return Err(ArtifactLeaseError::InvalidDescriptor(match label {
            "input" => "input has more than one hard link",
            _ => "output has more than one hard link",
        }));
    }
    Ok(())
}

fn descriptor_flags(fd: &OwnedFd) -> Result<i32, ArtifactLeaseError> {
    let result = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(result)
}

fn descriptor_stat(fd: &OwnedFd) -> Result<libc::stat, ArtifactLeaseError> {
    let mut stat = unsafe { mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(stat)
}

fn set_close_on_exec(fd: &OwnedFd) -> Result<(), ArtifactLeaseError> {
    let old = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    if old < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, old | libc::FD_CLOEXEC) } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn validate_socket_parent(path: &Path, expected_uid: u32) -> Result<(), UnixLeaseProtocolError> {
    let parent = path
        .parent()
        .ok_or(UnixLeaseProtocolError::UnsafeSocketParent)?;
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(UnixLeaseProtocolError::UnsafeSocketParent);
    }
    Ok(())
}

fn validate_client_socket(path: &Path) -> Result<(), UnixLeaseProtocolError> {
    let expected_uid = unsafe { libc::geteuid() };
    validate_socket_parent(path, expected_uid)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(UnixLeaseProtocolError::SocketOccupied);
    }
    Ok(())
}

fn write_json_line<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
) -> Result<(), UnixLeaseProtocolError> {
    let mut frame =
        serde_json::to_vec(value).map_err(|_| UnixLeaseProtocolError::MalformedResponse)?;
    frame.push(b'\n');
    if frame.len() > MAX_FRAME_BYTES {
        return Err(UnixLeaseProtocolError::MalformedResponse);
    }
    stream.write_all(&frame)?;
    Ok(())
}

fn read_json_line<T: for<'de> Deserialize<'de>>(
    stream: &mut UnixStream,
    max_bytes: usize,
) -> Result<T, UnixLeaseProtocolError> {
    let mut bytes = Vec::new();
    stream
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes
        || bytes.last() != Some(&b'\n')
        || bytes[..bytes.len() - 1].contains(&b'\n')
    {
        return Err(UnixLeaseProtocolError::MalformedResponse);
    }
    serde_json::from_slice(&bytes[..bytes.len() - 1])
        .map_err(|_| UnixLeaseProtocolError::MalformedResponse)
}

fn send_frame_with_fds(
    stream: &UnixStream,
    frame: &[u8],
    fds: [RawFd; 2],
) -> Result<(), std::io::Error> {
    let mut iov = libc::iovec {
        iov_base: frame.as_ptr().cast_mut().cast(),
        iov_len: frame.len(),
    };
    let control_len = unsafe { libc::CMSG_SPACE(mem::size_of_val(&fds) as u32) as usize };
    let mut control = vec![0_u8; control_len];
    let mut message = unsafe { mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(mem::size_of_val(&fds) as u32) as _;
        std::ptr::copy_nonoverlapping(
            fds.as_ptr().cast::<u8>(),
            libc::CMSG_DATA(header),
            mem::size_of_val(&fds),
        );
        let sent = libc::sendmsg(stream.as_raw_fd(), &message, 0);
        if sent < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if sent as usize != frame.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "partial artifact lease frame",
            ));
        }
    }
    Ok(())
}

fn receive_frame_with_fds(
    stream: &UnixStream,
) -> Result<(Vec<u8>, [OwnedFd; 2]), UnixLeaseProtocolError> {
    let mut frame = vec![0_u8; MAX_FRAME_BYTES + 1];
    let mut iov = libc::iovec {
        iov_base: frame.as_mut_ptr().cast(),
        iov_len: frame.len(),
    };
    let control_len =
        unsafe { libc::CMSG_SPACE((2 * mem::size_of::<libc::c_int>()) as u32) as usize };
    let mut control = vec![0_u8; control_len];
    let mut message = unsafe { mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;
    let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
    if received <= 0 {
        return Err(UnixLeaseProtocolError::MalformedRequest);
    }
    frame.truncate(received as usize);
    let mut tail = Vec::new();
    let tail_reader = stream;
    tail_reader
        .take((MAX_FRAME_BYTES + 1 - frame.len()) as u64)
        .read_to_end(&mut tail)?;
    frame.extend_from_slice(&tail);
    if frame.len() > MAX_FRAME_BYTES
        || frame.last() != Some(&b'\n')
        || frame[..frame.len() - 1].contains(&b'\n')
    {
        return Err(UnixLeaseProtocolError::MalformedRequest);
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(UnixLeaseProtocolError::MalformedRequest);
    }
    let mut received_fds = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data_len = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                if !data_len.is_multiple_of(mem::size_of::<libc::c_int>()) {
                    return Err(UnixLeaseProtocolError::MalformedRequest);
                }
                let count = data_len / mem::size_of::<libc::c_int>();
                let data = libc::CMSG_DATA(header).cast::<libc::c_int>();
                for index in 0..count {
                    received_fds.push(OwnedFd::from_raw_fd(*data.add(index)));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if received_fds.len() != 2 {
        return Err(UnixLeaseProtocolError::MalformedRequest);
    }
    let output = received_fds.pop().expect("two descriptors were validated");
    let input = received_fds.pop().expect("two descriptors were validated");
    Ok((frame, [input, output]))
}

#[cfg(target_os = "macos")]
fn peer_effective_uid(stream: &UnixStream) -> Result<u32, std::io::Error> {
    let mut uid = 0;
    let mut gid = 0;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(target_os = "linux")]
fn peer_effective_uid(stream: &UnixStream) -> Result<u32, std::io::Error> {
    let mut credentials = unsafe { mem::zeroed::<libc::ucred>() };
    let mut length = mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(credentials.uid)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_effective_uid(_stream: &UnixStream) -> Result<u32, std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "peer effective UID is not implemented on this Unix platform",
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        sync::Arc,
        thread,
    };

    use super::*;

    fn current_uid() -> u32 {
        unsafe { libc::geteuid() }
    }

    fn private_socket_dir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn files(root: &Path, input_len: usize) -> (File, File, PathBuf) {
        let input_path = root.join("input.bin");
        let output_path = root.join("output.bin");
        let mut writable_input = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&input_path)
            .unwrap();
        let pattern: Vec<u8> = (0..FIXTURE_STRIPE_BYTES)
            .map(|offset| (offset % 251) as u8)
            .collect();
        let mut remaining = input_len;
        while remaining > 0 {
            let write = remaining.min(pattern.len());
            writable_input.write_all(&pattern[..write]).unwrap();
            remaining -= write;
        }
        writable_input.sync_all().unwrap();
        drop(writable_input);
        let input = OpenOptions::new().read(true).open(input_path).unwrap();
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&output_path)
            .unwrap();
        (input, output, output_path)
    }

    #[test]
    fn owner_only_socket_registers_one_shot_scoped_handles_and_streams_bounded_fixture() {
        let root = private_socket_dir();
        let socket = root.path().join("lease.sock");
        let registry = Arc::new(ArtifactLeaseRegistry::new(current_uid(), "generation-a").unwrap());
        let ticket = registry
            .issue_ticket("shadow", "job-1", "generation-a", 10_000, 100)
            .unwrap();
        let server = ArtifactLeaseUnixServer::bind(&socket, Arc::clone(&registry)).unwrap();
        assert_eq!(
            fs::symlink_metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let worker = thread::spawn(move || server.accept_once(101).unwrap());
        let (input, output, output_path) = files(root.path(), 8 * 1024 * 1024 + 17);
        let (lease_id, expires_at_unix_ms) = register_unix_handles(
            &socket,
            &ticket.ticket_id,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
        .unwrap();
        drop(input);
        drop(output);
        let registered = worker.join().unwrap();
        assert_eq!(registered.lease_id, lease_id);
        assert_eq!(expires_at_unix_ms, 10_100);
        assert!(matches!(
            registry.consume(&lease_id, "shadow", "job-1", "generation-b", 102),
            Err(ArtifactLeaseError::GenerationMismatch)
        ));
        assert!(matches!(
            registry.consume(&lease_id, "other-app", "job-1", "generation-a", 102),
            Err(ArtifactLeaseError::ScopeMismatch)
        ));
        let lease = registry
            .consume(&lease_id, "shadow", "job-1", "generation-a", 102)
            .unwrap();
        let receipt = copy_striped_and_hash(lease).unwrap();
        assert_eq!(receipt.bytes_written, 8 * 1024 * 1024 + 17);
        assert_eq!(receipt.maximum_explicit_buffer_bytes, FIXTURE_STRIPE_BYTES);
        assert_eq!(receipt.explicit_full_payload_buffers, 0);
        assert_eq!(
            fs::metadata(output_path).unwrap().len(),
            receipt.bytes_written
        );
        assert!(matches!(
            registry.consume(&lease_id, "shadow", "job-1", "generation-a", 103),
            Err(ArtifactLeaseError::InvalidLease)
        ));
    }

    #[test]
    fn rejects_writable_input_nonempty_output_hardlinks_and_same_file() {
        let root = private_socket_dir();
        let registry = ArtifactLeaseRegistry::new(current_uid(), "generation-a").unwrap();

        let writable_input_path = root.path().join("writable-input");
        let writable_input = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&writable_input_path)
            .unwrap();
        let output_path = root.path().join("output");
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&output_path)
            .unwrap();
        assert!(validate_input_fd(&writable_input.into(), current_uid()).is_err());

        let input_path = root.path().join("input");
        fs::write(&input_path, b"input").unwrap();
        fs::set_permissions(&input_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&input_path, root.path().join("input-link")).unwrap();
        let input = OpenOptions::new().read(true).open(&input_path).unwrap();
        assert!(validate_input_fd(&input.into(), current_uid()).is_err());

        let permissive_path = root.path().join("permissive-input");
        fs::write(&permissive_path, b"input").unwrap();
        fs::set_permissions(&permissive_path, fs::Permissions::from_mode(0o644)).unwrap();
        let permissive = OpenOptions::new()
            .read(true)
            .open(&permissive_path)
            .unwrap();
        assert!(validate_input_fd(&permissive.into(), current_uid()).is_err());

        let mut nonempty_output = output;
        nonempty_output.write_all(b"x").unwrap();
        assert!(validate_output_fd(&nonempty_output.into(), current_uid()).is_err());

        let same_path = root.path().join("same");
        fs::write(&same_path, b"same").unwrap();
        let same_input = OpenOptions::new().read(true).open(&same_path).unwrap();
        let same_output = OpenOptions::new().write(true).open(&same_path).unwrap();
        let input_stat = descriptor_stat(&same_input.into()).unwrap();
        let output_stat = descriptor_stat(&same_output.into()).unwrap();
        assert_eq!(
            (input_stat.st_dev, input_stat.st_ino),
            (output_stat.st_dev, output_stat.st_ino)
        );

        assert_eq!(registry.active_counts().unwrap(), (0, 0));
    }

    #[test]
    fn ticket_generation_expiry_revoke_and_registry_drop_close_every_handle() {
        let root = private_socket_dir();
        let registry = Arc::new(ArtifactLeaseRegistry::new(current_uid(), "generation-a").unwrap());
        assert!(matches!(
            registry.issue_ticket("shadow", "job", "generation-b", 100, 0),
            Err(ArtifactLeaseError::GenerationMismatch)
        ));
        registry
            .issue_ticket("shadow", "expired-ticket", "generation-a", 10, 0)
            .unwrap();
        assert_eq!(registry.expire(11).unwrap(), (1, 0));

        let ticket = registry
            .issue_ticket("shadow", "job", "generation-a", 10, 0)
            .unwrap();
        let socket = root.path().join("lease.sock");
        let server = ArtifactLeaseUnixServer::bind(&socket, Arc::clone(&registry)).unwrap();
        let worker = thread::spawn(move || server.accept_once(1).unwrap());
        let (input, output, _) = files(root.path(), 1024);
        let (lease_id, _) = register_unix_handles(
            &socket,
            &ticket.ticket_id,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
        .unwrap();
        worker.join().unwrap();
        assert_eq!(registry.expire(11).unwrap(), (0, 1));
        assert!(
            !registry
                .revoke(&lease_id, "shadow", "job", "generation-a", 11)
                .unwrap()
        );

        let ticket = registry
            .issue_ticket("shadow", "job-2", "generation-a", 100, 20)
            .unwrap();
        let socket = root.path().join("lease-2.sock");
        let server = ArtifactLeaseUnixServer::bind(&socket, Arc::clone(&registry)).unwrap();
        let worker = thread::spawn(move || server.accept_once(21).unwrap());
        let input = OpenOptions::new()
            .read(true)
            .open(root.path().join("input.bin"))
            .unwrap();
        let output_path = root.path().join("output-2.bin");
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&output_path)
            .unwrap();
        let (lease_id, _) = register_unix_handles(
            &socket,
            &ticket.ticket_id,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
        .unwrap();
        worker.join().unwrap();
        drop(input);
        drop(output);
        assert!(
            registry
                .cancel(&lease_id, "shadow", "job-2", "generation-a", 22)
                .unwrap()
        );
        assert_eq!(registry.active_counts().unwrap(), (0, 0));
        let lock_probe = OpenOptions::new().write(true).open(&output_path).unwrap();
        assert_eq!(
            unsafe { libc::flock(lock_probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        drop(registry);
        fs::remove_file(output_path).unwrap();
    }

    #[test]
    fn registry_drop_releases_a_live_output_lock_like_daemon_crash_cleanup() {
        let root = private_socket_dir();
        let registry =
            Arc::new(ArtifactLeaseRegistry::new(current_uid(), "generation-crash").unwrap());
        let ticket = registry
            .issue_ticket("shadow", "job-crash", "generation-crash", 1_000, 0)
            .unwrap();
        let socket = root.path().join("crash.sock");
        let server = ArtifactLeaseUnixServer::bind(&socket, Arc::clone(&registry)).unwrap();
        let worker = thread::spawn(move || server.accept_once(1).unwrap());
        let (input, output, output_path) = files(root.path(), 2048);
        let (_lease_id, _) = register_unix_handles(
            &socket,
            &ticket.ticket_id,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
        .unwrap();
        worker.join().unwrap();
        drop(input);
        drop(output);
        let lock_probe = OpenOptions::new().write(true).open(&output_path).unwrap();
        assert_ne!(
            unsafe { libc::flock(lock_probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        drop(registry);
        assert_eq!(
            unsafe { libc::flock(lock_probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }

    #[test]
    fn strict_wire_rejects_unknown_fields_and_same_file_descriptors() {
        let registry = Arc::new(ArtifactLeaseRegistry::new(current_uid(), "generation-a").unwrap());
        let ticket = registry
            .issue_ticket("shadow", "job", "generation-a", 100, 0)
            .unwrap();
        let root = private_socket_dir();
        let (input, output, _) = files(root.path(), 32);
        let (client, server) = UnixStream::pair().unwrap();
        let registry_for_worker = Arc::clone(&registry);
        let worker = thread::spawn(move || receive_and_register(&server, &registry_for_worker, 1));
        let frame = format!(
            "{{\"schema\":\"{REQUEST_SCHEMA}\",\"schema_version\":\"{SCHEMA_VERSION}\",\"operation\":\"register\",\"ticket_id\":\"{}\",\"path\":\"forbidden\"}}\n",
            ticket.ticket_id
        );
        send_frame_with_fds(
            &client,
            frame.as_bytes(),
            [input.as_raw_fd(), output.as_raw_fd()],
        )
        .unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Err(UnixLeaseProtocolError::MalformedRequest)
        ));
        assert_eq!(registry.active_counts().unwrap(), (1, 0));

        let ticket = registry
            .issue_ticket("shadow", "same-file", "generation-a", 100, 2)
            .unwrap();
        let same_path = root.path().join("empty-same-file");
        let initial = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&same_path)
            .unwrap();
        drop(initial);
        let same_input = OpenOptions::new().read(true).open(&same_path).unwrap();
        let same_output = OpenOptions::new().write(true).open(&same_path).unwrap();
        let (client, server) = UnixStream::pair().unwrap();
        let registry_for_worker = Arc::clone(&registry);
        let worker = thread::spawn(move || receive_and_register(&server, &registry_for_worker, 3));
        let request = RegisterRequest {
            schema: REQUEST_SCHEMA.into(),
            schema_version: SCHEMA_VERSION.into(),
            operation: "register".into(),
            ticket_id: ticket.ticket_id,
        };
        let mut frame = serde_json::to_vec(&request).unwrap();
        frame.push(b'\n');
        send_frame_with_fds(
            &client,
            &frame,
            [same_input.as_raw_fd(), same_output.as_raw_fd()],
        )
        .unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Err(UnixLeaseProtocolError::Lease(
                ArtifactLeaseError::InvalidDescriptor("input and output refer to the same file")
            ))
        ));
    }

    #[test]
    fn consume_revalidates_output_identity_after_registration() {
        let root = private_socket_dir();
        let socket = root.path().join("drift.sock");
        let registry =
            Arc::new(ArtifactLeaseRegistry::new(current_uid(), "generation-drift").unwrap());
        let ticket = registry
            .issue_ticket("shadow", "job-drift", "generation-drift", 1_000, 0)
            .unwrap();
        let server = ArtifactLeaseUnixServer::bind(&socket, Arc::clone(&registry)).unwrap();
        let worker = thread::spawn(move || server.accept_once(1).unwrap());
        let (input, mut output, _) = files(root.path(), 4096);
        let (lease_id, _) = register_unix_handles(
            &socket,
            &ticket.ticket_id,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
        .unwrap();
        worker.join().unwrap();
        output.write_all(b"drift").unwrap();
        assert!(matches!(
            registry.consume(&lease_id, "shadow", "job-drift", "generation-drift", 2),
            Err(ArtifactLeaseError::InvalidDescriptor(
                "output identity changed before consumption"
            ))
        ));
        assert_eq!(registry.active_counts().unwrap(), (0, 0));
    }

    #[test]
    fn running_fixture_cancellation_stops_at_a_stripe_boundary_and_closes_handles() {
        let root = private_socket_dir();
        let socket = root.path().join("cancel.sock");
        let registry =
            Arc::new(ArtifactLeaseRegistry::new(current_uid(), "generation-cancel").unwrap());
        let ticket = registry
            .issue_ticket("shadow", "job-cancel", "generation-cancel", 1_000, 0)
            .unwrap();
        let server = ArtifactLeaseUnixServer::bind(&socket, Arc::clone(&registry)).unwrap();
        let worker = thread::spawn(move || server.accept_once(1).unwrap());
        let (input, output, output_path) = files(root.path(), 4 * FIXTURE_STRIPE_BYTES);
        let (lease_id, _) = register_unix_handles(
            &socket,
            &ticket.ticket_id,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
        .unwrap();
        worker.join().unwrap();
        drop(input);
        drop(output);
        let lease = registry
            .consume(&lease_id, "shadow", "job-cancel", "generation-cancel", 2)
            .unwrap();
        let mut boundaries = 0;
        assert!(matches!(
            copy_striped_and_hash_with_cancel(lease, || {
                boundaries += 1;
                boundaries > 2
            }),
            Err(ArtifactLeaseError::Cancelled)
        ));
        assert_eq!(
            fs::metadata(&output_path).unwrap().len(),
            FIXTURE_STRIPE_BYTES as u64 * 2
        );
        let lock_probe = OpenOptions::new().write(true).open(output_path).unwrap();
        assert_eq!(
            unsafe { libc::flock(lock_probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }

    #[test]
    fn socket_parent_and_replacement_cleanup_are_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let registry = Arc::new(ArtifactLeaseRegistry::new(current_uid(), "generation-a").unwrap());
        assert!(matches!(
            ArtifactLeaseUnixServer::bind(root.path().join("lease.sock"), Arc::clone(&registry)),
            Err(UnixLeaseProtocolError::UnsafeSocketParent)
        ));

        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.path().join("lease.sock");
        let server = ArtifactLeaseUnixServer::bind(&socket, registry).unwrap();
        fs::remove_file(&socket).unwrap();
        fs::write(&socket, b"replacement").unwrap();
        drop(server);
        assert_eq!(fs::read(socket).unwrap(), b"replacement");
    }

    #[test]
    fn process_lifetime_service_cleans_up_its_exact_socket() {
        let root = private_socket_dir();
        let socket = root.path().join("service.sock");
        let registry =
            Arc::new(ArtifactLeaseRegistry::new(current_uid(), "generation-service").unwrap());
        let service = ArtifactLeaseUnixService::start(&socket, registry).unwrap();
        let metadata = fs::symlink_metadata(&socket).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        service.shutdown();
        assert!(!socket.exists());
    }

    #[test]
    fn wire_identity_is_frozen_and_windows_remains_a_contract_only() {
        assert_eq!(
            crate::ARTIFACT_LEASE_CONTRACT,
            "infer-runtime.artifact-lease@20260811.1"
        );
        assert_eq!(crate::UNIX_FD_BINDING, "uds-scm-rights");
        assert_eq!(
            crate::WINDOWS_HANDLE_BINDING,
            "owner-only-named-pipe-duplicated-handle"
        );
    }
}
