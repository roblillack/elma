//! OpenPGP: reading signed and encrypted mail, and signing and encrypting it.
//!
//! The cryptography is the `pgp` crate's.  What this module adds is where the
//! keys come from -- GnuPG's own keyring and agent, so that whatever `gpg` can
//! do with a key, Elma can too, smartcards and cached passphrases included --
//! and the judgement calls a mail client has to make: which key signs for an
//! address, which keys a message is encrypted to, and what to tell the reader
//! about a signature.
//!
//! The MIME side, PGP/MIME (RFC 3156) and inline PGP, is in
//! [`crate::backend::mime`].

mod agent;
pub(crate) mod keyring;
pub mod prompt;

use agent::Agent;
use anyhow::{Context, Result, anyhow, bail};
use keyring::{Cert, KeyPart, KeyRef, Keyring, Usage};
use pgp::{
    composed::{
        ArmorOptions, CleartextSignedMessage, Deserializable, DetachedSignature, Esk, Message,
        MessageBuilder, PlainSessionKey, SignedSecretKey,
    },
    crypto::{hash::HashAlgorithm, sym::SymmetricKeyAlgorithm},
    packet::{PublicKeyEncryptedSessionKey, Signature},
    types::{DecryptionKey, EskType, KeyDetails, Password, PkeskVersion, SecretParams, SigningKey},
};
use prompt::{PassphraseRequest, PassphraseSource};
use std::{
    io::{BufReader, Cursor},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};
use zeroize::Zeroizing;

use crate::model::{SignatureState, SignatureVerdict};

/// The digest every signature Elma makes is computed with.  PGP/MIME names it
/// in the `micalg` parameter, so it is fixed rather than taken from the key.
pub(crate) const SIGNATURE_HASH: HashAlgorithm = HashAlgorithm::Sha256;
pub(crate) const MICALG: &str = "pgp-sha256";

/// How many times a wrong passphrase is asked for again.
const PASSPHRASE_ATTEMPTS: usize = 3;

/// What the configuration says about OpenPGP.
#[derive(Clone, Debug)]
pub struct Settings {
    /// GnuPG's home directory: `$GNUPGHOME`, or `~/.gnupg`.
    pub gnupg_home: Option<PathBuf>,
    /// Whether to ask gpg-agent for secret-key operations.
    pub use_agent: bool,
    /// Ask for passphrases in Elma rather than through the agent's pinentry.
    pub loopback_pinentry: bool,
    /// More key files, public or secret, armored or binary.
    pub key_files: Vec<PathBuf>,
    /// Sign new messages unless told otherwise.
    pub sign_by_default: bool,
    /// Encrypt new messages unless told otherwise.
    pub encrypt_by_default: bool,
    /// The key to sign with, by fingerprint, instead of the one for the
    /// sender's address.
    pub signing_key: Option<String>,
}

impl Settings {
    /// GnuPG's defaults: whatever `$GNUPGHOME` names, else `~/.gnupg`.
    pub fn from_environment() -> Self {
        let gnupg_home = std::env::var_os("GNUPGHOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|home| home.join(".gnupg")));
        Settings {
            gnupg_home,
            use_agent: true,
            loopback_pinentry: true,
            key_files: Vec::new(),
            sign_by_default: false,
            encrypt_by_default: false,
            signing_key: None,
        }
    }

    fn keyring_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(home) = &self.gnupg_home {
            // The file gpg itself reads: an existing `pubring.gpg` wins, and
            // `pubring.kbx` is only where a home without one keeps its keys.
            // GnuPG 2.1 does not migrate `pubring.gpg`, so a home can hold
            // both, with the keys in the old file and an empty keybox some
            // other tool created beside it.  (gpg tells the two formats apart
            // by content, not name, and so does `Keyring::load`.)
            let legacy = home.join("pubring.gpg");
            if legacy.exists() {
                paths.push(legacy);
            } else {
                paths.push(home.join("pubring.kbx"));
            }
        }
        paths.extend(self.key_files.iter().cloned());
        paths
    }
}

static CONTEXT: OnceLock<Arc<Crypto>> = OnceLock::new();

/// Make `crypto` the context every backend uses.  Called once, at start-up.
pub fn install(crypto: Crypto) {
    let _ = CONTEXT.set(Arc::new(crypto));
}

