//! Raw MIME, both ways: a fetched message into what the viewer shows, and a
//! composed body into what is sent -- each with OpenPGP where it applies.
//!
//! Reading understands PGP/MIME (RFC 3156: `multipart/signed` and
//! `multipart/encrypted`) and inline PGP (armored blocks inside `text/plain`).
//! A message that is signed or encrypted is shown as what it protects, and the
//! verdict goes into [`MessageContent::security`].  Writing produces PGP/MIME.

use crate::{
    backend::{LeafPart, OutgoingAttachment, PartRole, compose_address},
    model::{
        EncryptionState, MessageAttachment, MessageContent, MessageContentPart, SecurityInfo,
        SignatureVerdict,
    },
    openpgp::{self, Crypto},
};
use anyhow::{Context, Result, anyhow};
use lettre::message::{
    Attachment as LettreAttachment, MultiPart, SinglePart,
    header::{ContentDisposition, ContentTransferEncoding, ContentType},
};
use mailparse::{DispositionType, MailAddr, MailHeaderMap, ParsedMail};

/// How deep into nested multiparts to look.  Encryption and signatures each
/// add a level, so this is generous.
const MAX_PART_DEPTH: usize = 10;

const BEGIN_MESSAGE: &str = "-----BEGIN PGP MESSAGE-----";
const END_MESSAGE: &str = "-----END PGP MESSAGE-----";
const BEGIN_SIGNED: &str = "-----BEGIN PGP SIGNED MESSAGE-----";
const END_SIGNATURE: &str = "-----END PGP SIGNATURE-----";

/// What the viewer needs from a parsed message, decrypting and verifying with
/// the installed OpenPGP context.
pub(crate) fn build_message_content(mail: &ParsedMail<'_>) -> Result<MessageContent> {
    build_message_content_with(mail, openpgp::context().as_deref())
}

pub(crate) fn build_message_content_with(
    mail: &ParsedMail<'_>,
    crypto: Option<&Crypto>,
) -> Result<MessageContent> {
    let mailer = mail.headers.get_first_value("X-Mailer").unwrap_or_default();

    let mut collector = Collector {
        parts: Vec::new(),
        attachments: Vec::new(),
        security: SecurityInfo::default(),
        crypto,
        sender: sender_address(mail),
    };
    collector.collect(mail, 0)?;

    Ok(MessageContent {
        mailer,
        parts: collector.parts,
        attachments: collector.attachments,
        to: header_addresses(mail, "To"),
        cc: header_addresses(mail, "Cc"),
        bcc: header_addresses(mail, "Bcc"),
        security: (!collector.security.is_empty()).then_some(collector.security),
    })
}

/// Whether a message needs its raw form to be read properly: it is signed or
/// encrypted, by PGP/MIME or inline.  For backends that otherwise get the
/// parts pre-digested by the server.
pub(crate) fn looks_protected(content_types: &[&str], text: &[&str]) -> bool {
    content_types.iter().any(|ty| {
        let ty = ty.to_ascii_lowercase();
        ty.starts_with("multipart/signed") || ty.starts_with("multipart/encrypted")
    }) || text
        .iter()
        .any(|body| body.contains(BEGIN_MESSAGE) || body.contains(BEGIN_SIGNED))
}

/// The address a message says it is from.
fn sender_address(mail: &ParsedMail<'_>) -> Option<String> {
    let header = mail.headers.get_all_headers("From").into_iter().next()?;
    let list = mailparse::addrparse_header(header).ok()?;
    list.iter().find_map(|addr| match addr {
        MailAddr::Single(single) => Some(single.addr.clone()),
        MailAddr::Group(group) => group.addrs.first().map(|single| single.addr.clone()),
    })
}

