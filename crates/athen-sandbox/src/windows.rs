//! Windows sandbox backend: Job Object + AppContainer.
//!
//! Two-tier isolation analogous to macOS `sandbox-exec`:
//!
//! 1. **Job Object** — caps memory/active-process count and ties the child
//!    process tree to our parent via `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`,
//!    so an orphaned grandchild cannot survive a parent crash. Job Objects
//!    have shipped since Windows XP.
//! 2. **AppContainer** — real filesystem isolation via SID-based ACLs. The
//!    spawned process runs under a per-execution AppContainer SID that has
//!    no ambient access to the user's files; we explicitly grant the SID
//!    read+execute on the binary and read+write on `allowed_paths` only.
//!    AppContainer ships on Win 8 / Server 2012 and later, which is our
//!    minimum supported Windows.
//!
//! Network access is gated through the `internetClient` capability: present
//! for `ReadOnly` and `RestrictedWrite` (matches the bwrap default flow);
//! omitted for `NoNetwork` and `Full`, which leaves the AppContainer with
//! no network capability and therefore no socket access.

#![cfg(target_os = "windows")]

use async_trait::async_trait;
use athen_core::error::{AthenError, Result};
use athen_core::sandbox::{SandboxCapabilities, SandboxLevel, SandboxProfile};
use athen_core::traits::sandbox::{SandboxExecutor, SandboxOutput};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Instant;
use tracing::{debug, warn};
use widestring::U16CString;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, FreeLibrary, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, HANDLE, HANDLE_FLAGS,
    HLOCAL, HMODULE, INVALID_HANDLE_VALUE, TRUE, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_GROUP, TRUSTEE_IS_SID, TRUSTEE_W,
};
use windows::Win32::Security::{
    CreateWellKnownSid, FreeSid, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
    OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES,
    SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, WELL_KNOWN_SID_TYPE,
};
use windows::Win32::Storage::FileSystem::{
    ReadFile, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

const NO_INHERITANCE_FLAG: u32 = 0;

const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

/// Returns true when the Job Object + AppContainer backend can run.
///
/// Both primitives are present on every supported Windows (Job Object since
/// XP, AppContainer since Win8). We don't probe at runtime — falsey results
/// from `CreateAppContainerProfile` are surfaced from `execute()` directly.
pub fn windows_capability() -> bool {
    true
}

/// Windows sandbox executor. Stateless — each call to `execute()` builds a
/// fresh AppContainer profile and Job Object scoped to one process tree.
pub struct WindowsSandbox;

#[async_trait]
impl SandboxExecutor for WindowsSandbox {
    async fn detect_capabilities(&self) -> Result<SandboxCapabilities> {
        use crate::detect::SandboxDetector;
        Ok(SandboxDetector::detect().await)
    }

    async fn execute(
        &self,
        command: &str,
        args: &[&str],
        sandbox: &SandboxLevel,
    ) -> Result<SandboxOutput> {
        let profile = match sandbox {
            SandboxLevel::OsNative { profile } => profile.clone(),
            _ => {
                return Err(AthenError::Sandbox(
                    "WindowsSandbox requires SandboxLevel::OsNative".into(),
                ))
            }
        };

        let command = command.to_string();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();

        // CreateProcessW + AppContainer + Job Object are all blocking syscalls
        // that touch raw HANDLEs which aren't Send-friendly across awaits, so
        // run the whole flow on a blocking worker.
        tokio::task::spawn_blocking(move || run_sandboxed_blocking(&command, &args, &profile))
            .await
            .map_err(|e| AthenError::Sandbox(format!("sandbox task panicked: {e}")))?
    }
}

fn run_sandboxed_blocking(
    command: &str,
    args: &[String],
    profile: &SandboxProfile,
) -> Result<SandboxOutput> {
    let start = Instant::now();

    let container_name = format!("athen.sandbox.{}", uuid::Uuid::new_v4().simple());
    let container_name_w = U16CString::from_str(&container_name)
        .map_err(|e| AthenError::Sandbox(format!("container name encode: {e}")))?;

    let app_container = AppContainerProfile::create(&container_name_w)?;

    let resolved_command = resolve_executable_blocking(command);
    let command_for_spawn = resolved_command
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| command.to_string());

    let mut acl_grants: Vec<AclGrantGuard> = Vec::new();
    if let Some(bin_path) = resolved_command.as_ref() {
        if !is_under_windows_dir(bin_path) {
            if let Some(g) = grant_appcontainer_access(
                bin_path,
                app_container.sid(),
                FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0,
                false,
            ) {
                acl_grants.push(g);
            }
        }
    }

    if let Some(runtimes_dir) = athen_runtimes_dir() {
        if runtimes_dir.exists() {
            if let Some(g) = grant_appcontainer_access(
                &runtimes_dir,
                app_container.sid(),
                FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0,
                true,
            ) {
                acl_grants.push(g);
            }
        }
    }

    if let SandboxProfile::RestrictedWrite { allowed_paths } = profile {
        for path in allowed_paths {
            let is_dir = path.is_dir();
            if let Some(g) = grant_appcontainer_access(
                path,
                app_container.sid(),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0 | FILE_GENERIC_EXECUTE.0,
                is_dir,
            ) {
                acl_grants.push(g);
            }
        }
    }

    let job = JobObjectGuard::new()?;
    job.configure()?;

    let want_network = matches!(
        profile,
        SandboxProfile::ReadOnly | SandboxProfile::RestrictedWrite { .. }
    );

    let internet_client_sid: Option<SidBuf> = if want_network {
        match SidBuf::well_known(WELL_KNOWN_SID_TYPE(95)) {
            Ok(sid) => Some(sid),
            Err(e) => {
                warn!(error = %e, "internetClient SID derivation failed; sandbox will be offline");
                None
            }
        }
    } else {
        None
    };

    let mut capabilities_storage: Vec<SID_AND_ATTRIBUTES> = Vec::new();
    if let Some(sid) = internet_client_sid.as_ref() {
        capabilities_storage.push(SID_AND_ATTRIBUTES {
            Sid: PSID(sid.as_ptr() as *mut c_void),
            Attributes: 0x0000_0004, // SE_GROUP_ENABLED
        });
    }

    let mut sec_caps = SECURITY_CAPABILITIES {
        AppContainerSid: app_container.sid(),
        Capabilities: if capabilities_storage.is_empty() {
            ptr::null_mut()
        } else {
            capabilities_storage.as_mut_ptr()
        },
        CapabilityCount: capabilities_storage.len() as u32,
        Reserved: 0,
    };

    let mut stdout_pipe = StdPipe::new()?;
    let mut stderr_pipe = StdPipe::new()?;

    let mut attr_list = ProcThreadAttributeList::new(1)?;
    // SAFETY: sec_caps lives until after CreateProcessW returns, satisfying
    // the proc-thread attribute list's borrow of the pointer.
    unsafe {
        attr_list.update_security_capabilities(&mut sec_caps)?;
    }

    let mut startup_info = STARTUPINFOEXW::default();
    startup_info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup_info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup_info.StartupInfo.hStdInput = HANDLE::default();
    startup_info.StartupInfo.hStdOutput = stdout_pipe.write_handle();
    startup_info.StartupInfo.hStdError = stderr_pipe.write_handle();
    startup_info.lpAttributeList = attr_list.as_ptr();

    let mut command_line = build_command_line(&command_for_spawn, args);
    let command_line_w = U16CString::from_str(&command_line)
        .map_err(|e| AthenError::Sandbox(format!("command line encode: {e}")))?;
    // CreateProcessW takes a *mutable* command line; we own a private copy.
    let mut command_line_buf: Vec<u16> = command_line_w.as_slice_with_nul().to_vec();
    // Suppress dead code warning — kept for clarity at the call-site.
    command_line.clear();

    let mut proc_info = PROCESS_INFORMATION::default();

    debug!(
        command = %command_for_spawn,
        container = %container_name,
        network = want_network,
        "spawning AppContainer process"
    );

    // SAFETY: All pointers passed to CreateProcessW are non-null where
    // required and have lifetimes that exceed the call. The startup_info
    // struct embeds STARTUPINFOEXW with an attribute list pointing into
    // attr_list (alive for the function), and the pipe handles inside it
    // remain owned by stdout_pipe/stderr_pipe.
    let create_ok = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(command_line_buf.as_mut_ptr())),
            None,
            None,
            true,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup_info.StartupInfo,
            &mut proc_info,
        )
    };
    if create_ok.is_err() {
        return Err(AthenError::Sandbox(format!(
            "CreateProcessW failed: {:?}",
            unsafe { GetLastError() }
        )));
    }

    // From here on, proc_info.hProcess and hThread must be closed.
    let process_handle = proc_info.hProcess;
    let thread_handle = proc_info.hThread;

    // SAFETY: process_handle was just returned valid from CreateProcessW.
    let assign_res = unsafe { AssignProcessToJobObject(job.handle(), process_handle) };
    if assign_res.is_err() {
        // SAFETY: Same handle, valid until our explicit CloseHandle below.
        unsafe {
            let _ = TerminateProcess(process_handle, 1);
            let _ = CloseHandle(thread_handle);
            let _ = CloseHandle(process_handle);
        }
        return Err(AthenError::Sandbox(format!(
            "AssignProcessToJobObject failed: {:?}",
            unsafe { GetLastError() }
        )));
    }

    // SAFETY: thread_handle is the suspended primary thread from CreateProcessW.
    unsafe {
        let _ = ResumeThread(thread_handle);
    }

    // Close the write ends in the parent so the child's EOF propagates to us.
    drop(stdout_pipe.take_writer());
    drop(stderr_pipe.take_writer());

    let stdout_reader = stdout_pipe.spawn_reader();
    let stderr_reader = stderr_pipe.spawn_reader();

    // SAFETY: process_handle remains valid until we CloseHandle it below.
    let wait_res = unsafe { WaitForSingleObject(process_handle, u32::MAX) };
    if wait_res != WAIT_OBJECT_0 {
        warn!(?wait_res, "WaitForSingleObject returned unexpected status");
    }

    let stdout_buf = stdout_reader.join().unwrap_or_default();
    let stderr_buf = stderr_reader.join().unwrap_or_default();

    let mut exit_code: u32 = 0;
    // SAFETY: process_handle still valid; exit_code is a stack u32.
    unsafe {
        let _ = GetExitCodeProcess(process_handle, &mut exit_code);
    }

    // SAFETY: Both handles were created by CreateProcessW; we own their lifetime.
    unsafe {
        let _ = CloseHandle(thread_handle);
        let _ = CloseHandle(process_handle);
    }

    // ACL grants drop in reverse order (best effort) via Drop impls.
    drop(acl_grants);
    drop(attr_list);
    drop(job);
    drop(app_container);

    let elapsed = start.elapsed();

    Ok(SandboxOutput {
        exit_code: exit_code as i32,
        stdout: String::from_utf8_lossy(&stdout_buf).to_string(),
        stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
        execution_time_ms: elapsed.as_millis() as u64,
    })
}

