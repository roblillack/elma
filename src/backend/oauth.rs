//! OAuth 2.0 sign-in for JMAP accounts, discovered rather than configured.
//!
//! Nothing in here knows about a particular provider.  Everything the flow needs
//! is asked of the server the account is already pointed at, over four
//! specifications that chain together:
//!
//! 1. an unauthenticated `GET` of the JMAP session URL answers `401` with a
//!    `WWW-Authenticate: Bearer resource_metadata="..."` challenge (RFC 9728),
//! 2. that metadata document names the authorization server,
//! 3. the authorization server's own metadata (RFC 8414) gives the endpoints,
//!    the scopes it knows and whether it speaks PKCE,
//! 4. dynamic client registration (RFC 7591) hands out a `client_id` on the
//!    spot, so no credential of Elma's own has to be shipped or hosted.
//!
//! The authorization code then comes back to a listener on loopback and is
//! exchanged for tokens with PKCE (RFC 7636), which is what keeps the flow off
//! any server of ours: the token goes from the provider straight to the machine
//! the user is sitting at.
//!
//! FastMail satisfies all four; a JMAP server that does not is told apart from
//! one that does by [`discover`] failing, and the account can still use a
//! password or a token.

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    fmt, fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::Duration,
};
use url::Url;

/// How long the user has to finish in the browser before the flow gives up.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

/// An access token this close to running out is refreshed instead of used, so a
/// request cannot be issued with a token that expires while it is in flight.
const EXPIRY_MARGIN_SECS: i64 = 60;

/// Ceiling for the individual HTTP requests the flow makes.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How Elma introduces itself when registering as a client.
const CLIENT_NAME: &str = "Elma";
const CLIENT_URI: &str = "https://github.com/roblillack/elma";

/// The path the browser is sent back to, on loopback.
const REDIRECT_PATH: &str = "/oauth/callback";

/// Scopes that grant access to mail, most preferred first.  Scope names are not
/// standardised, so the one to ask for is whichever of these the server
/// advertises; `scopes` in the account configuration overrides the choice.
const MAIL_SCOPES: &[&str] = &[
    "urn:ietf:params:oauth:scope:mail",
    "https://www.fastmail.com/dev/mail",
    "mail",
];

/// Asking for this is what makes a server hand out a refresh token, turning
/// sign-in into a one-off rather than an hourly chore.
const OFFLINE_SCOPE: &str = "offline_access";

/// The endpoints and abilities an authorization server advertises.
#[derive(Clone, Debug)]
pub struct AuthorizationServer {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
}

/// What a completed sign-in leaves behind.
pub struct SignIn {
    pub credential: StoredCredential,
    /// How long the access token is good for, for something to tell the user.
    pub expires_in: Option<i64>,
}

/// One account's sign-in, as it sits in the token file.
///
/// Keyed by the endpoint rather than the issuer so that startup can find it
/// without asking the network who the issuer is.
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredCredential {
    /// The JMAP session URL this sign-in was made for.
    pub endpoint: String,
    pub username: String,
    pub issuer: String,
    pub client_id: String,
    pub token_endpoint: String,
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds, absent when the server named no lifetime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Written by hand so that formatting a credential -- in an error, a panic, a
/// debug print -- cannot spill the tokens it holds.
impl fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredCredential")
            .field("endpoint", &self.endpoint)
            .field("username", &self.username)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("token_endpoint", &self.token_endpoint)
            .field("access_token", &"***")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***"))
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl StoredCredential {
    /// Whether the access token is still worth sending.
    fn is_fresh(&self) -> bool {
        match self.expires_at {
            Some(expires_at) => Utc::now().timestamp() + EXPIRY_MARGIN_SECS < expires_at,
            // A server that names no lifetime is taken at its word until it
            // says otherwise with a 401.
            None => true,
        }
    }
}

/// The file that holds the sign-ins, one entry per account.
#[derive(Debug, Clone)]
pub struct TokenStore {
    path: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    credentials: Vec<StoredCredential>,
}

impl TokenStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `~/.elma/oauth.toml`, kept out of `~/.elmarc` so the configuration file
    /// stays something one can paste into a bug report.
    pub fn at_default_path() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("HOME is not set, so there is nowhere to keep the sign-in"))?;
        Ok(Self::new(
            PathBuf::from(home).join(".elma").join("oauth.toml"),
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The sign-in for `endpoint` and `username`, if there is one.
    pub fn find(&self, endpoint: &str, username: &str) -> Result<Option<StoredCredential>> {
        Ok(self
            .read()?
            .credentials
            .into_iter()
            .find(|credential| credential.endpoint == endpoint && credential.username == username))
    }

    /// Write `credential`, replacing any earlier sign-in for the same account.
    pub fn save(&self, credential: &StoredCredential) -> Result<()> {
        let mut file = self.read()?;
        file.credentials.retain(|existing| {
            existing.endpoint != credential.endpoint || existing.username != credential.username
        });
        file.credentials.push(credential.clone());
        self.write(&file)
    }

    fn read(&self) -> Result<StoreFile> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => toml::from_str(&raw)
                .with_context(|| format!("unable to parse {}", self.path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile::default()),
            Err(err) => {
                Err(anyhow::Error::new(err)
                    .context(format!("unable to read {}", self.path.display())))
            }
        }
    }

    /// Replace the file's contents, leaving the tokens readable only by their
    /// owner.  The write goes through a neighbouring temporary file so an
    /// interrupted save cannot leave a half-written store behind.
    fn write(&self, file: &StoreFile) -> Result<()> {
        let body = toml::to_string_pretty(file).context("unable to serialise the sign-in")?;
        let directory = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", self.path.display()))?;
        fs::create_dir_all(directory)
            .with_context(|| format!("unable to create {}", directory.display()))?;
        restrict_to_owner(directory, 0o700)?;

        let temporary = self.path.with_extension("toml.new");
        let mut handle = private_file(&temporary)?;
        handle
            .write_all(body.as_bytes())
            .with_context(|| format!("unable to write {}", temporary.display()))?;
        handle
            .sync_all()
            .with_context(|| format!("unable to flush {}", temporary.display()))?;
        drop(handle);
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("unable to move the sign-in into {}", self.path.display()))
    }
}

