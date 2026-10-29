//! OAuth resource server for a personal vault. The configured identity provider
//! owns authorization, PKCE, client registration, refresh tokens and login.
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use reqwest::Url;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::ToolFilter;

const CACHE_TTL: Duration = Duration::from_secs(300);
const REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_DOCUMENT: usize = 1_048_576;
const MAX_REQUEST: usize = 2_097_152;

#[derive(Clone)]
pub struct OAuthConfig {
    pub resource: String,
    pub issuer: String,
    pub audience: String,
    pub subjects: HashSet<String>,
}

fn https_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| "Invalid OAuth HTTPS URL".to_string())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("OAuth URLs must use HTTPS without credentials, query or fragment".into());
    }
    Ok(url)
}

impl OAuthConfig {
    pub fn from_env() -> Result<Option<Self>, String> {
        let names = [
            "OBSIDIAN_PUBLIC_URL",
            "OBSIDIAN_OAUTH_ISSUER",
            "OBSIDIAN_OAUTH_AUDIENCE",
            "OBSIDIAN_OAUTH_SUBJECTS",
        ];
        let values: Vec<Option<String>> = names
            .iter()
            .map(|name| match std::env::var(name) {
                Ok(value) => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(_) => Err(format!("{name} is not valid Unicode")),
            })
            .collect::<Result<_, _>>()?;
        if values.iter().all(Option::is_none) {
            return Ok(None);
        }
        let mut settings = Vec::new();
        for (name, value) in names.iter().zip(values) {
            settings.push(
                value
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| format!("{name} is required when OAuth is configured"))?,
            );
        }
        let origin = https_url(&settings[0])?;
        if origin.path() != "/" {
            return Err("OBSIDIAN_PUBLIC_URL must be an HTTPS origin with no path".into());
        }
        https_url(&settings[1])?;
        let subjects = settings[3]
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        if subjects.is_empty() {
            return Err("OBSIDIAN_OAUTH_SUBJECTS must name at least one allowed subject".into());
        }
        Ok(Some(Self {
            resource: format!("{}/mcp", origin.as_str().trim_end_matches('/')),
            issuer: settings[1].clone(),
            audience: settings[2].clone(),
            subjects,
        }))
    }

    fn metadata(&self) -> serde_json::Value {
        serde_json::json!({"resource": self.resource, "authorization_servers": [self.issuer],
            "scopes_supported": ["vault:read", "vault:write"], "bearer_methods_supported": ["header"]})
    }
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

struct KeyCache {
    keys: JwkSet,
    fetched: Instant,
}

pub struct OAuthState {
    pub config: OAuthConfig,
    http: reqwest::Client,
    jwks_uri: Url,
    keys: RwLock<KeyCache>,
}

