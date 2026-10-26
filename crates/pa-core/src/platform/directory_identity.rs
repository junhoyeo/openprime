//! Stable filesystem identity for an already-opened generation directory.
//! Callers treat `None` as unverifiable and never delete on that basis.

use std::io;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DirectoryIdentity {
    volume: u64,
    file: u64,
}

/// Identity of one direct directory entry. Symlinks and non-directories are
/// errors; unsupported platforms return `Ok(None)` so cleanup fails closed.
#[cfg(unix)]
pub(crate) fn directory_identity(path: &Path) -> io::Result<Option<DirectoryIdentity>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a direct directory: {}", path.display()),
        ));
    }
    Ok(Some(DirectoryIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
    }))
}

/// Windows uses `CreateFileW(FILE_FLAG_BACKUP_SEMANTICS | OPEN_REPARSE_POINT)`
/// plus `GetFileInformationByHandle`: stable Win32 ABI, no unstable std API.
#[cfg(windows)]
pub(crate) fn directory_identity(path: &Path) -> io::Result<Option<DirectoryIdentity>> {
    win32::directory_identity(path)
}

#[cfg(windows)]
mod win32 {
    #![allow(non_snake_case)]

    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use super::DirectoryIdentity;

    const FILE_SHARE_ALL: u32 = 0x0000_0007;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const INVALID_HANDLE_VALUE: isize = -1;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        dwLowDateTime: u32,
        dwHighDateTime: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ByHandleFileInformation {
        dwFileAttributes: u32,
        ftCreationTime: FileTime,
        ftLastAccessTime: FileTime,
        ftLastWriteTime: FileTime,
        dwVolumeSerialNumber: u32,
        nFileSizeHigh: u32,
        nFileSizeLow: u32,
        nNumberOfLinks: u32,
        nFileIndexHigh: u32,
        nFileIndexLow: u32,
    }

    type Handle = *mut c_void;

    extern "system" {
        fn CreateFileW(
            filename: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: Handle,
        ) -> Handle;
        fn GetFileInformationByHandle(
            handle: Handle,
            information: *mut ByHandleFileInformation,
        ) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    pub(super) fn directory_identity(path: &Path) -> io::Result<Option<DirectoryIdentity>> {
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        // SAFETY: `wide` is NUL-terminated and lives through the call; no
        // security/template pointers are supplied.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_ALL,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle as isize == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut information = ByHandleFileInformation::default();
        // SAFETY: `handle` is valid and the OS initializes the fixed-size
        // information struct. The handle is closed on every result path.
        let status = unsafe { GetFileInformationByHandle(handle, &raw mut information) };
        let information_error = (status == 0).then(io::Error::last_os_error);
        // SAFETY: the handle came from CreateFileW and is closed exactly once.
        unsafe { CloseHandle(handle) };
        if let Some(error) = information_error {
            return Err(error);
        }
        if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a direct directory: {}", path.display()),
            ));
        }
        Ok(Some(DirectoryIdentity {
            volume: u64::from(information.dwVolumeSerialNumber),
            file: u64::from(information.nFileIndexHigh) << 32
                | u64::from(information.nFileIndexLow),
        }))
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn directory_identity(_path: &Path) -> io::Result<Option<DirectoryIdentity>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_for_the_same_directory() {
        let temp = tempfile::tempdir().unwrap();
        let first = directory_identity(temp.path()).unwrap();
        let second = directory_identity(temp.path()).unwrap();
        #[cfg(any(unix, windows))]
        assert!(first.is_some());
        assert_eq!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn identity_changes_when_the_path_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("generation");
        let old = temp.path().join("old-generation");
        std::fs::create_dir(&path).unwrap();
        let first = directory_identity(&path).unwrap().unwrap();
        std::fs::rename(&path, &old).unwrap();
        std::fs::create_dir(&path).unwrap();
        let replacement = directory_identity(&path).unwrap().unwrap();
        assert_ne!(first, replacement);
    }
}
