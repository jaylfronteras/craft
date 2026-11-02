//! Pictures from any OpenAI-compatible image API (`POST {base}/images/generations`).
//!
//! The base URL is `[assets] base_url`, else `OPENAI_BASE_URL`, else `https://api.openai.com/v1`.
//! The key is the stored asset key, else `OPENAI_API_KEY`. A loopback server may omit the key.
//! The model name is the asset table's `endpoint` (or whatever `[assets] image_model` names).
//!
//! There is no standard 3D endpoint, so a 3D request is refused.

use super::{AssetProvider, AssetRequest, Generated, Shape};
use craft_anything_evidence::{ErrorCode, ToolError};
use serde_json::{Value, json};
use std::io::Read;
use std::time::Duration;

/// The provider id the asset table names.
pub const ID: &str = "openai";
/// Default API root (the `/v1` prefix, not the images path).
pub const DEFAULT_BASE: &str = "https://api.openai.com/v1";
/// Overrides [`DEFAULT_BASE`] when settings don't name a base URL.
pub const ENV_BASE: &str = "OPENAI_BASE_URL";
/// Read when no asset key is stored.
pub const ENV_KEY: &str = "OPENAI_API_KEY";
/// Largest picture we keep.
const MAX_BYTES: u64 = 32 << 20;

/// Settings win, then the environment variable, then [`DEFAULT_BASE`].
pub fn resolve_base(settings_base: Option<&str>) -> String {
    resolve_base_with(settings_base, std::env::var(ENV_BASE).ok().as_deref())
}

/// Same as [`resolve_base`], with the environment value passed in (tests don't touch the process).
pub fn resolve_base_with(settings_base: Option<&str>, env_base: Option<&str>) -> String {
    let pick = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(|s| s.trim_end_matches('/').to_string());
    pick(settings_base).or_else(|| pick(env_base)).unwrap_or_else(|| DEFAULT_BASE.to_string())
}

/// A server on this machine doesn't need a key (Ollama, LM Studio).
pub fn allows_empty_key(base: &str) -> bool {
    matches!(host_of(base), "localhost" | "127.0.0.1" | "::1")
}

fn host_of(base: &str) -> &str {
    let rest = base.split("://").nth(1).unwrap_or(base);
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let hostport = hostport.rsplit('@').next().unwrap_or(hostport);
    if let Some(inside) = hostport.strip_prefix('[') { inside.split(']').next().unwrap_or(inside) } else { hostport.split(':').next().unwrap_or(hostport) }
}

/// Pixel size the images API accepts. gpt-image uses 1536 on the long side; DALL-E 3 uses 1792.
pub fn image_size(model: &str, shape: Shape) -> &'static str {
    let gpt = model.starts_with("gpt-image");
    match (gpt, shape) {
        (_, Shape::Square) => "1024x1024",
        (true, Shape::Wide) => "1536x1024",
        (true, Shape::Tall) => "1024x1536",
        (false, Shape::Wide) => "1792x1024",
        (false, Shape::Tall) => "1024x1792",
    }
}

/// An OpenAI-compatible image server, as an [`AssetProvider`].
pub struct OpenAiCompat {
    base: String,
}

impl OpenAiCompat {
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into().trim().trim_end_matches('/').to_string() }
    }

    /// The base URL from settings, then `OPENAI_BASE_URL`, then the default.
    pub fn from_settings(settings_base: Option<&str>) -> Self {
        Self::new(resolve_base(settings_base))
    }

    fn images_url(&self) -> String {
        format!("{}/images/generations", self.base)
    }
}

impl Default for OpenAiCompat {
    fn default() -> Self {
        Self::from_settings(None)
    }
}

