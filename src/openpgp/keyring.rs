//! The certificates (public keys) Elma knows about.
//!
//! Read straight from GnuPG's home, so a key imported with `gpg --import` or
//! fetched with `gpg --locate-keys` is usable here without exporting anything:
//! `pubring.gpg` where it exists, as gpg does, and `pubring.kbx` otherwise.  Files
//! named in the configuration are read on top of that.  Everything is read
//! again when one of the files changes, so a key imported while Elma runs is
//! picked up by the next message that needs it.

use pgp::{
    composed::{
        Deserializable, SignedPublicKey, SignedPublicKeyParser, SignedPublicSubKey, SignedSecretKey,
    },
    packet::{self, PacketParser, PacketTrait, Signature, SignatureType},
    ser::Serialize,
    types::{
        EncryptionKey, EskType, Fingerprint, KeyDetails, KeyId, KeyVersion, PkeskBytes,
        PublicParams, SignatureBytes, Tag, Timestamp, VerifyingKey,
    },
};
use std::{
    collections::HashMap,
    io::{BufReader, Cursor},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

/// One component of a certificate: its primary key or one of its subkeys.
#[derive(Clone, Debug)]
pub(crate) enum KeyPart {
    Primary(packet::PublicKey),
    Sub(packet::PublicSubkey),
}

impl KeyDetails for KeyPart {
    fn version(&self) -> KeyVersion {
        match self {
            KeyPart::Primary(key) => key.version(),
            KeyPart::Sub(key) => key.version(),
        }
    }
    fn legacy_key_id(&self) -> KeyId {
        match self {
            KeyPart::Primary(key) => key.legacy_key_id(),
            KeyPart::Sub(key) => key.legacy_key_id(),
        }
    }
    fn fingerprint(&self) -> Fingerprint {
        match self {
            KeyPart::Primary(key) => key.fingerprint(),
            KeyPart::Sub(key) => key.fingerprint(),
        }
    }
    fn algorithm(&self) -> pgp::crypto::public_key::PublicKeyAlgorithm {
        match self {
            KeyPart::Primary(key) => key.algorithm(),
            KeyPart::Sub(key) => key.algorithm(),
        }
    }
    fn created_at(&self) -> Timestamp {
        match self {
            KeyPart::Primary(key) => key.created_at(),
            KeyPart::Sub(key) => key.created_at(),
        }
    }
    fn legacy_v3_expiration_days(&self) -> Option<u16> {
        match self {
            KeyPart::Primary(key) => key.legacy_v3_expiration_days(),
            KeyPart::Sub(key) => key.legacy_v3_expiration_days(),
        }
    }
    fn public_params(&self) -> &PublicParams {
        match self {
            KeyPart::Primary(key) => key.public_params(),
            KeyPart::Sub(key) => key.public_params(),
        }
    }
}

impl VerifyingKey for KeyPart {
    fn verify(
        &self,
        hash: pgp::crypto::hash::HashAlgorithm,
        data: &[u8],
        sig: &SignatureBytes,
    ) -> pgp::errors::Result<()> {
        match self {
            KeyPart::Primary(key) => VerifyingKey::verify(key, hash, data, sig),
            KeyPart::Sub(key) => VerifyingKey::verify(key, hash, data, sig),
        }
    }
}

impl EncryptionKey for KeyPart {
    fn encrypt<R: rand::CryptoRng + rand::Rng>(
        &self,
        rng: R,
        plain: &[u8],
        typ: EskType,
    ) -> pgp::errors::Result<PkeskBytes> {
        match self {
            KeyPart::Primary(key) => key.encrypt(rng, plain, typ),
            KeyPart::Sub(key) => key.encrypt(rng, plain, typ),
        }
    }
}

/// A certificate, with what lookups need already worked out.
#[derive(Debug)]
pub(crate) struct Cert {
    pub(crate) key: SignedPublicKey,
    /// Lower-cased addresses from the user IDs, unverified: a hint for lookup,
    /// confirmed by [`Cert::valid_user_for`] before anything relies on it.
    emails: Vec<String>,
}

/// What a key can be used for, at the moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Usage {
    Sign,
    Encrypt,
}

impl Cert {
    fn new(key: SignedPublicKey) -> Self {
        let emails = key
            .details
            .users
            .iter()
            .filter_map(|user| email_of(&user_id_text(&user.id)))
            .collect();
        Cert { key, emails }
    }

