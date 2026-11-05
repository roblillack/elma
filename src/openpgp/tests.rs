//! Round trips through the whole of it: keys generated here, a message built
//! the way the backends build one, sent through lettre's formatter, parsed back
//! with mailparse and read the way the viewer reads it.
//!
//! The keys mostly come from files, the way `[pgp] keys = [...]` provides
//! them.  [`gnupg_interoperability`] does it the other way: a real GnuPG home
//! and agent, and `gpg` on the other end of every message.

use super::{Crypto, Settings, prompt::PassphraseRequest, prompt::PassphraseSource};
use crate::backend::{
    OutgoingAttachment,
    mime::{Protection, build_compose_body_with, build_message_content_with},
};
use crate::model::{EncryptionState, MessageContent, SecurityLevel, SignatureVerdict};
use pgp::{
    composed::{
        ArmorOptions, CleartextSignedMessage, EncryptionCaps, KeyType, SecretKeyParamsBuilder,
        SignedPublicKey, SignedSecretKey, SubkeyParamsBuilder,
    },
    crypto::ecc_curve::ECCCurve,
    types::Password,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use zeroize::Zeroizing;

struct Person {
    address: &'static str,
    secret: SignedSecretKey,
}

fn person(name: &str, address: &'static str, passphrase: Option<&str>) -> Person {
    let mut encrypt = SubkeyParamsBuilder::default();
    encrypt
        .key_type(KeyType::ECDH(ECCCurve::Curve25519Legacy))
        .can_sign(false)
        .can_encrypt(EncryptionCaps::All)
        .passphrase(passphrase.map(str::to_string));
    let mut params = SecretKeyParamsBuilder::default();
    params
        .key_type(KeyType::Ed25519Legacy)
        .can_certify(true)
        .can_sign(true)
        .can_encrypt(EncryptionCaps::None)
        .passphrase(passphrase.map(str::to_string))
        .primary_user_id(format!("{name} <{address}>"))
        .subkeys(vec![encrypt.build().expect("subkey parameters")]);
    let secret = params
        .build()
        .expect("key parameters")
        .generate(rand::thread_rng())
        .expect("key generation");
    Person { address, secret }
}

impl Person {
    fn public(&self) -> SignedPublicKey {
        self.secret.to_public_key()
    }
}

/// Answers every passphrase question from a list, and remembers the questions.
#[derive(Clone, Default)]
struct Answers {
    answers: Arc<Mutex<Vec<&'static str>>>,
    asked: Arc<Mutex<Vec<PassphraseRequest>>>,
}

impl PassphraseSource for Answers {
    fn passphrase(&mut self, request: &PassphraseRequest) -> Option<Zeroizing<String>> {
        self.asked.lock().unwrap().push(request.clone());
        let mut answers = self.answers.lock().unwrap();
        (!answers.is_empty()).then(|| Zeroizing::new(answers.remove(0).to_string()))
    }
}

/// A context for `me`, knowing the certificates of `others`.
fn context_for(me: &Person, others: &[&Person], answers: Answers) -> (tempfile::TempDir, Crypto) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let mut files: Vec<PathBuf> = Vec::new();

    let own = dir.path().join("me.sec.asc");
    std::fs::write(
        &own,
        me.secret
            .to_armored_string(ArmorOptions::default())
            .expect("armoring"),
    )
    .unwrap();
    files.push(own);

    for (index, other) in others.iter().enumerate() {
        let path = dir.path().join(format!("other{index}.asc"));
        std::fs::write(
            &path,
            other
                .public()
                .to_armored_string(ArmorOptions::default())
                .expect("armoring"),
        )
        .unwrap();
        files.push(path);
    }

    let settings = Settings {
        gnupg_home: None,
        use_agent: false,
        loopback_pinentry: true,
        key_files: files,
        sign_by_default: false,
        encrypt_by_default: false,
        signing_key: None,
    };
    (dir, Crypto::new(settings, Box::new(answers)))
}

/// A message from `from` to `to`, built and formatted the way the backends
/// do, protected by `crypto` as `protection` says.
fn compose(
    crypto: &Crypto,
    from: &str,
    to: &str,
    text: &str,
    attachments: Vec<OutgoingAttachment>,
    sign: bool,
    encrypt: bool,
) -> anyhow::Result<Vec<u8>> {
    let protection = Protection {
        sign,
        encrypt,
        from: from.to_string(),
        recipients: vec![to.to_string()],
        draft: false,
    };
    let body = build_compose_body_with(
        text.to_string(),
        format!("<p>{text}</p>"),
        attachments,
        &protection,
        Some(crypto),
    )?;
    let message = lettre::Message::builder()
        .from(from.parse().unwrap())
        .to(to.parse().unwrap())
        .subject("Plans")
        .multipart(body)
        .expect("a valid message");
    Ok(message.formatted())
}