impl AssetProvider for OpenAiCompat {
    fn id(&self) -> &'static str {
        ID
    }

    fn name(&self) -> &'static str {
        "OpenAI-compatible"
    }

    fn key_env(&self) -> Option<&'static str> {
        Some(ENV_KEY)
    }

    fn allows_empty_key(&self) -> bool {
        allows_empty_key(&self.base)
    }

    fn generate(&self, key: &str, req: &AssetRequest) -> Result<Generated, ToolError> {
        let key = key.trim();
        match *req {
            AssetRequest::Image { model, prompt, shape } => {
                if !(self.base.starts_with("https://") || self.base.starts_with("http://")) || host_of(&self.base).is_empty() {
                    return Err(ToolError::new(ErrorCode::BadSettings, format!("The image API base URL {:?} isn't a web address.", self.base))
                        .with_remediation(
                            "Set OPENAI_BASE_URL or [assets] base_url to an http or https address ending in /v1, for example https://api.openai.com/v1.",
                        ));
                }
                if key.is_empty() && !self.allows_empty_key() {
                    return Err(ToolError::new(ErrorCode::BadSettings, "No OpenAI-compatible key is set up, so pictures can't be generated.")
                        .with_remediation("Set OPENAI_API_KEY, or add the key in Settings → AI → Game assets. A server on localhost can omit the key."));
                }
                let size = image_size(&model.endpoint, shape);
                let mut body = json!({"model": model.endpoint, "prompt": prompt, "n": 1, "size": size});
                // gpt-image always returns base64 and rejects response_format. DALL-E and most
                // compatible servers want b64_json so the picture isn't a short-lived URL.
                if !model.endpoint.starts_with("gpt-image")
                    && let Some(o) = body.as_object_mut()
                {
                    o.insert("response_format".into(), json!("b64_json"));
                }
                let v = self.post(key, &body)?;
                let (bytes, url) = image_bytes(&v)?;
                Ok(Generated { bytes, url, cost_usd: req.listed_cost() })
            }
            AssetRequest::Model3d { .. } => Err(ToolError::new(ErrorCode::FeatureMissing, "3D model generation isn't available on an OpenAI-compatible API.")
                .with_remediation("There is no standard 3D endpoint. Make a picture with assets_image_generate, or build the object from simple shapes.")),
        }
    }
}

impl OpenAiCompat {
    fn post(&self, key: &str, body: &Value) -> Result<Value, ToolError> {
        let mut req = agent(Duration::from_secs(180)).post(self.images_url()).header("Content-Type", "application/json");
        if !key.is_empty() {
            req = req.header("Authorization", &format!("Bearer {key}"));
        }
        let resp = req.send(body.to_string()).map_err(|e| {
            ToolError::new(ErrorCode::EngineMissing, format!("Couldn't reach the image API: {e}"))
                .with_remediation("Check the base URL and your internet connection, then try again.")
        })?;
        let status = resp.status().as_u16();
        let text = resp.into_body().read_to_string().unwrap_or_default();
        match status {
            200..=299 => {
                serde_json::from_str(&text).map_err(|e| ToolError::new(ErrorCode::GarbledOutput, format!("The image API sent an unreadable answer: {e}")))
            }
            401 | 403 => Err(ToolError::new(ErrorCode::BadSettings, "The image API didn't accept the key.")
                .with_remediation("Check OPENAI_API_KEY, or the key in Settings → AI → Game assets, and the base URL.")),
            402 | 429 => Err(ToolError::new(ErrorCode::OverLimit, "The image API refused the request (quota or rate limit).")
                .with_remediation("Wait and try again, or check the account's billing.")),
            400 if policy_block(&text) => {
                Err(ToolError::new(ErrorCode::ProgramFailed, "The image API's safety check blocked that picture.").with_remediation("Describe it differently."))
            }
            _ => Err(ToolError::new(ErrorCode::EngineMissing, format!("The image API answered {status}: {}", text.chars().take(300).collect::<String>()))),
        }
    }
}

fn policy_block(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("content_policy") || t.contains("safety")
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent("CraftAnything-dev")
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(Duration::from_secs(180)))
        .build()
        .new_agent()
}

fn image_bytes(v: &Value) -> Result<(Vec<u8>, String), ToolError> {
    let item = v.pointer("/data/0").ok_or_else(|| unexpected("picture", v))?;
    if let Some(b64) = item.get("b64_json").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        return Ok((base64_decode(b64)?, "inline".into()));
    }
    if let Some(url) = item.get("url").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        return Ok((download(url)?, url.to_string()));
    }
    Err(unexpected("picture", v))
}