/// Create `path` for writing, readable by its owner alone where the platform
/// has a say in it.
fn private_file(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("unable to create {}", path.display()))
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("unable to set the permissions of {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// A sign-in the backend can draw live access tokens from.
///
/// The lookup is deferred to the first token request rather than done when the
/// account is built: an account nobody has signed into yet should still let the
/// application start, and say what to do when its mailbox is opened.
pub struct OAuthCredential {
    store: TokenStore,
    endpoint: String,
    username: String,
    /// The account's name, for the message that asks the user to sign in.
    account: String,
    state: Mutex<Option<StoredCredential>>,
}

impl OAuthCredential {
    pub fn new(store: TokenStore, endpoint: &str, username: &str, account: &str) -> Self {
        Self {
            store,
            endpoint: endpoint.to_string(),
            username: username.to_string(),
            account: account.to_string(),
            state: Mutex::new(None),
        }
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// An access token that is good right now, refreshing it if it has aged out.
    pub async fn access_token(&self) -> Result<String> {
        let credential = self.current()?;
        if credential.is_fresh() {
            return Ok(credential.access_token);
        }

        let refresh_token = credential.refresh_token.clone().ok_or_else(|| {
            anyhow!(
                "the access token for {} has expired and the sign-in carries no refresh token; \
                 run `elma --login {}` to sign in again",
                self.username,
                self.account,
            )
        })?;

        let refreshed = refresh(
            &credential.token_endpoint,
            &credential.client_id,
            &refresh_token,
        )
        .await
        .with_context(|| {
            format!(
                "refreshing the sign-in for {}; run `elma --login {}` if it has been revoked",
                self.username, self.account
            )
        })?;

        let updated = StoredCredential {
            access_token: refreshed.access_token.clone(),
            // Servers that rotate refresh tokens send a new one, and the old one
            // stops working the moment it is used -- so keep whichever arrived.
            refresh_token: refreshed.refresh_token.or(Some(refresh_token)),
            expires_at: refreshed
                .expires_in
                .map(|seconds| Utc::now().timestamp() + seconds),
            ..credential
        };
        self.store.save(&updated)?;
        let token = updated.access_token.clone();
        *self.state.lock().unwrap() = Some(updated);
        Ok(token)
    }

    /// The stored sign-in, read from the file on first use.
    fn current(&self) -> Result<StoredCredential> {
        let mut state = self.state.lock().unwrap();
        if let Some(credential) = state.as_ref() {
            return Ok(credential.clone());
        }

        let credential = self
            .store
            .find(&self.endpoint, &self.username)?
            .ok_or_else(|| {
                anyhow!(
                    "no sign-in stored for {} at {}: run `elma --login {}`",
                    self.username,
                    self.endpoint,
                    self.account,
                )
            })?;
        *state = Some(credential.clone());
        Ok(credential)
    }
}

/// Ask the server at `session_url` who authorizes access to it.
pub async fn discover(session_url: &str) -> Result<AuthorizationServer> {
    let http = http_client()?;

    // The challenge on an unauthenticated request is the documented way to find
    // the metadata; the well-known path is the fallback for a server that
    // answers 401 without one.
    let probe = http
        .get(session_url)
        .send()
        .await
        .with_context(|| format!("asking {session_url} how to authenticate"))?;
    let metadata_url = match challenge_metadata_url(&probe) {
        Some(url) => url,
        None => default_protected_resource_url(session_url)?,
    };

    let resource: ProtectedResourceMetadata =
        fetch_json(&http, &metadata_url).await.with_context(|| {
            format!("{session_url} does not advertise an OAuth authorization server")
        })?;
    let issuer = resource
        .authorization_servers
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("{metadata_url} names no authorization server"))?;

    let metadata = fetch_server_metadata(&http, &issuer).await?;
    if !metadata
        .code_challenge_methods_supported
        .iter()
        .any(|method| method == "S256")
    {
        bail!(
            "the authorization server at {issuer} does not support PKCE (S256), which Elma \
             requires: without it the authorization code could be redeemed by anything else \
             listening on this machine"
        );
    }

    Ok(AuthorizationServer {
        issuer: metadata.issuer.unwrap_or(issuer),
        authorization_endpoint: metadata.authorization_endpoint,
        token_endpoint: metadata.token_endpoint,
        registration_endpoint: metadata.registration_endpoint,
        scopes_supported: metadata.scopes_supported,
    })
}

/// Sign in interactively, returning the credential to store.
///
/// Blocking, and meant to be called with the terminal to itself: the URL is
/// printed for the user, and the wait for the browser is the point of the call.
pub fn sign_in_blocking(
    endpoint: &str,
    username: &str,
    requested_scopes: Option<&[String]>,
) -> Result<SignIn> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("unable to start the runtime for the sign-in")?;
    runtime.block_on(sign_in(endpoint, username, requested_scopes))
}