fn read(crypto: &Crypto, raw: &[u8]) -> MessageContent {
    let parsed = mailparse::parse_mail(raw).expect("the message parses");
    build_message_content_with(&parsed, Some(crypto)).expect("the message reads")
}

fn plain_text(content: &MessageContent) -> String {
    String::from_utf8(
        content
            .part("text/plain")
            .expect("a text part")
            .content
            .clone(),
    )
    .unwrap()
}

#[test]
fn a_signed_message_verifies_for_its_recipient() {
    let alice = person("Alice", "alice@example.com", None);
    let bob = person("Bob", "bob@example.com", None);
    let (_a, alice_crypto) = context_for(&alice, &[&bob], Answers::default());
    let (_b, bob_crypto) = context_for(&bob, &[&alice], Answers::default());

    let text = "Lunch at 12? Ümlauts and trailing space \nare fine.";
    let raw = compose(
        &alice_crypto,
        alice.address,
        bob.address,
        text,
        vec![],
        true,
        false,
    )
    .expect("signing works");
    assert!(
        String::from_utf8_lossy(&raw).contains("multipart/signed"),
        "PGP/MIME, not inline"
    );

    let content = read(&bob_crypto, &raw);
    let security = content.security.as_ref().expect("a verdict");
    assert_eq!(security.encryption, None);
    assert_eq!(security.signatures.len(), 1, "{security:?}");
    let signature = &security.signatures[0];
    assert_eq!(signature.verdict, SignatureVerdict::Good, "{security:?}");
    assert!(signature.matches_sender);
    assert_eq!(
        signature.signer.as_deref(),
        Some("Alice <alice@example.com>")
    );
    assert_eq!(security.level(), SecurityLevel::Good);
    assert_eq!(plain_text(&content).replace("\r\n", "\n"), text);
    // The signature is the verdict, not something to save.
    assert!(content.attachments.is_empty(), "{:?}", content.attachments);
}

#[test]
fn a_message_changed_after_signing_fails_verification() {
    let alice = person("Alice", "alice@example.com", None);
    let bob = person("Bob", "bob@example.com", None);
    let (_a, alice_crypto) = context_for(&alice, &[&bob], Answers::default());
    let (_b, bob_crypto) = context_for(&bob, &[&alice], Answers::default());

    let raw = compose(
        &alice_crypto,
        alice.address,
        bob.address,
        "Pay 10 EUR",
        vec![],
        true,
        false,
    )
    .unwrap();
    let tampered = String::from_utf8(raw)
        .unwrap()
        .replacen("Pay 10", "Pay 99", 1);
    assert!(tampered.contains("Pay 99"));

    let content = read(&bob_crypto, tampered.as_bytes());
    let security = content.security.expect("a verdict");
    assert!(
        matches!(security.signatures[0].verdict, SignatureVerdict::Bad(_)),
        "{security:?}"
    );
    assert_eq!(security.level(), SecurityLevel::Bad);
}

#[test]
fn a_signature_by_an_unknown_key_says_so() {
    let alice = person("Alice", "alice@example.com", None);
    let bob = person("Bob", "bob@example.com", None);
    let (_a, alice_crypto) = context_for(&alice, &[], Answers::default());
    let (_b, bob_crypto) = context_for(&bob, &[], Answers::default());

    let raw = compose(
        &alice_crypto,
        alice.address,
        bob.address,
        "Hi",
        vec![],
        true,
        false,
    )
    .unwrap();
    let security = read(&bob_crypto, &raw).security.expect("a verdict");
    assert_eq!(security.signatures[0].verdict, SignatureVerdict::UnknownKey);
    assert_eq!(security.level(), SecurityLevel::Uncertain);
}