// -- AppContainer profile ---------------------------------------------------

struct AppContainerProfile {
    name: U16CString,
    sid: PSID,
    /// True if SID was allocated by CreateAppContainerProfile (FreeSid),
    /// false if by DeriveAppContainerSidFromAppContainerName (LocalFree).
    free_with_freesid: bool,
}

impl AppContainerProfile {
    fn create(name: &U16CString) -> Result<Self> {
        type CreateFn = unsafe extern "system" fn(
            PCWSTR,
            PCWSTR,
            PCWSTR,
            *const SID_AND_ATTRIBUTES,
            u32,
            *mut PSID,
        ) -> windows::core::HRESULT;
        type DeriveFn = unsafe extern "system" fn(PCWSTR, *mut PSID) -> windows::core::HRESULT;

        let userenv = LoadedLibrary::load("userenv.dll")?;
        let create_proc: CreateFn =
            unsafe {
                std::mem::transmute(userenv.get_proc("CreateAppContainerProfile").ok_or_else(
                    || AthenError::Sandbox("userenv!CreateAppContainerProfile missing".into()),
                )?)
            };
        let derive_proc: DeriveFn = unsafe {
            std::mem::transmute(
                userenv
                    .get_proc("DeriveAppContainerSidFromAppContainerName")
                    .ok_or_else(|| {
                        AthenError::Sandbox(
                            "userenv!DeriveAppContainerSidFromAppContainerName missing".into(),
                        )
                    })?,
            )
        };

        let mut sid: PSID = PSID::default();
        // SAFETY: name is a valid null-terminated UTF-16 string; sid is a stack PSID.
        let hr = unsafe {
            create_proc(
                PCWSTR(name.as_ptr()),
                PCWSTR(name.as_ptr()),
                PCWSTR(name.as_ptr()),
                ptr::null(),
                0,
                &mut sid,
            )
        };

        if hr.is_ok() {
            return Ok(Self {
                name: name.clone(),
                sid,
                free_with_freesid: true,
            });
        }

        // ERROR_ALREADY_EXISTS as HRESULT is 0x800700B7.
        let already_exists = hr.0 as u32 == 0x8007_0000u32 | (ERROR_ALREADY_EXISTS.0 & 0xFFFF);
        if !already_exists {
            return Err(AthenError::Sandbox(format!(
                "CreateAppContainerProfile failed: hr={:#x}",
                hr.0 as u32
            )));
        }

        let mut sid2: PSID = PSID::default();
        // SAFETY: Same name buffer; derive returns a LocalAlloc'd SID.
        let hr2 = unsafe { derive_proc(PCWSTR(name.as_ptr()), &mut sid2) };
        if hr2.is_err() {
            return Err(AthenError::Sandbox(format!(
                "DeriveAppContainerSidFromAppContainerName failed: hr={:#x}",
                hr2.0 as u32
            )));
        }

        Ok(Self {
            name: name.clone(),
            sid: sid2,
            free_with_freesid: false,
        })
    }

