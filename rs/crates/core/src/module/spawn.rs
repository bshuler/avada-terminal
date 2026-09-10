//! Trust at the process boundary: re-hash the binary against the install record, start
//! it with one socket end, and run the hello exchange.
//!
//! Order matters and is fixed here: hash **before** the process exists (a mismatch never
//! runs a byte of the module), handshake **before** any method is served (a manifest that
//! differs from the record closes the pipe).

use super::token::{hex, Token};
use super::transport::{self, Closer, FrameError, LineReader, LineWriter};
use avada_module_sdk::client::ENV_DATA_DIR;
use avada_module_sdk::contract::{self, HelloKind, HostHello, ModuleHello, CONTRACT_VERSION};
use avada_module_sdk::rights::InstallRecord;
use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use subtle::ConstantTimeEq;

/// Why a module did not start.
#[derive(Debug)]
pub enum SpawnError {
    /// The binary on disk is not the one that was installed. Nothing was run.
    HashMismatch {
        /// From the install record.
        expected: String,
        /// What the file hashes to now.
        actual: String,
    },
    /// The binary could not be read for hashing.
    Unreadable(io::Error),
    // ---- track G7 policy
    /// The notarization policy refused this artifact when it was installed. The host
    /// turns this into `ModuleStatus::Broken`; see `crate::policy`.
    Notarized {
        /// The refusal recorded at install time, verbatim.
        reason: String,
    },
    // ---- end track G7 policy
    /// The transport could not be set up (on Windows: not until track H7).
    Transport(io::Error),
    /// `Command::spawn` failed.
    Exec(io::Error),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::HashMismatch { expected, actual } => write!(
                f,
                "binary hash {} does not match the install record's {}",
                short(actual),
                short(expected)
            ),
            SpawnError::Unreadable(e) => write!(f, "cannot read the module binary: {e}"),
            // ---- track G7 policy
            SpawnError::Notarized { reason } => {
                write!(f, "refused by the notarization policy: {reason}")
            }
            // ---- end track G7 policy
            SpawnError::Transport(e) => write!(f, "transport: {e}"),
            SpawnError::Exec(e) => write!(f, "cannot start the module: {e}"),
        }
    }
}
impl std::error::Error for SpawnError {}

fn short(hex: &str) -> &str {
    hex.get(..12).unwrap_or(hex)
}

/// SHA-256 of a file, lowercase hex.
pub fn sha256_hex(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(hex(&hasher.finalize()))
}

/// Re-hash `binary` and compare (constant time) to the record's `artifact_sha256`,
/// and refuse outright if the notarization policy refused it at install time.
///
/// The hash check is unchanged and still the primary gate. The recorded verdict is an
/// addition that can only ever *stop* a spawn: a missing or unreadable
/// `<binary>.notarization.json` leaves the pre-G7 behaviour exactly as it was, so
/// deleting the sidecar cannot turn a refusal into a run. It is not in the MAC-signed
/// install record because that record's shape is frozen SDK; the limitation is written
/// down in `docs/notarization.md` rather than hidden.
pub fn verify_hash(binary: &Path, record: &InstallRecord) -> Result<(), SpawnError> {
    // ---- track G7 policy
    if let Some(reason) = crate::policy::recorded_refusal(binary) {
        return Err(SpawnError::Notarized { reason });
    }
    // ---- end track G7 policy
    let actual = sha256_hex(binary).map_err(SpawnError::Unreadable)?;
    let expected = record.artifact_sha256.trim().to_ascii_lowercase();
    if actual.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1 {
        Ok(())
    } else {
        Err(SpawnError::HashMismatch { expected, actual })
    }
}

/// A started module, not yet through the handshake.
pub struct Spawned {
    /// The process.
    pub child: Child,
    /// Host → reads what the module writes.
    pub reader: LineReader,
    /// Host → writes to the module.
    pub writer: LineWriter,
    /// Ends the conversation from any thread.
    pub closer: Closer,
}