/// The installed context, if OpenPGP is enabled.
pub fn context() -> Option<Arc<Crypto>> {
    CONTEXT.get().cloned()
}

/// Keys, and the means to use them.
pub struct Crypto {
    settings: Settings,
    keys: Mutex<Option<Keys>>,
    /// One connection, reused: the agent remembers nothing per connection that
    /// matters, but connecting costs a process spawn when it is not running.
    agent: Mutex<Option<Agent>>,
    passphrases: Mutex<Box<dyn PassphraseSource + Send>>,
}

struct Keys {
    keyring: Keyring,
    secrets: Vec<SignedSecretKey>,
}

/// A decrypted message.
pub(crate) struct Decrypted {
    pub(crate) data: Vec<u8>,
    pub(crate) signatures: Vec<SignatureState>,
}

/// Something that can make a signature: the agent, or a key read from a file.
enum Signer<'a> {
    Agent {
        crypto: &'a Crypto,
        keygrip: String,
        part: Box<KeyPart>,
    },
    Soft {
        crypto: &'a Crypto,
        secret: Box<SoftSecret>,
        description: String,
    },
}

#[derive(Clone)]
enum SoftSecret {
    Primary(pgp::packet::SecretKey),
    Sub(pgp::packet::SecretSubkey),
}

impl Crypto {
    /// A context using the keys `settings` names, asking `passphrases` for
    /// what it cannot unlock otherwise.
    pub fn new(settings: Settings, passphrases: Box<dyn PassphraseSource + Send>) -> Self {
        Crypto {
            settings,
            keys: Mutex::new(None),
            agent: Mutex::new(None),
            passphrases: Mutex::new(passphrases),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Run `f` with the keys, reading them first or again if the files have
    /// changed.
    fn with_keys<T>(&self, f: impl FnOnce(&Keys) -> T) -> T {
        let mut guard = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(keys) => keys.keyring.refresh(),
            None => {
                let keyring = Keyring::load(self.settings.keyring_paths());
                let secrets = self
                    .settings
                    .key_files
                    .iter()
                    .flat_map(read_secret_keys)
                    .collect();
                *guard = Some(Keys { keyring, secrets });
            }
        }
        f(guard.as_ref().expect("loaded above"))
    }

    /// Run `f` with a connection to the agent, connecting if need be.  A
    /// connection that fails is dropped, and the next call starts afresh.
    fn with_agent<T>(&self, f: impl FnOnce(&mut Agent) -> Result<T>) -> Result<T> {
        if !self.settings.use_agent {
            bail!("gpg-agent is disabled in the configuration");
        }
        let home = self
            .settings
            .gnupg_home
            .clone()
            .ok_or_else(|| anyhow!("no GnuPG home directory"))?;
        let mut guard = self.agent.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(Agent::connect(&home, self.settings.loopback_pinentry)?);
        }
        let result = f(guard.as_mut().expect("connected above"));
        if result.is_err() {
            *guard = None;
        }
        result
    }

    /// The agent's keygrip for `part`'s secret; `None` when the agent has no
    /// such key or is not to be used, an error when it cannot be asked.
    fn agent_keygrip(&self, part: &KeyPart) -> Result<Option<String>> {
        if !self.settings.use_agent {
            return Ok(None);
        }
        self.with_agent(|agent| agent.keygrip_for(part.public_params()))
    }

    fn soft_secret(&self, part: &KeyPart) -> Option<(SoftSecret, String)> {
        let fpr = part.fingerprint();
        self.with_keys(|keys| {
            keys.secrets.iter().find_map(|secret| {
                let name = keyring::user_id_text(
                    &secret.details.users.first().map(|user| user.id.clone())?,
                );
                if secret.primary_key.fingerprint() == fpr {
                    return Some((SoftSecret::Primary(secret.primary_key.clone()), name));
                }
                secret
                    .secret_subkeys
                    .iter()
                    .find(|sub| sub.key.fingerprint() == fpr)
                    .map(|sub| (SoftSecret::Sub(sub.key.clone()), name))
            })
        })
    }

