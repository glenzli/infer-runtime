use std::{collections::BTreeMap, os::fd::OwnedFd, sync::Mutex};

use std::{mem, os::fd::AsRawFd};

use thiserror::Error;
use uuid::Uuid;

const MAX_TTL_MS: u64 = 60_000;
const MAX_ID_LEN: usize = 128;

#[derive(Debug, Error)]
pub enum ArtifactLeaseError {
    #[error("artifact lease state is unavailable")]
    StateUnavailable,
    #[error("artifact lease identity is invalid")]
    InvalidIdentity,
    #[error("artifact lease TTL must be between 1 and 60000 milliseconds")]
    InvalidTtl,
    #[error("artifact lease daemon generation does not match")]
    GenerationMismatch,
    #[error("artifact lease registration ticket is unknown, expired, or already consumed")]
    InvalidTicket,
    #[error("artifact lease is unknown, expired, revoked, or already consumed")]
    InvalidLease,
    #[error("artifact lease does not belong to this App or Job")]
    ScopeMismatch,
    #[error("artifact lease execution was cancelled")]
    Cancelled,
    #[error("artifact lease file descriptor is invalid: {0}")]
    InvalidDescriptor(&'static str),
    #[error("artifact lease I/O failed")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LeaseRegistrationTicket {
    pub ticket_id: String,
    pub app_id: String,
    pub job_id: String,
    pub daemon_generation: String,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ArtifactLeaseIdentity {
    pub lease_id: String,
    pub app_id: String,
    pub job_id: String,
    pub daemon_generation: String,
    pub expires_at_unix_ms: u64,
    pub input_size_bytes: u64,
    pub input_device: u64,
    pub input_inode: u64,
    pub output_device: u64,
    pub output_inode: u64,
}

#[derive(Debug)]
pub struct ConsumedArtifactLease {
    pub identity: ArtifactLeaseIdentity,
    pub input: OwnedFd,
    pub output: OwnedFd,
}

#[derive(Debug)]
pub(crate) struct PendingDescriptors {
    pub input: OwnedFd,
    pub output: OwnedFd,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OpenObjectIdentity {
    pub input_size_bytes: u64,
    pub input_device: u64,
    pub input_inode: u64,
    pub output_device: u64,
    pub output_inode: u64,
}

#[derive(Debug)]
struct LeaseRecord {
    identity: ArtifactLeaseIdentity,
    descriptors: PendingDescriptors,
}

#[derive(Debug, Default)]
struct RegistryState {
    tickets: BTreeMap<String, LeaseRegistrationTicket>,
    leases: BTreeMap<String, LeaseRecord>,
}

#[derive(Debug)]
pub struct ArtifactLeaseRegistry {
    expected_uid: u32,
    daemon_generation: String,
    state: Mutex<RegistryState>,
}

impl ArtifactLeaseRegistry {
    pub fn new(
        expected_uid: u32,
        daemon_generation: impl Into<String>,
    ) -> Result<Self, ArtifactLeaseError> {
        let daemon_generation = daemon_generation.into();
        validate_id(&daemon_generation)?;
        Ok(Self {
            expected_uid,
            daemon_generation,
            state: Mutex::new(RegistryState::default()),
        })
    }

    pub const fn expected_uid(&self) -> u32 {
        self.expected_uid
    }

    pub fn daemon_generation(&self) -> &str {
        &self.daemon_generation
    }

    pub fn issue_ticket(
        &self,
        app_id: impl Into<String>,
        job_id: impl Into<String>,
        daemon_generation: &str,
        ttl_ms: u64,
        now_unix_ms: u64,
    ) -> Result<LeaseRegistrationTicket, ArtifactLeaseError> {
        if daemon_generation != self.daemon_generation {
            return Err(ArtifactLeaseError::GenerationMismatch);
        }
        if !(1..=MAX_TTL_MS).contains(&ttl_ms) {
            return Err(ArtifactLeaseError::InvalidTtl);
        }
        let app_id = app_id.into();
        let job_id = job_id.into();
        validate_id(&app_id)?;
        validate_id(&job_id)?;
        let ticket = LeaseRegistrationTicket {
            ticket_id: format!("ticket_{}", Uuid::new_v4().simple()),
            app_id,
            job_id,
            daemon_generation: self.daemon_generation.clone(),
            expires_at_unix_ms: now_unix_ms.saturating_add(ttl_ms),
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| ArtifactLeaseError::StateUnavailable)?;
        state
            .tickets
            .insert(ticket.ticket_id.clone(), ticket.clone());
        Ok(ticket)
    }

    pub(crate) fn register(
        &self,
        ticket_id: &str,
        descriptors: PendingDescriptors,
        objects: OpenObjectIdentity,
        now_unix_ms: u64,
    ) -> Result<ArtifactLeaseIdentity, ArtifactLeaseError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ArtifactLeaseError::StateUnavailable)?;
        let ticket = state
            .tickets
            .remove(ticket_id)
            .filter(|ticket| ticket.expires_at_unix_ms > now_unix_ms)
            .ok_or(ArtifactLeaseError::InvalidTicket)?;
        let identity = ArtifactLeaseIdentity {
            lease_id: format!("lease_{}", Uuid::new_v4().simple()),
            app_id: ticket.app_id,
            job_id: ticket.job_id,
            daemon_generation: ticket.daemon_generation,
            expires_at_unix_ms: ticket.expires_at_unix_ms,
            input_size_bytes: objects.input_size_bytes,
            input_device: objects.input_device,
            input_inode: objects.input_inode,
            output_device: objects.output_device,
            output_inode: objects.output_inode,
        };
        state.leases.insert(
            identity.lease_id.clone(),
            LeaseRecord {
                identity: identity.clone(),
                descriptors,
            },
        );
        Ok(identity)
    }

    pub fn consume(
        &self,
        lease_id: &str,
        app_id: &str,
        job_id: &str,
        daemon_generation: &str,
        now_unix_ms: u64,
    ) -> Result<ConsumedArtifactLease, ArtifactLeaseError> {
        if daemon_generation != self.daemon_generation {
            return Err(ArtifactLeaseError::GenerationMismatch);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| ArtifactLeaseError::StateUnavailable)?;
        let record = state
            .leases
            .get(lease_id)
            .ok_or(ArtifactLeaseError::InvalidLease)?;
        if record.identity.expires_at_unix_ms <= now_unix_ms {
            state.leases.remove(lease_id);
            return Err(ArtifactLeaseError::InvalidLease);
        }
        if record.identity.app_id != app_id || record.identity.job_id != job_id {
            return Err(ArtifactLeaseError::ScopeMismatch);
        }
        let record = state
            .leases
            .remove(lease_id)
            .expect("validated artifact lease remains present");
        revalidate_open_objects(&record, self.expected_uid)?;
        Ok(ConsumedArtifactLease {
            identity: record.identity,
            input: record.descriptors.input,
            output: record.descriptors.output,
        })
    }

    pub fn revoke(
        &self,
        lease_id: &str,
        app_id: &str,
        job_id: &str,
        daemon_generation: &str,
        now_unix_ms: u64,
    ) -> Result<bool, ArtifactLeaseError> {
        if daemon_generation != self.daemon_generation {
            return Err(ArtifactLeaseError::GenerationMismatch);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| ArtifactLeaseError::StateUnavailable)?;
        let Some(record) = state.leases.get(lease_id) else {
            return Ok(false);
        };
        if record.identity.expires_at_unix_ms <= now_unix_ms {
            state.leases.remove(lease_id);
            return Ok(false);
        }
        if record.identity.app_id != app_id || record.identity.job_id != job_id {
            return Err(ArtifactLeaseError::ScopeMismatch);
        }
        Ok(state.leases.remove(lease_id).is_some())
    }

    pub fn cancel(
        &self,
        lease_id: &str,
        app_id: &str,
        job_id: &str,
        daemon_generation: &str,
        now_unix_ms: u64,
    ) -> Result<bool, ArtifactLeaseError> {
        self.revoke(lease_id, app_id, job_id, daemon_generation, now_unix_ms)
    }

    pub fn expire(&self, now_unix_ms: u64) -> Result<(usize, usize), ArtifactLeaseError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ArtifactLeaseError::StateUnavailable)?;
        let tickets_before = state.tickets.len();
        state
            .tickets
            .retain(|_, ticket| ticket.expires_at_unix_ms > now_unix_ms);
        let leases_before = state.leases.len();
        state
            .leases
            .retain(|_, lease| lease.identity.expires_at_unix_ms > now_unix_ms);
        Ok((
            tickets_before - state.tickets.len(),
            leases_before - state.leases.len(),
        ))
    }

    /// Revoke every not-yet-consumed capability for one authenticated
    /// App/Job scope. This is used by typed Job cancellation; callers cannot
    /// broaden the scope with a ticket or lease id learned elsewhere.
    pub fn revoke_scope(
        &self,
        app_id: &str,
        job_id: &str,
        daemon_generation: &str,
    ) -> Result<(usize, usize), ArtifactLeaseError> {
        if daemon_generation != self.daemon_generation {
            return Err(ArtifactLeaseError::GenerationMismatch);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| ArtifactLeaseError::StateUnavailable)?;
        let tickets_before = state.tickets.len();
        state
            .tickets
            .retain(|_, ticket| ticket.app_id != app_id || ticket.job_id != job_id);
        let leases_before = state.leases.len();
        state
            .leases
            .retain(|_, lease| lease.identity.app_id != app_id || lease.identity.job_id != job_id);
        Ok((
            tickets_before - state.tickets.len(),
            leases_before - state.leases.len(),
        ))
    }

    pub fn active_counts(&self) -> Result<(usize, usize), ArtifactLeaseError> {
        let state = self
            .state
            .lock()
            .map_err(|_| ArtifactLeaseError::StateUnavailable)?;
        Ok((state.tickets.len(), state.leases.len()))
    }
}