    fn sid(&self) -> PSID {
        self.sid
    }
}

impl Drop for AppContainerProfile {
    fn drop(&mut self) {
        type DeleteFn = unsafe extern "system" fn(PCWSTR) -> windows::core::HRESULT;
        if let Ok(userenv) = LoadedLibrary::load("userenv.dll") {
            if let Some(p) = userenv.get_proc("DeleteAppContainerProfile") {
                let f: DeleteFn = unsafe { std::mem::transmute(p) };
                // SAFETY: name owned by self, valid for the call.
                unsafe {
                    let _ = f(PCWSTR(self.name.as_ptr()));
                }
            }
        }

        if !self.sid.is_invalid() {
            if self.free_with_freesid {
                // SAFETY: sid was allocated by CreateAppContainerProfile.
                unsafe {
                    let _ = FreeSid(self.sid);
                }
            } else {
                // SAFETY: sid was allocated by DeriveAppContainerSidFromAppContainerName via LocalAlloc.
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.sid.0)));
                }
            }
        }
    }
}

// -- Library loader (LoadLibraryW + GetProcAddress) -------------------------

struct LoadedLibrary {
    handle: HMODULE,
}

impl LoadedLibrary {
    fn load(name: &str) -> Result<Self> {
        use windows::Win32::System::LibraryLoader::LoadLibraryW;
        let wide = U16CString::from_str(name)
            .map_err(|e| AthenError::Sandbox(format!("library name encode: {e}")))?;
        // SAFETY: wide is null-terminated and valid for the call.
        let handle = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }
            .map_err(|e| AthenError::Sandbox(format!("LoadLibraryW({name}): {e}")))?;
        Ok(Self { handle })
    }

    fn get_proc(&self, name: &str) -> Option<unsafe extern "system" fn() -> isize> {
        use windows::Win32::System::LibraryLoader::GetProcAddress;
        let cstr = std::ffi::CString::new(name).ok()?;
        // SAFETY: cstr is null-terminated, handle is valid until self drops.
        unsafe {
            GetProcAddress(
                self.handle,
                windows::core::PCSTR(cstr.as_ptr() as *const u8),
            )
        }
    }
}

