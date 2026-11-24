use std::ffi::c_void;
use std::io;
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_MORE_FILES,
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_NONE, FindClose, FindFirstFileW, FindNextFileW, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, WIN32_FIND_DATAW, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
};

use crate::paths::pipe_prefix;
use crate::security::PipeSecurityDescriptor;

pub fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pipe_name(module: &str, user_sid: &str) -> String {
    format!("{}{module}.{user_sid}", pipe_prefix())
}

pub fn pipe_path(module: &str, user_sid: &str) -> String {
    format!(r"\\.\pipe\{}", pipe_name(module, user_sid))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredPeer {
    pub module: String,
    pub path: String,
}

/// Scans `\\.\pipe\*` for active `<prefix><module>.<user_sid>` pipes, excluding `self_module`.
pub fn discover_peers(self_module: &str, user_sid: &str) -> io::Result<Vec<DiscoveredPeer>> {
    let pattern = to_wide_null(r"\\.\pipe\*");
    let mut data = WIN32_FIND_DATAW::default();
    let find_handle = match unsafe { FindFirstFileW(PCWSTR(pattern.as_ptr()), &mut data) } {
        Ok(h) => h,
        Err(e) => return Err(io::Error::from_raw_os_error(e.code().0)),
    };

    struct FindGuard(HANDLE);
    impl Drop for FindGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = FindClose(self.0);
            }
        }
    }
    let _guard = FindGuard(find_handle);

    let prefix = pipe_prefix();
    let suffix = format!(".{user_sid}");
    let mut peers = Vec::new();
    loop {
        let len = data
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(data.cFileName.len());
        let name = String::from_utf16_lossy(&data.cFileName[..len]);
        if let Some(rest) = name.strip_prefix(&prefix) {
            if let Some(module) = rest.strip_suffix(&suffix) {
                if !module.is_empty() && module != self_module {
                    peers.push(DiscoveredPeer {
                        module: module.to_string(),
                        path: format!(r"\\.\pipe\{name}"),
                    });
                }
            }
        }

        if let Err(e) = unsafe { FindNextFileW(find_handle, &mut data) } {
            let code = e.code().0 as u32 & 0xFFFF;
            if code == ERROR_NO_MORE_FILES.0 {
                break;
            }
            return Err(io::Error::from_raw_os_error(e.code().0));
        }
    }

    Ok(peers)
}

pub struct EventHandle(pub HANDLE);
unsafe impl Send for EventHandle {}
unsafe impl Sync for EventHandle {}

impl EventHandle {
    pub fn new_manual(initial_state: bool) -> io::Result<Self> {
        let h = unsafe { CreateEventW(None, true, initial_state, None) }
            .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
        Ok(Self(h))
    }

    pub fn set(&self) {
        unsafe {
            let _ = SetEvent(self.0);
        }
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

pub fn create_server_pipe(path: &str, sd: &PipeSecurityDescriptor) -> io::Result<HANDLE> {
    let wide = to_wide_null(path);
    let mut sa = sd.as_security_attributes();
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            Some(&mut sa as *mut _),
        )
    };
    if handle.is_invalid() {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

/// Waits for a client connection on `pipe` (overlapped) or returns `Ok(false)` if `stop_event` fires.
pub fn connect_server_pipe(pipe: HANDLE, stop_event: HANDLE) -> io::Result<bool> {
    let io_event = EventHandle::new_manual(false)?;
    let mut ov = OVERLAPPED {
        hEvent: io_event.0,
        ..Default::default()
    };

    let res = unsafe { ConnectNamedPipe(pipe, Some(&mut ov)) };
    match res {
        Ok(()) => return Ok(true),
        Err(_) => {
            let err = io::Error::last_os_error();
            let raw = err.raw_os_error().unwrap_or(0) as u32;
            if raw == ERROR_PIPE_CONNECTED.0 {
                return Ok(true);
            }
            if raw != ERROR_IO_PENDING.0 {
                return Err(err);
            }
        }
    }

    let handles = [stop_event, io_event.0];
    let wait = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
    if wait == WAIT_OBJECT_0 {
        unsafe {
            let _ = CancelIoEx(pipe, Some(&ov));
        }
        return Ok(false);
    }
    if wait.0 == WAIT_OBJECT_0.0 + 1 {
        let mut transferred = 0u32;
        unsafe {
            GetOverlappedResult(pipe, &ov, &mut transferred, true)
                .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
        }
        return Ok(true);
    }

    Err(io::Error::last_os_error())
}

pub fn connect_client_pipe(path: &str) -> io::Result<HANDLE> {
    let wide = to_wide_null(path);
    for attempt in 0..5 {
        let res = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                None,
            )
        };
        match res {
            Ok(h) => return Ok(h),
            Err(e) => {
                let err = io::Error::from_raw_os_error(e.code().0);
                let code = err.raw_os_error().unwrap_or(0) as u32 & 0xFFFF;
                if code == ERROR_PIPE_BUSY.0 && attempt < 4 {
                    unsafe {
                        let _ = WaitNamedPipeW(PCWSTR(wide.as_ptr()), 250);
                    }
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "pipe busy"))
}

/// Thread-safe full-duplex overlapped named pipe wrapper.
pub struct OverlappedPipe {
    handle: HANDLE,
    write_lock: Mutex<()>,
    write_event: EventHandle,
    read_event: EventHandle,
}

impl std::fmt::Debug for OverlappedPipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlappedPipe")
            .field("handle", &self.handle.0)
            .finish()
    }
}