    pub(crate) fn fingerprint(&self) -> Fingerprint {
        self.key.primary_key.fingerprint()
    }

    pub(crate) fn fingerprint_hex(&self) -> String {
        hex_upper(self.fingerprint().as_bytes())
    }

    /// Why the certificate as a whole cannot be used, if it cannot.
    pub(crate) fn problem(&self, now: SystemTime) -> Option<String> {
        let primary = &self.key.primary_key;
        if self
            .key
            .details
            .revocation_signatures
            .iter()
            .any(|sig| sig.verify_key(primary).is_ok())
        {
            return Some("the key has been revoked".to_string());
        }
        let self_sig = self.newest_self_signature()?;
        if let Some(expires) = expiry(primary.created_at(), self_sig)
            && now >= expires
        {
            let date = chrono::DateTime::<chrono::Utc>::from(expires).format("%Y-%m-%d");
            return Some(format!("the key expired on {date}"));
        }
        None
    }

    /// Why [`Cert::usable_keys`] finds nothing for `usage`, in words the
    /// user can act on.
    pub(crate) fn unusable_reason(&self, usage: Usage, now: SystemTime) -> String {
        if let Some(problem) = self.problem(now) {
            return problem;
        }
        match usage {
            Usage::Encrypt
                if self.key.public_subkeys.iter().any(|sub| {
                    sub.key.algorithm().can_encrypt() && !can_encrypt_to(sub.key.algorithm())
                }) =>
            {
                "its encryption subkey is ElGamal, which Elma cannot encrypt to".to_string()
            }
            Usage::Encrypt => "it has no usable encryption subkey".to_string(),
            Usage::Sign => "it has no usable signing key".to_string(),
        }
    }

    /// The newest valid self-signature over one of the user IDs, which is
    /// what carries the primary key's expiry and flags.
    fn newest_self_signature(&self) -> Option<&Signature> {
        let primary = &self.key.primary_key;
        self.key
            .details
            .users
            .iter()
            .flat_map(|user| {
                user.signatures.iter().filter(move |sig| {
                    is_certification(sig)
                        && sig
                            .verify_certification(primary, Tag::UserId, &user.id)
                            .is_ok()
                })
            })
            .chain(
                self.key
                    .details
                    .direct_signatures
                    .iter()
                    .filter(|sig| sig.verify_key(primary).is_ok()),
            )
            .max_by_key(|sig| sig.created().map(Timestamp::as_secs).unwrap_or(0))
    }

    /// The user ID to name the certificate by: the one carrying `email` if
    /// asked for and present, otherwise the primary one.  Only user IDs with a
    /// valid self-signature that has not been revoked count.
    pub(crate) fn display_name(&self, email: Option<&str>) -> Option<String> {
        if let Some(email) = email
            && let Some(user) = self.valid_user_for(email)
        {
            return Some(user);
        }
        let primary = &self.key.primary_key;
        let mut valid = self
            .key
            .details
            .users
            .iter()
            .filter(|user| user_is_valid(primary, user));
        let first = valid.clone().next();
        valid
            .find(|user| user.is_primary())
            .or(first)
            .map(|user| user_id_text(&user.id))
    }

    /// The valid user ID carrying `email`, if there is one.
    pub(crate) fn valid_user_for(&self, email: &str) -> Option<String> {
        let wanted = email.trim().to_ascii_lowercase();
        let primary = &self.key.primary_key;
        self.key
            .details
            .users
            .iter()
            .filter(|user| email_of(&user_id_text(&user.id)).as_deref() == Some(wanted.as_str()))
            .find(|user| user_is_valid(primary, user))
            .map(|user| user_id_text(&user.id))
    }