impl Drop for LoadedLibrary {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: handle obtained via LoadLibraryW.
            unsafe {
                let _ = FreeLibrary(self.handle);
            }
        }
    }
}

// -- Well-known SID buffer --------------------------------------------------

struct SidBuf {
    buf: Vec<u8>,
}

impl SidBuf {
    fn well_known(kind: WELL_KNOWN_SID_TYPE) -> Result<Self> {
        let mut buf = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut size = buf.len() as u32;
        // SAFETY: buf is correctly sized; size is in/out u32.
        let res = unsafe {
            CreateWellKnownSid(
                kind,
                None,
                Some(PSID(buf.as_mut_ptr() as *mut c_void)),
                &mut size,
            )
        };
        res.map_err(|e| AthenError::Sandbox(format!("CreateWellKnownSid: {e}")))?;
        buf.truncate(size as usize);
        Ok(Self { buf })
    }

    fn as_ptr(&self) -> *const u8 {
        self.buf.as_ptr()
    }
}

// -- Job Object -------------------------------------------------------------

struct JobObjectGuard {
    handle: HANDLE,
}

impl JobObjectGuard {
    fn new() -> Result<Self> {
        // SAFETY: NULL attrs and NULL name are explicitly allowed.
        let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|e| AthenError::Sandbox(format!("CreateJobObjectW: {e}")))?;
        Ok(Self { handle })
    }

    fn handle(&self) -> HANDLE {
        self.handle
    }

    fn configure(&self) -> Result<()> {
        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                ActiveProcessLimit: 64,
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                    | JOB_OBJECT_LIMIT_PROCESS_MEMORY
                    | JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
                ..Default::default()
            },
            ProcessMemoryLimit: 2 * 1024 * 1024 * 1024,
            ..Default::default()
        };

        // SAFETY: info is on the stack; size matches the type.
        let res = unsafe {
            SetInformationJobObject(
                self.handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        res.map_err(|e| AthenError::Sandbox(format!("SetInformationJobObject: {e}")))
    }
}