async fn sign_in(
    endpoint: &str,
    username: &str,
    requested_scopes: Option<&[String]>,
) -> Result<SignIn> {
    // The port comes first: it is part of the redirect URI, and registering the
    // exact URI the listener will answer on avoids depending on the server to
    // allow an arbitrary loopback port (RFC 8252 asks it to, not all do).
    let listener = TcpListener::bind("127.0.0.1:0")
        .context("unable to listen on loopback for the browser to come back")?;
    let port = listener
        .local_addr()
        .context("unable to read back the loopback port")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{REDIRECT_PATH}");

    println!("Looking up how {endpoint} wants to be signed in to...");
    let server = discover(endpoint).await?;
    let scopes = choose_scopes(&server, requested_scopes)?;

    let client_id = register_client(&server, &redirect_uri, &scopes).await?;
    let pkce = Pkce::generate()?;
    let state = random_token(16)?;
    let authorize_url = authorize_url(&server, &client_id, &redirect_uri, &scopes, &state, &pkce)?;

    println!();
    println!("Open this URL to sign in as {username}:");
    println!();
    println!("    {authorize_url}");
    println!();
    if open_in_browser(&authorize_url) {
        println!("(Opened it in your browser.)");
    } else {
        println!("(Copy it into a browser -- opening one here did not work.)");
    }
    println!("Waiting for the browser to come back, up to five minutes. Ctrl-C aborts.");

    let expected_state = state.clone();
    let wait = tokio::task::spawn_blocking(move || wait_for_redirect(listener, &expected_state));
    let code = tokio::time::timeout(SIGN_IN_TIMEOUT, wait)
        .await
        .map_err(|_| anyhow!("gave up waiting for the browser after five minutes"))?
        .context("the listener waiting for the browser stopped unexpectedly")??;

    let tokens = exchange_code(
        &server.token_endpoint,
        &client_id,
        &code,
        &pkce.verifier,
        &redirect_uri,
    )
    .await?;

    let scopes = tokens
        .scope
        .as_deref()
        .map(|granted| granted.split_whitespace().map(str::to_string).collect())
        .unwrap_or(scopes);

    Ok(SignIn {
        credential: StoredCredential {
            endpoint: endpoint.to_string(),
            username: username.to_string(),
            issuer: server.issuer,
            client_id,
            token_endpoint: server.token_endpoint,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens
                .expires_in
                .map(|seconds| Utc::now().timestamp() + seconds),
            scopes,
        },
        expires_in: tokens.expires_in,
    })
}

/// Which scopes to ask for: what the account configured, or the mail scope this
/// server happens to call its own plus a refresh token.
fn choose_scopes(
    server: &AuthorizationServer,
    requested: Option<&[String]>,
) -> Result<Vec<String>> {
    if let Some(requested) = requested
        && !requested.is_empty()
    {
        return Ok(requested.to_vec());
    }

    let offers = |scope: &str| {
        server
            .scopes_supported
            .iter()
            .any(|supported| supported == scope)
    };

    let mail = MAIL_SCOPES
        .iter()
        .find(|scope| offers(scope))
        .ok_or_else(|| {
            anyhow!(
                "{} advertises no scope Elma recognises as granting access to mail (it offers: \
                 {}). Name the right one with `scopes = [\"...\"]` in the account.",
                server.issuer,
                if server.scopes_supported.is_empty() {
                    "nothing".to_string()
                } else {
                    server.scopes_supported.join(", ")
                }
            )
        })?;

    let mut scopes = vec![(*mail).to_string()];
    if offers(OFFLINE_SCOPE) {
        scopes.push(OFFLINE_SCOPE.to_string());
    }
    Ok(scopes)
}

