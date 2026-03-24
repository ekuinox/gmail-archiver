use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use rand::{Rng, distributions::Alphanumeric};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::{Path, PathBuf},
};
use url::Url;

pub const GMAIL_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";
pub const GMAIL_MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";

pub fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .context("Failed to build the HTTP client")
}

pub struct Authenticator {
    client: Client,
    oauth_client: GoogleClientSecret,
    token_store: PathBuf,
    oauth_scope: String,
    cached_token: Option<SavedToken>,
}

impl Authenticator {
    pub fn from_client_secret_file(
        client: Client,
        path: impl AsRef<Path>,
        token_store: PathBuf,
        oauth_scope: impl Into<String>,
    ) -> Result<Self> {
        let raw = fs::read_to_string(path.as_ref()).with_context(|| {
            format!(
                "Failed to read the OAuth client JSON: {}",
                path.as_ref().display()
            )
        })?;
        let parsed: GoogleClientSecretFile =
            serde_json::from_str(&raw).context("The OAuth client JSON is invalid")?;
        let oauth_client = parsed
            .installed
            .context("This tool only supports a Desktop app OAuth client. Create a Desktop app client in Google Cloud and use its downloaded JSON.")?;

        Ok(Self {
            client,
            oauth_client,
            token_store,
            oauth_scope: oauth_scope.into(),
            cached_token: None,
        })
    }

    pub async fn bearer_token(&mut self) -> Result<String> {
        self.ensure_token().await?;
        let token = self
            .cached_token
            .as_ref()
            .context("Failed to obtain an access token")?;
        Ok(token.access_token.clone())
    }

    pub fn invalidate_access_token(&mut self) {
        if let Some(token) = self.cached_token.as_mut() {
            token.expires_at = Some(Utc::now() - Duration::minutes(5));
        }
    }

    async fn ensure_token(&mut self) -> Result<()> {
        if self.cached_token.is_none() {
            self.cached_token = self.load_token()?;
        }

        if self
            .cached_token
            .as_ref()
            .is_some_and(|token| !token.has_compatible_scope(&self.oauth_scope))
        {
            println!(
                "The saved token does not cover {}, reauthorizing.",
                self.oauth_scope
            );
            self.cached_token = None;
        }

        if self
            .cached_token
            .as_ref()
            .is_some_and(SavedToken::is_currently_valid)
        {
            return Ok(());
        }

        if let Some(refresh_token) = self.cached_token.as_ref().and_then(|token| {
            if token.has_compatible_scope(&self.oauth_scope) {
                token.refresh_token.clone()
            } else {
                None
            }
        }) {
            match self.refresh_access_token(&refresh_token).await {
                Ok(token) => {
                    self.save_token(&token)?;
                    self.cached_token = Some(token);
                    return Ok(());
                }
                Err(error) => {
                    eprintln!("Refresh failed, falling back to browser login: {error:#}");
                }
            }
        }

        let token = self.authorize_interactively().await?;
        self.save_token(&token)?;
        self.cached_token = Some(token);
        Ok(())
    }

    fn load_token(&self) -> Result<Option<SavedToken>> {
        if !self.token_store.exists() {
            return Ok(None);
        }

        let raw = fs::read_to_string(&self.token_store).with_context(|| {
            format!(
                "Failed to read the token file: {}",
                self.token_store.display()
            )
        })?;
        let token = serde_json::from_str(&raw).with_context(|| {
            format!(
                "The token file contains invalid JSON: {}",
                self.token_store.display()
            )
        })?;
        Ok(Some(token))
    }