    fn signer_for(&self, part: &KeyPart) -> Result<Option<Signer<'_>>> {
        if let Some((secret, name)) = self.soft_secret(part) {
            return Ok(Some(Signer::Soft {
                crypto: self,
                secret: Box::new(secret),
                description: format!("Unlock the OpenPGP key of {name}"),
            }));
        }
        Ok(self.agent_keygrip(part)?.map(|keygrip| Signer::Agent {
            crypto: self,
            keygrip,
            part: Box::new(part.clone()),
        }))
    }

    /// Ask for a passphrase, through whatever this context was set up with.
    fn ask(&self, request: &PassphraseRequest) -> Option<Zeroizing<String>> {
        let mut source = self.passphrases.lock().unwrap_or_else(|e| e.into_inner());
        source.passphrase(request)
    }

    /// Run `f` with a key's passphrase: none at all if the key has none,
    /// otherwise asking, and asking again while the answer is wrong.
    fn with_passphrase<T>(
        &self,
        protected: bool,
        description: &str,
        mut f: impl FnMut(&Password) -> pgp::errors::Result<T>,
    ) -> Result<T> {
        if !protected {
            return f(&Password::empty()).map_err(|err| anyhow!("{err}"));
        }
        let mut last_error = None;
        for attempt in 0..PASSPHRASE_ATTEMPTS {
            let request = PassphraseRequest {
                description: description.to_string(),
                retry: attempt > 0,
            };
            let Some(passphrase) = self.ask(&request) else {
                bail!("no passphrase given");
            };
            let password = Password::from(passphrase.as_str());
            match f(&password) {
                Ok(value) => return Ok(value),
                Err(err) => last_error = Some(err),
            }
        }
        Err(anyhow!(
            "could not unlock the key: {}",
            last_error.map(|err| err.to_string()).unwrap_or_default()
        ))
    }

    // --- Reading -------------------------------------------------------------

    /// Verify a detached signature (`signature`, armored or binary) over
    /// `data`.  `sender` is the address the message says it is from.
    pub(crate) fn verify_detached(
        &self,
        data: &[u8],
        signature: &[u8],
        sender: Option<&str>,
    ) -> Vec<SignatureState> {
        let signatures = match parse_detached(signature) {
            Ok(signatures) if !signatures.is_empty() => signatures,
            Ok(_) => return vec![unreadable_signature("no signature found")],
            Err(err) => return vec![unreadable_signature(&err.to_string())],
        };
        signatures
            .iter()
            .map(|sig| {
                self.judge(&sig.signature, sender, |part| {
                    sig.verify(part, data).map_err(|err| err.to_string())
                })
            })
            .collect()
    }

    /// Verify an inline, clear-signed message, returning the signed text.
    pub(crate) fn verify_cleartext(
        &self,
        armored: &str,
        sender: Option<&str>,
    ) -> Result<(String, Vec<SignatureState>)> {
        let (message, _) = CleartextSignedMessage::from_string(armored)
            .map_err(|err| anyhow!("unreadable signed text: {err}"))?;
        let states = message
            .signatures()
            .iter()
            .map(|sig| {
                self.judge(sig, sender, |part| {
                    // Every signature is tried against the key its issuer
                    // names; the library reports which one matched.
                    message
                        .verify(part)
                        .map(|_| ())
                        .map_err(|err| err.to_string())
                })
            })
            .collect();
        Ok((message.text().to_string(), states))
    }

    /// Decrypt an OpenPGP message (armored or binary), verifying any
    /// signature inside.  A message that turns out to be signed but not
    /// encrypted is read and verified all the same.
    pub(crate) fn decrypt(&self, data: &[u8], sender: Option<&str>) -> Result<Decrypted> {
        let message = parse_message(data)?;

        let message = match message {
            Message::Encrypted { ref esk, .. } => {
                let session_key = self.find_session_key(esk)?;
                message
                    .decrypt_with_session_key(session_key)
                    .map_err(|err| anyhow!("decrypting the message: {err}"))?
            }
            other => other,
        };
        self.read_message(message, sender)
    }

    fn read_message(&self, mut message: Message<'_>, sender: Option<&str>) -> Result<Decrypted> {
        // Compression can wrap the signed message, and gpg compresses by
        // default.
        for _ in 0..4 {
            if message.is_compressed() {
                message = message
                    .decompress()
                    .map_err(|err| anyhow!("decompressing the message: {err}"))?;
            }
        }
        let data = message
            .as_data_vec()
            .map_err(|err| anyhow!("reading the decrypted message: {err}"))?;

        let mut signatures = Vec::new();
        if let Message::Signed { ref reader, .. } = message {
            for index in 0..reader.num_signatures() {
                let Some(sig) = reader.signature(index) else {
                    continue;
                };
                signatures.push(self.judge(sig, sender, |part| {
                    message
                        .verify_nested_explicit(index, part)
                        .map(|_| ())
                        .map_err(|err| err.to_string())
                }));
            }
        }
        Ok(Decrypted { data, signatures })
    }

    /// Find the key a signature claims, run `check` against it, and say what
    /// the reader should make of the result.
    fn judge(
        &self,
        sig: &Signature,
        sender: Option<&str>,
        check: impl FnOnce(&KeyPart) -> std::result::Result<(), String>,
    ) -> SignatureState {
        let issuers: Vec<KeyRef> = sig
            .issuer_fingerprint()
            .into_iter()
            .map(|fpr| KeyRef::Fingerprint(fpr.clone()))
            .chain(sig.issuer_key_id().into_iter().map(|id| KeyRef::Id(*id)))
            .collect();
        let key_text = issuers.first().map(KeyRef::hex).unwrap_or_default();

        let found = self.with_keys(|keys| issuers.iter().find_map(|id| keys.keyring.find(id)));
        let Some((cert, part)) = found else {
            return SignatureState {
                verdict: SignatureVerdict::UnknownKey,
                signer: None,
                key: key_text,
                matches_sender: false,
            };
        };

        let signer = cert.display_name(sender);
        let matches_sender = sender.is_some_and(|email| cert.valid_user_for(email).is_some());
        let key = cert.fingerprint_hex();
        let verdict = match check(&part) {
            Err(reason) => SignatureVerdict::Bad(reason),
            Ok(()) => match cert.may_sign(&part, signature_time(sig)) {
                Ok(()) => SignatureVerdict::Good,
                Err(problem) => SignatureVerdict::Unusable(problem),
            },
        };
        SignatureState {
            verdict,
            signer,
            key,
            matches_sender,
        }
    }

    fn find_session_key(&self, esks: &[Esk]) -> Result<PlainSessionKey> {
        let pkesks: Vec<&PublicKeyEncryptedSessionKey> = esks
            .iter()
            .filter_map(|esk| match esk {
                Esk::PublicKeyEncryptedSessionKey(pkesk) => Some(pkesk),
                Esk::SymKeyEncryptedSessionKey(_) => None,
            })
            .collect();
        if pkesks.is_empty() {
            bail!("the message is encrypted with a password, not to a key");
        }

        let mut errors = Vec::new();
        let mut recipients = Vec::new();
        for pkesk in &pkesks {
            let Ok(values) = pkesk.values() else { continue };
            let typ = match pkesk.version() {
                PkeskVersion::V3 => EskType::V3_4,
                PkeskVersion::V6 => EskType::V6,
                PkeskVersion::Other(_) => continue,
            };
            let id = match (pkesk.fingerprint(), pkesk.id()) {
                (Ok(Some(fpr)), _) => KeyRef::Fingerprint(fpr.clone()),
                (_, Ok(id)) => KeyRef::Id(*id),
                _ => continue,
            };
            recipients.push(id.hex());
            let Some((_, part)) = self.with_keys(|keys| keys.keyring.find(&id)) else {
                continue;
            };
            match self.decrypt_session_key(&part, values, typ) {
                Ok(Some(key)) => return Ok(key),
                Ok(None) => {}
                Err(err) => errors.push(format!("{err:#}")),
            }
        }

        if let Some(err) = errors.into_iter().next() {
            bail!(err);
        }
        bail!(
            "no secret key for {}",
            if recipients.is_empty() {
                "any recipient".to_string()
            } else {
                recipients.join(", ")
            }
        )
    }

    /// Recover a session key with whatever holds the secret for `part`;
    /// `None` when nothing does.
    fn decrypt_session_key(
        &self,
        part: &KeyPart,
        values: &pgp::types::PkeskBytes,
        typ: EskType,
    ) -> Result<Option<PlainSessionKey>> {
        if let Some((secret, name)) = self.soft_secret(part) {
            let description = format!("Unlock the OpenPGP key of {name} to read this message");
            let protected = secret.is_protected();
            let key = self.with_passphrase(protected, &description, |password| {
                let result = match &secret {
                    SoftSecret::Primary(key) => DecryptionKey::decrypt(key, password, values, typ),
                    SoftSecret::Sub(key) => DecryptionKey::decrypt(key, password, values, typ),
                };
                // The outer error is the passphrase; the inner, the message.
                result
            })?;
            return key.map(Some).map_err(|err| anyhow!("{err}"));
        }

        let Some(keygrip) = self.agent_keygrip(part)? else {
            return Ok(None);
        };
        let mut asking = Asking(self);
        self.with_agent(|agent| agent.decrypt(&keygrip, part, values, typ, &mut asking))
            .map(Some)
    }

    // --- Writing -------------------------------------------------------------

    /// The key that signs for `from`.
    fn signing_key(&self, from: &str) -> Result<(Arc<Cert>, KeyPart)> {
        let now = SystemTime::now();
        let candidates: Vec<Arc<Cert>> = self.with_keys(|keys| match &self.settings.signing_key {
            Some(fpr) => keys.keyring.by_fingerprint_hex(fpr).into_iter().collect(),
            None => keys
                .keyring
                .by_email(from)
                .into_iter()
                .filter(|cert| cert.valid_user_for(from).is_some())
                .collect(),
        });
        if candidates.is_empty() {
            match &self.settings.signing_key {
                Some(fpr) => bail!("the configured signing key {fpr} is not in the keyring"),
                None => bail!("there is no OpenPGP key for {from}"),
            }
        }
        let mut agent_error = None;
        for cert in &candidates {
            for part in cert.usable_keys(Usage::Sign, now) {
                match self.signer_for(&part) {
                    Ok(Some(_)) => return Ok((Arc::clone(cert), part)),
                    Ok(None) => {}
                    Err(err) => agent_error = Some(err),
                }
            }
        }
        match agent_error {
            Some(err) => Err(err.context(format!("looking for the secret key of {from}"))),
            None => bail!("there is no usable secret key to sign for {from}"),
        }
    }

    /// Make a detached signature over `data`, as `from`.
    pub(crate) fn sign_detached(&self, data: &[u8], from: &str) -> Result<String> {
        let (_, part) = self.signing_key(from)?;
        let signer = self
            .signer_for(&part)?
            .ok_or_else(|| anyhow!("the secret key went away"))?;
        let signature = DetachedSignature::sign_binary_data(
            rand::thread_rng(),
            &signer,
            &Password::empty(),
            SIGNATURE_HASH,
            data,
        )
        .map_err(|err| anyhow!("signing: {err}"))?;
        signature
            .to_armored_string(ArmorOptions::default())
            .map_err(|err| anyhow!("armoring the signature: {err}"))
    }

    /// The keys to encrypt to for each of `recipients`, failing with the list
    /// of addresses that have none.
    fn encryption_keys(&self, recipients: &[String]) -> Result<Vec<KeyPart>> {
        let now = SystemTime::now();
        let mut keys = Vec::new();
        let mut missing = Vec::new();
        self.with_keys(|store| {
            for address in recipients {
                let mut found = false;
                for cert in store.keyring.by_email(address) {
                    if cert.valid_user_for(address).is_none() {
                        continue;
                    }
                    if let Some(key) = cert.usable_keys(Usage::Encrypt, now).into_iter().next() {
                        keys.push(key);
                        found = true;
                    }
                }
                if !found {
                    missing.push(address.clone());
                }
            }
        });
        if !missing.is_empty() {
            bail!("no usable OpenPGP key for {}", missing.join(", "));
        }
        let mut seen = std::collections::HashSet::new();
        keys.retain(|key| seen.insert(key.fingerprint()));
        Ok(keys)
    }

    /// Encrypt `data` to `recipients`, and to `from` so the sender can read
    /// their own copy; with `sign`, signed by `from` inside the encryption.
    pub(crate) fn encrypt(
        &self,
        data: &[u8],
        recipients: &[String],
        from: &str,
        sign: bool,
    ) -> Result<String> {
        // A recipient without a key is fatal.  The sender without one is not:
        // the message can still go out, only its copy in Sent stays unreadable.
        let mut keys = self.encryption_keys(recipients)?;
        if let Ok(own) = self.encryption_keys(&[from.to_string()]) {
            for key in own {
                if !keys
                    .iter()
                    .any(|known| known.fingerprint() == key.fingerprint())
                {
                    keys.push(key);
                }
            }
        }

        let signer = if sign {
            let (_, part) = self.signing_key(from)?;
            Some(
                self.signer_for(&part)?
                    .ok_or_else(|| anyhow!("the secret key went away"))?,
            )
        } else {
            None
        };

        let mut rng = rand::thread_rng();
        let mut builder = MessageBuilder::from_bytes("", data.to_vec())
            .seipd_v1(&mut rng, SymmetricKeyAlgorithm::AES256);
        for key in &keys {
            builder.encrypt_to_key(&mut rng, key).map_err(|err| {
                anyhow!(
                    "encrypting to {}: {err}",
                    keyring::hex_upper(key.fingerprint().as_bytes())
                )
            })?;
        }
        if let Some(signer) = &signer {
            builder.sign(signer, Password::empty(), SIGNATURE_HASH);
        }
        builder
            .to_armored_string(&mut rng, ArmorOptions::default())
            .map_err(|err| anyhow!("encrypting: {err}"))
    }

    /// Encrypt `data` to `from` alone: for drafts.
    pub(crate) fn encrypt_to_self(&self, data: &[u8], from: &str) -> Result<String> {
        let keys = self
            .encryption_keys(&[from.to_string()])
            .context("encrypting the draft to yourself")?;
        let mut rng = rand::thread_rng();
        let mut builder = MessageBuilder::from_bytes("", data.to_vec())
            .seipd_v1(&mut rng, SymmetricKeyAlgorithm::AES256);
        for key in &keys {
            builder
                .encrypt_to_key(&mut rng, key)
                .map_err(|err| anyhow!("encrypting: {err}"))?;
        }
        builder
            .to_armored_string(&mut rng, ArmorOptions::default())
            .map_err(|err| anyhow!("encrypting: {err}"))
    }

    /// The addresses among `recipients` -- as typed into the compose
    /// fields, names and all -- that have no key to encrypt to.
    pub(crate) fn missing_keys(&self, recipients: &[String]) -> Vec<String> {
        recipients
            .iter()
            .map(|recipient| bare_address(recipient))
            .filter(|address| self.encryption_keys(std::slice::from_ref(address)).is_err())
            .collect()
    }
}