/// Register Elma with the server as a public client, on the spot.
async fn register_client(
    server: &AuthorizationServer,
    redirect_uri: &str,
    scopes: &[String],
) -> Result<String> {
    let endpoint = server.registration_endpoint.as_deref().ok_or_else(|| {
        anyhow!(
            "the authorization server at {} does not offer dynamic client registration, so Elma \
             has no way to obtain a client of its own; use an API token for this account instead",
            server.issuer
        )
    })?;

    let request = serde_json::json!({
        "client_name": CLIENT_NAME,
        "client_uri": CLIENT_URI,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        // A terminal application cannot keep a secret, and says so rather than
        // pretending to hold one.
        "token_endpoint_auth_method": "none",
        "application_type": "native",
        "scope": scopes.join(" "),
    });

    // The body is serialised here rather than through reqwest's `json` helper,
    // which would mean turning on a feature jmap-client does not ask for.
    let response = http_client()?
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&request).context("unable to serialise the registration")?)
        .send()
        .await
        .with_context(|| format!("registering with {endpoint}"))?;
    let response = check_response(response)
        .await
        .with_context(|| format!("registering with {endpoint}"))?;

    #[derive(Deserialize)]
    struct Registration {
        client_id: String,
    }
    let registration: Registration = parse_json(response)
        .await
        .with_context(|| format!("reading the registration from {endpoint}"))?;
    Ok(registration.client_id)
}

fn authorize_url(
    server: &AuthorizationServer,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    state: &str,
    pkce: &Pkce,
) -> Result<String> {
    let mut url = Url::parse(&server.authorization_endpoint).with_context(|| {
        format!(
            "the authorization endpoint {} is not a URL",
            server.authorization_endpoint
        )
    })?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", &scopes.join(" "))
        .append_pair("state", state)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

/// Serve loopback until the browser arrives with the authorization code.
fn wait_for_redirect(listener: TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (stream, _) = listener
            .accept()
            .context("unable to accept the browser's connection")?;
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        if reader
            .read_line(&mut request_line)
            .context("unable to read the browser's request")?
            == 0
        {
            continue;
        }

        let target = request_line.split_whitespace().nth(1).unwrap_or("/");
        // A browser asks for more than the page it was sent to -- a favicon, a
        // speculative connection -- and none of that is the redirect.
        let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
            respond(&stream, "404 Not Found", "Not here.");
            continue;
        };
        if url.path() != REDIRECT_PATH {
            respond(&stream, "404 Not Found", "Not here.");
            continue;
        }

        let mut code = None;
        let mut state = None;
        let mut error = None;
        let mut description = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "code" => code = Some(value.to_string()),
                "state" => state = Some(value.to_string()),
                "error" => error = Some(value.to_string()),
                "error_description" => description = Some(value.to_string()),
                _ => {}
            }
        }

        if let Some(error) = error {
            let detail = description.unwrap_or_else(|| "no reason given".to_string());
            respond(
                &stream,
                "200 OK",
                &format!("Sign-in failed: {error}. You can close this tab."),
            );
            bail!("the authorization server refused the sign-in: {error} ({detail})");
        }

        // Without this the code could have been planted by any page that guessed
        // the port, and we would redeem a code obtained for someone else.
        if state.as_deref() != Some(expected_state) {
            respond(
                &stream,
                "400 Bad Request",
                "That request did not come from Elma.",
            );
            bail!("the redirect carried the wrong `state`, so it was not the one Elma started");
        }

        let Some(code) = code else {
            respond(
                &stream,
                "400 Bad Request",
                "No authorization code in that redirect.",
            );
            bail!("the redirect carried no authorization code");
        };

        // Deliberately not "signed in": the code still has to be redeemed, and
        // the browser should not claim success the terminal may contradict.
        respond(
            &stream,
            "200 OK",
            "Elma has the authorization. You can close this tab and go back to the terminal.",
        );
        return Ok(code);
    }
}

