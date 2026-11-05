//! A client for `gpg-agent`, the process that holds GnuPG's secret keys.
//!
//! Since GnuPG 2.1 the secret keys are not in a file anyone else can read
//! with an OpenPGP library: they live in `private-keys-v1.d`, in the agent's
//! own format, behind the agent's passphrase cache -- or on a smartcard the
//! agent talks to.  So rather than open them, Elma asks the agent to do the
//! two operations a mail client needs, the way `gpg` itself does: decrypt a
//! session key (`PKDECRYPT`) and sign a digest (`PKSIGN`).  Everything else --
//! parsing, verifying, encrypting, the OpenPGP framing around both operations
//! -- is done by the `pgp` crate.
//!
//! The agent speaks the Assuan protocol over a Unix socket: one command per
//! line, answered by data (`D`), status (`S`) and inquiry (`INQUIRE`) lines and
//! closed by `OK` or `ERR`.  Keys are named by their *keygrip*, a hash over the
//! public key material.  Rather than recompute libgcrypt's keygrips (which
//! needs every curve's domain parameters), the agent is asked for the public
//! half of each key it holds and those are matched against the certificate.

use super::prompt::{PassphraseRequest, PassphraseSource};
use anyhow::{Context, Result, anyhow, bail};
use pgp::{
    composed::PlainSessionKey,
    crypto::{checksum, hash::HashAlgorithm, public_key::PublicKeyAlgorithm},
    types::{EskType, KeyDetails, Mpi, PkeskBytes, PublicParams, SignatureBytes},
};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

/// Longest line Assuan accepts, newline included.
const MAX_LINE: usize = 1000;

/// What the agent is asked to show in its own pinentry, should it use one.
const KEY_DESCRIPTION: &str = "Elma needs your OpenPGP key.";

#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;

/// A connection to a running agent.
pub(crate) struct Agent {
    #[cfg(unix)]
    reader: BufReader<Stream>,
    #[cfg(unix)]
    writer: Stream,
    /// Whether the agent hands passphrase questions to us rather than to its
    /// own pinentry.
    loopback: bool,
    /// The keygrip of every key the agent holds, by its public key material,
    /// and when that was asked.
    keygrips: Option<(Instant, HashMap<Vec<u8>, String>)>,
}

/// How long a listing of the agent's keys is trusted before a key it does not
/// have sends Elma back to ask again: a key imported meanwhile is found, and a
/// message to fifty people does not cost fifty listings.
const KEYGRIPS_FRESH_FOR: Duration = Duration::from_secs(30);

/// One answer from the agent, after the inquiries in between have been dealt
/// with.
#[derive(Default)]
struct Response {
    data: Vec<u8>,
    status: Vec<String>,
}