/// Asks through a [`Crypto`]'s passphrase source.
struct Asking<'a>(&'a Crypto);

impl PassphraseSource for Asking<'_> {
    fn passphrase(&mut self, request: &PassphraseRequest) -> Option<Zeroizing<String>> {
        self.0.ask(request)
    }
}

impl SoftSecret {
    fn is_protected(&self) -> bool {
        let params = match self {
            SoftSecret::Primary(key) => key.secret_params(),
            SoftSecret::Sub(key) => key.secret_params(),
        };
        !matches!(params, SecretParams::Plain(_))
    }
}

impl std::fmt::Debug for Signer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Signer::Agent { keygrip, .. } => write!(f, "Signer::Agent({keygrip})"),
            Signer::Soft { description, .. } => write!(f, "Signer::Soft({description})"),
        }
    }
}

impl KeyDetails for Signer<'_> {
    fn version(&self) -> pgp::types::KeyVersion {
        self.details().version()
    }
    fn legacy_key_id(&self) -> pgp::types::KeyId {
        self.details().legacy_key_id()
    }
    fn fingerprint(&self) -> pgp::types::Fingerprint {
        self.details().fingerprint()
    }
    fn algorithm(&self) -> pgp::crypto::public_key::PublicKeyAlgorithm {
        self.details().algorithm()
    }
    fn created_at(&self) -> pgp::types::Timestamp {
        self.details().created_at()
    }
    fn legacy_v3_expiration_days(&self) -> Option<u16> {
        self.details().legacy_v3_expiration_days()
    }
    fn public_params(&self) -> &pgp::types::PublicParams {
        self.details().public_params()
    }
}