/// Start `binary` with a fresh socket end, `AVADA_MODULE_FD` and `AVADA_MODULE_DATA`
/// set, plus `extra_env`. Does **not** hash — call [`verify_hash`] first; the host does.
pub fn spawn(
    binary: &Path,
    data_dir: &Path,
    extra_env: &[(String, String)],
    extra_args: &[String],
) -> Result<Spawned, SpawnError> {
    let (host_end, child_end) = transport::pair().map_err(SpawnError::Transport)?;
    let mut cmd = Command::new(binary);
    cmd.args(extra_args)
        .env(ENV_DATA_DIR, data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    child_end.attach(&mut cmd).map_err(SpawnError::Transport)?;
    let child = cmd.spawn().map_err(SpawnError::Exec)?;
    drop(child_end);
    let (reader, writer, closer) = host_end.split().map_err(SpawnError::Transport)?;
    Ok(Spawned {
        child,
        reader,
        writer,
        closer,
    })
}

/// Why the hello exchange failed. Every variant closes the pipe.
#[derive(Debug)]
pub enum HandshakeError {
    /// The module exited or closed before saying hello.
    Closed,
    /// The first line was not a module hello.
    NotAHello(String),
    /// The pipe broke or carried garbage.
    Frame(FrameError),
    /// No contract version in common.
    Contract {
        /// The module's range.
        module_min: u32,
        /// The module's range.
        module_max: u32,
    },
    /// The manifest the module presented is not the one that was installed.
    ManifestMismatch,
    /// Writing the host hello failed.
    Io(io::Error),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::Closed => f.write_str("module closed before saying hello"),
            HandshakeError::NotAHello(s) => write!(f, "first line was not module.hello: {s}"),
            HandshakeError::Frame(e) => write!(f, "{e}"),
            HandshakeError::Contract {
                module_min,
                module_max,
            } => write!(
                f,
                "module speaks contract {module_min}..={module_max}, host speaks {CONTRACT_VERSION}"
            ),
            HandshakeError::ManifestMismatch => {
                f.write_str("manifest differs from the install record")
            }
            HandshakeError::Io(e) => write!(f, "cannot write host hello: {e}"),
        }
    }
}
impl std::error::Error for HandshakeError {}

/// What the host learned from a successful handshake.
#[derive(Debug, Clone)]
pub struct Handshake {
    /// The module's hello, manifest verified.
    pub hello: ModuleHello,
    /// The version both sides speak.
    pub contract_version: u32,
}

