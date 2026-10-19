use std::{
    ffi::{CStr, OsStr},
    mem::MaybeUninit,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr,
};

/// Whether this process runs as root (effective uid 0).
pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Home directory of the user with this uid, from the passwd database. Static
/// musl builds read only `/etc/passwd`, so users that exist only in LDAP or
/// similar are not found.
pub fn home_dir(uid: u32) -> Option<PathBuf> {
    let mut passwd = MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = vec![0 as libc::c_char; 4096];
    let mut result = ptr::null_mut();
    // SAFETY: every pointer is valid for the length given, and the call only
    // writes the passwd entry and the strings it points to into `passwd` and
    // `buffer`, both of which outlive their use below.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            passwd.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    // SAFETY: a null `result` was ruled out, so `passwd` is initialised, and
    // `pw_dir` points into `buffer` as a NUL-terminated string.
    let home = unsafe {
        let passwd = passwd.assume_init();
        if passwd.pw_dir.is_null() {
            return None;
        }
        CStr::from_ptr(passwd.pw_dir).to_bytes().to_vec()
    };
    Some(PathBuf::from(OsStr::from_bytes(&home)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_root_home() {
        assert!(home_dir(0).is_some());
    }
}