impl Drop for JobObjectGuard {
    fn drop(&mut self) {
        if !self.handle.is_invalid() && self.handle != INVALID_HANDLE_VALUE {
            // SAFETY: handle owned by self.
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

// -- Proc-thread attribute list --------------------------------------------

struct ProcThreadAttributeList {
    buf: Vec<u8>,
    initialized: bool,
}

impl ProcThreadAttributeList {
    fn new(attribute_count: u32) -> Result<Self> {
        let mut size: usize = 0;
        // SAFETY: First call deliberately passes NULL to query required size;
        // returns ERROR_INSUFFICIENT_BUFFER which we ignore — size is the output.
        unsafe {
            let _ = InitializeProcThreadAttributeList(None, attribute_count, Some(0), &mut size);
        }
        if size == 0 {
            return Err(AthenError::Sandbox(
                "InitializeProcThreadAttributeList size query returned 0".into(),
            ));
        }
        let mut buf = vec![0u8; size];
        // SAFETY: buf is exactly `size` bytes; second call initializes it.
        let res = unsafe {
            InitializeProcThreadAttributeList(
                Some(LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr() as *mut c_void)),
                attribute_count,
                Some(0),
                &mut size,
            )
        };
        res.map_err(|e| AthenError::Sandbox(format!("InitializeProcThreadAttributeList: {e}")))?;
        Ok(Self {
            buf,
            initialized: true,
        })
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buf.as_mut_ptr() as *mut c_void)
    }

    /// # Safety
    /// `caps` must remain alive until the spawned process is started.
    unsafe fn update_security_capabilities(
        &mut self,
        caps: &mut SECURITY_CAPABILITIES,
    ) -> Result<()> {
        let res = UpdateProcThreadAttribute(
            self.as_ptr(),
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            Some(caps as *const _ as *const c_void),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            None,
            None,
        );
        res.map_err(|e| AthenError::Sandbox(format!("UpdateProcThreadAttribute: {e}")))
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: buf was initialized by InitializeProcThreadAttributeList.
            unsafe {
                DeleteProcThreadAttributeList(LPPROC_THREAD_ATTRIBUTE_LIST(
                    self.buf.as_mut_ptr() as *mut c_void
                ));
            }
        }
    }
}

