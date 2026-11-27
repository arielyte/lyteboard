//! Small safe boundary around Windows user security and filesystem APIs.
#![cfg(windows)]

use std::{
    ffi::{OsStr, c_void},
    fs::File,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, Cryptography::*, *},
    Storage::FileSystem::*,
    System::{
        Pipes::*, SystemInformation::GetTickCount64, SystemServices::ACCESS_ALLOWED_ACE_TYPE,
        Threading::*,
    },
    UI::Shell::*,
};
use zeroize::{Zeroize, Zeroizing};

fn wide(value: impl AsRef<OsStr>) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.as_ref().encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::other("NUL in Windows name"));
    }
    value.push(0);
    Ok(value)
}
fn checked(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Windows object is not private to the current user",
    )
}

struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn token_sid(process: HANDLE) -> io::Result<String> {
    unsafe {
        let mut token = ptr::null_mut();
        checked(OpenProcessToken(process, TOKEN_QUERY, &mut token))?;
        let token = OwnedHandle::from_raw_handle(token);
        let mut size = 0;
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut size,
        );
        // Vec<usize> gives TOKEN_USER its required alignment.
        let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        checked(GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        ))?;
        sid_text((*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid)
    }
}
unsafe fn sid_text(sid: PSID) -> io::Result<String> {
    unsafe {
        let mut text = ptr::null_mut();
        checked(ConvertSidToStringSidW(sid, &mut text))?;
        let _allocation = Local(text.cast());
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, len,
        )))
    }
}
pub fn user_sid() -> io::Result<String> {
    token_sid(unsafe { GetCurrentProcess() })
}

/// LocalAppData comes from the current user's Known Folder, not HOME or a system directory.
pub fn app_data() -> io::Result<PathBuf> {
    unsafe {
        let mut path = ptr::null_mut();
        let hr = SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, ptr::null_mut(), &mut path);
        if hr < 0 {
            return Err(io::Error::other(format!(
                "LocalAppData lookup failed: {hr:#x}"
            )));
        }
        let mut len = 0;
        while *path.add(len) != 0 {
            len += 1;
        }
        use std::os::windows::ffi::OsStringExt;
        let result = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
            path, len,
        )))
        .join("Lyteboard");
        windows_sys::Win32::System::Com::CoTaskMemFree(path.cast());
        Ok(result)
    }
}

struct Security(Local);
impl Security {
    fn new(directory: bool) -> io::Result<Self> {
        // Protected DACL, one allow ACE for this account. Children inherit only this ACE.
        let inherit = if directory { "OICI" } else { "" };
        let sid = user_sid()?;
        let text = wide(format!("O:{sid}D:P(A;{inherit};FA;;;{sid})"))?;
        unsafe {
            let mut sd = ptr::null_mut();
            checked(ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            ))?;
            Ok(Self(Local(sd)))
        }
    }
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: 0,
        }
    }
}