    fn save_token(&self, token: &SavedToken) -> Result<()> {
        if let Some(parent) = self.token_store.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create the token directory: {}", parent.display())
            })?;
        }

        let json = serde_json::to_string_pretty(token).context("Failed to serialize the token")?;
        fs::write(&self.token_store, json).with_context(|| {
            format!(
                "Failed to write the token file: {}",
                self.token_store.display()
            )
        })?;
        Ok(())
    }

    async fn authorize_interactively(&self) -> Result<SavedToken> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .context("Failed to start the OAuth callback listener")?;
        let port = listener
            .local_addr()
            .context("Failed to get the OAuth callback port")?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}");
        let code_verifier = generate_code_verifier();
        let code_challenge = generate_code_challenge(&code_verifier);
        let auth_url = build_authorization_url(
            &self.oauth_client,
            &redirect_uri,
            &code_challenge,
            &self.oauth_scope,
        )?;

        println!("Opening a browser for Google sign-in.");
        println!("If it does not open, visit this URL manually:\n{auth_url}");

        if let Err(error) = webbrowser::open(auth_url.as_str()) {
            eprintln!("Could not launch the browser automatically: {error}");
        }

        let callback_url = std::thread::spawn(move || wait_for_authorization_response(listener))
            .join()
            .map_err(|_| anyhow!("The OAuth callback thread exited unexpectedly"))??;

        let code = extract_code_from_callback(&callback_url)?;
        self.exchange_authorization_code(&code, &redirect_uri, &code_verifier)
            .await
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<SavedToken> {
        let mut params = vec![
            ("client_id", self.oauth_client.client_id.clone()),
            ("code", code.to_owned()),
            ("code_verifier", code_verifier.to_owned()),
            ("grant_type", "authorization_code".to_owned()),
            ("redirect_uri", redirect_uri.to_owned()),
        ];

        if let Some(client_secret) = &self.oauth_client.client_secret {
            params.push(("client_secret", client_secret.clone()));
        }

        let token = self.request_token(&params).await?;
        token.into_saved_token(None)
    }

    async fn refresh_access_token(&self, refresh_token: &str) -> Result<SavedToken> {
        let mut params = vec![
            ("client_id", self.oauth_client.client_id.clone()),
            ("grant_type", "refresh_token".to_owned()),
            ("refresh_token", refresh_token.to_owned()),
        ];

        if let Some(client_secret) = &self.oauth_client.client_secret {
            params.push(("client_secret", client_secret.clone()));
        }

        let token = self.request_token(&params).await?;
        token.into_saved_token(Some(refresh_token.to_owned()))
    }

    async fn request_token(&self, params: &[(&str, String)]) -> Result<TokenResponse> {
        let response = self
            .client
            .post(&self.oauth_client.token_uri)
            .form(params)
            .send()
            .await
            .context("Request to the Google OAuth token endpoint failed")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("Failed to read the Google OAuth response body")?;

        if !status.is_success() {
            if let Ok(error) = serde_json::from_str::<TokenErrorResponse>(&body) {
                if let Some(description) = error.error_description {
                    bail!("Google OAuth error: {} ({description})", error.error);
                }
                bail!("Google OAuth error: {}", error.error);
            }

            bail!("Google OAuth error: HTTP {status} - {body}");
        }

        serde_json::from_str(&body).context("Failed to parse the Google OAuth token JSON")
    }
}

#[derive(Debug, Deserialize)]
struct GoogleClientSecretFile {
    installed: Option<GoogleClientSecret>,
}

#[derive(Debug, Clone, Deserialize)]
struct GoogleClientSecret {
    client_id: String,
    client_secret: Option<String>,
    auth_uri: String,
    token_uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    scope: Option<String>,
    token_type: Option<String>,
}

impl SavedToken {
    fn is_currently_valid(&self) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at > Utc::now() + Duration::seconds(60))
    }

    fn has_compatible_scope(&self, required_scope: &str) -> bool {
        let Some(scope_string) = self.scope.as_deref() else {
            return required_scope == GMAIL_READONLY_SCOPE;
        };

        let scopes = scope_string.split_whitespace().collect::<Vec<_>>();
        if scopes.contains(&required_scope) {
            return true;
        }

        required_scope == GMAIL_READONLY_SCOPE && scopes.contains(&GMAIL_MODIFY_SCOPE)
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
    scope: Option<String>,
    token_type: Option<String>,
}

impl TokenResponse {
    fn into_saved_token(self, fallback_refresh_token: Option<String>) -> Result<SavedToken> {
        if self.access_token.is_empty() {
            bail!("The Google OAuth response did not contain an access token");
        }

        let expires_at = self.expires_in.map(|seconds| {
            let headroom = (seconds - 30).max(0);
            Utc::now() + Duration::seconds(headroom)
        });

        Ok(SavedToken {
            access_token: self.access_token,
            refresh_token: self.refresh_token.or(fallback_refresh_token),
            expires_at,
            scope: self.scope,
            token_type: self.token_type,
        })
    }
}

#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
    error_description: Option<String>,
}

fn build_authorization_url(
    oauth_client: &GoogleClientSecret,
    redirect_uri: &str,
    code_challenge: &str,
    scope: &str,
) -> Result<Url> {
    let mut url = Url::parse(&oauth_client.auth_uri)
        .context("Failed to build the Google OAuth authorization URL")?;
    url.query_pairs_mut()
        .append_pair("access_type", "offline")
        .append_pair("client_id", &oauth_client.client_id)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("prompt", "consent")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", scope);
    Ok(url)
}

fn generate_code_verifier() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(96)
        .map(char::from)
        .collect()
}

fn generate_code_challenge(code_verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let digest = hasher.finalize();
    URL_SAFE_NO_PAD.encode(digest)
}

fn wait_for_authorization_response(listener: TcpListener) -> Result<String> {
    let (mut stream, _) = listener
        .accept()
        .context("Failed to receive the Google OAuth callback")?;
    let mut request_line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader
            .read_line(&mut request_line)
            .context("Failed to read the OAuth callback request line")?;
    }

    let request_target = request_line
        .split_whitespace()
        .nth(1)
        .context("The OAuth callback request line is invalid")?;

    let callback_url = format!("http://localhost{request_target}");
    let body =
        "<html><body><h1>Authentication complete</h1><p>You can close this tab.</p></body></html>";
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .context("Failed to write the OAuth completion response")?;

    Ok(callback_url)
}

fn extract_code_from_callback(callback_url: &str) -> Result<String> {
    let url = Url::parse(callback_url).context("Failed to parse the OAuth callback URL")?;

    if let Some((_, error)) = url.query_pairs().find(|(key, _)| key == "error") {
        bail!("Google OAuth failed: {error}");
    }

    url.query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .context("The OAuth callback did not include an authorization code")
}