/// The addresses in a message's `field` headers, written for the compose
/// fields.
///
/// The envelope the message list is built from names people rather than
/// addresses, so a reopened draft cannot be addressed from it.  Groups are
/// opened up into their members; a header that will not parse is kept as
/// written, for the user to correct rather than to lose.
pub(crate) fn header_addresses(mail: &ParsedMail<'_>, field: &str) -> Vec<String> {
    let mut addresses = Vec::new();
    for header in mail.headers.get_all_headers(field) {
        let Ok(list) = mailparse::addrparse_header(header) else {
            let raw = header.get_value();
            if !raw.trim().is_empty() {
                addresses.push(raw.trim().to_string());
            }
            continue;
        };
        for addr in list.iter() {
            let members = match addr {
                MailAddr::Single(single) => std::slice::from_ref(single),
                MailAddr::Group(group) => &group.addrs[..],
            };
            addresses.extend(
                members
                    .iter()
                    .map(|member| compose_address(member.display_name.as_deref(), &member.addr)),
            );
        }
    }
    addresses
}

struct Collector<'c> {
    parts: Vec<MessageContentPart>,
    attachments: Vec<MessageAttachment>,
    security: SecurityInfo,
    crypto: Option<&'c Crypto>,
    sender: Option<String>,
}

impl Collector<'_> {
    fn collect(&mut self, mail: &ParsedMail<'_>, depth: usize) -> Result<()> {
        if depth >= MAX_PART_DEPTH {
            return Ok(());
        }

        let mimetype = mail.ctype.mimetype.to_ascii_lowercase();
        let protocol = mail
            .ctype
            .params
            .get("protocol")
            .map(|value| value.to_ascii_lowercase());

        if mimetype == "multipart/signed"
            && protocol.as_deref() == Some("application/pgp-signature")
            && mail.subparts.len() == 2
        {
            return self.collect_signed(mail, depth);
        }
        if mimetype == "multipart/encrypted"
            && protocol.as_deref() == Some("application/pgp-encrypted")
            && mail.subparts.len() == 2
        {
            return self.collect_encrypted(mail, depth);
        }

        if !mail.subparts.is_empty() {
            for sub in &mail.subparts {
                self.collect(sub, depth + 1)?;
            }
            return Ok(());
        }

        self.collect_leaf(mail)
    }

    /// RFC 3156 section 5: the first part is what was signed, byte for byte,
    /// the second the signature over it.
    fn collect_signed(&mut self, mail: &ParsedMail<'_>, depth: usize) -> Result<()> {
        let signed = &mail.subparts[0];
        if let Some(crypto) = self.crypto {
            let signature = mail.subparts[1]
                .get_body_raw()
                .context("reading the signature")?;
            let mut data = canonical_crlf(signed.raw_bytes);
            let mut states = crypto.verify_detached(&data, &signature, self.sender.as_deref());
            // Some signers count the line break before the boundary as part of
            // what they signed.  Appending a line break cannot disguise a
            // change to the content, so that reading is tried as well.
            if states
                .iter()
                .all(|state| matches!(state.verdict, SignatureVerdict::Bad(_)))
            {
                data.extend_from_slice(b"\r\n");
                let retry = crypto.verify_detached(&data, &signature, self.sender.as_deref());
                if retry
                    .iter()
                    .any(|state| state.verdict == SignatureVerdict::Good)
                {
                    states = retry;
                }
            }
            self.security.signatures.extend(states);
        }
        // The signature is not shown as an attachment: it is accounted for in
        // the verdict, and saving it is of no use to anyone.
        self.collect(signed, depth + 1)
    }

    /// RFC 3156 section 4: a version part, then the encrypted entity.
    fn collect_encrypted(&mut self, mail: &ParsedMail<'_>, depth: usize) -> Result<()> {
        let payload = mail.subparts[1]
            .get_body_raw()
            .context("reading the encrypted part")?;

        let outcome = match self.crypto {
            Some(crypto) => crypto.decrypt(&payload, self.sender.as_deref()),
            None => Err(anyhow!("OpenPGP support is disabled")),
        };

        match outcome {
            Ok(decrypted) => {
                self.security.encryption = Some(EncryptionState::Decrypted);
                self.security.signatures.extend(decrypted.signatures);
                let inner = mailparse::parse_mail(&decrypted.data)
                    .context("parsing the decrypted message")?;
                self.collect(&inner, depth + 1)
            }
            Err(err) => {
                let reason = format!("{err:#}");
                self.parts.push(MessageContentPart {
                    content_type: "text/plain".to_string(),
                    content: format!(
                        "This message is encrypted with OpenPGP and could not be decrypted:\n\n{reason}\n"
                    )
                    .into_bytes(),
                });
                // Kept so it can be saved and decrypted elsewhere.
                self.attachments.push(MessageAttachment {
                    filename: Some("encrypted.asc".to_string()),
                    mime_type: "application/pgp-encrypted".to_string(),
                    size: payload.len(),
                    data: Some(payload),
                    blob_id: None,
                    inline: false,
                });
                self.security.encryption = Some(EncryptionState::Failed(reason));
                Ok(())
            }
        }
    }

    fn collect_leaf(&mut self, mail: &ParsedMail<'_>) -> Result<()> {
        let content_type = mail.ctype.mimetype.clone();
        let data = if mail.ctype.mimetype.starts_with("text/") {
            match mail.get_body() {
                Ok(text) => text.into_bytes(),
                Err(_) => mail
                    .get_body_raw()
                    .context("reading message body segment")?,
            }
        } else {
            mail.get_body_raw()
                .context("reading message body segment")?
        };
        let disposition = mail.get_content_disposition();
        let filename = disposition
            .params
            .get("filename")
            .cloned()
            .or_else(|| disposition.params.get("name").cloned())
            .or_else(|| mail.ctype.params.get("name").cloned())
            .filter(|name| !name.trim().is_empty());
        let disposition_name = match disposition.disposition {
            DispositionType::Attachment => Some("attachment"),
            DispositionType::Inline => Some("inline"),
            _ => None,
        };
        let content_id = mail.headers.get_first_value("Content-ID");
        let role = LeafPart {
            major_type: mail.ctype.mimetype.split('/').next().unwrap_or_default(),
            has_filename: filename.is_some(),
            disposition: disposition_name,
            has_content_id: content_id.is_some_and(|id| !id.trim().is_empty()),
        }
        .role();

        // Inline PGP lives in body text; an attached `.asc` file is left as
        // the file it is.
        let data = if role == PartRole::Body && content_type.eq_ignore_ascii_case("text/plain") {
            self.inline_pgp(data)
        } else {
            data
        };

        // Inline parts are listed too: they earn no marker in the message list,
        // but an embedded photo is still a file the reader may want to keep.
        if role != PartRole::Body {
            self.attachments.push(MessageAttachment {
                filename,
                mime_type: content_type.clone(),
                size: data.len(),
                data: Some(data.clone()),
                blob_id: None,
                inline: role == PartRole::Inline,
            });
        }
        self.parts.push(MessageContentPart {
            content_type,
            content: data,
        });
        Ok(())
    }

    /// Replace armored PGP blocks in body text with what they protect.
    ///
    /// Text around a block is not covered by its signature or encryption --
    /// a mailing list footer, typically, but it could be anything -- so when
    /// there is any, the protected text is fenced off to make the boundary
    /// visible.
    fn inline_pgp(&mut self, data: Vec<u8>) -> Vec<u8> {
        let Some(crypto) = self.crypto else {
            return data;
        };
        let text = match String::from_utf8(data) {
            Ok(text) => text,
            // Not text after all; leave it to the viewer.
            Err(err) => return err.into_bytes(),
        };

        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        let mut changed = false;
        while let Some((start, kind)) = next_block(rest) {
            let end = match kind {
                Block::Encrypted => END_MESSAGE,
                Block::Signed => END_SIGNATURE,
            };
            let Some(end_at) = rest[start..].find(end).map(|pos| start + pos + end.len()) else {
                break;
            };
            let block = &rest[start..end_at];
            let before = &rest[..start];
            let after = &rest[end_at..];
            let surrounded =
                !before.trim().is_empty() || !after.trim().is_empty() || !out.trim().is_empty();

            let replacement = match kind {
                Block::Encrypted => {
                    match crypto.decrypt(block.as_bytes(), self.sender.as_deref()) {
                        Ok(decrypted) => {
                            self.security.encryption = Some(EncryptionState::Decrypted);
                            let signed = !decrypted.signatures.is_empty();
                            self.security.signatures.extend(decrypted.signatures);
                            let label = if signed {
                                "encrypted and signed"
                            } else {
                                "encrypted"
                            };
                            Some((String::from_utf8_lossy(&decrypted.data).into_owned(), label))
                        }
                        Err(err) => {
                            self.security.encryption =
                                Some(EncryptionState::Failed(format!("{err:#}")));
                            None
                        }
                    }
                }
                Block::Signed => match crypto.verify_cleartext(block, self.sender.as_deref()) {
                    Ok((signed_text, states)) => {
                        self.security.signatures.extend(states);
                        Some((signed_text, "signed"))
                    }
                    Err(_) => None,
                },
            };

            out.push_str(before);
            match replacement {
                Some((plain, label)) => {
                    changed = true;
                    if surrounded {
                        out.push_str(&format!("[-- Begin {label} text --]\n"));
                    }
                    out.push_str(&plain);
                    if surrounded {
                        if !plain.ends_with('\n') {
                            out.push('\n');
                        }
                        out.push_str(&format!("[-- End {label} text --]"));
                    }
                }
                None => out.push_str(block),
            }
            rest = after;
        }
        out.push_str(rest);

        if changed {
            out.into_bytes()
        } else {
            text.into_bytes()
        }
    }
}