    /// The components usable for `usage` right now, newest first.
    pub(crate) fn usable_keys(&self, usage: Usage, now: SystemTime) -> Vec<KeyPart> {
        if self.problem(now).is_some() {
            return Vec::new();
        }
        let primary = &self.key.primary_key;
        let mut found: Vec<(u32, KeyPart)> = self
            .key
            .public_subkeys
            .iter()
            .filter_map(|sub| {
                let binding = current_binding(primary, sub)?;
                let flags = binding.key_flags();
                let fits = match usage {
                    Usage::Sign => flags.sign(),
                    Usage::Encrypt => {
                        (flags.encrypt_comms() || flags.encrypt_storage())
                            && can_encrypt_to(sub.key.algorithm())
                    }
                };
                if !fits || expired(sub.key.created_at(), binding, now) {
                    return None;
                }
                if usage == Usage::Sign && sub.verify_bindings(primary).is_err() {
                    // A signing subkey has to vouch for its primary in turn;
                    // without that back-signature anyone could claim it.
                    return None;
                }
                Some((
                    sub.key.created_at().as_secs(),
                    KeyPart::Sub(sub.key.clone()),
                ))
            })
            .collect();
        found.sort_by_key(|(created, _)| std::cmp::Reverse(*created));
        let mut keys: Vec<KeyPart> = found.into_iter().map(|(_, key)| key).collect();

        // The primary key is the fallback: older RSA keys sign and encrypt with
        // it.  Without flags at all, its algorithm decides.
        if let Some(sig) = self.newest_self_signature() {
            let flags = sig.key_flags();
            let has_flags =
                flags.sign() || flags.certify() || flags.encrypt_comms() || flags.encrypt_storage();
            let fits = match usage {
                Usage::Sign => flags.sign() || !has_flags,
                Usage::Encrypt => flags.encrypt_comms() || flags.encrypt_storage() || !has_flags,
            };
            let capable = match usage {
                Usage::Sign => primary.algorithm().can_sign(),
                Usage::Encrypt => can_encrypt_to(primary.algorithm()),
            };
            if fits && capable {
                keys.push(KeyPart::Primary(primary.clone()));
            }
        }
        keys
    }

    /// The component with `id`, primary or sub.
    pub(crate) fn component(&self, id: &KeyRef) -> Option<KeyPart> {
        let primary = &self.key.primary_key;
        if id.matches(primary) {
            return Some(KeyPart::Primary(primary.clone()));
        }
        self.key
            .public_subkeys
            .iter()
            .find(|sub| id.matches(&sub.key))
            .map(|sub| KeyPart::Sub(sub.key.clone()))
    }

    /// Whether `part` may make signatures for this certificate right now.
    pub(crate) fn may_sign(&self, part: &KeyPart, now: SystemTime) -> Result<(), String> {
        if let Some(problem) = self.problem(now) {
            return Err(problem);
        }
        match part {
            KeyPart::Primary(_) => Ok(()),
            KeyPart::Sub(key) => {
                let fpr = key.fingerprint();
                if self
                    .usable_keys(Usage::Sign, now)
                    .iter()
                    .any(|usable| usable.fingerprint() == fpr)
                {
                    Ok(())
                } else {
                    Err("the signing subkey is revoked, expired or not bound to the key".into())
                }
            }
        }
    }
}

fn is_certification(sig: &Signature) -> bool {
    matches!(
        sig.typ(),
        Some(
            SignatureType::CertGeneric
                | SignatureType::CertPersona
                | SignatureType::CertCasual
                | SignatureType::CertPositive
        )
    )
}

fn user_is_valid(primary: &packet::PublicKey, user: &pgp::types::SignedUser) -> bool {
    let newest = user
        .signatures
        .iter()
        .filter(|sig| {
            (is_certification(sig) || sig.typ() == Some(SignatureType::CertRevocation))
                && sig
                    .verify_certification(primary, Tag::UserId, &user.id)
                    .is_ok()
        })
        .max_by_key(|sig| sig.created().map(Timestamp::as_secs).unwrap_or(0));
    newest.is_some_and(is_certification)
}

/// The newest valid binding or revocation of `sub`, if the newest is a
/// binding.
fn current_binding<'a>(
    primary: &packet::PublicKey,
    sub: &'a SignedPublicSubKey,
) -> Option<&'a Signature> {
    let newest = sub
        .signatures
        .iter()
        .filter(|sig| sig.verify_subkey_binding(primary, &sub.key).is_ok())
        .max_by_key(|sig| sig.created().map(Timestamp::as_secs).unwrap_or(0))?;
    (newest.typ() == Some(SignatureType::SubkeyBinding)).then_some(newest)
}

/// Whether rPGP can encrypt to a key of `algorithm`.  It cannot to ElGamal,
/// the encryption half of the DSA keys of the 2000s: such a key is no
/// encryption key at all here, rather than one that fails the whole message.
fn can_encrypt_to(algorithm: pgp::crypto::public_key::PublicKeyAlgorithm) -> bool {
    use pgp::crypto::public_key::PublicKeyAlgorithm::{Elgamal, ElgamalEncrypt};
    algorithm.can_encrypt() && !matches!(algorithm, Elgamal | ElgamalEncrypt)
}

