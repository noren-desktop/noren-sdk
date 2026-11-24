use std::ffi::c_void;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenIntegrityLevel, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::error::SdkError;

/// Process integrity level and UAC elevation status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityInfo {
    pub level: String,
    pub elevated: bool,
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn open_process_query_token() -> Result<OwnedHandle, SdkError> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| SdkError::Security(format!("OpenProcessToken failed: {e}")))?;
    }
    Ok(OwnedHandle(token))
}

fn query_token_bytes(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u8>, SdkError> {
    let mut needed = 0u32;
    unsafe {
        let _ = GetTokenInformation(token, class, None, 0, &mut needed);
    }
    if needed == 0 {
        return Err(SdkError::Security(
            "GetTokenInformation returned size 0".to_string(),
        ));
    }
    let mut buf = vec![0u8; needed as usize];
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            needed,
            &mut needed,
        )
        .map_err(|e| SdkError::Security(format!("GetTokenInformation failed: {e}")))?;
    }
    Ok(buf)
}

/// Returns the string SID (`S-1-5-21-...`) of the current Windows user.
pub fn current_user_sid() -> Result<String, SdkError> {
    let token = open_process_query_token()?;
    let buf = query_token_bytes(token.0, TokenUser)?;
    let user = unsafe { &*(buf.as_ptr().cast::<TOKEN_USER>()) };
    let mut sid_str = PWSTR::null();
    unsafe {
        ConvertSidToStringSidW(user.User.Sid, &mut sid_str)
            .map_err(|e| SdkError::Security(format!("ConvertSidToStringSidW failed: {e}")))?;
        let text = sid_str
            .to_string()
            .map_err(|e| SdkError::Security(format!("SID UTF-16 decode failed: {e}")));
        let _ = LocalFree(Some(HLOCAL(sid_str.0.cast::<c_void>())));
        text
    }
}

/// Returns the current process's Windows integrity level and elevation flag.
pub fn current_integrity_info() -> IntegrityInfo {
    let Ok(token) = open_process_query_token() else {
        return IntegrityInfo {
            level: "Unknown".to_string(),
            elevated: false,
        };
    };

    let elevated = query_token_bytes(token.0, TokenElevation)
        .map(|buf| {
            let info = unsafe { &*(buf.as_ptr().cast::<TOKEN_ELEVATION>()) };
            info.TokenIsElevated != 0
        })
        .unwrap_or(false);

    let level = query_token_bytes(token.0, TokenIntegrityLevel)
        .map(|buf| unsafe {
            let label = &*(buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>());
            let sid = label.Label.Sid;
            let count_ptr = GetSidSubAuthorityCount(sid);
            if count_ptr.is_null() || *count_ptr == 0 {
                return "Unknown".to_string();
            }
            let rid = *GetSidSubAuthority(sid, (*count_ptr as u32) - 1);
            match rid {
                r if r < 0x1000 => format!("Untrusted(0x{r:X})"),
                r if r < 0x2000 => "Low".to_string(),
                r if r < 0x3000 => "Medium".to_string(),
                r if r < 0x4000 => "High".to_string(),
                _ => "System".to_string(),
            }
        })
        .unwrap_or_else(|_| "Unknown".to_string());

    IntegrityInfo { level, elevated }
}

/// Security descriptor allowing same-user access (+ SYSTEM/Administrators) and
/// admitting Medium-integrity peers to connect to High-integrity (elevated) servers.
pub struct PipeSecurityDescriptor {
    sd: PSECURITY_DESCRIPTOR,
    pub sddl: String,
}

unsafe impl Send for PipeSecurityDescriptor {}
unsafe impl Sync for PipeSecurityDescriptor {}

impl PipeSecurityDescriptor {
    pub fn same_user_medium_ml(user_sid: &str) -> Result<Self, SdkError> {
        let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user_sid})S:(ML;;NW;;;ME)");
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .map_err(|e| SdkError::Security(format!("SDDL conversion failed ({sddl}): {e}")))?;
        }
        Ok(Self { sd, sddl })
    }

    pub fn as_security_attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for PipeSecurityDescriptor {
    fn drop(&mut self) {
        if !self.sd.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.sd.0)));
            }
            self.sd = PSECURITY_DESCRIPTOR::default();
        }
    }
}