impl Signer<'_> {
    fn details(&self) -> &dyn KeyDetails {
        match self {
            Signer::Agent { part, .. } => part.as_ref(),
            Signer::Soft { secret, .. } => match secret.as_ref() {
                SoftSecret::Primary(key) => key,
                SoftSecret::Sub(key) => key,
            },
        }
    }
}

impl SigningKey for Signer<'_> {
    fn sign(
        &self,
        _key_pw: &Password,
        hash: HashAlgorithm,
        digest: &[u8],
    ) -> pgp::errors::Result<pgp::types::SignatureBytes> {
        let result =
            match self {
                Signer::Agent {
                    crypto,
                    keygrip,
                    part,
                } => {
                    let mut asking = Asking(crypto);
                    crypto.with_agent(|agent| {
                        agent.sign(keygrip, part.algorithm(), hash, digest, &mut asking)
                    })
                }
                Signer::Soft {
                    crypto,
                    secret,
                    description,
                } => crypto.with_passphrase(secret.is_protected(), description, |password| {
                    match secret.as_ref() {
                        SoftSecret::Primary(key) => SigningKey::sign(key, password, hash, digest),
                        SoftSecret::Sub(key) => SigningKey::sign(key, password, hash, digest),
                    }
                }),
            };
        result.map_err(|err| pgp::errors::Error::Message {
            message: format!("{err:#}"),
            backtrace: None,
        })
    }

    fn hash_alg(&self) -> HashAlgorithm {
        SIGNATURE_HASH
    }
}