#[test]
fn a_good_signature_from_someone_other_than_the_sender_is_not_good_enough() {
    let alice = person("Alice", "alice@example.com", None);
    let bob = person("Bob", "bob@example.com", None);
    let (_a, alice_crypto) = context_for(&alice, &[], Answers::default());
    let (_b, bob_crypto) = context_for(&bob, &[&alice], Answers::default());

    // Signed with Alice's key, but claiming to be from Mallory.
    let protection = Protection {
        sign: true,
        from: alice.address.to_string(),
        recipients: vec![bob.address.to_string()],
        ..Default::default()
    };
    let body = build_compose_body_with(
        "Trust me".into(),
        "<p>Trust me</p>".into(),
        vec![],
        &protection,
        Some(&alice_crypto),
    )
    .unwrap();
    let raw = lettre::Message::builder()
        .from("mallory@example.com".parse().unwrap())
        .to(bob.address.parse().unwrap())
        .subject("Hi")
        .multipart(body)
        .unwrap()
        .formatted();

    let security = read(&bob_crypto, &raw).security.expect("a verdict");
    let signature = &security.signatures[0];
    assert_eq!(signature.verdict, SignatureVerdict::Good);
    assert!(!signature.matches_sender);
    assert_eq!(security.level(), SecurityLevel::Uncertain);
    assert!(
        signature.summary().contains("not the sender"),
        "{}",
        signature.summary()
    );
}

#[test]
fn an_encrypted_and_signed_message_opens_for_the_recipient_and_the_sender() {
    let alice = person("Alice", "alice@example.com", None);
    let bob = person("Bob", "bob@example.com", None);
    let carol = person("Carol", "carol@example.com", None);
    let (_a, alice_crypto) = context_for(&alice, &[&bob], Answers::default());
    let (_b, bob_crypto) = context_for(&bob, &[&alice], Answers::default());
    let (_c, carol_crypto) = context_for(&carol, &[&alice, &bob], Answers::default());

    let attachment = OutgoingAttachment {
        filename: "plan.txt".into(),
        mime_type: "text/plain".into(),
        data: b"step one".to_vec(),
    };
    let raw = compose(
        &alice_crypto,
        alice.address,
        bob.address,
        "The secret plan",
        vec![attachment],
        true,
        true,
    )
    .expect("encryption works");
    let wire = String::from_utf8_lossy(&raw);
    assert!(wire.contains("multipart/encrypted"));
    assert!(!wire.contains("secret plan"), "the text is not on the wire");
    assert!(!wire.contains("plan.txt"), "nor are attachment names");

    for (who, crypto) in [("bob", &bob_crypto), ("alice", &alice_crypto)] {
        let content = read(crypto, &raw);
        let security = content.security.as_ref().expect("a verdict");
        assert_eq!(
            security.encryption,
            Some(EncryptionState::Decrypted),
            "{who}"
        );
        assert_eq!(security.signatures.len(), 1, "{who}: {security:?}");
        assert_eq!(
            security.signatures[0].verdict,
            SignatureVerdict::Good,
            "{who}"
        );
        assert!(plain_text(&content).contains("The secret plan"), "{who}");
        assert_eq!(content.attachments.len(), 1, "{who}");
        assert_eq!(content.attachments[0].filename.as_deref(), Some("plan.txt"));
        assert_eq!(
            content.attachments[0].data.as_deref(),
            Some(&b"step one"[..])
        );
    }

    // Somebody it was not encrypted to gets an explanation and the
    // ciphertext, not an error.
    let content = read(&carol_crypto, &raw);
    let security = content.security.clone().expect("a verdict");
    assert!(matches!(
        security.encryption,
        Some(EncryptionState::Failed(_))
    ));
    assert_eq!(security.level(), SecurityLevel::Bad);
    assert!(plain_text(&content).contains("could not be decrypted"));
    assert_eq!(
        content.attachments[0].filename.as_deref(),
        Some("encrypted.asc")
    );
}

#[test]
fn encrypting_to_someone_without_a_key_names_them() {
    let alice = person("Alice", "alice@example.com", None);
    let (_a, alice_crypto) = context_for(&alice, &[], Answers::default());
    let err = compose(
        &alice_crypto,
        alice.address,
        "nokey@example.com",
        "Hi",
        vec![],
        false,
        true,
    )
    .expect_err("there is no key to encrypt to");
    assert!(format!("{err:#}").contains("nokey@example.com"), "{err:#}");
    assert_eq!(
        alice_crypto.missing_keys(&["Somebody <nokey@example.com>".into(), alice.address.into()]),
        vec!["nokey@example.com".to_string()]
    );
}