fn expired(created: Timestamp, sig: &Signature, now: SystemTime) -> bool {
    expiry(created, sig).is_some_and(|expires| now >= expires)
}

/// When a key created at `created` expires by `sig`, if it does.
fn expiry(created: Timestamp, sig: &Signature) -> Option<SystemTime> {
    let lifetime = sig.key_expiration_time()?;
    if lifetime.as_secs() == 0 {
        return None;
    }
    Some(SystemTime::from(created) + std::time::Duration::from_secs(lifetime.as_secs().into()))
}

/// A key as a signature or ciphertext names it.
#[derive(Clone, Debug)]
pub(crate) enum KeyRef {
    Fingerprint(Fingerprint),
    Id(KeyId),
}

impl KeyRef {
    fn matches(&self, key: &impl KeyDetails) -> bool {
        match self {
            KeyRef::Fingerprint(fpr) => key.fingerprint() == *fpr,
            KeyRef::Id(id) => key.legacy_key_id() == *id,
        }
    }

    pub(crate) fn hex(&self) -> String {
        match self {
            KeyRef::Fingerprint(fpr) => hex_upper(fpr.as_bytes()),
            KeyRef::Id(id) => hex_upper(id.as_ref()),
        }
    }
}

/// Where the certificates came from, and when each file last changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Sources(Vec<(PathBuf, Option<SystemTime>)>);

impl Sources {
    fn scan(paths: &[PathBuf]) -> Self {
        Sources(
            paths
                .iter()
                .map(|path| {
                    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                    (path.clone(), modified)
                })
                .collect(),
        )
    }
}

/// Every known certificate, indexed for the lookups mail needs.
#[derive(Debug, Default)]
pub(crate) struct Keyring {
    certs: Vec<Arc<Cert>>,
    by_email: HashMap<String, Vec<usize>>,
    /// Every component's key ID, to its certificate.
    by_key_id: HashMap<Vec<u8>, Vec<usize>>,
    sources: Sources,
    paths: Vec<PathBuf>,
}

impl Keyring {
    /// Read everything at `paths`; missing files are skipped.
    pub(crate) fn load(paths: Vec<PathBuf>) -> Self {
        let sources = Sources::scan(&paths);
        let mut keyring = Keyring {
            paths,
            sources,
            ..Default::default()
        };
        for path in keyring.paths.clone() {
            let Ok(data) = std::fs::read(&path) else {
                continue;
            };
            for key in read_certificates(&path, &data) {
                keyring.add(key);
            }
        }
        keyring
    }

    /// Read the files again if any of them has changed.
    pub(crate) fn refresh(&mut self) {
        if Sources::scan(&self.paths) != self.sources {
            *self = Keyring::load(std::mem::take(&mut self.paths));
        }
    }

    pub(crate) fn add(&mut self, key: SignedPublicKey) {
        let fpr = key.primary_key.fingerprint();
        if self.certs.iter().any(|cert| cert.fingerprint() == fpr) {
            return;
        }
        let index = self.certs.len();
        let cert = Cert::new(key);
        for email in &cert.emails {
            self.by_email.entry(email.clone()).or_default().push(index);
        }
        let ids = std::iter::once(cert.key.primary_key.legacy_key_id()).chain(
            cert.key
                .public_subkeys
                .iter()
                .map(|sub| sub.key.legacy_key_id()),
        );
        for id in ids {
            self.by_key_id
                .entry(id.as_ref().to_vec())
                .or_default()
                .push(index);
        }
        self.certs.push(Arc::new(cert));
    }

    /// Certificates with a user ID for `email`.
    pub(crate) fn by_email(&self, email: &str) -> Vec<Arc<Cert>> {
        let wanted = email.trim().to_ascii_lowercase();
        self.by_email
            .get(&wanted)
            .into_iter()
            .flatten()
            .map(|&index| Arc::clone(&self.certs[index]))
            .collect()
    }

    /// The certificate and component named by `id`.
    pub(crate) fn find(&self, id: &KeyRef) -> Option<(Arc<Cert>, KeyPart)> {
        let key_id = match id {
            KeyRef::Id(id) => id.as_ref().to_vec(),
            KeyRef::Fingerprint(fpr) => legacy_id_of(fpr)?,
        };
        self.by_key_id
            .get(&key_id)
            .into_iter()
            .flatten()
            .find_map(|&index| {
                let cert = &self.certs[index];
                cert.component(id).map(|part| (Arc::clone(cert), part))
            })
    }