fn download(url: &str) -> Result<Vec<u8>, ToolError> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(ToolError::new(ErrorCode::GarbledOutput, "The image API sent a picture address that isn't a web address."));
    }
    let resp =
        agent(Duration::from_secs(120)).get(url).call().map_err(|e| ToolError::new(ErrorCode::EngineMissing, format!("Couldn't download the picture: {e}")))?;
    if !resp.status().is_success() {
        return Err(ToolError::new(ErrorCode::EngineMissing, format!("Downloading the picture failed ({}).", resp.status().as_u16())));
    }
    let mut bytes = Vec::new();
    resp.into_body()
        .into_reader()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ToolError::new(ErrorCode::EngineMissing, format!("The download broke off: {e}")))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(ToolError::new(ErrorCode::OverLimit, "The generated picture is too large."));
    }
    Ok(bytes)
}

fn unexpected(what: &str, v: &Value) -> ToolError {
    ToolError::new(ErrorCode::GarbledOutput, format!("The image API didn't return a {what}."))
        .with_details(json!({"answer": v.to_string().chars().take(500).collect::<String>()}))
}

fn base64_decode(s: &str) -> Result<Vec<u8>, ToolError> {
    use base64::Engine as _;
    let s = s.trim();
    if s.len() as u64 > MAX_BYTES.saturating_mul(2) {
        return Err(ToolError::new(ErrorCode::OverLimit, "The generated picture is too large."));
    }
    base64::engine::general_purpose::STANDARD.decode(s).map_err(|e| ToolError::new(ErrorCode::GarbledOutput, format!("invalid base64: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::Catalogue;
    use craft_anything_platform::sync::Mutex;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    /// A tiny local server that answers each request in turn and records what it was sent.
    /// `make` sees the base URL so an answer can point at this same server.
    fn serve(make: impl FnOnce(&str) -> Vec<(u16, String)> + Send + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let answers = make(&base);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            for (status, body) in answers {
                let (mut stream, _) = l.accept().unwrap();
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    head.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut req_body = vec![0; len];
                r.read_exact(&mut req_body).unwrap();
                s2.lock().unwrap().push(format!("{head}{}", String::from_utf8_lossy(&req_body)));
                write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                    .unwrap();
            }
        });
        (base, seen)
    }

    fn image(api: &OpenAiCompat, key: &str, model_id: Option<&str>, prompt: &str, shape: Shape) -> Result<Generated, ToolError> {
        let c = Catalogue::built_in();
        let model = c.resolve_image(model_id).unwrap();
        api.generate(key, &AssetRequest::Image { model: &model, prompt, shape })
    }

    #[test]
    fn a_square_picture_is_a_chat_style_images_request() {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG fake");
        let (base, seen) = serve(move |_| vec![(200, json!({"data": [{"b64_json": png}]}).to_string())]);
        let api = OpenAiCompat::new(format!("{base}/v1"));
        let pic = image(&api, " k-123 ", None, "a wooden crate, game asset", Shape::Square).unwrap();
        assert_eq!(pic.bytes, b"\x89PNG fake");
        assert!((pic.cost_usd - 0.04).abs() < 1e-9, "dall-e-3 standard 1024");
        let seen = seen.lock().unwrap();
        assert!(seen[0].starts_with("POST /v1/images/generations "), "{}", seen[0]);
        assert!(seen[0].to_ascii_lowercase().contains("authorization: bearer k-123"), "{}", seen[0]);
        assert!(seen[0].contains("\"model\":\"dall-e-3\"") && seen[0].contains("\"size\":\"1024x1024\""), "{}", seen[0]);
        assert!(seen[0].contains("\"response_format\":\"b64_json\"") && seen[0].contains("\"n\":1"), "{}", seen[0]);
    }

    #[test]
    fn gpt_image_wide_omits_response_format_and_uses_1536() {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD.encode(b"PNG");
        let (base, seen) = serve(move |_| vec![(200, json!({"data": [{"b64_json": png}]}).to_string())]);
        let pic = image(&OpenAiCompat::new(base), "k", Some("gpt-image-1"), "a night sky", Shape::Wide).unwrap();
        assert_eq!(pic.bytes, b"PNG");
        assert!((pic.cost_usd - 0.06).abs() < 1e-9, "the wide estimate");
        let seen = seen.lock().unwrap();
        assert!(seen[0].starts_with("POST /images/generations "), "{}", seen[0]);
        assert!(seen[0].contains("\"model\":\"gpt-image-1\"") && seen[0].contains("\"size\":\"1536x1024\""), "{}", seen[0]);
        assert!(!seen[0].contains("response_format"), "{}", seen[0]);
    }

    #[test]
    fn a_custom_model_name_is_sent_as_given() {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD.encode(b"x");
        let (base, seen) = serve(move |_| vec![(200, json!({"data": [{"b64_json": png}]}).to_string())]);
        let pic = image(&OpenAiCompat::new(base), "k", Some("local-flux"), "a crate", Shape::Tall).unwrap();
        assert!((pic.cost_usd - 0.08).abs() < 1e-9, "priced like the default model's tall size");
        let seen = seen.lock().unwrap();
        assert!(seen[0].contains("\"model\":\"local-flux\"") && seen[0].contains("\"size\":\"1024x1792\""), "{}", seen[0]);
    }

    #[test]
    fn a_url_answer_is_downloaded_and_a_loopback_server_needs_no_key() {
        let (base, seen) = serve(|base| vec![(200, json!({"data": [{"url": format!("{base}/pic.png")}]}).to_string()), (200, "PNGDATA".into())]);
        let pic = image(&OpenAiCompat::new(base), "", None, "a crate", Shape::Square).unwrap();
        assert_eq!(pic.bytes, b"PNGDATA");
        let seen = seen.lock().unwrap();
        assert!(!seen[0].to_ascii_lowercase().contains("authorization"), "no key, no header: {}", seen[0]);
        assert!(seen[1].starts_with("GET /pic.png "), "{}", seen[1]);
    }

    #[test]
    fn refusals_say_what_to_do() {
        let (base, _) = serve(|_| vec![(401, "{}".into()), (429, "{}".into()), (400, json!({"error": {"code": "content_policy_violation"}}).to_string())]);
        let api = OpenAiCompat::new(base);
        assert_eq!(image(&api, "bad", None, "x", Shape::Wide).unwrap_err().code, ErrorCode::BadSettings);
        assert_eq!(image(&api, "bad", None, "x", Shape::Wide).unwrap_err().code, ErrorCode::OverLimit);
        assert!(image(&api, "bad", None, "x", Shape::Wide).unwrap_err().message.contains("safety check"));
    }

    #[test]
    fn a_public_server_without_a_key_is_refused_before_any_request() {
        let c = Catalogue::built_in();
        let model = c.resolve_image(None).unwrap();
        let err =
            OpenAiCompat::new("https://api.openai.com/v1").generate("", &AssetRequest::Image { model: &model, prompt: "x", shape: Shape::Square }).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadSettings);
        assert!(err.remediation.unwrap_or_default().contains("OPENAI_API_KEY"), "{}", err.message);
        let placeholder = crate::assets::Model3d {
            id: "none".into(),
            provider: ID.into(),
            name: "none".into(),
            endpoint: "none".into(),
            usd: 0.0,
            textured_usd: 0.0,
            faces: [1, 2],
            licence: String::new(),
        };
        let err = OpenAiCompat::new("https://api.openai.com/v1")
            .generate("k", &AssetRequest::Model3d { model: &placeholder, image_url: "https://x/a.png", textured: true, faces: 100 })
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::FeatureMissing);
        assert!(err.message.contains("3D"), "{}", err.message);
    }

    #[test]
    fn base_url_and_sizes() {
        assert_eq!(resolve_base_with(None, None), DEFAULT_BASE);
        assert_eq!(resolve_base_with(None, Some(" http://127.0.0.1:11434/v1/ ")), "http://127.0.0.1:11434/v1");
        assert_eq!(resolve_base_with(Some("https://example.test/v1"), Some("http://127.0.0.1:11434/v1")), "https://example.test/v1");
        assert!(allows_empty_key("http://127.0.0.1:11434/v1"));
        assert!(allows_empty_key("http://localhost:1234/v1"));
        assert!(!allows_empty_key(DEFAULT_BASE));
        assert!(!allows_empty_key("https://api.groq.com/openai/v1"));
        assert_eq!(image_size("dall-e-3", Shape::Tall), "1024x1792");
        assert_eq!(image_size("gpt-image-1", Shape::Square), "1024x1024");
        assert_eq!(OpenAiCompat::default().key_env(), Some(ENV_KEY));
    }
}