#[test]
fn keys_come_from_the_keyring_gpg_reads_even_beside_an_empty_keybox() {
    use pgp::ser::Serialize;

    let bob = person("Bob", "bob@example.com", None);
    let home = tempfile::tempdir().expect("a temporary directory");
    // GnuPG 2.1 and later keep reading a `pubring.gpg` they find...
    std::fs::write(
        home.path().join("pubring.gpg"),
        bob.public().to_bytes().expect("serialising"),
    )
    .unwrap();
    // ...even with a keybox beside it that holds nothing but its header.
    let mut kbx = vec![0, 0, 0, 32, 1, 1, 0, 0];
    kbx.extend(b"KBXf");
    kbx.resize(32, 0);
    std::fs::write(home.path().join("pubring.kbx"), kbx).unwrap();

    let settings = Settings {
        gnupg_home: Some(home.path().to_path_buf()),
        use_agent: false,
        loopback_pinentry: true,
        key_files: Vec::new(),
        sign_by_default: false,
        encrypt_by_default: false,
        signing_key: None,
    };
    let crypto = Crypto::new(settings, Box::new(Answers::default()));
    assert!(crypto.missing_keys(&[bob.address.into()]).is_empty());
}

#[test]
fn a_locked_key_asks_for_its_passphrase_and_again_when_it_was_wrong() {
    let alice = person("Alice", "alice@example.com", None);
    let bob = person("Bob", "bob@example.com", Some("correct horse"));
    let (_a, alice_crypto) = context_for(&alice, &[&bob], Answers::default());
    let answers = Answers::default();
    answers
        .answers
        .lock()
        .unwrap()
        .extend(["wrong", "correct horse"]);
    let (_b, bob_crypto) = context_for(&bob, &[&alice], answers.clone());

    let raw = compose(
        &alice_crypto,
        alice.address,
        bob.address,
        "Psst",
        vec![],
        false,
        true,
    )
    .unwrap();
    let content = read(&bob_crypto, &raw);
    assert_eq!(
        content.security.as_ref().unwrap().encryption,
        Some(EncryptionState::Decrypted)
    );
    assert!(plain_text(&content).contains("Psst"));

    let asked = answers.asked.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert!(!asked[0].retry);
    assert!(asked[1].retry);
}

#[test]
fn declining_the_passphrase_leaves_the_message_unread() {
    let alice = person("Alice", "alice@example.com", None);
    let bob = person("Bob", "bob@example.com", Some("secret"));
    let (_a, alice_crypto) = context_for(&alice, &[&bob], Answers::default());
    let (_b, bob_crypto) = context_for(&bob, &[&alice], Answers::default());

    let raw = compose(
        &alice_crypto,
        alice.address,
        bob.address,
        "Psst",
        vec![],
        false,
        true,
    )
    .unwrap();
    let content = read(&bob_crypto, &raw);
    assert!(matches!(
        content.security.unwrap().encryption,
        Some(EncryptionState::Failed(_))
    ));
}

#[test]
fn an_encrypted_draft_is_for_the_sender_alone() {
    let alice = person("Alice", "alice@example.com", None);
    let bob = person("Bob", "bob@example.com", None);
    let (_a, alice_crypto) = context_for(&alice, &[&bob], Answers::default());
    let (_b, bob_crypto) = context_for(&bob, &[&alice], Answers::default());

    let protection = Protection {
        sign: true,
        encrypt: true,
        from: alice.address.to_string(),
        recipients: vec![bob.address.to_string()],
        draft: true,
    };
    let body = build_compose_body_with(
        "Draft".into(),
        "<p>Draft</p>".into(),
        vec![],
        &protection,
        Some(&alice_crypto),
    )
    .unwrap();
    let raw = lettre::Message::builder()
        .from(alice.address.parse().unwrap())
        .to(bob.address.parse().unwrap())
        .subject("Draft")
        .multipart(body)
        .unwrap()
        .formatted();

    let mine = read(&alice_crypto, &raw).security.unwrap();
    assert_eq!(mine.encryption, Some(EncryptionState::Decrypted));
    assert!(mine.signatures.is_empty(), "drafts are not signed");
    let theirs = read(&bob_crypto, &raw).security.unwrap();
    assert!(matches!(
        theirs.encryption,
        Some(EncryptionState::Failed(_))
    ));
}