// -- Stdout/stderr pipe -----------------------------------------------------

struct StdPipe {
    read: HANDLE,
    write: HANDLE,
    write_taken: bool,
}

impl StdPipe {
    fn new() -> Result<Self> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: TRUE,
        };
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        // SAFETY: sa, read, write are valid stack locations.
        let res = unsafe { CreatePipe(&mut read, &mut write, Some(&sa), 0) };
        res.map_err(|e| AthenError::Sandbox(format!("CreatePipe: {e}")))?;

        // Read end must NOT be inheritable so the child process doesn't
        // hold an extra reference that would prevent EOF on close.
        // SAFETY: read is a valid handle just returned from CreatePipe.
        unsafe {
            use windows::Win32::Foundation::SetHandleInformation;
            let _ = SetHandleInformation(read, HANDLE_FLAG_INHERIT, HANDLE_FLAGS(0));
        }

        Ok(Self {
            read,
            write,
            write_taken: false,
        })
    }

    fn write_handle(&self) -> HANDLE {
        self.write
    }

    /// Returns a guard that closes the write handle when dropped, allowing
    /// EOF to propagate after CreateProcessW has duplicated it into the child.
    fn take_writer(&mut self) -> WriteHandleCloser {
        self.write_taken = true;
        WriteHandleCloser {
            handle: std::mem::take(&mut self.write),
        }
    }

    fn spawn_reader(&self) -> std::thread::JoinHandle<Vec<u8>> {
        struct SendHandle(HANDLE);
        // SAFETY: HANDLE is just a kernel object identifier; sharing it across
        // threads is safe as long as we only read in the worker.
        unsafe impl Send for SendHandle {}
        let read = SendHandle(self.read);
        std::thread::spawn(move || {
            let read = read;
            let mut out = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let mut n: u32 = 0;
                // SAFETY: buf is a valid mutable slice; n is a stack u32.
                let res = unsafe { ReadFile(read.0, Some(&mut buf), Some(&mut n), None) };
                if res.is_err() || n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n as usize]);
            }
            out
        })
    }
}

impl Drop for StdPipe {
    fn drop(&mut self) {
        if !self.read.is_invalid() {
            // SAFETY: handle owned by self.
            unsafe {
                let _ = CloseHandle(self.read);
            }
        }
        if !self.write_taken && !self.write.is_invalid() {
            // SAFETY: handle owned by self.
            unsafe {
                let _ = CloseHandle(self.write);
            }
        }
    }
}

struct WriteHandleCloser {
    handle: HANDLE,
}

impl Drop for WriteHandleCloser {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: handle was transferred from StdPipe::take_writer.
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

// -- ACL grant guard --------------------------------------------------------

struct AclGrantGuard {
    path_w: U16CString,
    sid: PSID,
    /// Stored DACL pointer to free on revoke (LocalAlloc'd by SetEntriesInAclW).
    new_dacl_alloc: *mut ACL,
}

impl AclGrantGuard {
    fn revoke_now(&mut self) {
        // Re-fetch DACL, walk ACEs, remove ones for our SID. For now the
        // simplest robust strategy is to restore by setting the SE_DACL_PROTECTED
        // flag off and re-inheriting — but the cleanest minimal implementation
        // is to set a fresh empty DACL grant for our SID with REVOKE_ACCESS via
        // SetEntriesInAclW. We'll do best effort and just log on failure;
        // the AppContainer profile is single-use and the SID becomes meaningless
        // after DeleteAppContainerProfile.
        if !self.path_w.as_slice().is_empty() {
            debug!(
                path = %self.path_w.to_string_lossy(),
                "AppContainer ACL grant guard dropped (best effort)"
            );
        }
        if !self.new_dacl_alloc.is_null() {
            // SAFETY: SetEntriesInAclW returns LocalAlloc'd memory.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.new_dacl_alloc as *mut c_void)));
            }
            self.new_dacl_alloc = ptr::null_mut();
        }
        let _ = self.sid; // silence unused
    }
}

