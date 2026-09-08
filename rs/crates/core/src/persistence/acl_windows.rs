//! Owner-only DACLs on Windows — the `chmod 0600` of track H7 (docs/modules-fanout-plan.md
//! §2: "owner-only files on every OS").
//!
//! One security descriptor shape is used everywhere: a protected DACL with exactly one
//! ACE, `FILE_ALL_ACCESS` for the current user's SID and nothing for anybody else. The
//! named-pipe transport hands it to `CreateNamedPipeW` as `SECURITY_ATTRIBUTES`
//! ([`OwnerOnly::security_attributes`]); the install store applies it to `record.json`,
//! `control.json` and the key files with [`restrict_to_owner`]. `PROTECTED_DACL` stops
//! the parent directory's inherited "Users: read" entries from coming back.
//!
//! The descriptor is built by hand (`InitializeAcl` / `AddAccessAllowedAce`) rather than
//! from an SDDL string, because the SDDL converter lives behind the `Win32_Security_Authorization`
//! feature, which the frozen `Cargo.toml` does not enable. The SID decoding, the ACL size
//! arithmetic and the SDDL rendering are pure functions with tests that run on every OS;
//! only the Win32 calls are `#[cfg(windows)]`.
//!
//! Nothing here ever handles key bytes; the paths it touches are the only thing that
//! reaches a log line.

#![cfg_attr(not(windows), allow(dead_code))]

use std::fmt;

/// The access mask granted to the owner: `FILE_ALL_ACCESS`. Full control for the owner
/// is the Windows reading of `0600` — on Unix the owner can always `chmod` too.
pub const OWNER_ACCESS_MASK: u32 = 0x001F_01FF;

/// `SECURITY_DESCRIPTOR_REVISION` (the `windows` crate does not export the constant).
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

/// Size of the fixed `ACL` header.
const ACL_HEADER_LEN: usize = 8;

/// Size of `ACCESS_ALLOWED_ACE` minus the `SidStart` placeholder the SID overwrites.
const ACE_HEADER_LEN: usize = 8;

/// Longest SID Windows accepts: `SID_MAX_SUB_AUTHORITIES`.
const SID_MAX_SUB_AUTHORITIES: usize = 15;

/// Only SID revision Windows has ever shipped.
const SID_REVISION: u8 = 1;

/// A security identifier decoded from its binary (`SID` struct) form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sid {
    revision: u8,
    /// 48-bit identifier authority (`SID_IDENTIFIER_AUTHORITY`, big-endian on the wire).
    identifier_authority: u64,
    sub_authorities: Vec<u32>,
}

impl Sid {
    /// Decode a binary SID. `None` when the bytes are not a well-formed SID (wrong
    /// revision, more than 15 sub-authorities, or a length that does not match the count).
    pub fn parse(bytes: &[u8]) -> Option<Sid> {
        let len = sid_byte_len(bytes)?;
        if bytes.len() != len || bytes[0] != SID_REVISION {
            return None;
        }
        let count = bytes[1] as usize;
        let mut authority = 0u64;
        for b in &bytes[2..8] {
            authority = (authority << 8) | u64::from(*b);
        }
        let sub_authorities = bytes[8..]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect::<Vec<_>>();
        debug_assert_eq!(sub_authorities.len(), count);
        Some(Sid {
            revision: bytes[0],
            identifier_authority: authority,
            sub_authorities,
        })
    }

    /// Number of bytes the binary form occupies.
    pub fn byte_len(&self) -> usize {
        8 + 4 * self.sub_authorities.len()
    }

    /// The binary (`SID` struct) form.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.byte_len());
        out.push(self.revision);
        out.push(self.sub_authorities.len() as u8);
        out.extend_from_slice(&self.identifier_authority.to_be_bytes()[2..]);
        for sub in &self.sub_authorities {
            out.extend_from_slice(&sub.to_le_bytes());
        }
        out
    }
}

impl fmt::Display for Sid {
    /// The `S-1-5-21-…` string form (what SDDL and `whoami /user` print).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "S-{}-{}", self.revision, self.identifier_authority)?;
        for sub in &self.sub_authorities {
            write!(f, "-{sub}")?;
        }
        Ok(())
    }
}