unsafe impl Send for OverlappedPipe {}
unsafe impl Sync for OverlappedPipe {}

impl OverlappedPipe {
    pub fn new(handle: HANDLE) -> io::Result<Self> {
        Ok(Self {
            handle,
            write_lock: Mutex::new(()),
            write_event: EventHandle::new_manual(false)?,
            read_event: EventHandle::new_manual(false)?,
        })
    }

    pub fn read_chunk(&self, buf: &mut [u8], stop_event: HANDLE) -> io::Result<usize> {
        unsafe {
            let _ = ResetEvent(self.read_event.0);
        }
        let mut ov = OVERLAPPED {
            hEvent: self.read_event.0,
            ..Default::default()
        };
        let mut bytes_read = 0u32;
        let res = unsafe { ReadFile(self.handle, Some(buf), Some(&mut bytes_read), Some(&mut ov)) };
        match res {
            Ok(()) => return Ok(bytes_read as usize),
            Err(_) => {
                let err = io::Error::last_os_error();
                let code = err.raw_os_error().unwrap_or(0) as u32 & 0xFFFF;
                if code == ERROR_BROKEN_PIPE.0 || code == ERROR_PIPE_NOT_CONNECTED.0 {
                    return Ok(0);
                }
                if code == ERROR_MORE_DATA.0 {
                    return Ok(bytes_read as usize);
                }
                if code != ERROR_IO_PENDING.0 {
                    return Err(err);
                }
            }
        }

        let handles = [stop_event, self.read_event.0];
        let wait = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        if wait == WAIT_OBJECT_0 {
            unsafe {
                let _ = CancelIoEx(self.handle, Some(&ov));
            }
            return Ok(0);
        }
        if wait.0 == WAIT_OBJECT_0.0 + 1 {
            let mut transferred = 0u32;
            match unsafe { GetOverlappedResult(self.handle, &ov, &mut transferred, true) } {
                Ok(()) => return Ok(transferred as usize),
                Err(e) => {
                    let code = e.code().0 as u32 & 0xFFFF;
                    if code == ERROR_BROKEN_PIPE.0 || code == ERROR_PIPE_NOT_CONNECTED.0 {
                        return Ok(0);
                    }
                    if code == ERROR_MORE_DATA.0 {
                        return Ok(transferred as usize);
                    }
                    return Err(io::Error::from_raw_os_error(e.code().0));
                }
            }
        }

        Err(io::Error::last_os_error())
    }

    pub fn write_all(&self, data: &[u8]) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        let mut offset = 0usize;
        while offset < data.len() {
            unsafe {
                let _ = ResetEvent(self.write_event.0);
            }
            let mut ov = OVERLAPPED {
                hEvent: self.write_event.0,
                ..Default::default()
            };
            let slice = &data[offset..];
            let mut written = 0u32;
            let res = unsafe {
                WriteFile(
                    self.handle,
                    Some(slice),
                    Some(&mut written),
                    Some(&mut ov),
                )
            };
            let step = match res {
                Ok(()) => written as usize,
                Err(_) => {
                    let err = io::Error::last_os_error();
                    let code = err.raw_os_error().unwrap_or(0) as u32 & 0xFFFF;
                    if code != ERROR_IO_PENDING.0 {
                        return Err(err);
                    }
                    let mut transferred = 0u32;
                    unsafe {
                        GetOverlappedResult(self.handle, &ov, &mut transferred, true)
                            .map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
                    }
                    transferred as usize
                }
            };
            if step == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "zero bytes written"));
            }
            offset += step;
        }
        Ok(())
    }

    pub fn close_handle(&self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CancelIoEx(self.handle, None);
            }
        }
    }
}

impl Drop for OverlappedPipe {
    fn drop(&mut self) {
        if !self.handle.is_invalid() && self.handle.0 != std::ptr::null_mut::<c_void>() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}