async fn json_document<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: Url,
) -> Result<T, String> {
    let mut response = http
        .get(url)
        .send()
        .await
        .map_err(|_| "OAuth document request failed")?
        .error_for_status()
        .map_err(|_| "OAuth document request was rejected")?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Could not read OAuth document")?
    {
        if bytes.len() + chunk.len() > MAX_DOCUMENT {
            return Err("OAuth document too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "Invalid OAuth discovery/JWKS JSON".into())
}

impl OAuthState {
    pub async fn new(config: OAuthConfig) -> Result<Arc<Self>, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "Could not create OAuth HTTP client")?;
        // OIDC discovery supports path-based issuers (e.g. a realm in a homelab IdP).
        let discovery: Discovery = json_document(
            &http,
            https_url(&format!(
                "{}/.well-known/openid-configuration",
                config.issuer.trim_end_matches('/')
            ))?,
        )
        .await?;
        if discovery.issuer != config.issuer {
            return Err("OAuth discovery issuer mismatch".into());
        }
        let jwks_uri = https_url(&discovery.jwks_uri)?;
        let keys = json_document(&http, jwks_uri.clone()).await?;
        Ok(Arc::new(Self {
            config,
            http,
            jwks_uri,
            keys: RwLock::new(KeyCache {
                keys,
                fetched: Instant::now(),
            }),
        }))
    }

    async fn authorize(&self, token: &str) -> Result<bool, ()> {
        let header = decode_header(token).map_err(|_| ())?;
        if !matches!(
            header.alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::ES256
                | Algorithm::ES384
        ) {
            return Err(());
        }
        let kid = header.kid.as_deref().ok_or(())?;
        {
            let cache = self.keys.read().await;
            let stale = cache.fetched.elapsed() >= CACHE_TTL;
            let unknown =
                cache.keys.find(kid).is_none() && cache.fetched.elapsed() >= REFRESH_COOLDOWN;
            if stale || unknown {
                drop(cache);
                let mut cache = self.keys.write().await;
                if cache.fetched.elapsed() >= CACHE_TTL
                    || (cache.keys.find(kid).is_none()
                        && cache.fetched.elapsed() >= REFRESH_COOLDOWN)
                {
                    // Serialize refreshes. If unavailable, fail closed; do not trust stale keys.
                    let refreshed = json_document(&self.http, self.jwks_uri.clone()).await;
                    cache.fetched = Instant::now();
                    match refreshed {
                        Ok(keys) => cache.keys = keys,
                        Err(_) => {
                            // Fail closed and rate-limit retries during an IdP outage.
                            cache.keys = JwkSet { keys: vec![] };
                            return Err(());
                        }
                    }
                }
            }
        }
        let cache = self.keys.read().await;
        validate_token(&self.config, token, &cache.keys)
    }

    fn challenge(&self, status: StatusCode, error: &str) -> Response {
        let origin = self.config.resource.trim_end_matches("/mcp");
        let challenge = format!(
            "Bearer resource_metadata=\"{origin}/.well-known/oauth-protected-resource/mcp\", error=\"{error}\", scope=\"vault:read\""
        );
        let mut response = (status, Json(serde_json::json!({"error": error}))).into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_str(&challenge).expect("validated public URL"),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    scp: Vec<String>,
}

fn validate_token(config: &OAuthConfig, token: &str, keys: &JwkSet) -> Result<bool, ()> {
    let header = decode_header(token).map_err(|_| ())?;
    if !matches!(
        header.alg,
        Algorithm::RS256
            | Algorithm::RS384
            | Algorithm::RS512
            | Algorithm::ES256
            | Algorithm::ES384
    ) {
        return Err(());
    }
    let jwk = keys.find(header.kid.as_deref().ok_or(())?).ok_or(())?;
    let key = DecodingKey::from_jwk(jwk).map_err(|_| ())?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[&config.issuer]);
    validation.set_audience(&[&config.audience]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.validate_nbf = true;
    validation.leeway = 30;
    let claims = decode::<Claims>(token, &key, &validation)
        .map_err(|_| ())?
        .claims;
    if !config.subjects.contains(&claims.sub) {
        return Err(());
    }
    let scopes: HashSet<&str> = claims
        .scope
        .split_whitespace()
        .chain(claims.scp.iter().map(String::as_str))
        .collect();
    if !scopes.contains("vault:read") {
        return Err(());
    }
    Ok(scopes.contains("vault:write"))
}

#[derive(Clone)]
pub struct AuthToolRestrictions(pub HashSet<String>);

pub fn metadata_routes(state: Arc<OAuthState>) -> Router {
    let metadata = state.config.metadata();
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get({
                let metadata = metadata.clone();
                move || async move { Json(metadata) }
            }),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(move || async move { Json(metadata) }),
        )
}

/// Validate every HTTP request, including requests using an existing MCP session.
/// Scope checks on each tool call prevent a read token reusing a write session.
pub async fn authenticate(
    State(state): State<Arc<OAuthState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let expected = state.config.resource.trim_end_matches("/mcp");
        if origin.to_str().ok() != Some(expected) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let Some(token) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return state.challenge(StatusCode::UNAUTHORIZED, "invalid_token");
    };
    let can_write = match state.authorize(token).await {
        Ok(value) => value,
        Err(()) => return state.challenge(StatusCode::UNAUTHORIZED, "invalid_token"),
    };
    let disabled = if can_write {
        HashSet::new()
    } else {
        ToolFilter::Profile("read".into()).disabled_tools()
    };
    if request.method() == axum::http::Method::POST {
        let (mut parts, body) = request.into_parts();
        let bytes = match to_bytes(body, MAX_REQUEST).await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        };
        let message = match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) if value.is_object() => value,
            _ => return StatusCode::BAD_REQUEST.into_response(),
        };
        if message.get("method").and_then(|v| v.as_str()) == Some("tools/call") {
            let name = message.pointer("/params/name").and_then(|v| v.as_str());
            if !can_write && name.is_none_or(|name| disabled.contains(name)) {
                return state.challenge(StatusCode::FORBIDDEN, "insufficient_scope");
            }
            let mutates_existing = matches!(
                name,
                Some("note_write" | "note_insert" | "note_patch" | "note_delete" | "note_move")
            ) || (name == Some("frontmatter")
                && message
                    .pointer("/params/arguments/action")
                    .and_then(|v| v.as_str())
                    .is_none_or(|action| !action.eq_ignore_ascii_case("get")));
            if mutates_existing
                && message
                    .pointer("/params/arguments/expected_revision")
                    .and_then(|v| v.as_str())
                    .is_none()
            {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "expected_revision_required", "message": "Call note_read and pass its revision as expected_revision."}))).into_response();
            }
        }
        parts.extensions.insert(AuthToolRestrictions(disabled));
        request = Request::from_parts(parts, Body::from(bytes));
    } else {
        request
            .extensions_mut()
            .insert(AuthToolRestrictions(disabled));
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::Request as HttpRequest, middleware, routing::post};
    use jsonwebtoken::{EncodingKey, Header, encode};
    use tower::ServiceExt;

    fn config() -> OAuthConfig {
        OAuthConfig {
            resource: "https://notes.example.test/mcp".into(),
            issuer: "https://issuer.example.test/realm".into(),
            audience: "https://notes.example.test/mcp".into(),
            subjects: HashSet::from(["ryan".into()]),
        }
    }

    fn keys() -> JwkSet {
        serde_json::from_str(include_str!("../tests/fixtures/oauth/jwks.json")).unwrap()
    }

    fn token(scope: &str, changes: serde_json::Value) -> String {
        let mut claims = serde_json::json!({
            "iss": config().issuer, "aud": config().audience, "sub": "ryan",
            "exp": chrono::Utc::now().timestamp() + 600,
            "nbf": chrono::Utc::now().timestamp() - 60, "scope": scope
        });
        for (k, v) in changes.as_object().unwrap() {
            claims[k] = v.clone();
        }
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".into());
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(include_bytes!(
                "../tests/fixtures/oauth/TEST_ONLY_private.pem"
            ))
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn validates_signatures_identity_audience_expiry_and_scopes() {
        assert_eq!(
            validate_token(
                &config(),
                &token("vault:read", serde_json::json!({})),
                &keys()
            ),
            Ok(false)
        );
        assert_eq!(
            validate_token(
                &config(),
                &token("vault:read vault:write", serde_json::json!({})),
                &keys()
            ),
            Ok(true)
        );
        for changes in [
            serde_json::json!({"iss":"https://wrong.test"}),
            serde_json::json!({"aud":"another-service"}),
            serde_json::json!({"sub":"someone-else"}),
            serde_json::json!({"exp":chrono::Utc::now().timestamp()-100}),
            serde_json::json!({"nbf":chrono::Utc::now().timestamp()+100}),
            serde_json::json!({"exp":null}),
        ] {
            assert!(validate_token(&config(), &token("vault:read", changes), &keys()).is_err());
        }
        assert!(
            validate_token(
                &config(),
                &token("vault:write", serde_json::json!({})),
                &keys()
            )
            .is_err()
        );
        assert!(validate_token(&config(), "malformed", &keys()).is_err());
        let mut forged = token("vault:read", serde_json::json!({}));
        let index = forged.rfind('.').unwrap() + 1;
        let replacement = if &forged[index..index + 1] == "A" {
            "B"
        } else {
            "A"
        };
        forged.replace_range(index..index + 1, replacement);
        assert!(validate_token(&config(), &forged, &keys()).is_err());
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("test-key".into());
        let symmetric = encode(
            &header,
            &serde_json::json!({"sub":"ryan"}),
            &EncodingKey::from_secret(b"not-a-valid-signing-key"),
        )
        .unwrap();
        assert!(validate_token(&config(), &symmetric, &keys()).is_err());
    }

    fn app() -> Router {
        let state = Arc::new(OAuthState {
            config: config(),
            http: reqwest::Client::new(),
            jwks_uri: Url::parse("https://unused.example.test/jwks").unwrap(),
            keys: RwLock::new(KeyCache {
                keys: keys(),
                fetched: Instant::now(),
            }),
        });
        Router::new()
            .route("/mcp", post(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(state.clone(), authenticate))
            .merge(metadata_routes(state))
    }

    async fn call(scope: &str, name: &str, arguments: serde_json::Value) -> Response {
        app().oneshot(HttpRequest::post("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {}", token(scope, serde_json::json!({}))))
            // Scope validation must hold even when a session was initialized with a write token.
            .header("Mcp-Session-Id", "writer-session")
            .body(Body::from(serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":name,"arguments":arguments}}).to_string())).unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn enforces_scope_and_revision_on_every_request() {
        assert_eq!(
            call(
                "vault:read",
                "note_write",
                serde_json::json!({"expected_revision":"revision"})
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call("vault:read", "note_read", serde_json::json!({}))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            call(
                "vault:read vault:write",
                "note_write",
                serde_json::json!({})
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(
                "vault:read vault:write",
                "note_write",
                serde_json::json!({"expected_revision":"revision"})
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            call(
                "vault:read vault:write",
                "frontmatter",
                serde_json::json!({"action":"get"})
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            call(
                "vault:read vault:write",
                "frontmatter",
                serde_json::json!({"action":"set"})
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn discovery_is_public_but_tools_require_auth_and_reject_foreign_origins() {
        let response = app()
            .oneshot(
                HttpRequest::get("/.well-known/oauth-protected-resource/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let meta: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(meta["resource"], config().resource);
        let response = app()
            .oneshot(HttpRequest::post("/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .contains("resource_metadata=")
        );
        let response = app()
            .oneshot(
                HttpRequest::post("/mcp")
                    .header(header::ORIGIN, "https://evil.test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