/// The length of the SID that starts at `bytes[0]`, from its sub-authority count —
/// what `GetLengthSid` computes. `None` if the header is too short or the count is
/// out of range.
pub fn sid_byte_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 8 {
        return None;
    }
    let count = bytes[1] as usize;
    if count > SID_MAX_SUB_AUTHORITIES {
        return None;
    }
    Some(8 + 4 * count)
}

/// Bytes needed for an ACL holding one `ACCESS_ALLOWED_ACE` for a SID of `sid_len`
/// bytes, rounded up to the DWORD boundary `InitializeAcl` requires.
pub fn single_ace_acl_size(sid_len: usize) -> usize {
    (ACL_HEADER_LEN + ACE_HEADER_LEN + sid_len).div_ceil(4) * 4
}

/// The same policy as an SDDL string: protected DACL, one allow-all ACE for `owner`.
/// Diagnostic only — the descriptor itself is built binary; see the module docs.
pub fn owner_only_sddl(owner: &Sid) -> String {
    format!("D:P(A;;FA;;;{owner})")
}

#[cfg(windows)]
pub use win::{current_user_sid, restrict_to_owner, OwnerOnly};

#[cfg(windows)]
mod win {
    use super::{single_ace_acl_size, Sid, OWNER_ACCESS_MASK, SECURITY_DESCRIPTOR_REVISION};
    use std::ffi::c_void;
    use std::io;
    use std::path::Path;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl,
        InitializeSecurityDescriptor, SetFileSecurityW, SetSecurityDescriptorDacl, TokenUser, ACL,
        ACL_REVISION, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The binary SID of the user this process runs as (from the primary token).
    #[allow(unsafe_code)] // token query; SAFETY notes inline
    pub fn current_user_sid() -> io::Result<Vec<u8>> {
        let mut token = HANDLE::default();
        // SAFETY: GetCurrentProcess is a pseudo-handle that needs no closing; `token`
        // is a valid out-pointer and is closed below on every path.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }?;
        let result = token_user_sid(token);
        // SAFETY: the handle was opened above and is not used after this.
        let _ = unsafe { CloseHandle(token) };
        result
    }

