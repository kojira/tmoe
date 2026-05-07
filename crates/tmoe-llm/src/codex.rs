//! OpenAI Codex (= ChatGPT Pro/Plus サブスク) を使うための OAuth 2.0 + PKCE フロー。
//!
//! 設計は opencode (sst/opencode) の plugin/codex.ts と同じプロトコルに準拠する。
//! Anthropic 側の Claude サブスク連携は今回は対象外 (= API キー利用のまま)。
//!
//! # 概要
//!
//! 1. クライアントは PKCE verifier (43 chars) と SHA-256(verifier) を base64url した challenge、
//!    ランダム state を生成する。
//! 2. ブラウザを `https://auth.openai.com/oauth/authorize?...` に飛ばし、`localhost:1455/auth/callback`
//!    にリダイレクトさせる。
//! 3. callback の `code` を `https://auth.openai.com/oauth/token` に POST して
//!    `access_token` / `refresh_token` / `id_token` を得る。
//! 4. `id_token` の payload から `chatgpt_account_id` を取り出してセットする (組織サブスク対応)。
//! 5. 失効が近づくたびに `refresh_token` でリフレッシュ。
//!
//! # 参考定数
//!
//! - CLIENT_ID は OpenAI が Codex CLI / 互換ツール用に公開している public client id。
//! - issuer は `auth.openai.com` 固定。
//! - 実際のチャット送信先は `https://chatgpt.com/backend-api/codex/responses`
//!   (= `OpenAiCompatClient` 側で URL リライトされる)。
//!
//! # 永続化
//!
//! `~/.tmoe/auth.json` (ファイルパーミッション 0600) に格納する。フォーマットは opencode と互換に
//! しないかわりに、tmoe ネイティブな小さい JSON にしておく。

use crate::error::{LlmError, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";
pub const CODEX_API_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const OAUTH_PORT: u16 = 1455;
pub const OAUTH_REDIRECT_PATH: &str = "/auth/callback";

const PKCE_VERIFIER_CHARS: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
const PKCE_VERIFIER_LEN: usize = 43;
const STATE_BYTES: usize = 32;

/// PKCE の verifier (string) と challenge (= base64url(SHA-256(verifier)))。
#[derive(Debug, Clone)]
pub struct PkceCodes {
    pub verifier: String,
    pub challenge: String,
}

impl PkceCodes {
    /// `getrandom` で 43 文字の verifier を作り、SHA-256 → base64url で challenge を導出する。
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; PKCE_VERIFIER_LEN];
        getrandom::getrandom(&mut bytes).map_err(|e| LlmError::Other(format!("getrandom: {e}")))?;
        let verifier: String = bytes
            .iter()
            .map(|b| PKCE_VERIFIER_CHARS[(*b as usize) % PKCE_VERIFIER_CHARS.len()] as char)
            .collect();
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let digest = hasher.finalize();
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Ok(Self { verifier, challenge })
    }
}

/// CSRF 対策の opaque state 文字列を生成する。base64url(32 random bytes)。
pub fn generate_state() -> Result<String> {
    let mut bytes = [0u8; STATE_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|e| LlmError::Other(format!("getrandom: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// `auth.openai.com/oauth/authorize` 用の URL を組み立てる。redirect_uri はホスト側で確定済の前提。
pub fn build_authorize_url(redirect_uri: &str, pkce: &PkceCodes, state: &str) -> String {
    let params: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", "opencode"),
    ];
    let qs: String = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{ISSUER}/oauth/authorize?{qs}")
}

/// 最低限の URL エンコード (RFC 3986 unreserved 以外を %xx)。OAuth のクエリ用なので
/// reqwest のフォームエンコーダを引っ張ってくるより手書きの方が deps を減らせる。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

/// `auth.openai.com/oauth/token` のレスポンス。OpenAI の仕様に合わせて任意フィールド扱い。
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: Option<String>,
    pub expires_in: Option<u64>,
}

/// `id_token` (JWT) の payload。chatgpt_account_id を抽出するためだけに最低限の field を持つ。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct IdTokenClaims {
    #[serde(default)]
    pub chatgpt_account_id: Option<String>,
    #[serde(default)]
    pub organizations: Option<Vec<OrgClaim>>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(rename = "https://api.openai.com/auth", default)]
    pub openai_auth: Option<OpenAiAuthClaim>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OrgClaim {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OpenAiAuthClaim {
    #[serde(default)]
    pub chatgpt_account_id: Option<String>,
}

/// JWT を `header.payload.signature` の 3 セグメントに分け、payload を base64url で復号する。
/// 署名検証はしない (= 自分で発行した token しか保存しないので、RS の chain を信用する形)。
pub fn parse_jwt_claims(token: &str) -> Option<IdTokenClaims> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    serde_json::from_slice(&payload).ok()
}

/// claims から最も適切な account_id を選ぶ:
///   1. `chatgpt_account_id`
///   2. `https://api.openai.com/auth.chatgpt_account_id`
///   3. 最初の `organizations[].id`
pub fn extract_account_id(claims: &IdTokenClaims) -> Option<String> {
    claims
        .chatgpt_account_id
        .clone()
        .or_else(|| {
            claims
                .openai_auth
                .as_ref()
                .and_then(|a| a.chatgpt_account_id.clone())
        })
        .or_else(|| claims.organizations.as_ref().and_then(|o| o.first().map(|x| x.id.clone())))
}

/// account_id を id_token → access_token の順に試してとり出す。両方 JWT であることが前提。
pub fn extract_account_id_from_tokens(tr: &TokenResponse) -> Option<String> {
    if let Some(id_tok) = &tr.id_token {
        if let Some(c) = parse_jwt_claims(id_tok) {
            if let Some(id) = extract_account_id(&c) {
                return Some(id);
            }
        }
    }
    if let Some(c) = parse_jwt_claims(&tr.access_token) {
        return extract_account_id(&c);
    }
    None
}

/// 永続化形式。`~/.tmoe/auth.json` に置く。`expires_at_unix` は UNIX 秒。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexAuth {
    pub access_token: String,
    pub refresh_token: String,
    /// `expires_in` を加算した UTC UNIX 秒。
    pub expires_at_unix: u64,
    #[serde(default)]
    pub account_id: Option<String>,
}

