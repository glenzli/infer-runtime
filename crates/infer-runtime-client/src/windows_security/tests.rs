use std::{
    io::Read,
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{null, null_mut},
};

use windows_sys::Win32::Security::{
    Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW},
    DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
};

use super::*;

fn descriptor(sddl: &str) -> SecurityDescriptor {
    let text: Vec<_> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut result = SecurityDescriptor(null_mut());
    // SAFETY: text is terminated and result has writable pointer storage.
    assert_ne!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut result.0,
                null_mut(),
            )
        },
        0
    );
    result
}

fn user_sid_string() -> String {
    let user = current_user().unwrap();
    let sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text = null_mut();
    assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut text) }, 0);
    let mut length = 0;
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    let value = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) }).unwrap();
    unsafe { LocalFree(text.cast()) };
    value
}

pub(crate) fn set_acl(path: &Path, additional_ace: &str) {
    let sid = user_sid_string();
    let descriptor = descriptor(&format!("O:{sid}D:P(A;;FA;;;{sid}){additional_ace}"));
    let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    assert_ne!(
        unsafe {
            SetFileSecurityW(
                path.as_ptr(),
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor.0,
            )
        },
        0
    );
}

#[test]
fn reads_current_user_file_and_accepts_system_maintenance_access() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("consumer.token");
    std::fs::write(&path, "synthetic-test-credential\n").unwrap();
    set_acl(&path, "(A;;FA;;;SY)");
    let mut file = open_owner_only_file(&path).unwrap();
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "synthetic-test-credential\n");
}

#[test]
fn refuses_access_granted_to_everyone() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("consumer.token");
    std::fs::write(&path, "synthetic-test-credential").unwrap();
    set_acl(&path, "(A;;GR;;;WD)");
    assert!(open_owner_only_file(&path).is_err());
}

#[test]
fn discovery_directory_requires_a_private_dacl() {
    let directory = tempfile::tempdir().unwrap();
    set_acl(directory.path(), "");
    validate_owner_only_directory(directory.path()).unwrap();
    set_acl(directory.path(), "(A;;GR;;;BU)");
    assert!(validate_owner_only_directory(directory.path()).is_err());
}

#[test]
fn refuses_foreign_owner_and_null_dacl() {
    let current_user = current_user().unwrap();
    let user = unsafe { (*current_user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    for sddl in ["O:SYD:P(A;;FA;;;SY)", "O:SYD:NO_ACCESS_CONTROL"] {
        let descriptor = descriptor(sddl);
        let mut owner = null_mut();
        let mut defaulted = 0;
        let mut present = 0;
        let mut dacl = null_mut();
        assert_ne!(
            unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted) },
            0
        );
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
            },
            0
        );
        assert!(unsafe { validate_owner_and_acl(owner, dacl, user) }.is_err());
    }
    assert!(unsafe { validate_owner_and_acl(user, null(), user) }.is_err());
}

#[test]
fn refuses_junction_in_parent_path() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("real");
    std::fs::create_dir(&target).unwrap();
    let path = target.join("consumer.token");
    std::fs::write(&path, "synthetic-test-credential").unwrap();
    set_acl(&path, "");
    let alias = directory.path().join("alias");
    let status = std::process::Command::new("cmd")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&alias)
        .arg(&target)
        .output()
        .unwrap();
    assert!(status.status.success(), "junction fixture creation failed");
    assert!(open_owner_only_file(&alias.join("consumer.token")).is_err());
    std::fs::remove_dir(alias).unwrap();
}

#[test]
fn refuses_network_and_alternate_stream_paths_before_opening() {
    assert!(open_owner_only_file(Path::new(r"\\server\share\consumer.token")).is_err());
    assert!(open_owner_only_file(Path::new(r"C:\consumer.token:secret")).is_err());
}