#[test]
fn a_signed_draft_that_is_not_encrypted_is_stored_as_written() {
    let alice = person("Alice", "alice@example.com", None);
    let (_a, alice_crypto) = context_for(&alice, &[], Answers::default());
    let protection = Protection {
        sign: true,
        from: alice.address.to_string(),
        recipients: vec!["bob@example.com".into()],
        draft: true,
        ..Default::default()
    };
    let body = build_compose_body_with(
        "Draft".into(),
        "<p>Draft</p>".into(),
        vec![],
        &protection,
        Some(&alice_crypto),
    )
    .unwrap();
    let formatted = String::from_utf8(body.formatted()).unwrap();
    assert!(formatted.contains("multipart/alternative"));
    assert!(!formatted.contains("pgp"));
}

#[test]
fn inline_clear_signed_text_is_verified_and_unwrapped() {
    let alice = person("Alice", "alice@example.com", None);
    let bob = person("Bob", "bob@example.com", None);
    let (_b, bob_crypto) = context_for(&bob, &[&alice], Answers::default());

    let signed = CleartextSignedMessage::sign(
        rand::thread_rng(),
        "Meet me at noon.\n- Alice\n",
        &alice.secret.primary_key,
        &Password::empty(),
    )
    .unwrap()
    .to_armored_string(ArmorOptions::default())
    .unwrap();
    let raw = format!(
        "From: alice@example.com\r\nTo: bob@example.com\r\nSubject: Hi\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n--\r\nmailing list footer\r\n",
        signed.replace('\n', "\r\n")
    );

    let content = read(&bob_crypto, raw.as_bytes());
    let security = content.security.as_ref().expect("a verdict");
    assert_eq!(
        security.signatures[0].verdict,
        SignatureVerdict::Good,
        "{security:?}"
    );
    let text = plain_text(&content);
    assert!(!text.contains("BEGIN PGP"), "{text}");
    assert!(text.contains("Meet me at noon."), "{text}");
    // The footer is outside the signature, and fenced off as such.
    assert!(text.contains("[-- End signed text --]"), "{text}");
    assert!(text.contains("mailing list footer"), "{text}");
}

#[test]
fn inline_encrypted_text_is_decrypted_in_place() {
    let alice = person("Alice", "alice@example.com", None);
    let bob = person("Bob", "bob@example.com", None);
    let (_a, alice_crypto) = context_for(&alice, &[&bob], Answers::default());
    let (_b, bob_crypto) = context_for(&bob, &[&alice], Answers::default());

    let armored = alice_crypto
        .encrypt(b"Inline secret", &[bob.address.into()], alice.address, true)
        .unwrap();
    let raw = format!(
        "From: alice@example.com\r\nTo: bob@example.com\r\nSubject: Hi\r\n\
         Content-Type: text/plain\r\n\r\n{}\r\n",
        armored.replace('\n', "\r\n")
    );

    let content = read(&bob_crypto, raw.as_bytes());
    let security = content.security.as_ref().expect("a verdict");
    assert_eq!(security.encryption, Some(EncryptionState::Decrypted));
    assert_eq!(security.signatures[0].verdict, SignatureVerdict::Good);
    assert_eq!(plain_text(&content).trim(), "Inline secret");
}

#[test]
fn ordinary_mail_has_no_verdict() {
    let alice = person("Alice", "alice@example.com", None);
    let (_a, crypto) = context_for(&alice, &[], Answers::default());
    let raw = b"From: a@example.com\r\nSubject: Hi\r\nContent-Type: text/plain\r\n\r\nHello\r\n";
    assert_eq!(read(&crypto, raw).security, None);
}

/// A GnuPG home of its own, with its agent stopped when the test is done.
struct GnupgHome {
    dir: tempfile::TempDir,
}