impl Agent {
    /// Connect to the agent serving `gnupg_home`, starting it if need be.
    #[cfg(unix)]
    pub(crate) fn connect(gnupg_home: &Path, loopback: bool) -> Result<Self> {
        let socket = socket_path(gnupg_home);
        let stream = match Stream::connect(&socket) {
            Ok(stream) => stream,
            Err(_) => {
                // Nothing listening yet.  gpg starts the agent on demand, and
                // so does everything else built on it.
                let _ = std::process::Command::new("gpgconf")
                    .env("GNUPGHOME", gnupg_home)
                    .args(["--launch", "gpg-agent"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                Stream::connect(&socket)
                    .with_context(|| format!("connecting to gpg-agent at {}", socket.display()))?
            }
        };
        stream
            .set_read_timeout(None)
            .context("configuring the gpg-agent socket")?;

        let writer = stream.try_clone().context("cloning the gpg-agent socket")?;
        let mut agent = Agent {
            reader: BufReader::new(stream),
            writer,
            loopback: false,
            keygrips: None,
        };

        let greeting = agent.read_line()?;
        if !greeting.starts_with(b"OK") {
            bail!(
                "gpg-agent greeted with {:?}",
                greeting.escape_ascii().to_string()
            );
        }

        if loopback {
            // Refused when the agent is configured without
            // `allow-loopback-pinentry`; its own pinentry has to do then.
            agent.loopback = agent
                .transact("OPTION pinentry-mode=loopback", &mut NoPassphrase)
                .is_ok();
        }
        if !agent.loopback {
            agent.forward_display();
        }
        Ok(agent)
    }

    #[cfg(not(unix))]
    pub(crate) fn connect(_gnupg_home: &Path, _loopback: bool) -> Result<Self> {
        bail!("talking to gpg-agent is only supported on Unix-like systems")
    }

    /// Tell the agent where a graphical pinentry should appear.
    ///
    /// The terminal is deliberately *not* passed on: a curses pinentry drawing
    /// over the running client would leave both unusable.
    fn forward_display(&mut self) {
        for (option, var) in [
            ("display", "DISPLAY"),
            ("putenv=WAYLAND_DISPLAY", "WAYLAND_DISPLAY"),
            (
                "putenv=DBUS_SESSION_BUS_ADDRESS",
                "DBUS_SESSION_BUS_ADDRESS",
            ),
            ("putenv=XAUTHORITY", "XAUTHORITY"),
        ] {
            if let Ok(value) = std::env::var(var) {
                let line = format!("OPTION {option}={}", escape(value.as_bytes(), false));
                let _ = self.transact(&line, &mut NoPassphrase);
            }
        }
    }

    /// The keygrip of the agent's secret key for `public`, if it has one.
    pub(crate) fn keygrip_for(&mut self, public: &PublicParams) -> Result<Option<String>> {
        let Some(material) = super::keyring::key_material(public) else {
            return Ok(None);
        };
        let stale = self.keygrips.as_ref().is_none_or(|(listed, keys)| {
            !keys.contains_key(&material) && listed.elapsed() > KEYGRIPS_FRESH_FOR
        });
        if stale {
            self.keygrips = Some((Instant::now(), self.list_keys()?));
        }
        Ok(self
            .keygrips
            .as_ref()
            .and_then(|(_, keys)| keys.get(&material))
            .cloned())
    }

    /// Ask the agent for every key it holds, and the public half of each.
    fn list_keys(&mut self) -> Result<HashMap<Vec<u8>, String>> {
        let listing = self.transact("KEYINFO --list", &mut NoPassphrase)?;
        let mut keys = HashMap::new();
        for line in &listing.status {
            let mut words = line.split_whitespace();
            if words.next() != Some("KEYINFO") {
                continue;
            }
            let Some(grip) = words.next() else { continue };
            let Ok(public) = self.transact(&format!("READKEY {grip}"), &mut NoPassphrase) else {
                continue;
            };
            if let Some(material) = sexp_key_material(&public.data) {
                keys.insert(material, grip.to_string());
            }
        }
        Ok(keys)
    }

    /// Have the agent recover the session key from one PKESK addressed to
    /// `key`, whose secret it holds under `keygrip`.
    pub(crate) fn decrypt(
        &mut self,
        keygrip: &str,
        key: &dyn KeyDetails,
        values: &PkeskBytes,
        typ: EskType,
        passphrase: &mut dyn PassphraseSource,
    ) -> Result<PlainSessionKey> {
        let ciphertext = match values {
            PkeskBytes::Rsa { mpi } => sexp(&[
                Atom(b"enc-val"),
                List(&[Atom(b"rsa"), List(&[Atom(b"a"), Atom(mpi.as_ref())])]),
            ]),
            PkeskBytes::Elgamal { first, second } => sexp(&[
                Atom(b"enc-val"),
                List(&[
                    Atom(b"elg"),
                    List(&[Atom(b"a"), Atom(first.as_ref())]),
                    List(&[Atom(b"b"), Atom(second.as_ref())]),
                ]),
            ]),
            PkeskBytes::Ecdh {
                public_point,
                encrypted_session_key,
            } => {
                // gpg passes the wrapped key along as it sits in the packet,
                // length octet first.  The agent only needs the point, but
                // insists on the pair.
                let mut wrapped = vec![encrypted_session_key.len() as u8];
                wrapped.extend_from_slice(encrypted_session_key);
                sexp(&[
                    Atom(b"enc-val"),
                    List(&[
                        Atom(b"ecdh"),
                        List(&[Atom(b"s"), Atom(&wrapped)]),
                        List(&[Atom(b"e"), Atom(public_point.as_ref())]),
                    ]),
                ])
            }
            other => bail!(
                "gpg-agent decryption of {:?} keys is not supported",
                std::mem::discriminant(other)
            ),
        };

        self.transact(&format!("SETKEY {keygrip}"), &mut NoPassphrase)?;
        self.set_description();
        let response = self.transact_with_ciphertext("PKDECRYPT", &ciphertext, passphrase)?;
        let value = sexp_value(&response.data, b"value")
            .ok_or_else(|| anyhow!("gpg-agent returned no decrypted value"))?;

        let padding_removed = response
            .status
            .iter()
            .any(|line| line.trim() == "PADDING 0");

        let frame: Zeroizing<Vec<u8>> = match values {
            // ElGamal pads the session key the way RSA does (RFC 4880 section
            // 13.1), and the agent hands it back padded just the same.
            PkeskBytes::Rsa { .. } | PkeskBytes::Elgamal { .. } => {
                Zeroizing::new(strip_pkcs1(&value, padding_removed)?)
            }
            PkeskBytes::Ecdh {
                encrypted_session_key,
                ..
            } => {
                let PublicParams::ECDH(params) = key.public_params() else {
                    bail!("ECDH ciphertext for a key that is not ECDH");
                };
                let shared = shared_x(&value);
                let (hash, alg_sym) = ecdh_kdf_params(params)?;
                pgp::crypto::ecdh::derive_session_key(
                    shared,
                    encrypted_session_key,
                    encrypted_session_key.len(),
                    params.curve(),
                    hash,
                    alg_sym,
                    key.fingerprint().as_bytes(),
                )
                .context("unwrapping the ECDH session key")?
            }
            _ => unreachable!("rejected above"),
        };

        session_key_from_frame(&frame, typ)
    }

    /// Have the agent sign `digest`, made with `hash`, using the key under
    /// `keygrip`.
    pub(crate) fn sign(
        &mut self,
        keygrip: &str,
        algorithm: PublicKeyAlgorithm,
        hash: HashAlgorithm,
        digest: &[u8],
        passphrase: &mut dyn PassphraseSource,
    ) -> Result<SignatureBytes> {
        let hash_name = match hash {
            HashAlgorithm::Sha1 => "sha1",
            HashAlgorithm::Sha224 => "sha224",
            HashAlgorithm::Sha256 => "sha256",
            HashAlgorithm::Sha384 => "sha384",
            HashAlgorithm::Sha512 => "sha512",
            other => bail!("gpg-agent cannot sign a {other:?} digest"),
        };

        self.transact(&format!("SIGKEY {keygrip}"), &mut NoPassphrase)?;
        self.set_description();
        self.transact(
            &format!("SETHASH --hash={hash_name} {}", hex(digest)),
            &mut NoPassphrase,
        )?;
        let response = self.transact("PKSIGN", &mut Asking(passphrase))?;

        let r = sexp_value(&response.data, b"r");
        let s = sexp_value(&response.data, b"s")
            .ok_or_else(|| anyhow!("gpg-agent returned no signature"))?;

        Ok(match (algorithm, r) {
            (PublicKeyAlgorithm::RSA | PublicKeyAlgorithm::RSASign, _) => {
                SignatureBytes::Mpis(vec![Mpi::from_slice(&s)])
            }
            (PublicKeyAlgorithm::Ed25519, Some(r)) => {
                let mut native = left_pad(&r, 32);
                native.extend(left_pad(&s, 32));
                SignatureBytes::Native(native.into())
            }
            (
                PublicKeyAlgorithm::EdDSALegacy
                | PublicKeyAlgorithm::ECDSA
                | PublicKeyAlgorithm::DSA,
                Some(r),
            ) => SignatureBytes::Mpis(vec![Mpi::from_slice(&r), Mpi::from_slice(&s)]),
            (other, _) => bail!("gpg-agent signing with {other:?} keys is not supported"),
        })
    }

    fn set_description(&mut self) {
        let _ = self.transact(
            &format!("SETKEYDESC {}", escape(KEY_DESCRIPTION.as_bytes(), true)),
            &mut NoPassphrase,
        );
    }

    /// Run `command`, answering the agent's `INQUIRE CIPHERTEXT` with
    /// `ciphertext`.
    fn transact_with_ciphertext(
        &mut self,
        command: &str,
        ciphertext: &[u8],
        passphrase: &mut dyn PassphraseSource,
    ) -> Result<Response> {
        let mut source = WithCiphertext {
            ciphertext,
            passphrase,
        };
        self.transact(command, &mut source)
    }

    /// Send one command and collect what comes back up to its `OK`.
    fn transact(&mut self, command: &str, inquiries: &mut dyn Inquiries) -> Result<Response> {
        self.write_line(command.as_bytes())?;
        let mut response = Response::default();
        let mut attempts = 0usize;
        loop {
            // Kept as bytes: data lines escape only `%`, CR and LF, so what
            // they carry -- key material, plaintext -- is binary.
            let line = self.read_line()?;
            if line == b"OK" || line.starts_with(b"OK ") {
                return Ok(response);
            } else if let Some(err) = line.strip_prefix(b"ERR ") {
                let text = String::from_utf8_lossy(&unescape(err)).into_owned();
                let message = text.split_once(' ').map(|(_, text)| text).unwrap_or(&text);
                bail!("gpg-agent: {message}");
            } else if let Some(data) = line.strip_prefix(b"D ") {
                response.data.extend(unescape(data));
            } else if let Some(status) = line.strip_prefix(b"S ") {
                response
                    .status
                    .push(String::from_utf8_lossy(status).into_owned());
            } else if let Some(inquiry) = line.strip_prefix(b"INQUIRE ") {
                let inquiry = String::from_utf8_lossy(inquiry);
                let keyword = inquiry.split_whitespace().next().unwrap_or_default();
                if keyword == "PASSPHRASE" || keyword == "NEW_PASSPHRASE" {
                    attempts += 1;
                }
                match inquiries.answer(keyword, attempts)? {
                    Some(answer) => {
                        let escaped = Zeroizing::new(escape(&answer, false));
                        for chunk in chunks(&escaped) {
                            let mut line = Zeroizing::new(b"D ".to_vec());
                            line.extend_from_slice(chunk.as_bytes());
                            self.write_line(&line)?;
                        }
                        self.write_line(b"END")?;
                    }
                    None => self.write_line(b"CAN")?,
                }
            }
            // Comments and anything newer than this client: skipped.
        }
    }

    fn write_line(&mut self, line: &[u8]) -> Result<()> {
        #[cfg(unix)]
        {
            self.writer
                .write_all(line)
                .and_then(|()| self.writer.write_all(b"\n"))
                .and_then(|()| self.writer.flush())
                .context("writing to gpg-agent")
        }
        #[cfg(not(unix))]
        {
            let _ = line;
            bail!("gpg-agent is not available")
        }
    }

    fn read_line(&mut self) -> Result<Vec<u8>> {
        #[cfg(unix)]
        {
            let mut buf = Vec::new();
            let read = self
                .reader
                .read_until(b'\n', &mut buf)
                .context("reading from gpg-agent")?;
            if read == 0 {
                bail!("gpg-agent closed the connection");
            }
            while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                buf.pop();
            }
            Ok(buf)
        }
        #[cfg(not(unix))]
        {
            bail!("gpg-agent is not available")
        }
    }
}

/// Where the agent for `gnupg_home` listens.
///
/// `gpgconf` knows -- including the hashed directory under `/run/user` that a
/// non-default home gets -- so it is asked first, and the two places the
/// socket usually is are the fallback.
fn socket_path(gnupg_home: &Path) -> PathBuf {
    let asked = std::process::Command::new("gpgconf")
        .env("GNUPGHOME", gnupg_home)
        .args(["--list-dirs", "agent-socket"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    if let Some(path) = asked {
        return follow_redirect(PathBuf::from(unescape_gpgconf(&path)));
    }

    #[cfg(unix)]
    {
        let in_home = gnupg_home.join("S.gpg-agent");
        let default_home = std::env::home_dir().map(|home| home.join(".gnupg"));
        if default_home.as_deref() == Some(gnupg_home) {
            // SAFETY: getuid cannot fail and touches no memory.
            let uid = unsafe { libc::getuid() };
            let run = PathBuf::from(format!("/run/user/{uid}/gnupg/S.gpg-agent"));
            if run.exists() {
                return run;
            }
        }
        follow_redirect(in_home)
    }
    #[cfg(not(unix))]
    {
        gnupg_home.join("S.gpg-agent")
    }
}

/// `gpgconf` writes `:` and `%` in paths as `%3a` and `%25`.
fn unescape_gpgconf(path: &str) -> String {
    String::from_utf8_lossy(&unescape(path.as_bytes())).into_owned()
}

/// A socket file can be a plain file pointing somewhere else: `%Assuan%`
/// followed by `socket=<path>`, for file systems that cannot hold sockets.
fn follow_redirect(path: PathBuf) -> PathBuf {
    let Ok(meta) = std::fs::metadata(&path) else {
        return path;
    };
    if !meta.is_file() || meta.len() > 4096 {
        return path;
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return path;
    };
    let mut lines = text.lines();
    if lines.next() != Some("%Assuan%") {
        return path;
    }
    lines
        .find_map(|line| line.strip_prefix("socket="))
        .map(PathBuf::from)
        .unwrap_or(path)
}

/// Answers to the agent's inquiries.
trait Inquiries {
    /// The data to send for `keyword`, or `None` to cancel.  `attempt` counts
    /// the passphrase questions so far in this command.
    fn answer(&mut self, keyword: &str, attempt: usize) -> Result<Option<Zeroizing<Vec<u8>>>>;
}

/// For commands that never ask for anything sensitive.
struct NoPassphrase;

impl Inquiries for NoPassphrase {
    fn answer(&mut self, keyword: &str, _attempt: usize) -> Result<Option<Zeroizing<Vec<u8>>>> {
        // `PINENTRY_LAUNCHED` and friends are notifications that want an
        // empty answer; a question for a secret gets none.
        if keyword.contains("PASSPHRASE")
            || keyword.contains("PIN") && keyword != "PINENTRY_LAUNCHED"
        {
            return Ok(None);
        }
        Ok(Some(Zeroizing::new(Vec::new())))
    }
}

/// Passphrase questions go to the user; everything else as [`NoPassphrase`].
struct Asking<'a>(&'a mut dyn PassphraseSource);

impl Inquiries for Asking<'_> {
    fn answer(&mut self, keyword: &str, attempt: usize) -> Result<Option<Zeroizing<Vec<u8>>>> {
        if keyword == "PASSPHRASE" || keyword == "NEW_PASSPHRASE" {
            let request = PassphraseRequest {
                description: "Unlock your OpenPGP key (held by gpg-agent)".to_string(),
                retry: attempt > 1,
            };
            return Ok(self
                .0
                .passphrase(&request)
                .map(|pass| Zeroizing::new(pass.as_bytes().to_vec())));
        }
        NoPassphrase.answer(keyword, attempt)
    }
}

struct WithCiphertext<'a> {
    ciphertext: &'a [u8],
    passphrase: &'a mut dyn PassphraseSource,
}

