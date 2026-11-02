//! Who is on the other end of a Unix socket (PROTOCOL.md 5.1).
//!
//! On macOS the kernel gives the peer's audit token (`LOCAL_PEERTOKEN`); the
//! code signature of that *running* process decides the trust level. A
//! process id is never used for trust: ids get reused.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// An application bundle signed by Askantis, with the hardened runtime.
    Askantis,
    /// Any other application bundle with a Developer ID or App Store signature and the hardened runtime.
    Signed,
    /// Everything else: unsigned, ad-hoc, Apple's own command-line tools, command-line tools, injectable apps.
    Unsigned,
}

#[derive(Debug, Clone, Serialize)]
pub struct Identity {
    pub trust: Trust,
    /// Bundle (or code) identifier from the signature.
    pub identifier: Option<String>,
    pub team: Option<String>,
    /// Path of the main executable.
    pub path: Option<String>,
    /// Hex cdhash of the running code.
    pub cdhash: Option<String>,
    /// Interpreters (python, node, …) can't connect over the socket.
    pub interpreter: bool,
    /// Process id, for display only.
    pub pid: Option<i32>,
    /// Why the caller is `unsigned`, for logs and the approval prompt.
    pub reason: Option<String>,
}

impl Identity {
    /// The key approvals are remembered by (PROTOCOL.md 5.2): team and bundle
    /// identifier for signed apps, the cdhash for anything else.
    pub fn approval_key(&self) -> String {
        match (self.trust, &self.team, &self.identifier) {
            (Trust::Askantis | Trust::Signed, Some(team), Some(id)) => format!("team:{team}/id:{id}"),
            _ => format!("unsigned:{}", self.cdhash.as_deref().unwrap_or("unknown")),
        }
    }

    /// Whether this caller is Local Router's own menu-bar app.
    pub fn is_local_router(&self, bundle_id: &str, team: &str) -> bool {
        self.trust == Trust::Askantis
            && self.identifier.as_deref() == Some(bundle_id)
            && self.team.as_deref() == Some(team)
    }

    pub fn unknown(reason: &str) -> Self {
        Identity {
            trust: Trust::Unsigned,
            identifier: None,
            team: None,
            path: None,
            cdhash: None,
            interpreter: false,
            pid: None,
            reason: Some(reason.into()),
        }
    }
}

const INTERPRETERS: &[&str] = &[
    "python", "python3", "node", "deno", "bun", "ruby", "perl", "php", "osascript", "sh", "bash", "zsh",
    "fish", "dash", "tclsh", "lua", "java",
];

/// `python3.13` and `Python` count as `python`.
pub fn is_interpreter(executable: &str) -> bool {
    let name = executable.rsplit('/').next().unwrap_or(executable).to_ascii_lowercase();
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    INTERPRETERS.contains(&base)
}

/// Entitlements that let another program run code inside an app.
pub const INJECTION_ENTITLEMENTS: &[&str] = &[
    "com.apple.security.get-task-allow",
    "com.apple.security.cs.disable-library-validation",
    "com.apple.security.cs.allow-dyld-environment-variables",
];

/// The facts the trust decision is made from, separated from the platform
/// calls so the decision can be tested.
#[derive(Debug, Clone, Default)]
pub struct SigningFacts {
    pub valid_apple_anchor: bool,
    pub apple_platform: bool,
    pub askantis_team: bool,
    pub hardened_runtime: bool,
    pub adhoc: bool,
    pub app_bundle: bool,
    pub injection_entitlements: Vec<String>,
}

pub fn classify(f: &SigningFacts) -> (Trust, Option<String>) {
    let unsigned = |why: &str| (Trust::Unsigned, Some(why.to_string()));
    if f.adhoc || !f.valid_apple_anchor {
        return unsigned("no valid Developer ID or App Store signature");
    }
    if f.apple_platform {
        return unsigned("Apple's own software, which any program can run");
    }
    if !f.app_bundle {
        return unsigned("not an application bundle");
    }
    if !f.hardened_runtime {
        return unsigned("built without the hardened runtime");
    }
    if let Some(e) = f.injection_entitlements.first() {
        return (Trust::Unsigned, Some(format!("allows code injection ({e})")));
    }
    (if f.askantis_team { Trust::Askantis } else { Trust::Signed }, None)
}

#[cfg(target_os = "macos")]
pub use macos::{identify_fd, identify_pid, identify_self};