impl GnupgHome {
    /// `None` when there is no usable `gpg` here: the test is then skipped.
    fn new() -> Option<Self> {
        let installed = std::process::Command::new("gpg")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !installed {
            eprintln!("gpg is not installed; skipping the GnuPG interoperability test");
            return None;
        }
        let dir = tempfile::Builder::new()
            .prefix("elma-gnupg")
            .tempdir()
            .ok()?;
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .ok()?;
        Some(GnupgHome { dir })
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Run gpg in this home, failing the test if it fails.
    fn gpg(&self, args: &[&str], input: &[u8]) -> Vec<u8> {
        let output = self.gpg_status(args, input);
        assert!(
            output.status.success(),
            "gpg {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn gpg_status(&self, args: &[&str], input: &[u8]) -> std::process::Output {
        use std::io::Write;
        let mut child = std::process::Command::new("gpg")
            .env("GNUPGHOME", self.path())
            // The test reads gpg's messages, so they had better be English.
            .env("LC_ALL", "C")
            .env("LANGUAGE", "C")
            .args([
                "--batch",
                "--yes",
                "--no-tty",
                "--pinentry-mode",
                "loopback",
            ])
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("gpg starts");
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().expect("gpg finishes")
    }

    fn agent(&self, command: &str) {
        let _ = std::process::Command::new("gpg-connect-agent")
            .env("GNUPGHOME", self.path())
            .args([command, "/bye"])
            .output();
    }

    fn crypto(&self, answers: Answers) -> Crypto {
        let settings = Settings {
            gnupg_home: Some(self.path().to_path_buf()),
            use_agent: true,
            loopback_pinentry: true,
            key_files: Vec::new(),
            sign_by_default: false,
            encrypt_by_default: false,
            signing_key: None,
        };
        Crypto::new(settings, Box::new(answers))
    }

    fn fingerprint(&self, email: &str) -> String {
        String::from_utf8(self.gpg(&["--with-colons", "-k", email], b""))
            .unwrap()
            .lines()
            .find_map(|line| {
                line.strip_prefix("fpr:")
                    .map(|rest| rest.trim_matches(':').to_string())
            })
            .unwrap()
    }

    fn keygrips(&self, email: &str) -> Vec<String> {
        String::from_utf8(self.gpg(&["--with-colons", "--with-keygrip", "-K", email], b""))
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("grp:"))
            .map(|rest| rest.trim_matches(':').to_string())
            .collect()
    }
}

impl Drop for GnupgHome {
    fn drop(&mut self) {
        let _ = std::process::Command::new("gpgconf")
            .env("GNUPGHOME", self.path())
            .args(["--kill", "gpg-agent"])
            .output();
    }
}

/// Wrap an armored OpenPGP message the way RFC 3156 says.
fn pgp_mime_encrypted(from: &str, to: &str, armored: &[u8]) -> Vec<u8> {
    let mut raw = format!(
        "From: {from}\r\nTo: {to}\r\nSubject: From gpg\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"XX\"\r\n\r\n\
         --XX\r\nContent-Type: application/pgp-encrypted\r\n\r\nVersion: 1\r\n\r\n\
         --XX\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    raw.extend_from_slice(armored);
    raw.extend_from_slice(b"\r\n--XX--\r\n");
    raw
}

/// The two halves of a PGP/MIME signed message as Elma sent it: the signed
/// entity, canonicalised, and the signature.
fn signed_halves(raw: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let parsed = mailparse::parse_mail(raw).unwrap();
    assert_eq!(parsed.ctype.mimetype, "multipart/signed");
    let entity = parsed.subparts[0].raw_bytes.to_vec();
    let signature = parsed.subparts[1].get_body_raw().unwrap();
    (entity, signature)
}

#[test]
fn gnupg_interoperability() {
    let Some(home) = GnupgHome::new() else {
        return;
    };

    // Alice: Ed25519 and Curve25519, no passphrase.  Bob: RSA, with one.
    home.gpg(
        &[
            "--passphrase",
            "",
            "--quick-gen-key",
            "Alice <alice@example.com>",
            "ed25519",
            "sign,cert",
            "never",
        ],
        b"",
    );
    let alice_fpr = home.fingerprint("alice@example.com");
    home.gpg(
        &[
            "--passphrase",
            "",
            "--quick-add-key",
            &alice_fpr,
            "cv25519",
            "encr",
            "never",
        ],
        b"",
    );
    home.gpg(
        &[
            "--passphrase",
            "bobpass",
            "--quick-gen-key",
            "Bob <bob@example.com>",
            "rsa2048",
            "sign,cert",
            "never",
        ],
        b"",
    );
    let bob_fpr = home.fingerprint("bob@example.com");
    home.gpg(
        &[
            "--passphrase",
            "bobpass",
            "--quick-add-key",
            &bob_fpr,
            "rsa2048",
            "encr",
            "never",
        ],
        b"",
    );

    // Carol: DSA and ElGamal, the kind of key everyone made around 2000.
    home.gpg(
        &[
            "--passphrase",
            "",
            "--quick-gen-key",
            "Carol <carol@example.com>",
            "dsa2048",
            "sign,cert",
            "never",
        ],
        b"",
    );
    let carol_fpr = home.fingerprint("carol@example.com");
    home.gpg(
        &[
            "--passphrase",
            "",
            "--quick-add-key",
            &carol_fpr,
            "elg2048",
            "encr",
            "never",
        ],
        b"",
    );

    let forget_bobs_passphrase = || {
        for grip in home.keygrips("bob@example.com") {
            home.agent(&format!("CLEAR_PASSPHRASE --mode=normal {grip}"));
        }
    };

    // --- gpg writes, Elma reads -------------------------------------------

    // Encrypted to Alice (Curve25519, through the agent) and signed by Bob.
    let entity = b"Content-Type: text/plain; charset=utf-8\r\n\r\nHello from gpg\r\n";
    let armored = home.gpg(
        &[
            "--passphrase",
            "bobpass",
            "--armor",
            "--sign",
            "--encrypt",
            "-u",
            "bob@example.com",
            "-r",
            "alice@example.com",
        ],
        entity,
    );
    let raw = pgp_mime_encrypted("bob@example.com", "alice@example.com", &armored);
    let crypto = home.crypto(Answers::default());
    let content = read(&crypto, &raw);
    let security = content.security.as_ref().expect("a verdict");
    assert_eq!(
        security.encryption,
        Some(EncryptionState::Decrypted),
        "{security:?}"
    );
    assert_eq!(security.signatures.len(), 1, "{security:?}");
    assert_eq!(
        security.signatures[0].verdict,
        SignatureVerdict::Good,
        "{security:?}"
    );
    assert!(security.signatures[0].matches_sender);
    assert!(plain_text(&content).contains("Hello from gpg"));

    // Encrypted to Bob (RSA, locked): the agent asks Elma for the passphrase.
    forget_bobs_passphrase();
    let armored = home.gpg(&["--armor", "--encrypt", "-r", "bob@example.com"], entity);
    let raw = pgp_mime_encrypted("alice@example.com", "bob@example.com", &armored);
    let answers = Answers::default();
    answers.answers.lock().unwrap().push("bobpass");
    let crypto = home.crypto(answers.clone());
    let content = read(&crypto, &raw);
    assert_eq!(
        content.security.as_ref().unwrap().encryption,
        Some(EncryptionState::Decrypted),
        "{:?}",
        content.security
    );
    assert!(plain_text(&content).contains("Hello from gpg"));
    assert_eq!(
        answers.asked.lock().unwrap().len(),
        1,
        "asked once, in Elma"
    );

    // Encrypted to Carol (ElGamal, through the agent), as gpg writes it now...
    let crypto = home.crypto(Answers::default());
    let armored = home.gpg(&["--armor", "--encrypt", "-r", "carol@example.com"], entity);
    let raw = pgp_mime_encrypted("alice@example.com", "carol@example.com", &armored);
    let content = read(&crypto, &raw);
    let security = content.security.as_ref().expect("a verdict");
    assert_eq!(
        security.encryption,
        Some(EncryptionState::Decrypted),
        "{security:?}"
    );
    assert!(plain_text(&content).contains("Hello from gpg"));

    // ...and as mail was written before integrity protection: it opens, and
    // says what it is.
    let armored = home.gpg(
        &[
            "--armor",
            "--rfc2440",
            "--cipher-algo",
            "CAST5",
            "--encrypt",
            "-r",
            "carol@example.com",
        ],
        entity,
    );
    let raw = pgp_mime_encrypted("alice@example.com", "carol@example.com", &armored);
    let content = read(&crypto, &raw);
    let security = content.security.as_ref().expect("a verdict");
    assert_eq!(
        security.encryption,
        Some(EncryptionState::Unprotected),
        "{security:?}"
    );
    assert_eq!(security.level(), SecurityLevel::Uncertain);
    assert!(
        security.summary().contains("without integrity protection"),
        "{}",
        security.summary()
    );
    assert!(plain_text(&content).contains("Hello from gpg"));

    // A PGP/MIME signature made by gpg.
    // RFC 3156: the CRLF before the boundary is the boundary's, not the
    // entity's.  Some signers include it anyway; both have to verify.
    let signed_entity = &entity[..entity.len() - 2];
    let signature = home.gpg(
        &[
            "--passphrase",
            "",
            "--armor",
            "--detach-sign",
            "-u",
            "alice@example.com",
        ],
        signed_entity,
    );
    let sloppy_signature = home.gpg(
        &[
            "--passphrase",
            "",
            "--armor",
            "--detach-sign",
            "-u",
            "alice@example.com",
        ],
        entity,
    );
    let mut raw = b"From: alice@example.com\r\nSubject: Signed\r\nMIME-Version: 1.0\r\n\
        Content-Type: multipart/signed; micalg=pgp-sha512; protocol=\"application/pgp-signature\"; boundary=\"SS\"\r\n\r\n\
        --SS\r\n"
        .to_vec();
    raw.extend_from_slice(&entity[..entity.len() - 2]);
    raw.extend_from_slice(b"\r\n--SS\r\nContent-Type: application/pgp-signature\r\n\r\n");
    raw.extend_from_slice(&signature);
    raw.extend_from_slice(b"\r\n--SS--\r\n");
    let content = read(&crypto, &raw);
    assert_eq!(
        content.security.as_ref().unwrap().signatures[0].verdict,
        SignatureVerdict::Good,
        "{:?}",
        content.security
    );
    let sloppy = String::from_utf8(raw.clone()).unwrap().replace(
        std::str::from_utf8(&signature).unwrap(),
        std::str::from_utf8(&sloppy_signature).unwrap(),
    );
    let content = read(&crypto, sloppy.as_bytes());
    assert_eq!(
        content.security.as_ref().unwrap().signatures[0].verdict,
        SignatureVerdict::Good,
        "{:?}",
        content.security
    );

    // --- Elma writes, gpg reads -------------------------------------------

    // Signed by Alice through the agent (Ed25519).
    let crypto = home.crypto(Answers::default());
    let raw = compose(
        &crypto,
        "alice@example.com",
        "bob@example.com",
        "Signed by Elma",
        vec![],
        true,
        false,
    )
    .expect("Elma signs with the agent's Ed25519 key");
    let (entity, signature) = signed_halves(&raw);
    let dir = tempfile::tempdir().unwrap();
    let (entity_path, signature_path) = (dir.path().join("entity"), dir.path().join("sig.asc"));
    std::fs::write(&entity_path, &entity).unwrap();
    std::fs::write(&signature_path, &signature).unwrap();
    let verified = home.gpg_status(
        &[
            "--verify",
            signature_path.to_str().unwrap(),
            entity_path.to_str().unwrap(),
        ],
        b"",
    );
    assert!(
        verified.status.success(),
        "gpg rejects Elma's signature: {}",
        String::from_utf8_lossy(&verified.stderr)
    );
    assert!(String::from_utf8_lossy(&verified.stderr).contains("Good signature from \"Alice"));

    // Signed by Bob (RSA, locked), the passphrase asked for in Elma.
    forget_bobs_passphrase();
    let answers = Answers::default();
    answers.answers.lock().unwrap().push("bobpass");
    let crypto = home.crypto(answers.clone());
    let raw = compose(
        &crypto,
        "bob@example.com",
        "alice@example.com",
        "RSA-signed",
        vec![],
        true,
        false,
    )
    .expect("Elma signs with the agent's RSA key");
    let (entity, signature) = signed_halves(&raw);
    std::fs::write(&entity_path, &entity).unwrap();
    std::fs::write(&signature_path, &signature).unwrap();
    let verified = home.gpg_status(
        &[
            "--verify",
            signature_path.to_str().unwrap(),
            entity_path.to_str().unwrap(),
        ],
        b"",
    );
    assert!(
        verified.status.success(),
        "gpg rejects Elma's RSA signature: {}",
        String::from_utf8_lossy(&verified.stderr)
    );
    assert_eq!(answers.asked.lock().unwrap().len(), 1);

    // Encrypted to Bob and signed by Alice: gpg decrypts and verifies.
    let crypto = home.crypto(Answers::default());
    let raw = compose(
        &crypto,
        "alice@example.com",
        "bob@example.com",
        "For Bob only",
        vec![],
        true,
        true,
    )
    .expect("Elma encrypts");
    let parsed = mailparse::parse_mail(&raw).unwrap();
    assert_eq!(parsed.ctype.mimetype, "multipart/encrypted");
    let ciphertext = parsed.subparts[1].get_body_raw().unwrap();
    let decrypted = home.gpg_status(&["--passphrase", "bobpass", "--decrypt"], &ciphertext);
    assert!(
        decrypted.status.success(),
        "gpg cannot decrypt Elma's message: {}",
        String::from_utf8_lossy(&decrypted.stderr)
    );
    assert!(String::from_utf8_lossy(&decrypted.stdout).contains("For Bob only"));
    assert!(String::from_utf8_lossy(&decrypted.stderr).contains("Good signature from \"Alice"));
}