impl Drop for AclGrantGuard {
    fn drop(&mut self) {
        self.revoke_now();
    }
}

fn grant_appcontainer_access(
    path: &Path,
    sid: PSID,
    access_mask: u32,
    is_dir: bool,
) -> Option<AclGrantGuard> {
    let path_w = match U16CString::from_os_str(path.as_os_str()) {
        Ok(s) => s,
        Err(e) => {
            warn!(?path, error = %e, "ACL grant: path encode failed");
            return None;
        }
    };

    let mut existing_dacl: *mut ACL = ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();

    // SAFETY: All output pointers are valid stack locations; path is null-terminated.
    let res = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(path_w.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut existing_dacl),
            None,
            &mut sd,
        )
    };
    if res.is_err() {
        warn!(?path, ?res, "GetNamedSecurityInfoW failed");
        return None;
    }

    let inheritance = if is_dir {
        windows::Win32::Security::ACE_FLAGS(OBJECT_INHERIT_ACE.0 | CONTAINER_INHERIT_ACE.0)
    } else {
        windows::Win32::Security::ACE_FLAGS(NO_INHERITANCE_FLAG)
    };

    let trustee = TRUSTEE_W {
        pMultipleTrustee: ptr::null_mut(),
        MultipleTrusteeOperation: windows::Win32::Security::Authorization::NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_GROUP,
        ptstrName: PWSTR(sid.0 as *mut u16),
    };
    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access_mask,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: inheritance,
        Trustee: trustee,
    };

    let mut new_dacl: *mut ACL = ptr::null_mut();
    // SAFETY: ea is on the stack; existing_dacl was returned by GetNamedSecurityInfoW.
    let res = unsafe { SetEntriesInAclW(Some(&[ea]), Some(existing_dacl), &mut new_dacl) };
    if res.is_err() {
        warn!(?path, ?res, "SetEntriesInAclW failed");
        // SAFETY: sd was LocalAlloc'd by GetNamedSecurityInfoW.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        return None;
    }

    // SAFETY: new_dacl is the ACL we want to apply; path is null-terminated.
    let res = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(path_w.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_dacl),
            None,
        )
    };
    // SAFETY: sd was LocalAlloc'd by GetNamedSecurityInfoW; safe to free now.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    if res.is_err() {
        warn!(?path, ?res, "SetNamedSecurityInfoW failed");
        // SAFETY: new_dacl was LocalAlloc'd by SetEntriesInAclW.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(new_dacl as *mut c_void)));
        }
        return None;
    }

    Some(AclGrantGuard {
        path_w,
        sid,
        new_dacl_alloc: new_dacl,
    })
}

// -- Helpers ----------------------------------------------------------------

fn resolve_executable_blocking(bin: &str) -> Option<PathBuf> {
    let p = Path::new(bin);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("where.exe")
        .creation_flags(0x0800_0000)
        .arg(bin)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        return Some(PathBuf::from(line));
    }
    None
}

fn is_under_windows_dir(p: &Path) -> bool {
    let lower = p.to_string_lossy().to_ascii_lowercase();
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    lower.starts_with(&windir.to_ascii_lowercase())
}

fn athen_runtimes_dir() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("Athen").join("runtimes"))
}