/// Validate the owner and every DACL entry; reject broad grants and NULL DACLs.
pub fn check_private_file(file: &File) -> io::Result<()> {
    unsafe {
        let mut owner = ptr::null_mut();
        let mut acl = ptr::null_mut();
        let mut sd = ptr::null_mut();
        let code = GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut sd,
        );
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        let _allocation = Local(sd);
        let sid = user_sid()?;
        if owner.is_null() || sid_text(owner)? != sid || acl.is_null() || (*acl).AceCount != 1 {
            return Err(denied());
        }
        let mut ace = ptr::null_mut();
        checked(GetAce(acl, 0, &mut ace))?;
        let header = &*ace.cast::<ACE_HEADER>();
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8
            || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Err(denied());
        }
        let entry = &*ace.cast::<ACCESS_ALLOWED_ACE>();
        if entry.Mask != FILE_ALL_ACCESS
            || sid_text((&entry.SidStart as *const u32).cast_mut().cast())? != sid
        {
            return Err(denied());
        }
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        checked(GetFileInformationByHandle(file.as_raw_handle(), &mut info))?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 && info.nNumberOfLinks != 1)
            || u32::from(entry.Header.AceFlags) & INHERIT_ONLY_ACE != 0
        {
            return Err(denied());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            let mut control = 0;
            let mut revision = 0;
            checked(GetSecurityDescriptorControl(
                sd,
                &mut control,
                &mut revision,
            ))?;
            // An unprotected directory could gain broad grants from a parent later.
            // Both inheritance flags are required so SQLite sidecars stay user-only.
            if control & SE_DACL_PROTECTED == 0
                || u32::from(entry.Header.AceFlags) & (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE)
                    != OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
            {
                return Err(denied());
            }
        }
        Ok(())
    }
}
fn open(path: &Path, access: u32, creation: u32, directory: bool) -> io::Result<File> {
    let path = wide(path)?;
    let security = Security::new(directory)?;
    let attributes = security.attributes();
    unsafe {
        let handle = CreateFileW(
            path.as_ptr(),
            access | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            creation,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let file = File::from_raw_handle(handle);
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        checked(GetFileInformationByHandle(file.as_raw_handle(), &mut info))?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        {
            return Err(denied());
        }
        Ok(file)
    }
}

/// Hold every ancestor without FILE_SHARE_DELETE, preventing path replacement while in use.
pub struct PrivateDir {
    path: PathBuf,
    _guards: Vec<File>,
}
impl PrivateDir {
    pub fn open(path: &Path) -> io::Result<Self> {
        use std::path::Component;
        if !path.is_absolute()
            || path.components().any(|p| matches!(p, Component::ParentDir))
            || path.to_str().is_none()
        {
            return Err(denied());
        }
        let mut current = PathBuf::new();
        let mut guards = Vec::new();
        for part in path.components() {
            match part {
                Component::Prefix(p) => {
                    // Local drive paths only: no UNC/network shares or device namespaces.
                    if !matches!(p.kind(), std::path::Prefix::Disk(_)) {
                        return Err(denied());
                    }
                    current.push(part.as_os_str());
                    continue;
                }
                Component::RootDir => {
                    current.push(part.as_os_str());
                }
                Component::Normal(_) => {
                    current.push(part.as_os_str());
                }
                _ => return Err(denied()),
            }
            let security = Security::new(true)?;
            let attributes = security.attributes();
            if !current.exists() {
                let name = wide(&current)?;
                unsafe {
                    if CreateDirectoryW(name.as_ptr(), &attributes) == 0
                        && GetLastError() != ERROR_ALREADY_EXISTS
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
            }
            let handle = open(&current, FILE_READ_ATTRIBUTES, OPEN_EXISTING, true)?;
            guards.push(handle);
        }
        check_private_file(guards.last().ok_or_else(denied)?)?;
        Ok(Self {
            path: path.to_owned(),
            _guards: guards,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn open_file(&self, name: &str, create: bool) -> io::Result<File> {
        if name.is_empty() || name.contains(['/', '\\', ':']) || name == "." || name == ".." {
            return Err(denied());
        }
        let file = open(
            &self.path.join(name),
            GENERIC_READ | GENERIC_WRITE,
            if create { OPEN_ALWAYS } else { OPEN_EXISTING },
            false,
        )?;
        check_private_file(&file)?;
        Ok(file)
    }
    pub fn replace(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        // Validate an existing target without following a reparse point.
        match self.open_file(name, false) {
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let suffix = format!(".{:x}-{:x}-{sequence:x}.tmp", std::process::id(), unsafe {
            GetTickCount64()
        });
        let temporary = self.path.join(&suffix);
        let mut file = open(&temporary, GENERIC_READ | GENERIC_WRITE, CREATE_NEW, false)?;
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            unsafe {
                checked(MoveFileExW(
                    wide(&temporary)?.as_ptr(),
                    wide(self.path.join(name))?.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                ))
            }
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }
}

/// DPAPI current-user scope, with UI forbidden. No plaintext persistence path.
pub fn protect(bytes: &[u8], decrypt: bool) -> io::Result<Zeroizing<Vec<u8>>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into().map_err(|_| denied())?,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let mut output: CRYPT_INTEGER_BLOB = unsafe { std::mem::zeroed() };
    unsafe {
        let ok = if decrypt {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptProtectData(
                &input,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        checked(ok)?;
        let _allocation = Local(output.pbData.cast());
        if output.cbData == 0 {
            return Ok(Zeroizing::new(Vec::new()));
        }
        let raw = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let result = Zeroizing::new(raw.to_vec());
        raw.zeroize();
        Ok(result)
    }
}

/// Both peers must have a kernel-reported process token matching this user.
/// The client uses SECURITY_IDENTIFICATION, so the server cannot act as its user.
pub fn validate_pipe_peer(handle: &impl AsRawHandle, server_end: bool) -> io::Result<()> {
    validate_pipe_peer_for_user(handle, server_end, &user_sid()?)
}

fn validate_pipe_peer_for_user(
    handle: &impl AsRawHandle,
    server_end: bool,
    expected_sid: &str,
) -> io::Result<()> {
    unsafe {
        let mut pid = 0;
        checked(if server_end {
            GetNamedPipeClientProcessId(handle.as_raw_handle(), &mut pid)
        } else {
            GetNamedPipeServerProcessId(handle.as_raw_handle(), &mut pid)
        })?;
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = OwnedHandle::from_raw_handle(process);
        if token_sid(process.as_raw_handle())? != expected_sid {
            return Err(denied());
        }
        Ok(())
    }
}

pub fn pipe_server(
    name: &str,
    first: bool,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let security = Security::new(false)?;
    let attributes = security.attributes();
    // Tokio always sets FILE_FLAG_OVERLAPPED; reject_remote_clients sets PIPE_REJECT_REMOTE_CLIENTS.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&attributes as *const SECURITY_ATTRIBUTES)
                    .cast_mut()
                    .cast(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dpapi_round_trip_and_tamper_rejection() {
        assert!(
            protect(&protect(&[], false).unwrap(), true)
                .unwrap()
                .is_empty()
        );
        let secret = b"disposable identity material";
        let protected = protect(secret, false).unwrap();
        assert_ne!(&*protected, secret);
        assert_eq!(&*protect(&protected, true).unwrap(), secret);
        let mut tampered = protected.to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x80;
        assert!(protect(&tampered, true).is_err());
        assert!(protect(b"plaintext is not a DPAPI blob", true).is_err());
    }
    #[test]
    fn private_storage_rejects_broad_dacl_hardlinks_and_path_escape() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private");
        let dir = PrivateDir::open(&path).unwrap();
        std::fs::create_dir(path.join("inherited-directory")).unwrap();
        assert!(PrivateDir::open(&path.join("inherited-directory")).is_err());
        std::fs::write(path.join("sqlite-sidecar"), b"private").unwrap();
        dir.open_file("sqlite-sidecar", false).unwrap();
        let file = dir.open_file("data", true).unwrap();
        check_private_file(&file).unwrap();
        assert!(dir.open_file("../escape", true).is_err());
        assert!(dir.open_file("data:stream", true).is_err());
        assert!(PrivateDir::open(Path::new("relative")).is_err());
        assert!(PrivateDir::open(&path.join("..\\escape")).is_err());
        drop(file);
        std::fs::hard_link(path.join("data"), path.join("linked")).unwrap();
        assert!(dir.open_file("data", false).is_err());
        std::fs::remove_file(path.join("linked")).unwrap();
        let file = open(
            &path.join("data"),
            GENERIC_READ | WRITE_DAC,
            OPEN_EXISTING,
            false,
        )
        .unwrap();
        unsafe {
            // A NULL DACL permits everyone. Existing handles must still detect it.
            let result = SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
            );
            assert_eq!(result, 0);
        }
        assert!(check_private_file(&file).is_err());
        assert!(dir.open_file("data", false).is_err());
    }
    #[tokio::test]
    async fn pipe_credentials_and_first_instance_are_enforced() {
        use tokio::net::windows::named_pipe::ClientOptions;
        let name = format!(
            r"\\.\pipe\lyteboard-security-test-{}-{}",
            user_sid().unwrap(),
            std::process::id()
        );
        let server = pipe_server(&name, true).unwrap();
        assert!(pipe_server(&name, true).is_err());
        let client = ClientOptions::new()
            .security_qos_flags(0x00110000)
            .open(&name)
            .unwrap();
        server.connect().await.unwrap();
        validate_pipe_peer(&server, true).unwrap();
        validate_pipe_peer(&client, false).unwrap();
        assert!(validate_pipe_peer_for_user(&client, false, "S-1-0-0").is_err());
        assert!(validate_pipe_peer_for_user(&server, true, "S-1-0-0").is_err());
    }
}