/// Read the module hello, check it against `record`, answer with `reply` carrying the
/// per-module `token` in `HostHello::token`. It is never logged.
pub fn handshake(
    reader: &mut LineReader,
    writer: &mut LineWriter,
    record: &InstallRecord,
    reply: &HostHello,
    token: &Token,
) -> Result<Handshake, HandshakeError> {
    let line = reader
        .read_line()
        .map_err(HandshakeError::Frame)?
        .ok_or(HandshakeError::Closed)?;
    let hello: ModuleHello =
        serde_json::from_str(&line).map_err(|e| HandshakeError::NotAHello(e.to_string()))?;
    if hello.kind != HelloKind::Module {
        return Err(HandshakeError::NotAHello("type is not module.hello".into()));
    }
    let contract_version = contract::negotiate(
        hello.contract_min,
        hello.contract_max,
        CONTRACT_VERSION,
        CONTRACT_VERSION,
    )
    .ok_or(HandshakeError::Contract {
        module_min: hello.contract_min,
        module_max: hello.contract_max,
    })?;
    if !record.matches_hello(&hello.manifest) {
        return Err(HandshakeError::ManifestMismatch);
    }
    let answer = HostHello {
        kind: HelloKind::Host,
        contract_version,
        token: Some(token.expose().to_string()),
        ..reply.clone()
    };
    let line = serde_json::to_string(&answer).map_err(|e| HandshakeError::Io(e.into()))?;
    writer.write_line(&line).map_err(HandshakeError::Io)?;
    Ok(Handshake {
        hello,
        contract_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::rpc::tests::tempdir::Dir;
    use crate::module::testkit;
    use avada_module_sdk::caps::Capability;
    use std::io::Cursor;

    fn reply() -> HostHello {
        HostHello {
            kind: HelloKind::Host,
            contract_version: CONTRACT_VERSION,
            host_version: "0.0.0".into(),
            product: "Avada Terminal".into(),
            granted: vec![Capability::UiRail],
            methods: vec![],
            data_dir: "/nowhere".into(),
            workspace: None,
            token: None,
            control_url: None,
        }
    }

    struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn run(
        first_line: &str,
        record: &InstallRecord,
    ) -> (Result<Handshake, HandshakeError>, String) {
        let mut reader = LineReader::new(Box::new(Cursor::new(
            format!("{first_line}\n").into_bytes(),
        )));
        let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut writer = LineWriter::new(Box::new(Buf(out.clone())));
        let token = Token::mint();
        let r = handshake(&mut reader, &mut writer, record, &reply(), &token);
        let written = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        // The token must have gone out exactly when the handshake succeeded.
        assert_eq!(r.is_ok(), written.contains(token.expose()));
        (r, written)
    }

    #[test]
    fn hash_check_accepts_the_recorded_binary_and_refuses_a_changed_one() {
        let dir = Dir::new("hash");
        let bin = dir.0.join("module.bin");
        std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut record = testkit::record(&[]);
        record.artifact_sha256 = sha256_hex(&bin).unwrap().to_ascii_uppercase();
        verify_hash(&bin, &record).unwrap();
        std::fs::write(&bin, b"#!/bin/sh\nexit 1\n").unwrap();
        match verify_hash(&bin, &record) {
            Err(SpawnError::HashMismatch { expected, actual }) => {
                assert_ne!(expected, actual);
                assert_eq!(actual.len(), 64);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            verify_hash(&dir.0.join("missing"), &record),
            Err(SpawnError::Unreadable(_))
        ));
    }

    #[test]
    fn handshake_succeeds_on_a_matching_manifest_and_answers_with_host_hello() {
        let record = testkit::record(&[Capability::UiRail]);
        let hello = testkit::module_hello(&record.manifest);
        let (r, written) = run(&serde_json::to_string(&hello).unwrap(), &record);
        let h = r.unwrap();
        assert_eq!(h.contract_version, CONTRACT_VERSION);
        let parsed: HostHello = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(parsed.kind, HelloKind::Host);
        assert_eq!(parsed.granted, vec![Capability::UiRail]);
    }

    #[test]
    fn manifest_mismatch_is_refused_before_any_reply() {
        let record = testkit::record(&[]);
        let mut hello = testkit::module_hello(&record.manifest);
        hello.manifest.module.name = "Renamed".into();
        let (r, written) = run(&serde_json::to_string(&hello).unwrap(), &record);
        assert!(matches!(r, Err(HandshakeError::ManifestMismatch)));
        assert!(written.is_empty());
    }

    #[test]
    fn wrong_contract_range_and_wrong_first_line_are_refused() {
        let record = testkit::record(&[]);
        let mut hello = testkit::module_hello(&record.manifest);
        hello.contract_min = CONTRACT_VERSION + 5;
        hello.contract_max = CONTRACT_VERSION + 9;
        let (r, _) = run(&serde_json::to_string(&hello).unwrap(), &record);
        assert!(matches!(r, Err(HandshakeError::Contract { .. })));
        let (r, _) = run("{\"type\":\"host.hello\"}", &record);
        assert!(matches!(r, Err(HandshakeError::NotAHello(_))));
        let (r, _) = run("", &record);
        assert!(matches!(r, Err(HandshakeError::NotAHello(_))));
    }

    #[cfg(unix)]
    #[test]
    fn spawn_sets_the_environment_and_the_child_sees_the_descriptor() {
        // The script reads the fd number from the env and writes one line down it.
        // bash, not sh: under a test binary the descriptor is routinely 10 or higher,
        // and dash (Debian's /bin/sh) rejects `>&10` outright as a "Bad fd number".
        let dir = Dir::new("spawn");
        let script = dir.0.join("mod.sh");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\nprintf '{\"jsonrpc\":\"2.0\",\"method\":\"module.event\",\"params\":{\"d\":\"'\"$AVADA_MODULE_DATA\"'\"}}\\n' >&$AVADA_MODULE_FD\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let data = dir.0.join("data");
        let mut s = spawn(&script, &data, &[], &[]).unwrap();
        let msg = s.reader.read_message().unwrap().unwrap();
        match msg {
            contract::Message::Notification(n) => {
                assert_eq!(n.method, "module.event");
                assert_eq!(n.params["d"], data.to_string_lossy().as_ref());
            }
            other => panic!("{other:?}"),
        }
        assert!(s.child.wait().unwrap().success());
    }
}