impl Inquiries for WithCiphertext<'_> {
    fn answer(&mut self, keyword: &str, attempt: usize) -> Result<Option<Zeroizing<Vec<u8>>>> {
        if keyword == "CIPHERTEXT" {
            return Ok(Some(Zeroizing::new(self.ciphertext.to_vec())));
        }
        Asking(&mut *self.passphrase).answer(keyword, attempt)
    }
}

/// Assuan's percent-escaping: `%`, CR and LF always; with `plus`, also spaces
/// as `+` (and `+` itself), which is what `SETKEYDESC` takes.
fn escape(data: &[u8], plus: bool) -> String {
    let mut out = String::with_capacity(data.len());
    for &byte in data {
        match byte {
            b' ' if plus => out.push('+'),
            b'%' | b'\r' | b'\n' => out.push_str(&format!("%{byte:02X}")),
            b'+' if plus => out.push_str("%2B"),
            b if !(0x20..0x7f).contains(&b) => out.push_str(&format!("%{b:02X}")),
            b => out.push(b as char),
        }
    }
    out
}

fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'%'
            && i + 2 < data.len()
            && let Some(byte) = std::str::from_utf8(&data[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// Split escaped data into pieces that fit a `D` line, never through the
/// middle of a `%XX`.
fn chunks(escaped: &str) -> Vec<&str> {
    let limit = MAX_LINE - 4;
    let mut out = Vec::new();
    let mut rest = escaped;
    while rest.len() > limit {
        let mut cut = limit;
        // Back off an escape that would be split.
        if let Some(pos) = rest[cut.saturating_sub(2)..cut].find('%') {
            cut = cut - 2 + pos;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out.push(rest);
    out
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02X}")).collect()
}

fn left_pad(data: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0; len.saturating_sub(data.len())];
    out.extend_from_slice(data);
    out
}

/// Take the session-key frame out of an RSA or ElGamal decryption.
///
/// A software key comes back with its PKCS#1 v1.5 padding still on; a card
/// strips it and says so with `PADDING 0`.
fn strip_pkcs1(value: &[u8], padding_removed: bool) -> Result<Vec<u8>> {
    if padding_removed {
        return Ok(value.to_vec());
    }
    // The leading zero octet is usually gone already, eaten as an MPI.
    let value = value.strip_prefix(&[0]).unwrap_or(value);
    let Some(rest) = value.strip_prefix(&[2]) else {
        bail!("decryption returned an unexpected block");
    };
    let separator = rest
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| anyhow!("decryption returned a block without a separator"))?;
    Ok(rest[separator + 1..].to_vec())
}

/// The X coordinate of the shared point the agent returns for ECDH: after a
/// `0x40` prefix for the Montgomery curves, or the first half of an
/// uncompressed `0x04` point for the NIST ones.
fn shared_x(point: &[u8]) -> &[u8] {
    match point.first() {
        Some(0x40) => &point[1..],
        Some(0x04) if point.len() % 2 == 1 => &point[1..1 + point.len() / 2],
        _ => point,
    }
}

fn ecdh_kdf_params(
    params: &pgp::types::EcdhPublicParams,
) -> Result<(HashAlgorithm, pgp::crypto::sym::SymmetricKeyAlgorithm)> {
    use pgp::types::EcdhPublicParams as P;
    Ok(match params {
        P::Curve25519Legacy { hash, alg_sym, .. }
        | P::P256 { hash, alg_sym, .. }
        | P::P384 { hash, alg_sym, .. }
        | P::P521 { hash, alg_sym, .. }
        | P::Brainpool256 { hash, alg_sym, .. }
        | P::Brainpool384 { hash, alg_sym, .. }
        | P::Brainpool512 { hash, alg_sym, .. } => (*hash, *alg_sym),
        P::Unsupported { curve, .. } => bail!("unsupported ECDH curve {curve:?}"),
    })
}

/// Turn the decrypted frame -- algorithm, key, checksum -- into a session key.
fn session_key_from_frame(frame: &[u8], typ: EskType) -> Result<PlainSessionKey> {
    use pgp::crypto::sym::SymmetricKeyAlgorithm;
    match typ {
        EskType::V3_4 => {
            let (&alg, rest) = frame
                .split_first()
                .ok_or_else(|| anyhow!("empty session key"))?;
            let sym_alg = SymmetricKeyAlgorithm::from(alg);
            let size = sym_alg.key_size();
            if sym_alg == SymmetricKeyAlgorithm::Plaintext || rest.len() != size + 2 {
                bail!("the decrypted session key is malformed");
            }
            let (key, sum) = rest.split_at(size);
            checksum::simple(sum.try_into().expect("two octets"), key)
                .map_err(|_| anyhow!("the decrypted session key fails its checksum"))?;
            Ok(PlainSessionKey::V3_4 {
                sym_alg,
                key: key.into(),
            })
        }
        EskType::V6 => {
            if frame.len() < 2 {
                bail!("the decrypted session key is malformed");
            }
            let (key, sum) = frame.split_at(frame.len() - 2);
            checksum::simple(sum.try_into().expect("two octets"), key)
                .map_err(|_| anyhow!("the decrypted session key fails its checksum"))?;
            Ok(PlainSessionKey::V6 { key: key.into() })
        }
    }
}

// --- Canonical S-expressions -------------------------------------------------
//
// The agent speaks libgcrypt's canonical form: `(` and `)` around lists,
// atoms as `<decimal length>:<bytes>`.  Only the little this client needs.

#[derive(Debug, PartialEq, Eq)]
enum Sexp {
    Atom(Vec<u8>),
    List(Vec<Sexp>),
}

/// What [`sexp`] builds lists from.
enum Build<'a> {
    Atom(&'a [u8]),
    List(&'a [Build<'a>]),
}
use Build::{Atom, List};