#[cfg(not(target_os = "macos"))]
pub fn identify_fd(_fd: i32) -> Identity {
    Identity::unknown("caller identity isn't implemented on this platform yet")
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::os::fd::RawFd;

    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::data::CFData;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use core_foundation::url::CFURL;
    use security_framework::os::macos::code_signing::{Flags, GuestAttributes, SecCode, SecRequirement};

    use super::*;

    const K_SEC_CS_SIGNING_INFORMATION: u32 = 1 << 1;
    const CS_ADHOC: u32 = 0x0000_0002;
    const CS_RUNTIME: u32 = 0x0001_0000;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecCodeCopySigningInformation(code: *const c_void, flags: u32, info: *mut CFDictionaryRef) -> i32;
        static kSecCodeInfoIdentifier: CFStringRef;
        static kSecCodeInfoTeamIdentifier: CFStringRef;
        static kSecCodeInfoFlags: CFStringRef;
        static kSecCodeInfoEntitlementsDict: CFStringRef;
        static kSecCodeInfoMainExecutable: CFStringRef;
        static kSecCodeInfoUnique: CFStringRef;
    }

    /// The peer of a connected Unix socket.
    pub fn identify_fd(fd: RawFd) -> Identity {
        let pid = peer_pid(fd);
        let Some(token) = peer_audit_token(fd) else {
            return Identity { pid, ..Identity::unknown("the system didn't say who is calling") };
        };
        let mut attrs = GuestAttributes::new();
        let data = CFData::from_buffer(&token);
        attrs.set_audit_token(data.as_concrete_TypeRef());
        match SecCode::copy_guest_with_attribues(None, &attrs, Flags::NONE) {
            Ok(code) => Identity { pid, ..identify_code(&code) },
            Err(_) => Identity { pid, ..Identity::unknown("the caller's code couldn't be inspected") },
        }
    }

    /// A process by id. For diagnostics only: process ids get reused, so this
    /// must never decide trust (PROTOCOL.md 16).
    pub fn identify_pid(pid: i32) -> Identity {
        let mut attrs = GuestAttributes::new();
        attrs.set_pid(pid);
        match SecCode::copy_guest_with_attribues(None, &attrs, Flags::NONE) {
            Ok(code) => Identity { pid: Some(pid), ..identify_code(&code) },
            Err(_) => Identity { pid: Some(pid), ..Identity::unknown("no such process, or not inspectable") },
        }
    }

    /// This process, for tests and logs.
    pub fn identify_self() -> Identity {
        match SecCode::for_self(Flags::NONE) {
            Ok(code) => identify_code(&code),
            Err(_) => Identity::unknown("own code couldn't be inspected"),
        }
    }

    fn requirement_ok(code: &SecCode, req: &str) -> bool {
        req.parse::<SecRequirement>().map(|r| code.check_validity(Flags::NONE, &r).is_ok()).unwrap_or(false)
    }

    fn identify_code(code: &SecCode) -> Identity {
        let info = signing_info(code);
        let get = |key: CFStringRef| -> Option<CFType> {
            let info = info.as_ref()?;
            let key = unsafe { CFString::wrap_under_get_rule(key) };
            info.find(&key).map(|v| v.clone())
        };
        let identifier = get(unsafe { kSecCodeInfoIdentifier })
            .and_then(|v| v.downcast::<CFString>())
            .map(|s| s.to_string());
        let team = get(unsafe { kSecCodeInfoTeamIdentifier })
            .and_then(|v| v.downcast::<CFString>())
            .map(|s| s.to_string());
        let flags = get(unsafe { kSecCodeInfoFlags })
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
            .unwrap_or(0) as u32;
        let path = get(unsafe { kSecCodeInfoMainExecutable })
            .and_then(|v| v.downcast::<CFURL>())
            .and_then(|u| u.to_path())
            .map(|p| p.display().to_string());
        let cdhash = get(unsafe { kSecCodeInfoUnique })
            .and_then(|v| v.downcast::<CFData>())
            .map(|d| d.bytes().iter().map(|b| format!("{b:02x}")).collect::<String>());
        let mut injection = Vec::new();
        if let Some(ents) = get(unsafe { kSecCodeInfoEntitlementsDict })
            .and_then(|v| v.downcast::<CFDictionary>())
        {
            let ents: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_get_rule(ents.as_concrete_TypeRef()) };
            for e in INJECTION_ENTITLEMENTS {
                let on = ents
                    .find(CFString::new(e))
                    .and_then(|v| v.downcast::<CFBoolean>())
                    .map(bool::from)
                    .unwrap_or(false);
                if on {
                    injection.push(e.to_string());
                }
            }
        }
        let interpreter = path.as_deref().map(is_interpreter).unwrap_or(false);
        let facts = SigningFacts {
            valid_apple_anchor: requirement_ok(code, "anchor apple generic"),
            apple_platform: requirement_ok(code, "anchor apple"),
            askantis_team: requirement_ok(
                code,
                &format!("anchor apple generic and certificate leaf[subject.OU] = \"{}\"", ASKANTIS_TEAM),
            ),
            hardened_runtime: flags & CS_RUNTIME != 0,
            adhoc: flags & CS_ADHOC != 0,
            app_bundle: path.as_deref().is_some_and(|p| p.contains(".app/Contents/MacOS/")),
            injection_entitlements: injection,
        };
        let (trust, reason) = classify(&facts);
        Identity { trust, identifier, team, path, cdhash, interpreter, pid: None, reason }
    }

    const ASKANTIS_TEAM: &str = "PB9T4S67XB";

    fn signing_info(code: &SecCode) -> Option<CFDictionary<CFString, CFType>> {
        let mut dict: CFDictionaryRef = std::ptr::null();
        let status = unsafe {
            SecCodeCopySigningInformation(code.as_concrete_TypeRef() as *const c_void, K_SEC_CS_SIGNING_INFORMATION, &mut dict)
        };
        (status == 0 && !dict.is_null()).then(|| unsafe { CFDictionary::wrap_under_create_rule(dict) })
    }

    fn peer_audit_token(fd: RawFd) -> Option<[u8; 32]> {
        let mut token = [0u8; 32];
        let mut len = token.len() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERTOKEN, token.as_mut_ptr().cast(), &mut len)
        };
        (rc == 0 && len as usize == token.len()).then_some(token)
    }

    fn peer_pid(fd: RawFd) -> Option<i32> {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&mut pid as *mut libc::pid_t).cast(), &mut len)
        };
        (rc == 0).then_some(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_app() -> SigningFacts {
        SigningFacts {
            valid_apple_anchor: true,
            apple_platform: false,
            askantis_team: false,
            hardened_runtime: true,
            adhoc: false,
            app_bundle: true,
            injection_entitlements: vec![],
        }
    }

    #[test]
    fn trust_levels_follow_the_protocol() {
        assert_eq!(classify(&signed_app()).0, Trust::Signed);
        assert_eq!(classify(&SigningFacts { askantis_team: true, ..signed_app() }).0, Trust::Askantis);
        for (facts, why) in [
            (SigningFacts { adhoc: true, ..signed_app() }, "no valid"),
            (SigningFacts { valid_apple_anchor: false, ..signed_app() }, "no valid"),
            (SigningFacts { apple_platform: true, ..signed_app() }, "Apple's own software"),
            (SigningFacts { app_bundle: false, ..signed_app() }, "not an application"),
            (SigningFacts { hardened_runtime: false, ..signed_app() }, "hardened runtime"),
            (SigningFacts { injection_entitlements: vec!["com.apple.security.get-task-allow".into()], askantis_team: true, ..signed_app() }, "injection"),
        ] {
            let (trust, reason) = classify(&facts);
            assert_eq!(trust, Trust::Unsigned, "{facts:?}");
            assert!(reason.unwrap().contains(why));
        }
    }

    #[test]
    fn spots_interpreters() {
        for p in ["/usr/bin/python3", "/opt/homebrew/bin/python3.13", "/usr/local/bin/node", "/bin/zsh", "/usr/bin/osascript"] {
            assert!(is_interpreter(p), "{p}");
        }
        for p in ["/Applications/Scribely.app/Contents/MacOS/Scribely", "/usr/bin/curl", "/usr/local/bin/pythonista-tool"] {
            assert!(!is_interpreter(p), "{p}");
        }
    }

    #[test]
    fn approvals_are_keyed_by_identity_not_by_claim() {
        let signed = Identity { trust: Trust::Signed, identifier: Some("com.x.app".into()), team: Some("ABCDE12345".into()), cdhash: Some("aa".into()), ..Identity::unknown("") };
        assert_eq!(signed.approval_key(), "team:ABCDE12345/id:com.x.app");
        let unsigned = Identity { cdhash: Some("deadbeef".into()), identifier: Some("com.x.app".into()), ..Identity::unknown("") };
        assert_eq!(unsigned.approval_key(), "unsigned:deadbeef");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_test_binary_is_unsigned() {
        let me = identify_self();
        assert_eq!(me.trust, Trust::Unsigned);
        assert!(me.cdhash.is_some(), "cdhash of the running test binary: {me:?}");
    }
}
