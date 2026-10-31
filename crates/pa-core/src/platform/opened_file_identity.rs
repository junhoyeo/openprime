//! Live-object identity from an already-opened file, not a pathname.
//!
//! This is an OS identity query, not a filesystem trust or permissions check.
//! Callers own opening/no-follow policy, object-kind checks, trusted ancestors,
//! and the supported filesystem/provider policy. A successful query alone does
//! not establish any of those properties.

use std::fs::File;
use std::io;

/// Identity bound to a borrowed, still-open file.
///
/// Compare these values only while retaining both original files through the
/// protected operation. If a path is reopened, query the new handle and compare
/// it before using that handle; the old path is not proof of object identity.
/// File IDs can be reused after deletion/close. This type deliberately has no
/// serialization or detached-ID API and is not a permanent namespace key.
///
/// Equality says nothing about content, ownership, ACLs, links, trusted roots,
/// leases, or durability. The caller must establish those separately.
pub struct OpenedFileIdentity<'a> {
    _file: &'a File,
    identity: Identity,
}

impl std::fmt::Debug for OpenedFileIdentity<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenedFileIdentity")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Identity {
    volume: u64,
    #[cfg(unix)]
    file: u64,
    #[cfg(windows)]
    file: [u8; 16],
}

impl PartialEq for OpenedFileIdentity<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}

impl Eq for OpenedFileIdentity<'_> {}

/// Query the identity of the caller's open file without opening another path.
///
/// Unix uses the opened file's device/inode. Windows uses `FileIdInfo`'s full
/// 64-bit volume serial and 128-bit file ID, never the older 64-bit file index.
/// The caller must retain the file for the whole identity-dependent operation.
/// This query does not select or certify a filesystem/provider support profile.
///
/// # Errors
/// Returns the OS error if the handle query fails (including unsupported
/// information classes/filesystems), `InvalidData` for a Windows zero volume
/// serial or all-zero file ID, or `Unsupported` on other platforms. Rejecting
/// zero is conservative even if a provider might use it legitimately. An
/// error is unknown identity, not absence, inequality, or permission to fall
/// back to path, timestamp, length, zero, or a narrower identity.
///
/// Two retained handles can be compared without transferring ownership:
/// ```no_run
/// use pa_core::platform::opened_file_identity;
/// let first = std::fs::File::open("owned-fixture")?;
/// let second = first.try_clone()?;
/// let first_identity = opened_file_identity(&first)?;
/// let second_identity = opened_file_identity(&second)?;
/// assert_eq!(first_identity, second_identity);
/// let _still_open = first.metadata()?;
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// The identity cannot outlive its file:
/// ```compile_fail
/// use pa_core::platform::opened_file_identity;
/// let identity = {
///     let file = std::fs::File::open("owned-fixture").unwrap();
///     opened_file_identity(&file).unwrap()
/// };
/// assert_eq!(identity, identity);
/// ```
pub fn opened_file_identity(file: &File) -> io::Result<OpenedFileIdentity<'_>> {
    Ok(OpenedFileIdentity {
        _file: file,
        identity: query_identity(file)?,
    })
}

#[cfg(unix)]
fn query_identity(file: &File) -> io::Result<Identity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok(Identity {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
fn query_identity(file: &File) -> io::Result<Identity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let mut information = FILE_ID_INFO::default();
    // SAFETY: File owns a valid handle, borrowed for this whole call. The
    // aligned, writable FILE_ID_INFO buffer has the SDK's exact size/layout;
    // FileIdInfo requests that structure. Neither pointer escapes this call.
    // We do not transfer ownership or close the caller's handle. On failure
    // capture the OS error immediately and never inspect the output buffer.
    let status = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut information).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if status == 0 {
        return Err(io::Error::last_os_error());
    }
    information.try_into()
}

#[cfg(windows)]
impl TryFrom<windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO> for Identity {
    type Error = io::Error;

    fn try_from(
        information: windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO,
    ) -> io::Result<Self> {
        // A successful API status with an empty identifier is not usable
        // identity evidence. Refuse conservatively, never compare sentinels.
        if information.VolumeSerialNumber == 0 || information.FileId.Identifier == [0; 16] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "opened-file query returned an unusable zero identity",
            ));
        }
        Ok(Self {
            volume: information.VolumeSerialNumber,
            file: information.FileId.Identifier,
        })
    }
}