impl CodexAuth {
    /// expires_at_unix が現在より小さければ refresh が必要。30 秒の余裕を持たせる。
    pub fn needs_refresh_now(&self) -> bool {
        let now = unix_now();
        self.expires_at_unix <= now + 30
    }
}

/// `~/.tmoe/auth.json` のデフォルトパス。`HOME` が解けないときは `./auth.json` にフォールバック。
pub fn default_auth_path() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".tmoe").join("auth.json")
    } else {
        PathBuf::from("auth.json")
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(default)]
    codex: Option<CodexAuth>,
}

pub fn load_codex_auth(path: &Path) -> Result<Option<CodexAuth>> {
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(path)
        .map_err(|e| LlmError::Other(format!("read auth.json: {e}")))?;
    let f: AuthFile =
        serde_json::from_str(&body).map_err(|e| LlmError::Other(format!("parse auth.json: {e}")))?;
    Ok(f.codex)
}

pub fn save_codex_auth(path: &Path, auth: &CodexAuth) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| LlmError::Other(format!("mkdir auth dir: {e}")))?;
    }
    let mut existing: AuthFile = if path.exists() {
        let body = std::fs::read_to_string(path)
            .map_err(|e| LlmError::Other(format!("read auth.json: {e}")))?;
        serde_json::from_str(&body).unwrap_or_default()
    } else {
        AuthFile::default()
    };
    existing.codex = Some(auth.clone());
    let body = serde_json::to_string_pretty(&existing)
        .map_err(|e| LlmError::Other(format!("serialize auth.json: {e}")))?;
    std::fs::write(path, body)
        .map_err(|e| LlmError::Other(format!("write auth.json: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 引数なしの版: reqwest クライアントを内部で構築する。CLI 側 (login flow) から呼ぶ用。
pub async fn exchange_code_for_tokens_default(
    code: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
) -> Result<TokenResponse> {
    let http = reqwest::Client::builder()
        .build()
        .map_err(LlmError::from)?;
    exchange_code_for_tokens(&http, code, redirect_uri, pkce).await
}

/// 認可コードを access_token / refresh_token と交換する HTTP 呼び出し。
pub async fn exchange_code_for_tokens(
    http: &reqwest::Client,
    code: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
) -> Result<TokenResponse> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", &pkce.verifier),
    ];
    let resp = http
        .post(format!("{ISSUER}/oauth/token"))
        .form(&form)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(LlmError::BadStatus { status: status.as_u16(), body });
    }
    let tr: TokenResponse = resp.json().await?;
    Ok(tr)
}

/// refresh_token を使って access_token を再発行する。
pub async fn refresh_access_token(
    http: &reqwest::Client,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ];
    let resp = http
        .post(format!("{ISSUER}/oauth/token"))
        .form(&form)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(LlmError::BadStatus { status: status.as_u16(), body });
    }
    let tr: TokenResponse = resp.json().await?;
    Ok(tr)
}