/// `Name <addr>` down to `addr`.
fn bare_address(recipient: &str) -> String {
    recipient
        .parse::<lettre::message::Mailbox>()
        .map(|mailbox| mailbox.email.to_string())
        .unwrap_or_else(|_| recipient.trim().to_string())
}

fn unreadable_signature(reason: &str) -> SignatureState {
    SignatureState {
        verdict: SignatureVerdict::Bad(format!("unreadable signature: {reason}")),
        signer: None,
        key: String::new(),
        matches_sender: false,
    }
}

/// When a signature says it was made, for checking the key was valid then.
fn signature_time(sig: &Signature) -> SystemTime {
    sig.created()
        .map(SystemTime::from)
        .unwrap_or_else(SystemTime::now)
}

fn is_armored(data: &[u8]) -> bool {
    data.trim_ascii_start().starts_with(b"-----BEGIN PGP")
}

fn parse_detached(data: &[u8]) -> Result<Vec<DetachedSignature>> {
    if is_armored(data) {
        let (signatures, _) = DetachedSignature::from_armor_many(Cursor::new(data))
            .map_err(|err| anyhow!("{err}"))?;
        return signatures
            .collect::<pgp::errors::Result<Vec<_>>>()
            .map_err(|err| anyhow!("{err}"));
    }
    DetachedSignature::from_bytes_many(Cursor::new(data))
        .map_err(|err| anyhow!("{err}"))?
        .collect::<pgp::errors::Result<Vec<_>>>()
        .map_err(|err| anyhow!("{err}"))
}

fn parse_message(data: &[u8]) -> Result<Message<'_>> {
    if is_armored(data) {
        let (message, _) = Message::from_armor(BufReader::new(Cursor::new(data)))
            .map_err(|err| anyhow!("unreadable OpenPGP message: {err}"))?;
        return Ok(message);
    }
    Message::from_bytes(BufReader::new(Cursor::new(data)))
        .map_err(|err| anyhow!("unreadable OpenPGP message: {err}"))
}

fn read_secret_keys(path: &PathBuf) -> Vec<SignedSecretKey> {
    let Ok(data) = std::fs::read(path) else {
        return Vec::new();
    };
    if is_armored(&data) {
        return SignedSecretKey::from_armor_many(Cursor::new(&data))
            .map(|(keys, _)| keys.flatten().collect())
            .unwrap_or_default();
    }
    SignedSecretKey::from_bytes_many(Cursor::new(&data))
        .map(|keys| keys.flatten().collect())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests;