#[cfg(not(any(unix, windows)))]
fn query_identity(_file: &File) -> io::Result<Identity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "opened-file identity is unsupported on this platform",
    ))
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;
    use std::fs::{self, FileTimes, OpenOptions};
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    fn fixture_file(path: &Path) -> File {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            // The rename/replacement fixtures must really run with the old
            // handle retained; sharing denial is not identity evidence.
            options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        }
        options.open(path).unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn sdk_conversion_and_comparison_preserve_all_identity_bits() {
        use windows_sys::Win32::Storage::FileSystem::{FILE_ID_128, FILE_ID_INFO};

        let temp = tempfile::tempdir().unwrap();
        let file = fixture_file(&temp.path().join("file"));
        let information = FILE_ID_INFO {
            VolumeSerialNumber: 7,
            FileId: FILE_ID_128 {
                Identifier: [3; 16],
            },
        };
        let identity = OpenedFileIdentity {
            _file: &file,
            identity: information.try_into().unwrap(),
        };
        // Exercise the actual SDK-result conversion. These are width tests,
        // not evidence that this runner has a ReFS volume.
        for byte in 0..16 {
            let mut different = information;
            different.FileId.Identifier[byte] ^= 1;
            assert_ne!(
                identity,
                OpenedFileIdentity {
                    _file: &file,
                    identity: different.try_into().unwrap(),
                }
            );
        }
        for bit in 0..64 {
            let mut different = information;
            different.VolumeSerialNumber ^= 1 << bit;
            assert_ne!(
                identity,
                OpenedFileIdentity {
                    _file: &file,
                    identity: different.try_into().unwrap(),
                }
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn unsupported_pipe_query_returns_os_error_without_closing_the_handle() {
        use std::io::Read;
        use std::os::windows::io::OwnedHandle;

        // A valid, owned anonymous pipe has no disk-file FileIdInfo. Use safe
        // std ownership conversions, never fabricate an invalid File handle.
        let (reader, writer) = std::io::pipe().unwrap();
        let mut file = File::from(OwnedHandle::from(reader));
        let error = opened_file_identity(&file).unwrap_err();
        assert!(error.raw_os_error().is_some_and(|code| code != 0));
        // Closing our sole writer makes this a bounded EOF read, and proves
        // the failed query did not close/take ownership of the borrowed file.
        drop(writer);
        assert_eq!(file.read(&mut [0]).unwrap(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn sdk_conversion_refuses_zero_volume_or_file_identity() {
        use windows_sys::Win32::Storage::FileSystem::{FILE_ID_128, FILE_ID_INFO};

        let valid = FILE_ID_INFO {
            VolumeSerialNumber: 7,
            FileId: FILE_ID_128 {
                Identifier: [3; 16],
            },
        };
        assert!(Identity::try_from(valid).is_ok());
        let mut zero_volume = valid;
        zero_volume.VolumeSerialNumber = 0;
        let mut zero_file = valid;
        zero_file.FileId.Identifier = [0; 16];
        for unusable in [zero_volume, zero_file, FILE_ID_INFO::default()] {
            assert_eq!(
                Identity::try_from(unusable).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn repeated_queries_and_duplicate_handles_identify_the_same_open_file() {
        let temp = tempfile::tempdir().unwrap();
        let file = fixture_file(&temp.path().join("file"));
        let duplicate = file.try_clone().unwrap();
        let identity = opened_file_identity(&file).unwrap();
        assert_eq!(identity, opened_file_identity(&file).unwrap());
        assert_eq!(identity, opened_file_identity(&duplicate).unwrap());
        // Queries never take ownership or close the borrowed handle.
        file.set_len(17).unwrap();
        assert_eq!(duplicate.metadata().unwrap().len(), 17);
    }

    #[test]
    fn hard_link_and_rename_preserve_retained_file_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        let alias = temp.path().join("alias");
        let renamed = temp.path().join("renamed");
        let file = fixture_file(&path);
        let identity = opened_file_identity(&file).unwrap();
        fs::hard_link(&path, &alias).unwrap();
        let alias_file = File::open(&alias).unwrap();
        assert_eq!(identity, opened_file_identity(&alias_file).unwrap());
        fs::rename(&path, &renamed).unwrap();
        let renamed_file = File::open(&renamed).unwrap();
        assert_eq!(identity, opened_file_identity(&renamed_file).unwrap());
        assert_eq!(identity, opened_file_identity(&file).unwrap());
    }

    #[test]
    fn rebound_path_does_not_inherit_the_retained_old_file_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        let moved = temp.path().join("old-file");
        let file = fixture_file(&path);
        let identity = opened_file_identity(&file).unwrap();
        fs::rename(&path, &moved).unwrap();
        let replacement = fixture_file(&path);
        assert_ne!(identity, opened_file_identity(&replacement).unwrap());
        assert_eq!(identity, opened_file_identity(&file).unwrap());
        file.set_len(19).unwrap();
        assert_eq!(replacement.metadata().unwrap().len(), 0);
        assert_eq!(fs::metadata(&moved).unwrap().len(), 19);
    }

    #[test]
    fn equal_content_length_and_timestamps_are_not_file_identity() {
        let temp = tempfile::tempdir().unwrap();
        let first_path = temp.path().join("first");
        let second_path = temp.path().join("second");
        let first = fixture_file(&first_path);
        let second = fixture_file(&second_path);
        fs::write(&first_path, b"identical bytes").unwrap();
        fs::write(&second_path, b"identical bytes").unwrap();
        let timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let times = FileTimes::new()
            .set_accessed(timestamp)
            .set_modified(timestamp);
        first.set_times(times).unwrap();
        second.set_times(times).unwrap();
        assert_eq!(
            fs::read(&first_path).unwrap(),
            fs::read(&second_path).unwrap()
        );
        assert_eq!(
            first.metadata().unwrap().len(),
            second.metadata().unwrap().len()
        );
        assert_eq!(
            first.metadata().unwrap().modified().unwrap(),
            second.metadata().unwrap().modified().unwrap()
        );
        assert_ne!(
            opened_file_identity(&first).unwrap(),
            opened_file_identity(&second).unwrap()
        );
    }
}