    /// The certificate whose primary fingerprint is `fpr`, written in hex.
    pub(crate) fn by_fingerprint_hex(&self, fpr: &str) -> Option<Arc<Cert>> {
        let wanted: String = fpr
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_uppercase()
            .trim_start_matches("0X")
            .to_string();
        self.certs
            .iter()
            .find(|cert| {
                let hex = cert.fingerprint_hex();
                hex == wanted || (wanted.len() >= 16 && hex.ends_with(&wanted))
            })
            .cloned()
    }
}

/// The key ID a v4 fingerprint implies: its last eight octets.  v6 key IDs are
/// the first eight.
fn legacy_id_of(fpr: &Fingerprint) -> Option<Vec<u8>> {
    let bytes = fpr.as_bytes();
    match fpr.version() {
        Some(KeyVersion::V4) if bytes.len() == 20 => Some(bytes[12..].to_vec()),
        Some(KeyVersion::V6) if bytes.len() == 32 => Some(bytes[..8].to_vec()),
        _ => None,
    }
}

/// Every certificate in one file, whatever its format: a GnuPG keybox, a
/// binary keyring, or ASCII armor.  Secret keys contribute their public half.
pub(crate) fn read_certificates(path: &Path, data: &[u8]) -> Vec<SignedPublicKey> {
    if let Some(blocks) = keybox_blocks(data) {
        return blocks.into_iter().flat_map(parse_public).collect();
    }
    let _ = path;
    let armored = data.starts_with(b"-----") || data.trim_ascii_start().starts_with(b"-----");
    if armored {
        let mut keys = Vec::new();
        if let Ok((iter, _)) = SignedPublicKey::from_armor_many(Cursor::new(data)) {
            keys.extend(iter.flatten());
        }
        if keys.is_empty()
            && let Ok((iter, _)) = SignedSecretKey::from_armor_many(Cursor::new(data))
        {
            keys.extend(iter.flatten().map(|key| key.to_public_key()));
        }
        return keys;
    }
    let keys = parse_public(data);
    if !keys.is_empty() {
        return keys;
    }
    SignedSecretKey::from_bytes_many(Cursor::new(data))
        .map(|iter| iter.flatten().map(|key| key.to_public_key()).collect())
        .unwrap_or_default()
}

fn parse_public(data: &[u8]) -> Vec<SignedPublicKey> {
    // GnuPG keeps its own trust packets after keys, user IDs and signatures,
    // in the keybox and in old-style keyrings alike.  They mean nothing
    // outside GnuPG, and the certificate parser stops at the first one, so
    // they are dropped before the packets are put together.
    let packets = PacketParser::new(BufReader::new(data))
        .filter(|packet| !matches!(packet, Ok(packet) if packet.tag() == Tag::Trust));
    // Keys the library cannot make sense of -- v3, or an algorithm it does
    // not know -- are skipped one by one rather than costing the whole file.
    SignedPublicKeyParser::from_packets(packets.peekable())
        .flatten()
        .collect()
}

/// The OpenPGP keyblocks in a GnuPG keybox (`pubring.kbx`), or `None` if
/// `data` is not one.
///
/// A keybox is a run of blobs, each starting with its length (four octets,
/// big-endian) and type.  The first is a header blob saying `KBXf`; OpenPGP
/// blobs (type 2) give the offset and length of the keyblock inside them.
fn keybox_blocks(data: &[u8]) -> Option<Vec<&[u8]>> {
    const HEADER: u8 = 1;
    const OPENPGP: u8 = 2;

    let first_len = u32::from_be_bytes(data.get(0..4)?.try_into().ok()?) as usize;
    if data.get(4) != Some(&HEADER) || data.get(8..12) != Some(b"KBXf") || first_len < 12 {
        return None;
    }

    let mut blocks = Vec::new();
    let mut pos = 0;
    while pos + 5 <= data.len() {
        let len = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        if len < 5 || pos + len > data.len() {
            break;
        }
        let blob = &data[pos..pos + len];
        if blob[4] == OPENPGP && blob.len() >= 16 {
            let offset = u32::from_be_bytes(blob[8..12].try_into().ok()?) as usize;
            let size = u32::from_be_bytes(blob[12..16].try_into().ok()?) as usize;
            if let Some(block) = offset
                .checked_add(size)
                .and_then(|end| blob.get(offset..end))
            {
                blocks.push(block);
            }
        }
        pos += len;
    }
    Some(blocks)
}

