//! Handle-based Windows admission for local credentials and Discovery files.
//!
//! Inspect and read the same object, reject reparse points throughout the path,
//! and keep ancestor handles open without delete sharing until the read ends.

use std::{
    ffi::c_void,
    fs::File,
    io::{self, Read},
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr::{addr_of, null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation, INHERIT_ONLY_ACE,
        IsValidAcl, IsValidSid, IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK,
        FileAttributeTagInfo, GetDriveTypeW, GetFileInformationByHandleEx, GetFileType,
        OPEN_EXISTING, READ_CONTROL,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub(crate) struct OwnerOnlyFile {
    file: File,
    _ancestors: Vec<File>,
}

impl OwnerOnlyFile {
    pub(crate) fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.file.metadata()
    }
}

impl Read for OwnerOnlyFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

pub(crate) fn open_owner_only_file(path: &Path) -> io::Result<OwnerOnlyFile> {
    open_checked(path, false)
}

pub(crate) fn validate_owner_only_directory(path: &Path) -> io::Result<()> {
    open_checked(path, true).map(|_| ())
}

fn open_checked(path: &Path, directory: bool) -> io::Result<OwnerOnlyFile> {
    let path = local_path(path)?;
    let mut ancestors = Vec::new();
    let mut parent = PathBuf::new();
    let components: Vec<_> = path.components().collect();
    for component in &components[..components.len() - 1] {
        parent.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        ancestors.push(open_object(&parent, true, false)?);
    }
    let file = open_object(&path, directory, true)?;
    validate_security(&file)?;
    Ok(OwnerOnlyFile {
        file,
        _ancestors: ancestors,
    })
}

fn local_path(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let mut components = path.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return unsafe_path("only local drive paths are supported"),
        },
        _ => return unsafe_path("a local absolute path is required"),
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return unsafe_path("a drive-relative path is not supported");
    }
    for component in components {
        let Component::Normal(name) = component else {
            return unsafe_path("parent-directory aliases are not supported");
        };
        if name
            .encode_wide()
            .any(|unit| unit == 0 || unit == b':' as u16)
        {
            return unsafe_path("NUL bytes and alternate data streams are not supported");
        }
    }
    let root = [drive as u16, b':' as u16, b'\\' as u16, 0];
    // SAFETY: root is a terminated drive-root string. DRIVE_FIXED is 3.
    if unsafe { GetDriveTypeW(root.as_ptr()) } != 3 {
        return unsafe_path("credentials and Discovery require a local fixed drive");
    }
    if path.components().count() < 3 {
        return unsafe_path("a drive root is not a credential or Discovery directory");
    }
    Ok(path)
}

fn open_object(path: &Path, directory: bool, inspect_security: bool) -> io::Result<File> {
    let name: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let access = if inspect_security {
        READ_CONTROL
            | if directory {
                FILE_READ_ATTRIBUTES
            } else {
                GENERIC_READ
            }
    } else {
        FILE_READ_ATTRIBUTES
    };
    // Ancestors cannot be renamed while path resolution and reading are in progress.
    let sharing = FILE_SHARE_READ
        | if inspect_security && !directory {
            0
        } else {
            FILE_SHARE_WRITE
        };
    // SAFETY: name is terminated, security/template pointers are null, and the
    // returned handle is transferred exactly once into File below.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            sharing,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a fresh owned file handle.
    let file = unsafe { File::from_raw_handle(handle) };
    let mut attributes: FILE_ATTRIBUTE_TAG_INFO = unsafe { zeroed() };
    // SAFETY: the file handle is live and attributes has the required size.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&mut attributes as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the file owns this live handle.
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK
        || attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
    {
        return unsafe_path("reparse points and unexpected object types are not supported");
    }
    Ok(file)
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: GetSecurityInfo allocates this descriptor with LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

fn validate_security(file: &File) -> io::Result<()> {
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor = SecurityDescriptor(null_mut());
    // SAFETY: the handle is live and all output pointers refer to local storage.
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor.0,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let user = current_user()?;
    // SAFETY: current_user returns an aligned, complete TOKEN_USER buffer.
    let user_sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    // SAFETY: descriptor owns the returned owner/DACL for this whole function.
    unsafe { validate_owner_and_acl(owner, dacl, user_sid) }
}

unsafe fn validate_owner_and_acl(owner: PSID, dacl: *const ACL, user: PSID) -> io::Result<()> {
    // SAFETY: these pointers come from the live Windows security descriptor and token.
    if owner.is_null()
        || unsafe { IsValidSid(owner) } == 0
        || unsafe { EqualSid(owner, user) } == 0
        || dacl.is_null()
        || unsafe { IsValidAcl(dacl) } == 0
    {
        return unsafe_path("object must have the current user as owner and a non-null DACL");
    }
    // SAFETY: IsValidAcl verified the ACL header and its entries.
    for index in 0..unsafe { (*dacl).AceCount } {
        let mut ace: *mut c_void = null_mut();
        if unsafe { GetAce(dacl, index as u32, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceFlags as u32 & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        match header.AceType {
            0 => {
                // ACCESS_ALLOWED_ACE_TYPE
                if (header.AceSize as usize) < size_of::<ACCESS_ALLOWED_ACE>() {
                    return unsafe_path("invalid allow ACE");
                }
                let sid = unsafe { addr_of!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart) }
                    .cast_mut()
                    .cast();
                if unsafe { IsValidSid(sid) } == 0
                    || (unsafe { EqualSid(sid, user) } == 0
                        && unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } == 0)
                {
                    return unsafe_path("object grants access to another principal");
                }
            }
            1 => {} // A deny ACE cannot widen access.
            _ => return unsafe_path("unsupported discretionary ACE type"),
        }
    }
    Ok(())
}

fn current_user() -> io::Result<Vec<usize>> {
    let mut token = null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle; token is writable storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned a fresh owned token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut length = 0;
    // SAFETY: the null/zero call obtains the required buffer length.
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length) };
    if length < size_of::<TOKEN_USER>() as u32 || length > 64 * 1024 {
        return unsafe_path("invalid current-user token size");
    }
    let mut buffer = vec![0_usize; (length as usize).div_ceil(size_of::<usize>())];
    // SAFETY: buffer is aligned and at least length bytes long.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(buffer)
}

fn unsafe_path<T>(message: &str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::PermissionDenied, message))
}

#[cfg(test)]
mod tests;
