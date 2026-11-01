//! The Anthropic API key for Smart cleanup. It lives in Windows Credential
//! Manager, encrypted for this Windows user, never in settings.json. The
//! ANTHROPIC_API_KEY environment variable wins when it's set, for
//! development and the cleanup bench.

use windows::core::{HSTRING, PWSTR};
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

const TARGET: &str = "Offhand/anthropic-api-key";
/// Where the key was saved when the app was called Sotto.
const OLD_TARGET: &str = "Sotto/anthropic-api-key";

fn from_env() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY").ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

pub fn get() -> Option<String> {
    from_env().or_else(|| read(TARGET))
}

fn read(target: &str) -> Option<String> {
    unsafe {
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        CredReadW(&HSTRING::from(target), CRED_TYPE_GENERIC, None, &mut cred).ok()?;
        let c = &*cred;
        let blob = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
        let key = String::from_utf8(blob.to_vec()).ok();
        CredFree(cred as *const _);
        key.filter(|k| !k.is_empty())
    }
}

/// Moves a key saved under the old name to the new one. The old entry goes
/// only once the new one is safely written.
pub fn adopt_old_name() {
    if read(TARGET).is_some() {
        return;
    }
    if let Some(key) = read(OLD_TARGET) {
        if set(&key).is_ok() {
            let _ = unsafe { CredDeleteW(&HSTRING::from(OLD_TARGET), CRED_TYPE_GENERIC, None) };
        }
    }
}

pub fn is_set() -> bool {
    get().is_some()
}

pub fn set(key: &str) -> Result<(), String> {
    let mut blob = key.trim().as_bytes().to_vec();
    if blob.is_empty() {
        return Err("the key is empty".into());
    }
    let mut target: Vec<u16> = TARGET.encode_utf16().chain([0]).collect();
    let mut user: Vec<u16> = "Offhand".encode_utf16().chain([0]).collect();
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        UserName: PWSTR(user.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    unsafe { CredWriteW(&cred, 0) }.map_err(|e| e.message())
}

pub fn delete() {
    let _ = unsafe { CredDeleteW(&HSTRING::from(TARGET), CRED_TYPE_GENERIC, None) };
}