/// The public key material of a key, in a form that can be compared with what
/// gpg-agent reports: the modulus for RSA, the point for elliptic curves, the
/// public value `y` for ElGamal.
pub(crate) fn key_material(params: &PublicParams) -> Option<Vec<u8>> {
    let bytes = params.to_bytes().ok()?;
    match params {
        PublicParams::RSA(_) => first_mpi(&bytes).map(|n| strip_zeros(n).to_vec()),
        PublicParams::ECDH(_) | PublicParams::ECDSA(_) | PublicParams::EdDSALegacy(_) => {
            // One octet of OID length, the OID, then the point as an MPI.
            let oid_len = *bytes.first()? as usize;
            let rest = bytes.get(1 + oid_len..)?;
            first_mpi(rest).map(normalize_point)
        }
        PublicParams::Ed25519(_) | PublicParams::X25519(_) => {
            let mut point = vec![0x40];
            point.extend_from_slice(&bytes);
            Some(point)
        }
        // `p`, `g`, then `y`.  Keys made by one program often share `p` and
        // `g`; `y` is the key's own.
        PublicParams::Elgamal(_) => nth_mpi(&bytes, 2).map(|y| strip_zeros(y).to_vec()),
        _ => None,
    }
}

fn first_mpi(data: &[u8]) -> Option<&[u8]> {
    let bits = u16::from_be_bytes(data.get(0..2)?.try_into().ok()?) as usize;
    data.get(2..2 + bits.div_ceil(8))
}

/// The MPI at `index` in a run of them.
fn nth_mpi(mut data: &[u8], index: usize) -> Option<&[u8]> {
    for _ in 0..index {
        let skip = 2 + first_mpi(data)?.len();
        data = data.get(skip..)?;
    }
    first_mpi(data)
}

pub(crate) fn strip_zeros(data: &[u8]) -> &[u8] {
    let start = data.iter().position(|b| *b != 0).unwrap_or(data.len());
    &data[start..]
}

/// A 32-octet point without its `0x40` prefix gets one, so that both ways of
/// writing a Curve25519 point compare equal.
pub(crate) fn normalize_point(point: &[u8]) -> Vec<u8> {
    if point.len() == 32 {
        let mut out = vec![0x40];
        out.extend_from_slice(point);
        return out;
    }
    point.to_vec()
}

pub(crate) fn user_id_text(id: &packet::UserId) -> String {
    String::from_utf8_lossy(id.id()).into_owned()
}

/// The address in a user ID: between angle brackets, or the whole thing when
/// it is nothing but an address.
pub(crate) fn email_of(user_id: &str) -> Option<String> {
    let candidate = match (user_id.rfind('<'), user_id.rfind('>')) {
        (Some(start), Some(end)) if start < end => &user_id[start + 1..end],
        _ => user_id,
    };
    let candidate = candidate.trim();
    (candidate.contains('@') && !candidate.contains(' ')).then(|| candidate.to_ascii_lowercase())
}

pub(crate) fn hex_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_come_out_of_user_ids() {
        assert_eq!(
            email_of("Alice Example <Alice@Example.com>").as_deref(),
            Some("alice@example.com")
        );
        assert_eq!(
            email_of("bob@example.com").as_deref(),
            Some("bob@example.com")
        );
        assert_eq!(email_of("Just A Name"), None);
    }

    #[test]
    fn a_keybox_yields_its_openpgp_blocks() {
        fn blob(kind: u8, payload: &[u8]) -> Vec<u8> {
            // length, type, version, flags, offset, size, then the payload.
            let header_len = 16;
            let len = (header_len + payload.len()) as u32;
            let mut out = len.to_be_bytes().to_vec();
            out.extend([kind, 1, 0, 0]);
            out.extend((header_len as u32).to_be_bytes());
            out.extend((payload.len() as u32).to_be_bytes());
            out.extend_from_slice(payload);
            out
        }
        let mut kbx = vec![0, 0, 0, 32, 1, 1, 0, 0];
        kbx.extend(b"KBXf");
        kbx.resize(32, 0);
        kbx.extend(blob(2, b"first"));
        kbx.extend(blob(3, b"x509"));
        kbx.extend(blob(2, b"second"));

        let blocks = keybox_blocks(&kbx).expect("recognised as a keybox");
        assert_eq!(blocks, vec![&b"first"[..], &b"second"[..]]);
        assert!(keybox_blocks(b"-----BEGIN PGP").is_none());
    }
}