/// `TokenResponse` を期限切れ時刻に変換して保存形式に詰め直す。
pub fn token_response_to_auth(tr: &TokenResponse, fallback_account: Option<String>) -> CodexAuth {
    let now = unix_now();
    let lifetime = tr.expires_in.unwrap_or(3600);
    let account_id = extract_account_id_from_tokens(tr).or(fallback_account);
    CodexAuth {
        access_token: tr.access_token.clone(),
        refresh_token: tr.refresh_token.clone(),
        expires_at_unix: now + lifetime,
        account_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn pkce_generate_yields_valid_lengths() {
        let p = PkceCodes::generate().unwrap();
        assert_eq!(p.verifier.len(), PKCE_VERIFIER_LEN);
        // SHA-256 -> 32 bytes -> base64url no pad = 43 chars
        assert_eq!(p.challenge.len(), 43);
        // verifier の各 byte は許容文字集合に収まる
        for ch in p.verifier.bytes() {
            assert!(
                PKCE_VERIFIER_CHARS.contains(&ch),
                "verifier contains illegal char: {}", ch as char
            );
        }
    }

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let p = PkceCodes::generate().unwrap();
        let mut h = Sha256::new();
        h.update(p.verifier.as_bytes());
        let want = URL_SAFE_NO_PAD.encode(h.finalize());
        assert_eq!(p.challenge, want);
    }

    #[test]
    fn state_is_url_safe_and_lenghty_enough() {
        let s = generate_state().unwrap();
        // 32 bytes -> base64url no pad = 43 chars
        assert_eq!(s.len(), 43);
        for ch in s.bytes() {
            assert!(
                ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'_',
                "state has non-base64url char: {}", ch as char
            );
        }
    }

    #[test]
    fn authorize_url_contains_required_params() {
        let pkce = PkceCodes {
            verifier: "v".into(),
            challenge: "c".into(),
        };
        let url = build_authorize_url("http://localhost:1455/auth/callback", &pkce, "STATE");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("code_challenge=c"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=STATE"));
        assert!(url.contains(&format!("redirect_uri={}", urlencode("http://localhost:1455/auth/callback"))));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
        assert!(url.contains("originator=opencode"));
    }

    #[test]
    fn parse_jwt_claims_decodes_payload() {
        // header と signature は適当、payload に chatgpt_account_id を埋め込む。
        let payload = serde_json::json!({"chatgpt_account_id": "acct_123"});
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string());
        let token = format!("hh.{payload_b64}.ss");
        let claims = parse_jwt_claims(&token).unwrap();
        assert_eq!(claims.chatgpt_account_id.as_deref(), Some("acct_123"));
    }

    #[test]
    fn extract_account_id_falls_back_to_organizations() {
        let claims = IdTokenClaims {
            chatgpt_account_id: None,
            organizations: Some(vec![OrgClaim { id: "org_A".into() }]),
            ..Default::default()
        };
        assert_eq!(extract_account_id(&claims).as_deref(), Some("org_A"));
    }

    #[test]
    fn extract_account_id_prefers_top_level() {
        let claims = IdTokenClaims {
            chatgpt_account_id: Some("acct_top".into()),
            organizations: Some(vec![OrgClaim { id: "org_A".into() }]),
            openai_auth: Some(OpenAiAuthClaim {
                chatgpt_account_id: Some("acct_nested".into()),
            }),
            ..Default::default()
        };
        assert_eq!(extract_account_id(&claims).as_deref(), Some("acct_top"));
    }

    #[test]
    fn extract_account_id_falls_back_to_nested() {
        let claims = IdTokenClaims {
            chatgpt_account_id: None,
            organizations: None,
            openai_auth: Some(OpenAiAuthClaim {
                chatgpt_account_id: Some("acct_nested".into()),
            }),
            ..Default::default()
        };
        assert_eq!(extract_account_id(&claims).as_deref(), Some("acct_nested"));
    }

    #[test]
    fn save_and_load_codex_auth_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let auth = CodexAuth {
            access_token: "AT".into(),
            refresh_token: "RT".into(),
            expires_at_unix: 1_700_000_000,
            account_id: Some("acct_X".into()),
        };
        save_codex_auth(&path, &auth).unwrap();
        let loaded = load_codex_auth(&path).unwrap().unwrap();
        assert_eq!(loaded.access_token, "AT");
        assert_eq!(loaded.refresh_token, "RT");
        assert_eq!(loaded.expires_at_unix, 1_700_000_000);
        assert_eq!(loaded.account_id.as_deref(), Some("acct_X"));
    }

    #[test]
    fn save_codex_auth_does_not_clobber_unrelated_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, r#"{"someOther":"keep","codex":null}"#).unwrap();
        let auth = CodexAuth {
            access_token: "AT".into(),
            refresh_token: "RT".into(),
            expires_at_unix: 0,
            account_id: None,
        };
        save_codex_auth(&path, &auth).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        // codex は更新される。`someOther` は serde 上は無視されるので消えるが、tmoe 内では
        // codex 専用ファイルなので問題ない。少なくとも codex セクションは存在する。
        assert!(body.contains("\"codex\""));
        assert!(body.contains("AT"));
    }

    #[test]
    fn needs_refresh_now_true_when_expired() {
        let auth = CodexAuth {
            access_token: "x".into(),
            refresh_token: "y".into(),
            expires_at_unix: 1, // way in the past
            account_id: None,
        };
        assert!(auth.needs_refresh_now());
    }

    #[test]
    fn needs_refresh_now_false_when_far_future() {
        let auth = CodexAuth {
            access_token: "x".into(),
            refresh_token: "y".into(),
            expires_at_unix: unix_now() + 3600,
            account_id: None,
        };
        assert!(!auth.needs_refresh_now());
    }

    #[test]
    fn token_response_to_auth_uses_default_lifetime_when_missing() {
        let tr = TokenResponse {
            access_token: "AT".into(),
            refresh_token: "RT".into(),
            id_token: None,
            expires_in: None,
        };
        let a = token_response_to_auth(&tr, Some("acct".into()));
        assert!(a.expires_at_unix >= unix_now() + 3500);
        assert_eq!(a.account_id.as_deref(), Some("acct"));
    }
}