fn sexp(items: &[Build<'_>]) -> Vec<u8> {
    fn write(out: &mut Vec<u8>, item: &Build<'_>) {
        match item {
            Atom(bytes) => {
                out.extend(bytes.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(bytes);
            }
            List(items) => {
                out.push(b'(');
                for item in *items {
                    write(out, item);
                }
                out.push(b')');
            }
        }
    }
    let mut out = Vec::new();
    write(&mut out, &List(items));
    out
}

fn parse_sexp(data: &[u8]) -> Option<Sexp> {
    fn parse(data: &[u8], pos: &mut usize, depth: usize) -> Option<Sexp> {
        if depth > 32 {
            return None;
        }
        match data.get(*pos)? {
            b'(' => {
                *pos += 1;
                let mut items = Vec::new();
                loop {
                    match data.get(*pos)? {
                        b')' => {
                            *pos += 1;
                            return Some(Sexp::List(items));
                        }
                        _ => items.push(parse(data, pos, depth + 1)?),
                    }
                }
            }
            b'0'..=b'9' => {
                let colon = *pos + data[*pos..].iter().position(|b| *b == b':')?;
                let len: usize = std::str::from_utf8(&data[*pos..colon]).ok()?.parse().ok()?;
                let start = colon + 1;
                let end = start.checked_add(len)?;
                let atom = data.get(start..end)?.to_vec();
                *pos = end;
                Some(Sexp::Atom(atom))
            }
            _ => None,
        }
    }
    let mut pos = 0;
    parse(data, &mut pos, 0)
}

/// The value of the first `(name value)` pair anywhere in `data`.
fn sexp_value(data: &[u8], name: &[u8]) -> Option<Vec<u8>> {
    fn find(node: &Sexp, name: &[u8]) -> Option<Vec<u8>> {
        let Sexp::List(items) = node else { return None };
        if let [Sexp::Atom(key), Sexp::Atom(value), ..] = items.as_slice()
            && key == name
        {
            return Some(value.clone());
        }
        items.iter().find_map(|item| find(item, name))
    }
    find(&parse_sexp(data)?, name)
}

/// The key material in a `public-key` expression, in the form
/// [`super::keyring::key_material`] produces from a certificate.
fn sexp_key_material(data: &[u8]) -> Option<Vec<u8>> {
    match sexp_algorithm(data).as_deref() {
        Some(b"elg") => sexp_value(data, b"y").map(|y| super::keyring::strip_zeros(&y).to_vec()),
        // A DSA key has a `q` as well, but it is the group order, not a point.
        Some(b"dsa") => None,
        _ => {
            if let Some(q) = sexp_value(data, b"q") {
                return Some(super::keyring::normalize_point(&q));
            }
            sexp_value(data, b"n").map(|n| super::keyring::strip_zeros(&n).to_vec())
        }
    }
}

/// The algorithm a `public-key` expression is for: `rsa`, `elg`, `ecc`...
fn sexp_algorithm(data: &[u8]) -> Option<Vec<u8>> {
    let Sexp::List(items) = parse_sexp(data)? else {
        return None;
    };
    let [_, Sexp::List(key), ..] = items.as_slice() else {
        return None;
    };
    let [Sexp::Atom(algorithm), ..] = key.as_slice() else {
        return None;
    };
    Some(algorithm.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_round_trips_everything_assuan_cares_about() {
        let data = b"50% off\r\nnow+then";
        assert_eq!(escape(data, false), "50%25 off%0D%0Anow+then");
        assert_eq!(escape(data, true), "50%25+off%0D%0Anow%2Bthen");
        assert_eq!(unescape(escape(data, false).as_bytes()), data);
    }

    #[test]
    fn long_data_is_split_without_breaking_an_escape() {
        let data = vec![b'%'; 700];
        let escaped = escape(&data, false);
        let pieces = chunks(&escaped);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|piece| piece.len() <= MAX_LINE - 4));
        assert!(pieces.iter().all(|piece| piece.len() % 3 == 0));
        assert_eq!(pieces.concat(), escaped);
    }

    #[test]
    fn s_expressions_are_written_and_read_back() {
        let built = sexp(&[
            Atom(b"enc-val"),
            List(&[Atom(b"rsa"), List(&[Atom(b"a"), Atom(b"\x01\x02")])]),
        ]);
        assert_eq!(built, b"(7:enc-val(3:rsa(1:a2:\x01\x02)))");
        assert_eq!(sexp_value(&built, b"a"), Some(vec![1, 2]));
        assert_eq!(
            sexp_value(b"(5:value3:abc)", b"value"),
            Some(b"abc".to_vec())
        );
        assert_eq!(sexp_value(b"(5:value9:abc)", b"value"), None);
    }

    #[test]
    fn the_agents_public_keys_reduce_to_their_material() {
        let ecc = b"(10:public-key(3:ecc(5:curve7:Ed25519)(5:flags5:eddsa)(1:q3:@\x01\x02)))";
        assert_eq!(sexp_key_material(ecc), Some(vec![0x40, 1, 2]));
        let rsa = b"(10:public-key(3:rsa(1:n3:\x00\x80\x01)(1:e3:\x01\x00\x01)))";
        assert_eq!(sexp_key_material(rsa), Some(vec![0x80, 1]));
    }

    #[test]
    fn pkcs1_padding_is_removed_unless_the_card_already_did() {
        let padded = [0, 2, 0xAA, 0xBB, 0, 9, 1, 2];
        assert_eq!(strip_pkcs1(&padded, false).unwrap(), vec![9, 1, 2]);
        assert_eq!(strip_pkcs1(&padded[1..], false).unwrap(), vec![9, 1, 2]);
        assert_eq!(strip_pkcs1(&[9, 1, 2], true).unwrap(), vec![9, 1, 2]);
        assert!(strip_pkcs1(&[1, 2, 3], false).is_err());
    }

    #[test]
    fn the_shared_point_is_cut_down_to_its_x_coordinate() {
        assert_eq!(shared_x(&[0x40, 1, 2, 3]), &[1, 2, 3]);
        assert_eq!(shared_x(&[0x04, 1, 2, 3, 4]), &[1, 2]);
        assert_eq!(shared_x(&[7, 7]), &[7, 7]);
    }

    #[test]
    fn a_redirect_file_points_at_the_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("S.gpg-agent");
        std::fs::write(&file, "%Assuan%\nsocket=/elsewhere/S.gpg-agent\n").unwrap();
        assert_eq!(
            follow_redirect(file),
            PathBuf::from("/elsewhere/S.gpg-agent")
        );
    }
}