#[derive(Clone, Copy)]
enum Block {
    Encrypted,
    Signed,
}

/// The first armored block in `text` that starts a line.
fn next_block(text: &str) -> Option<(usize, Block)> {
    let at_line_start = |marker: &str| {
        text.match_indices(marker)
            .map(|(pos, _)| pos)
            .find(|&pos| pos == 0 || text.as_bytes()[pos - 1] == b'\n')
    };
    let encrypted = at_line_start(BEGIN_MESSAGE).map(|pos| (pos, Block::Encrypted));
    let signed = at_line_start(BEGIN_SIGNED).map(|pos| (pos, Block::Signed));
    match (encrypted, signed) {
        (Some(a), Some(b)) => Some(if a.0 < b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// Line endings as MIME puts them on the wire: what a PGP/MIME signature is
/// computed over, whatever the message went through on its way to disk.
fn canonical_crlf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 32);
    let mut previous = 0u8;
    for &byte in data {
        if byte == b'\n' && previous != b'\r' {
            out.push(b'\r');
        }
        out.push(byte);
        previous = byte;
    }
    out
}

// --- Writing -------------------------------------------------------------------

/// How an outgoing message is to be protected.
#[derive(Clone, Debug, Default)]
pub(crate) struct Protection {
    pub(crate) sign: bool,
    pub(crate) encrypt: bool,
    /// The sender's address: whose key signs, and who else can decrypt.
    pub(crate) from: String,
    /// Everyone the message is addressed to, Bcc included.
    pub(crate) recipients: Vec<String>,
    /// A draft is encrypted to the sender alone, and never signed: it is not
    /// finished, and nobody else is meant to read it yet.
    pub(crate) draft: bool,
}

impl Protection {
    fn is_needed(&self) -> bool {
        if self.draft {
            self.encrypt
        } else {
            self.sign || self.encrypt
        }
    }
}

/// Assemble the MIME body of an outgoing message.
///
/// The two bodies always go into a `multipart/alternative`; attachments wrap
/// that in a `multipart/mixed`.  With `protection`, the result is signed
/// and/or encrypted as PGP/MIME.
pub(crate) fn build_compose_body(
    text_body: String,
    html_body: String,
    attachments: Vec<OutgoingAttachment>,
    protection: &Protection,
) -> Result<MultiPart> {
    let crypto = openpgp::context();
    build_compose_body_with(
        text_body,
        html_body,
        attachments,
        protection,
        crypto.as_deref(),
    )
}

pub(crate) fn build_compose_body_with(
    text_body: String,
    html_body: String,
    attachments: Vec<OutgoingAttachment>,
    protection: &Protection,
    crypto: Option<&Crypto>,
) -> Result<MultiPart> {
    let protected = protection.is_needed();
    let alternative = MultiPart::alternative()
        .singlepart(text_part(ContentType::TEXT_PLAIN, text_body, protected))
        .singlepart(text_part(ContentType::TEXT_HTML, html_body, protected));

    let body = if attachments.is_empty() {
        alternative
    } else {
        let mut mixed = MultiPart::mixed().multipart(alternative);
        for attachment in attachments {
            let content_type: ContentType = attachment
                .mime_type
                .parse()
                .unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
            let part =
                LettreAttachment::new(attachment.filename).body(attachment.data, content_type);
            mixed = mixed.singlepart(part);
        }
        mixed
    };

    if !protected {
        return Ok(body);
    }
    let crypto = crypto.ok_or_else(|| anyhow!("OpenPGP support is disabled"))?;
    protect(body, protection, crypto)
}

/// A text part, made 7-bit safe when it is going to be signed or encrypted.
///
/// A signature covers the exact bytes, and a relay that touches 8-bit text --
/// or trailing whitespace on a 7-bit line -- breaks it.  Quoted-printable
/// leaves nothing for a relay to touch (RFC 3156 section 3).
fn text_part(content_type: ContentType, body: String, protected: bool) -> SinglePart {
    let builder = SinglePart::builder().header(content_type);
    if protected {
        builder
            .header(ContentTransferEncoding::QuotedPrintable)
            .body(body)
    } else {
        builder.body(body)
    }
}

/// Wrap `body` in PGP/MIME.
fn protect(body: MultiPart, protection: &Protection, crypto: &Crypto) -> Result<MultiPart> {
    if protection.draft {
        let entity = body.formatted();
        let armored = crypto.encrypt_to_self(&entity, &protection.from)?;
        return Ok(encrypted_multipart(armored));
    }

    if protection.encrypt {
        // Signed and encrypted in one OpenPGP message (RFC 3156 section 6.2):
        // what every current client writes, and one layer less to unwrap.
        let entity = body.formatted();
        let armored = crypto
            .encrypt(
                &entity,
                &protection.recipients,
                &protection.from,
                protection.sign,
            )
            .context("encrypting the message")?;
        return Ok(encrypted_multipart(armored));
    }

    // Signed only.  The CRLF lettre ends the part with belongs to the boundary
    // that follows it, not to the part (RFC 2046 section 5.1.1).
    let mut entity = body.formatted();
    if entity.ends_with(b"\r\n") {
        entity.truncate(entity.len() - 2);
    }
    let signature = crypto
        .sign_detached(&entity, &protection.from)
        .context("signing the message")?;
    Ok(MultiPart::signed(
        "application/pgp-signature".to_string(),
        openpgp::MICALG.to_string(),
    )
    .multipart(body)
    .singlepart(
        SinglePart::builder()
            .header(
                ContentType::parse("application/pgp-signature; name=\"signature.asc\"")
                    .expect("a valid content type"),
            )
            .header(ContentDisposition::attachment("signature.asc"))
            .header(ContentTransferEncoding::SevenBit)
            .body(signature),
    ))
}

fn encrypted_multipart(armored: String) -> MultiPart {
    MultiPart::encrypted("application/pgp-encrypted".to_string())
        .singlepart(
            SinglePart::builder()
                .header(ContentType::parse("application/pgp-encrypted").expect("a valid type"))
                .header(ContentTransferEncoding::SevenBit)
                .body(String::from("Version: 1\r\n")),
        )
        .singlepart(
            SinglePart::builder()
                .header(
                    ContentType::parse("application/octet-stream; name=\"encrypted.asc\"")
                        .expect("a valid type"),
                )
                .header(ContentDisposition::inline_with_name("encrypted.asc"))
                .header(ContentTransferEncoding::SevenBit)
                .body(armored),
        )
}