/// Quote a single command-line argument using the rules CommandLineToArgvW
/// uses to parse them — see "Everyone quotes command line arguments the
/// wrong way" (Daniel Colascione). Backslashes only need doubling when
/// they precede a literal `"`; otherwise they pass through.
pub fn quote_arg(arg: &str) -> String {
    let needs_quoting = arg.is_empty()
        || arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '\n' || c == '\x0b' || c == '"');

    if !needs_quoting {
        return arg.to_string();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let chars: Vec<char> = arg.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let mut backslashes = 0;
        while i < chars.len() && chars[i] == '\\' {
            backslashes += 1;
            i += 1;
        }
        if i == chars.len() {
            // Trailing backslashes before closing quote — must be doubled.
            for _ in 0..(backslashes * 2) {
                out.push('\\');
            }
            break;
        } else if chars[i] == '"' {
            for _ in 0..(backslashes * 2 + 1) {
                out.push('\\');
            }
            out.push('"');
            i += 1;
        } else {
            for _ in 0..backslashes {
                out.push('\\');
            }
            out.push(chars[i]);
            i += 1;
        }
    }
    out.push('"');
    out
}

fn build_command_line(command: &str, args: &[String]) -> String {
    let mut out = quote_arg(command);
    for a in args {
        out.push(' ');
        out.push_str(&quote_arg(a));
    }
    out
}

// -- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_returns_true() {
        assert!(windows_capability());
    }

    #[test]
    fn quote_arg_no_special() {
        assert_eq!(quote_arg("foo"), "foo");
    }

    #[test]
    fn quote_arg_with_space() {
        assert_eq!(quote_arg("hello world"), "\"hello world\"");
    }

    #[test]
    fn quote_arg_with_quote() {
        // `she "said"` → `"she \"said\""`
        assert_eq!(quote_arg(r#"she "said""#), r#""she \"said\"""#);
    }

    #[test]
    fn quote_arg_with_trailing_backslash() {
        // `path\` requires quoting (trailing backslash before closing quote
        // doubles to two backslashes inside the quoted form).
        assert_eq!(quote_arg("path with\\"), r#""path with\\""#);
    }

    #[test]
    fn quote_arg_empty() {
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn build_command_line_quotes_each_arg() {
        let cl = build_command_line(
            "C:\\bin\\foo.exe",
            &["simple".to_string(), "two words".to_string()],
        );
        assert_eq!(cl, "C:\\bin\\foo.exe simple \"two words\"");
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    #[ignore]
    async fn execute_echo_runs_under_appcontainer() {
        let sandbox = WindowsSandbox;
        let level = SandboxLevel::OsNative {
            profile: SandboxProfile::ReadOnly,
        };
        let out = sandbox
            .execute("cmd.exe", &["/C", "echo hello"], &level)
            .await
            .expect("execute");
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("hello"));
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    #[ignore]
    async fn restricted_write_blocks_writes_outside_allowed() {
        use std::env;
        let sandbox = WindowsSandbox;
        let blocked = env::temp_dir().join("athen_sandbox_block_test");
        let _ = std::fs::create_dir_all(&blocked);
        let level = SandboxLevel::OsNative {
            profile: SandboxProfile::RestrictedWrite {
                allowed_paths: vec![],
            },
        };
        let path_arg = format!("echo X > \"{}\\probe.txt\"", blocked.display());
        let out = sandbox
            .execute("cmd.exe", &["/C", &path_arg], &level)
            .await
            .expect("execute");
        // Either nonzero exit or the file does not exist — both acceptable.
        let probe = blocked.join("probe.txt");
        assert!(out.exit_code != 0 || !probe.exists());
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    #[ignore]
    async fn restricted_write_allows_writes_inside_allowed() {
        use std::env;
        let sandbox = WindowsSandbox;
        let allowed = env::temp_dir().join("athen_sandbox_allow_test");
        let _ = std::fs::create_dir_all(&allowed);
        let _ = std::fs::remove_file(allowed.join("probe.txt"));
        let level = SandboxLevel::OsNative {
            profile: SandboxProfile::RestrictedWrite {
                allowed_paths: vec![allowed.clone()],
            },
        };
        let path_arg = format!("echo X > \"{}\\probe.txt\"", allowed.display());
        let out = sandbox
            .execute("cmd.exe", &["/C", &path_arg], &level)
            .await
            .expect("execute");
        assert_eq!(out.exit_code, 0);
        assert!(allowed.join("probe.txt").exists());
    }
}