    #[allow(unsafe_code)]
    fn token_user_sid(token: HANDLE) -> io::Result<Vec<u8>> {
        let mut needed = 0u32;
        // SAFETY: a null buffer with length 0 is the documented size query; the call
        // fails with ERROR_INSUFFICIENT_BUFFER and fills `needed`.
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
        if needed == 0 {
            return Err(io::Error::other(
                "GetTokenInformation reported no TOKEN_USER",
            ));
        }
        // TOKEN_USER is pointer-aligned; a u64 vector keeps the buffer aligned.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buf` is `needed` bytes (rounded up) of writable, suitably aligned
        // memory and `needed` is what the kernel asked for.
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast::<c_void>()),
                needed,
                &mut needed,
            )
        }?;
        // SAFETY: on success the buffer starts with a TOKEN_USER whose Sid points into
        // the same buffer; GetLengthSid reads only the SID header.
        let (sid_ptr, len) = unsafe {
            let user = buf.as_ptr().cast::<TOKEN_USER>();
            let sid = (*user).User.Sid;
            (sid, GetLengthSid(sid) as usize)
        };
        if sid_ptr.0.is_null() || len < 8 {
            return Err(io::Error::other("token carries no usable user SID"));
        }
        // SAFETY: `sid_ptr` points at `len` readable bytes inside `buf`, which outlives
        // the copy.
        let bytes = unsafe { std::slice::from_raw_parts(sid_ptr.0.cast::<u8>(), len) };
        if Sid::parse(bytes).is_none() {
            return Err(io::Error::other("token user SID is malformed"));
        }
        Ok(bytes.to_vec())
    }

    /// An owner-only security descriptor: protected DACL, one ACE granting
    /// [`OWNER_ACCESS_MASK`] to the current user. Keep it alive for as long as any
    /// `SECURITY_ATTRIBUTES` or `PSECURITY_DESCRIPTOR` taken from it is in use — they
    /// point into its buffers.
    pub struct OwnerOnly {
        // `descriptor.Dacl` points into `acl`, whose contents embed a copy of `sid`.
        // Both are heap buffers, so moving the struct moves no pointee.
        descriptor: Box<SECURITY_DESCRIPTOR>,
        acl: Vec<u64>,
        _sid: Vec<u8>,
    }

    // SAFETY: the raw pointers inside point only at this struct's own heap buffers,
    // which are never mutated after construction.
    #[allow(unsafe_code)]
    unsafe impl Send for OwnerOnly {}
    #[allow(unsafe_code)]
    unsafe impl Sync for OwnerOnly {}

    impl OwnerOnly {
        /// Build the descriptor for the current user.
        pub fn for_current_user() -> io::Result<OwnerOnly> {
            Self::for_sid(current_user_sid()?)
        }

        /// Build the descriptor for an explicit binary SID (tests use this to keep the
        /// token query out of the way).
        #[allow(unsafe_code)] // ACL / descriptor construction; SAFETY notes inline
        pub fn for_sid(sid: Vec<u8>) -> io::Result<OwnerOnly> {
            let mut sid = sid;
            if Sid::parse(&sid).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a well-formed SID",
                ));
            }
            let acl_len = single_ace_acl_size(sid.len());
            let mut acl = vec![0u64; acl_len.div_ceil(8)];
            let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
            // SAFETY: `acl` is `acl_len` writable bytes (rounded up), DWORD-aligned;
            // `sid` was validated above and outlives the copy AddAccessAllowedAce makes.
            unsafe {
                InitializeAcl(acl_ptr, acl_len as u32, ACL_REVISION)?;
                AddAccessAllowedAce(
                    acl_ptr,
                    ACL_REVISION,
                    OWNER_ACCESS_MASK,
                    PSID(sid.as_mut_ptr().cast::<c_void>()),
                )?;
            }
            let mut descriptor = Box::new(SECURITY_DESCRIPTOR::default());
            let psd = PSECURITY_DESCRIPTOR((&mut *descriptor as *mut SECURITY_DESCRIPTOR).cast());
            // SAFETY: `descriptor` is a boxed, writable SECURITY_DESCRIPTOR; the ACL it is
            // pointed at lives in `acl`, which the returned struct owns.
            unsafe {
                InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION)?;
                SetSecurityDescriptorDacl(psd, true, Some(acl_ptr.cast_const()), false)?;
            }
            Ok(OwnerOnly {
                descriptor,
                acl,
                _sid: sid,
            })
        }

        /// The descriptor pointer for `Set*Security` calls. Valid while `self` lives.
        pub fn descriptor(&self) -> PSECURITY_DESCRIPTOR {
            PSECURITY_DESCRIPTOR(
                (&*self.descriptor as *const SECURITY_DESCRIPTOR)
                    .cast_mut()
                    .cast(),
            )
        }

        /// `SECURITY_ATTRIBUTES` for a `Create*` call; the handle is never inheritable.
        /// Valid while `self` lives.
        pub fn security_attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.descriptor().0,
                bInheritHandle: false.into(),
            }
        }

        /// Number of ACEs in the DACL — always one; exposed for tests.
        pub fn ace_count(&self) -> u16 {
            // SAFETY: `acl` holds an initialised ACL.
            #[allow(unsafe_code)]
            unsafe {
                (*self.acl.as_ptr().cast::<ACL>()).AceCount
            }
        }
    }

    /// Replace `path`'s DACL with the owner-only one, inheritance off. The Windows
    /// `chmod 0600`: apply right after creating `record.json`, `control.json` and key
    /// files (see the install store).
    #[allow(unsafe_code)] // SetFileSecurityW; SAFETY note inline
    pub fn restrict_to_owner(path: &Path) -> io::Result<()> {
        let owner = OwnerOnly::for_current_user()?;
        let wide = HSTRING::from(path.as_os_str());
        // SAFETY: `wide` is a NUL-terminated wide string and the descriptor outlives the
        // call.
        let ok = unsafe {
            SetFileSecurityW(
                &wide,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                owner.descriptor(),
            )
        };
        if ok.as_bool() {
            Ok(())
        } else {
            Err(windows::core::Error::from_thread().into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // S-1-5-21-1-2-3: revision 1, NT authority (5), four sub-authorities.
    fn sample() -> Vec<u8> {
        let mut v = vec![1u8, 4, 0, 0, 0, 0, 0, 5];
        for sub in [21u32, 1, 2, 3] {
            v.extend_from_slice(&sub.to_le_bytes());
        }
        v
    }

    #[test]
    fn parses_and_prints_a_sid() {
        let sid = Sid::parse(&sample()).unwrap();
        assert_eq!(sid.to_string(), "S-1-5-21-1-2-3");
        assert_eq!(sid.byte_len(), 24);
        assert_eq!(sid.to_bytes(), sample());
    }

    #[test]
    fn well_known_sids_round_trip() {
        // S-1-1-0 (Everyone) and S-1-5-18 (LocalSystem).
        let everyone = Sid::parse(&[1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]).unwrap();
        assert_eq!(everyone.to_string(), "S-1-1-0");
        let system = Sid::parse(&[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]).unwrap();
        assert_eq!(system.to_string(), "S-1-5-18");
        assert_eq!(Sid::parse(&system.to_bytes()), Some(system));
    }

    #[test]
    fn rejects_malformed_sids() {
        assert!(Sid::parse(&[]).is_none());
        assert!(Sid::parse(&[1, 0, 0, 0, 0, 0]).is_none(), "short header");
        let mut wrong_rev = sample();
        wrong_rev[0] = 2;
        assert!(Sid::parse(&wrong_rev).is_none());
        let mut truncated = sample();
        truncated.pop();
        assert!(Sid::parse(&truncated).is_none());
        let mut too_long = sample();
        too_long.extend_from_slice(&[0, 0, 0, 0]);
        assert!(
            Sid::parse(&too_long).is_none(),
            "length must match the count"
        );
        let mut too_many = sample();
        too_many[1] = 16;
        assert!(sid_byte_len(&too_many).is_none());
    }

    #[test]
    fn sid_length_follows_the_sub_authority_count() {
        assert_eq!(sid_byte_len(&sample()), Some(24));
        assert_eq!(sid_byte_len(&[1, 0, 0, 0, 0, 0, 0, 0]), Some(8));
        assert_eq!(sid_byte_len(&[1, 15, 0, 0, 0, 0, 0, 0]), Some(68));
        assert_eq!(sid_byte_len(&[1, 2, 0]), None);
    }

    #[test]
    fn acl_size_covers_one_ace_and_is_dword_aligned() {
        // 8 (ACL) + 8 (ACE minus SidStart) + 24 (SID) = 40.
        assert_eq!(single_ace_acl_size(24), 40);
        assert_eq!(single_ace_acl_size(12), 28);
        for len in 8..80 {
            let size = single_ace_acl_size(len);
            assert_eq!(size % 4, 0);
            assert!(size >= 16 + len);
        }
    }

    #[test]
    fn sddl_is_protected_full_access_for_the_owner_only() {
        let sid = Sid::parse(&sample()).unwrap();
        assert_eq!(owner_only_sddl(&sid), "D:P(A;;FA;;;S-1-5-21-1-2-3)");
    }

    #[test]
    fn owner_mask_is_file_all_access() {
        assert_eq!(OWNER_ACCESS_MASK, 0x1F01FF);
    }
}

#[cfg(all(test, windows))]
mod win_tests {
    use super::*;
    use std::io::{Read, Write};
    use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    #[test]
    fn owner_mask_matches_the_crate_constant() {
        assert_eq!(OWNER_ACCESS_MASK, FILE_ALL_ACCESS.0);
    }

    #[test]
    fn current_user_sid_is_an_nt_account() {
        let sid = Sid::parse(&current_user_sid().unwrap()).unwrap();
        assert!(sid.to_string().starts_with("S-1-5-"), "{sid}");
    }

    #[test]
    fn descriptor_holds_exactly_one_ace() {
        let owner = OwnerOnly::for_current_user().unwrap();
        assert_eq!(owner.ace_count(), 1);
        let attrs = owner.security_attributes();
        assert!(!attrs.lpSecurityDescriptor.is_null());
        assert!(!attrs.bInheritHandle.as_bool());
        assert!(OwnerOnly::for_sid(vec![9, 9, 9]).is_err());
    }

    #[test]
    fn restricted_file_stays_usable_by_its_owner() {
        let dir = std::env::temp_dir().join(format!(
            "avada-acl-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("record.json");
        std::fs::write(&path, b"{}").unwrap();
        restrict_to_owner(&path).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"ok\":1}").unwrap();
        let mut s = String::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert!(s.contains("ok"));
        assert!(restrict_to_owner(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