/// Answer the browser with a page it can render, and nothing it has to fetch.
fn respond(mut stream: &std::net::TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Elma</title></head>\
         <body style=\"font-family:system-ui,sans-serif;margin:4rem auto;max-width:32rem\">\
         <h1>Elma</h1><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

async fn exchange_code(
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse> {
    let response = http_client()?
        .post(token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .with_context(|| format!("redeeming the authorization code at {token_endpoint}"))?;
    let response = check_response(response)
        .await
        .with_context(|| format!("redeeming the authorization code at {token_endpoint}"))?;
    parse_json(response)
        .await
        .with_context(|| format!("reading the tokens from {token_endpoint}"))
}

/// Trade a refresh token for a new access token.
pub async fn refresh(
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let response = http_client()?
        .post(token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await
        .with_context(|| format!("refreshing the access token at {token_endpoint}"))?;
    let response = check_response(response).await?;
    parse_json(response)
        .await
        .with_context(|| format!("reading the refreshed tokens from {token_endpoint}"))
}

/// What a token endpoint answers with.
#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Seconds from now, as OAuth counts it.
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
}

/// A PKCE verifier and the challenge derived from it.
struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    fn generate() -> Result<Self> {
        let verifier = random_token(32)?;
        let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
        Ok(Self {
            challenge: URL_SAFE_NO_PAD.encode(digest.as_ref()),
            verifier,
        })
    }
}

/// `bytes` bytes of randomness, spelled in the alphabet OAuth parameters use.
fn random_token(bytes: usize) -> Result<String> {
    use ring::rand::SecureRandom;
    let mut buffer = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut buffer)
        .map_err(|_| anyhow!("unable to draw random bytes for the sign-in"))?;
    Ok(URL_SAFE_NO_PAD.encode(&buffer))
}

#[derive(Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

#[derive(Deserialize)]
struct ServerMetadata {
    issuer: Option<String>,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

/// The `resource_metadata` a `WWW-Authenticate: Bearer` challenge points at.
fn challenge_metadata_url(response: &reqwest::Response) -> Option<String> {
    let header = response
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)?
        .to_str()
        .ok()?;
    resource_metadata_from_challenge(header)
}

/// Pick the `resource_metadata` parameter out of a challenge header.
fn resource_metadata_from_challenge(header: &str) -> Option<String> {
    let (_, rest) = header.split_once("resource_metadata=")?;
    let rest = rest.trim_start();
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => rest.split([',', ' ']).next()?,
    };
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Where RFC 9728 says a resource's metadata lives when it does not say:
/// the well-known segment goes between the host and the resource's own path.
fn default_protected_resource_url(session_url: &str) -> Result<String> {
    let url = Url::parse(session_url)
        .with_context(|| format!("the JMAP endpoint {session_url} is not a URL"))?;
    let path = url.path().trim_end_matches('/');
    let origin = format!(
        "{}://{}",
        url.scheme(),
        url.host_str()
            .ok_or_else(|| anyhow!("the JMAP endpoint {session_url} names no host"))?
    );
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    Ok(format!(
        "{origin}{port}/.well-known/oauth-protected-resource{path}"
    ))
}

/// RFC 8414's document, or the OpenID one that predates it.
async fn fetch_server_metadata(http: &reqwest::Client, issuer: &str) -> Result<ServerMetadata> {
    let base = issuer.trim_end_matches('/');
    let mut failure = None;
    for path in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/openid-configuration",
    ] {
        match fetch_json::<ServerMetadata>(http, &format!("{base}{path}")).await {
            Ok(metadata) => return Ok(metadata),
            Err(err) => failure = Some(err),
        }
    }
    Err(failure
        .unwrap_or_else(|| anyhow!("no metadata"))
        .context(format!(
            "{issuer} publishes no usable authorization server metadata"
        )))
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
) -> Result<T> {
    let response = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    let response = check_response(response)
        .await
        .with_context(|| format!("fetching {url}"))?;
    parse_json(response)
        .await
        .with_context(|| format!("reading {url}"))
}

/// Turn a refusal into the reason the server gave for it.
///
/// OAuth answers a bad request with a JSON body naming the error, which is far
/// more use than the status code on its own.
async fn check_response(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    #[derive(Deserialize)]
    struct OAuthError {
        error: String,
        error_description: Option<String>,
    }
    if let Ok(error) = serde_json::from_str::<OAuthError>(&body) {
        let detail = error
            .error_description
            .map(|detail| format!(": {detail}"))
            .unwrap_or_default();
        bail!("the server answered {status} -- {}{detail}", error.error);
    }

    let excerpt: String = body.trim().chars().take(200).collect();
    if excerpt.is_empty() {
        bail!("the server answered {status}");
    }
    bail!("the server answered {status}: {excerpt}");
}

async fn parse_json<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let body = response
        .text()
        .await
        .context("unable to read the response")?;
    serde_json::from_str(&body).with_context(|| {
        let excerpt: String = body.trim().chars().take(200).collect();
        format!("the response was not the JSON expected: {excerpt}")
    })
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("Elma/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("unable to build an HTTP client for the sign-in")
}