fn validate_id(value: &str) -> Result<(), ArtifactLeaseError> {
    if value.is_empty()
        || value.len() > MAX_ID_LEN
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ArtifactLeaseError::InvalidIdentity);
    }
    Ok(())
}

fn revalidate_open_objects(
    record: &LeaseRecord,
    expected_uid: u32,
) -> Result<(), ArtifactLeaseError> {
    let input = descriptor_snapshot(&record.descriptors.input)?;
    let output = descriptor_snapshot(&record.descriptors.output)?;
    let input_flags = descriptor_flags(&record.descriptors.input)?;
    let output_flags = descriptor_flags(&record.descriptors.output)?;
    if input.st_mode & libc::S_IFMT != libc::S_IFREG
        || input.st_uid != expected_uid
        || input.st_mode & 0o077 != 0
        || input.st_nlink != 1
        || input_flags & libc::O_ACCMODE != libc::O_RDONLY
        || input.st_size < 0
        || input.st_size as u64 != record.identity.input_size_bytes
        || u64::try_from(input.st_dev).ok() != Some(record.identity.input_device)
        || input.st_ino != record.identity.input_inode
    {
        return Err(ArtifactLeaseError::InvalidDescriptor(
            "input identity changed before consumption",
        ));
    }
    if output.st_mode & libc::S_IFMT != libc::S_IFREG
        || output.st_uid != expected_uid
        || output.st_mode & 0o077 != 0
        || output.st_nlink != 1
        || output_flags & libc::O_ACCMODE != libc::O_WRONLY
        || output_flags & libc::O_APPEND != 0
        || output.st_size != 0
        || u64::try_from(output.st_dev).ok() != Some(record.identity.output_device)
        || output.st_ino != record.identity.output_inode
    {
        return Err(ArtifactLeaseError::InvalidDescriptor(
            "output identity changed before consumption",
        ));
    }
    Ok(())
}

fn descriptor_snapshot(fd: &OwnedFd) -> Result<libc::stat, ArtifactLeaseError> {
    let mut stat = unsafe { mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(stat)
}

fn descriptor_flags(fd: &OwnedFd) -> Result<i32, ArtifactLeaseError> {
    let result = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(result)
}