/// Ask the desktop to open `url`, reporting whether it took.
fn open_in_browser(url: &str) -> bool {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    } else {
        Command::new("xdg-open")
    };

    command
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential() -> StoredCredential {
        StoredCredential {
            endpoint: "https://api.example.com/jmap/session".to_string(),
            username: "rob@example.com".to_string(),
            issuer: "https://api.example.com".to_string(),
            client_id: "a-client".to_string(),
            token_endpoint: "https://api.example.com/oauth/refresh".to_string(),
            access_token: "an-access-token".to_string(),
            refresh_token: Some("a-refresh-token".to_string()),
            expires_at: Some(Utc::now().timestamp() + 3600),
            scopes: vec!["urn:ietf:params:oauth:scope:mail".to_string()],
        }
    }

    fn server() -> AuthorizationServer {
        AuthorizationServer {
            issuer: "https://api.example.com".to_string(),
            authorization_endpoint: "https://api.example.com/oauth/authorize".to_string(),
            token_endpoint: "https://api.example.com/oauth/refresh".to_string(),
            registration_endpoint: Some("https://api.example.com/oauth/register".to_string()),
            scopes_supported: vec![
                "urn:ietf:params:oauth:scope:mail".to_string(),
                "urn:ietf:params:oauth:scope:calendars".to_string(),
                OFFLINE_SCOPE.to_string(),
            ],
        }
    }

    #[test]
    fn a_stored_sign_in_survives_the_round_trip() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let store = TokenStore::new(directory.path().join("oauth.toml"));

        assert!(
            store
                .find("https://api.example.com/jmap/session", "rob@example.com")
                .expect("an empty store should read as empty")
                .is_none()
        );

        store.save(&credential()).expect("the sign-in should save");
        let found = store
            .find("https://api.example.com/jmap/session", "rob@example.com")
            .expect("the store should be readable")
            .expect("the sign-in should be found");
        assert_eq!(found.access_token, "an-access-token");
        assert_eq!(found.refresh_token.as_deref(), Some("a-refresh-token"));
        assert_eq!(found.scopes, vec!["urn:ietf:params:oauth:scope:mail"]);
    }

    #[test]
    fn signing_in_again_replaces_the_earlier_token() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let store = TokenStore::new(directory.path().join("oauth.toml"));
        store.save(&credential()).expect("the first sign-in saves");
        store
            .save(&StoredCredential {
                access_token: "a-newer-token".to_string(),
                ..credential()
            })
            .expect("the second sign-in saves");

        let file: StoreFile = toml::from_str(
            &fs::read_to_string(store.path()).expect("the store should be readable"),
        )
        .expect("the store should parse");
        assert_eq!(file.credentials.len(), 1, "one account, one entry");
        assert_eq!(file.credentials[0].access_token, "a-newer-token");
    }

    /// Two accounts on one server are told apart by their username.
    #[test]
    fn two_accounts_on_one_server_are_kept_apart() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let store = TokenStore::new(directory.path().join("oauth.toml"));
        store.save(&credential()).expect("the first sign-in saves");
        store
            .save(&StoredCredential {
                username: "other@example.com".to_string(),
                access_token: "another-token".to_string(),
                ..credential()
            })
            .expect("the second sign-in saves");

        assert_eq!(
            store
                .find("https://api.example.com/jmap/session", "other@example.com")
                .expect("readable")
                .expect("found")
                .access_token,
            "another-token"
        );
        assert_eq!(
            store
                .find("https://api.example.com/jmap/session", "rob@example.com")
                .expect("readable")
                .expect("found")
                .access_token,
            "an-access-token"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("a temporary directory");
        let store = TokenStore::new(directory.path().join("nested").join("oauth.toml"));
        store.save(&credential()).expect("the sign-in should save");

        let mode = fs::metadata(store.path())
            .expect("the store should exist")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the tokens are the owner's business alone");
    }

    /// Nothing that formats a credential may print the tokens in it.
    #[test]
    fn a_credential_does_not_debug_print_its_tokens() {
        let formatted = format!("{:?}", credential());
        assert!(!formatted.contains("an-access-token"), "{formatted}");
        assert!(!formatted.contains("a-refresh-token"), "{formatted}");
        assert!(formatted.contains("rob@example.com"), "{formatted}");
    }

    #[test]
    fn an_expired_access_token_is_not_fresh() {
        let stale = StoredCredential {
            expires_at: Some(Utc::now().timestamp() - 1),
            ..credential()
        };
        assert!(!stale.is_fresh());

        let expiring = StoredCredential {
            expires_at: Some(Utc::now().timestamp() + EXPIRY_MARGIN_SECS / 2),
            ..credential()
        };
        assert!(
            !expiring.is_fresh(),
            "a token about to expire is refreshed before it is sent"
        );

        assert!(credential().is_fresh());
        assert!(
            StoredCredential {
                expires_at: None,
                ..credential()
            }
            .is_fresh(),
            "a server that names no lifetime is taken at its word"
        );
    }

    #[test]
    fn an_account_that_never_signed_in_says_how_to() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let credential = OAuthCredential::new(
            TokenStore::new(directory.path().join("oauth.toml")),
            "https://api.example.com/jmap/session",
            "rob@example.com",
            "Work",
        );

        let error = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
            .block_on(credential.access_token())
            .expect_err("there is no sign-in to draw a token from");
        assert!(
            error.to_string().contains("elma --login Work"),
            "unexpected error: {error}"
        );
    }

    /// Read one HTTP request off `stream` and hand back its body.
    ///
    /// The request has to be consumed before the answer goes out: a client that
    /// is still writing its body when the response arrives treats that as a
    /// protocol error rather than as the answer to its question.
    fn read_request_body(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;

        let mut raw = Vec::new();
        let mut buffer = [0u8; 512];
        let mut body_at = None;
        let mut length = 0usize;
        while body_at.is_none_or(|start| raw.len() < start + length) {
            let read = stream.read(&mut buffer).unwrap_or(0);
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&buffer[..read]);
            if body_at.is_none()
                && let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let headers = String::from_utf8_lossy(&raw[..position]).to_lowercase();
                length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                body_at = Some(position + 4);
            }
        }

        String::from_utf8_lossy(&raw[body_at.unwrap_or(raw.len())..]).to_string()
    }

    /// An expired access token is traded in for a new one, the rotated refresh
    /// token replaces the old one, and the next call reuses what it got.
    ///
    /// Driven against a socket on loopback: this is the path that runs
    /// unattended for as long as the account exists, so what goes on the wire
    /// and what ends up in the file both matter.
    #[test]
    fn an_expired_access_token_is_refreshed_and_kept() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the client to connect");
            let body = read_request_body(&mut stream);
            let payload = "{\"access_token\":\"a-newer-token\",\"refresh_token\":\"a-rotated-token\",\
                           \"expires_in\":3600}";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
            body
        });

        let directory = tempfile::tempdir().expect("a temporary directory");
        let store = TokenStore::new(directory.path().join("oauth.toml"));
        store
            .save(&StoredCredential {
                token_endpoint: format!("http://127.0.0.1:{port}/oauth/refresh"),
                expires_at: Some(Utc::now().timestamp() - 1),
                ..credential()
            })
            .expect("the sign-in should save");

        let live = OAuthCredential::new(
            store.clone(),
            "https://api.example.com/jmap/session",
            "rob@example.com",
            "Work",
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");

        let token = runtime
            .block_on(live.access_token())
            .expect("the expired token should be refreshed");
        assert_eq!(token, "a-newer-token");

        let request = server.join().expect("the server thread to finish");
        assert!(request.contains("grant_type=refresh_token"), "{request}");
        assert!(
            request.contains("refresh_token=a-refresh-token"),
            "{request}"
        );
        assert!(request.contains("client_id=a-client"), "{request}");

        let stored = store
            .find("https://api.example.com/jmap/session", "rob@example.com")
            .expect("the store should be readable")
            .expect("the sign-in should still be there");
        assert_eq!(stored.access_token, "a-newer-token");
        assert_eq!(
            stored.refresh_token.as_deref(),
            Some("a-rotated-token"),
            "a rotated refresh token replaces the one that was just spent"
        );
        assert!(
            stored.expires_at.unwrap_or_default() > Utc::now().timestamp(),
            "the new expiry lies ahead"
        );

        // The listener is gone, so a second round trip would fail: getting the
        // token back proves the refreshed one is held rather than re-fetched.
        assert_eq!(
            runtime
                .block_on(live.access_token())
                .expect("the fresh token should be reused"),
            "a-newer-token"
        );
    }

    /// A sign-in the server has withdrawn says what to do about it.
    #[test]
    fn a_refusal_to_refresh_points_at_signing_in_again() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the client to connect");
            read_request_body(&mut stream);
            let payload = "{\"error\":\"invalid_grant\",\"error_description\":\"token revoked\"}";
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });

        let directory = tempfile::tempdir().expect("a temporary directory");
        let store = TokenStore::new(directory.path().join("oauth.toml"));
        store
            .save(&StoredCredential {
                token_endpoint: format!("http://127.0.0.1:{port}/oauth/refresh"),
                expires_at: Some(Utc::now().timestamp() - 1),
                ..credential()
            })
            .expect("the sign-in should save");

        let live = OAuthCredential::new(
            store,
            "https://api.example.com/jmap/session",
            "rob@example.com",
            "Work",
        );
        let error = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(live.access_token())
            .expect_err("a revoked sign-in cannot be refreshed");
        let _ = server.join();

        let message = format!("{error:#}");
        assert!(message.contains("elma --login Work"), "{message}");
        assert!(message.contains("invalid_grant"), "{message}");
        assert!(message.contains("token revoked"), "{message}");
    }

    #[test]
    fn the_mail_scope_is_taken_from_what_the_server_offers() {
        let scopes = choose_scopes(&server(), None).expect("the server offers a mail scope");
        assert_eq!(
            scopes,
            vec!["urn:ietf:params:oauth:scope:mail", "offline_access"],
            "a refresh token is asked for alongside mail"
        );
    }

    #[test]
    fn a_configured_scope_wins() {
        let scopes = choose_scopes(&server(), Some(&["something:else".to_string()]))
            .expect("a configured scope is used as it stands");
        assert_eq!(scopes, vec!["something:else"]);
    }

    #[test]
    fn a_server_without_a_mail_scope_says_what_it_has() {
        let error = choose_scopes(
            &AuthorizationServer {
                scopes_supported: vec!["openid".to_string(), "profile".to_string()],
                ..server()
            },
            None,
        )
        .expect_err("no scope here grants mail");
        let message = error.to_string();
        assert!(message.contains("openid, profile"), "{message}");
        assert!(message.contains("scopes = "), "{message}");
    }

    #[test]
    fn the_offline_scope_is_left_out_when_unsupported() {
        let scopes = choose_scopes(
            &AuthorizationServer {
                scopes_supported: vec!["urn:ietf:params:oauth:scope:mail".to_string()],
                ..server()
            },
            None,
        )
        .expect("mail alone is enough");
        assert_eq!(scopes, vec!["urn:ietf:params:oauth:scope:mail"]);
    }

    /// The header FastMail answers an unauthenticated session request with.
    #[test]
    fn the_challenge_names_where_the_metadata_lives() {
        assert_eq!(
            resource_metadata_from_challenge(
                "Bearer resource_metadata=\"https://api.example.com/.well-known/oauth-protected-resource/jmap/session\""
            )
            .as_deref(),
            Some("https://api.example.com/.well-known/oauth-protected-resource/jmap/session")
        );
        assert_eq!(
            resource_metadata_from_challenge(
                "Bearer realm=\"jmap\", resource_metadata=https://example.com/meta, charset=\"UTF-8\""
            )
            .as_deref(),
            Some("https://example.com/meta"),
            "an unquoted parameter ends at the next delimiter"
        );
    }

    #[test]
    fn a_challenge_without_metadata_falls_back_to_the_well_known_path() {
        assert!(resource_metadata_from_challenge("Bearer").is_none());
        assert!(resource_metadata_from_challenge("Basic realm=\"mail\"").is_none());

        assert_eq!(
            default_protected_resource_url("https://api.example.com/jmap/session").expect("a URL"),
            "https://api.example.com/.well-known/oauth-protected-resource/jmap/session"
        );
        assert_eq!(
            default_protected_resource_url("https://mail.example.com:8443/jmap/").expect("a URL"),
            "https://mail.example.com:8443/.well-known/oauth-protected-resource/jmap"
        );
    }

    #[test]
    fn the_authorize_url_carries_the_challenge_and_state() {
        let pkce = Pkce::generate().expect("PKCE should be generated");
        let url = authorize_url(
            &server(),
            "a-client",
            "http://127.0.0.1:1234/oauth/callback",
            &["urn:ietf:params:oauth:scope:mail".to_string()],
            "a-state",
            &pkce,
        )
        .expect("the URL should build");

        let parsed = Url::parse(&url).expect("a URL");
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["client_id"], "a-client");
        assert_eq!(
            pairs["redirect_uri"],
            "http://127.0.0.1:1234/oauth/callback"
        );
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["code_challenge"], pkce.challenge);
        assert_eq!(pairs["state"], "a-state");
        assert!(!pkce.verifier.is_empty());
        assert_ne!(
            pkce.challenge, pkce.verifier,
            "the challenge is a digest, not the verifier"
        );
    }

    /// The verifier is what proves the exchange belongs to this flow, so it must
    /// not repeat between sign-ins.
    #[test]
    fn every_sign_in_draws_a_fresh_verifier() {
        let first = Pkce::generate().expect("PKCE");
        let second = Pkce::generate().expect("PKCE");
        assert_ne!(first.verifier, second.verifier);
        assert_ne!(first.challenge, second.challenge);
        assert!(
            first.verifier.len() >= 43,
            "RFC 7636 wants at least 43 characters: {}",
            first.verifier.len()
        );
    }

    /// The flow, as far as it goes without a person at the browser: discovery,
    /// registration, and an authorization URL the server is willing to render.
    ///
    /// Left out of the default run because it talks to FastMail -- and registers
    /// a client there, the way a real sign-in does.  Run it with
    /// `cargo test -- --ignored --nocapture` after touching discovery or
    /// registration.
    #[test]
    #[ignore = "talks to FastMail over the network"]
    fn fastmail_offers_everything_the_flow_needs() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");

        let server = runtime
            .block_on(discover("https://api.fastmail.com/jmap/session"))
            .expect("FastMail should advertise an authorization server");
        assert_eq!(server.issuer, "https://api.fastmail.com");
        assert_eq!(
            server.registration_endpoint.as_deref(),
            Some("https://api.fastmail.com/oauth/register"),
            "registration is what saves Elma from shipping a client of its own"
        );

        let scopes = choose_scopes(&server, None).expect("a mail scope should be on offer");
        assert!(scopes.contains(&OFFLINE_SCOPE.to_string()), "{scopes:?}");

        let redirect_uri = "http://127.0.0.1:1/oauth/callback";
        let client_id = runtime
            .block_on(register_client(&server, redirect_uri, &scopes))
            .expect("registration should succeed");
        assert!(!client_id.is_empty());

        let pkce = Pkce::generate().expect("PKCE");
        let url = authorize_url(&server, &client_id, redirect_uri, &scopes, "a-state", &pkce)
            .expect("the URL should build");

        // A sign-in page (or a redirect to one) means the parameters were
        // accepted; a 400 would mean they were not.
        let http = http_client().expect("an HTTP client");
        let status = runtime
            .block_on(async { http.get(&url).send().await })
            .expect("the authorization endpoint should answer")
            .status();
        assert!(
            status.is_success() || status.is_redirection(),
            "the authorization endpoint refused the request: {status}"
        );
    }

    /// The challenge is the SHA-256 of the verifier, base64url without padding.
    #[test]
    fn the_challenge_is_the_digest_rfc_7636_describes() {
        // The example from RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
        assert_eq!(
            URL_SAFE_NO_PAD.encode(digest.as_ref()),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
