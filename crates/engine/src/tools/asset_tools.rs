//! Game assets the builder generates with an OpenAI-compatible image API: `assets_image_generate`
//! (a picture: a texture, a sky, concept art). `assets_model_generate` stays so a caller hears that
//! 3D models have no standard endpoint. Each picture is paid for, so a project has a budget: the
//! builder spends freely under it, and the person is asked before it goes over. Nothing is generated
//! unless the person turned it on for the project (`project_assets_allow`). Every asset and its cost
//! is listed in the project (assets.json). The provider comes from the `AssetProviders` registry and
//! the models and prices from the asset table, so the cost is known before anything is spent.

use crate::inbox::Inbox;
use crate::tool::{Ctx, Effects, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use crate::tools::interaction_tools::{Verdict, ask_approval, inbox, inside, resolve, workspace_of};
use crate::workspace::{AssetRecord, Workspace};
use craft_anything_engines::assets::{AssetProvider, AssetProviders, AssetRequest, Catalogue, Generated, Shape, about_usd, openai};
use craft_anything_evidence::{ErrorCode, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The person's key for each asset provider (by provider id), put in the session by the tool
/// server (read from the keychain; a key never travels on a command line or into a log).
#[derive(Default)]
pub struct AssetKeys(pub std::collections::BTreeMap<String, String>);

impl AssetKeys {
    /// The key for `provider`, if one is set up.
    pub fn get(&self, provider: &str) -> Option<&str> {
        self.0.get(provider).map(String::as_str).filter(|k| !k.trim().is_empty())
    }
}

/// Remembered when the person says "always" to going over the budget.
const OVER_BUDGET: &str = "assets:over-budget";

pub fn specs() -> Vec<ToolSpec> {
    let effects = Effects::ACCESSES_NETWORK.union(Effects::WRITES_FILESYSTEM);
    vec![
        ToolSpec {
            id: "assets_image_generate",
            title: "Generate a picture",
            summary: &summaries().0,
            beginner: "Makes a picture for your game, like a texture or a sky.",
            toolset: Toolset::Core,
            effects,
            requires: Requires::Nothing,
            input_schema: schema_of::<ImageParams>,
            run: assets_image_generate,
        },
        ToolSpec {
            id: "assets_model_generate",
            title: "Generate a 3D model",
            summary: &summaries().1,
            beginner: "3D models aren't available from an OpenAI-compatible API.",
            toolset: Toolset::Core,
            effects,
            requires: Requires::Nothing,
            input_schema: schema_of::<ModelParams>,
            run: assets_model_generate,
        },
    ]
}

/// The two tools' summaries. The picture price comes from the asset table's default model.
fn summaries() -> &'static (String, String) {
    static TEXT: OnceLock<(String, String)> = OnceLock::new();
    TEXT.get_or_init(|| {
        let c = Catalogue::built_in();
        let (picture, picture_model) = c.image_model(None).map(|m| (m.cost(Shape::Square), m.name.as_str())).unwrap_or((0.0, "the default model"));
        let image = format!(
            "Make a picture with an OpenAI-compatible API ({picture_model}) and save it as a PNG inside app/: a texture, a sky or background, an icon, a title screen or concept art. shape: square (1024x1024, default), wide (1792x1024, or 1536x1024 for a gpt-image model) or tall (1024x1792, or 1024x1536 for a gpt-image model). Costs {}, counted against the project's asset budget. Describe original designs in the original's style; never ask for recognisable characters, logos or brand artwork. Returns where it was saved, the cost and the budget left.",
            about_usd(picture)
        );
        let model = "3D model generation isn't available. OpenAI-compatible APIs have no image-to-3D endpoint, so this does not make a GLB. Use assets_image_generate for a texture, sky or icon, or build the object from simple shapes in the game engine. Describe original designs only; never ask for recognisable characters, logos or brand artwork.".to_string();
        (image, model)
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageParams {
    /// What to draw, in detail (subject, style, colours, lighting).
    pub prompt: String,
    /// Where to save it, relative to app/, ending in .png (e.g. Assets/Textures/crate.png).
    pub save_as: String,
    /// square (default), wide or tall.
    #[serde(default)]
    pub shape: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelParams {
    /// Where to save it, relative to app/, ending in .glb (e.g. Assets/Models/crate.glb).
    pub save_as: String,
    /// What to make: one object, described in detail. Used when no image is given.
    #[serde(default)]
    pub prompt: String,
    /// A picture to turn into the model: a web address, or a picture inside the project.
    #[serde(default)]
    pub image: String,
    /// Ignored: there is no 3D endpoint.
    #[serde(default)]
    pub textured: Option<bool>,
    /// Most faces the model may have. Ignored: there is no 3D endpoint.
    #[serde(default)]
    pub faces: Option<u32>,
}

/// What a generation needs: the keys, the providers, the inbox (for the over-budget question) and
/// the project.
struct Job {
    keys: std::sync::Arc<AssetKeys>,
    providers: AssetProviders,
    inbox: Inbox,
    ws: Workspace,
}

fn providers_for(ctx: &Ctx<'_>) -> AssetProviders {
    if let Some(custom) = ctx.session.ext.get::<AssetProviders>() {
        return custom.as_ref().clone();
    }
    let mut providers = AssetProviders::built_in();
    // Settings name the server; otherwise OPENAI_BASE_URL, otherwise api.openai.com.
    providers.register(std::sync::Arc::new(openai::OpenAiCompat::from_settings(ctx.session.settings.assets.base_url.as_deref())));
    providers
}

fn job(ctx: &Ctx<'_>) -> Result<Job, ToolError> {
    let keys = ctx.session.ext.get::<AssetKeys>().unwrap_or_default();
    let providers = providers_for(ctx);
    let not_project = || ToolError::new(ErrorCode::FeatureMissing, "Assets can only be generated inside a Craft Anything project.");
    let inbox = inbox(ctx).ok_or_else(not_project)?;
    let ws = workspace_of(&inbox).ok_or_else(not_project)?;
    if !ws.manifest.generate_assets {
        return Err(ToolError::new(ErrorCode::NoPermission, "The person hasn't turned on generated assets for this project.").with_remediation(
            "Make these assets with simple shapes and colours instead, and tell the person they can turn on generated assets for this game in the app.",
        ));
    }
    Ok(Job { keys, providers, inbox, ws })
}

fn no_key(provider: &str) -> ToolError {
    ToolError::new(ErrorCode::BadSettings, format!("No {provider} key is set up, so pictures can't be generated.")).with_remediation(format!(
        "Make these with simple shapes and colours instead, and tell the person they can set OPENAI_API_KEY or add a {provider} key in Settings → AI for realistic pictures."
    ))
}

impl Job {
    /// The key for `provider`, checked before anything is asked or spent. A loopback server may have none.
    fn key(&self, provider: &dyn AssetProvider) -> Result<String, ToolError> {
        match self.keys.get(provider.id()) {
            Some(k) => Ok(k.to_string()),
            None if provider.allows_empty_key() => Ok(String::new()),
            None => Err(no_key(provider.name())),
        }
    }
}

/// Where to save inside app/: (full path, path relative to app/). `ext` is the required extension.
fn save_path(ws: &Workspace, save_as: &str, ext: &str) -> Result<(PathBuf, String), ToolError> {
    let app = ws.app_dir();
    // Paths given from the workspace ("app/Assets/x.glb") mean the same place.
    let rel = save_as.trim().trim_start_matches("./").trim_start_matches("app/").trim_start_matches("app\\");
    let path = resolve(&app, rel).filter(|p| inside(&app, p) && *p != app).ok_or_else(|| ToolError::invalid("save_as must be a file path inside app/"))?;
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)) {
        return Err(ToolError::invalid(format!("save_as must end in .{ext}")));
    }
    let shown = path.strip_prefix(&app).unwrap_or(&path).display().to_string().replace('\\', "/");
    Ok((path, shown))
}

/// Go ahead if this stays within the budget, else ask the person (unless they said "always").
fn within_budget(ctx: &mut Ctx<'_>, inbox: &Inbox, ws: &mut Workspace, cost: f64) -> Result<(), ToolError> {
    let spent = ws.assets_spent();
    let budget = ws.manifest.asset_budget_usd;
    if spent + cost <= budget + 1e-9 || ws.manifest.allowed_actions.iter().any(|k| k == OVER_BUDGET) {
        return Ok(());
    }
    // Auto mode doesn't stop to ask, and doesn't spend the person's money past the budget unasked.
    if ctx.session.settings.auto_mode {
        return Err(ToolError::new(ErrorCode::OverLimit, format!("This project's ${budget:.2} asset budget is used up (${spent:.2} so far)."))
            .with_remediation("Use simple shapes, colours and the assets already made for the rest. The person can raise the budget in the app."));
    }
    let title = format!("Spend about ${cost:.2} more on game pictures and 3D models?");
    let detail = format!(
        "This project's ${budget:.2} budget for generated assets is used up (${spent:.2} so far). Allow once for this asset, or always to let the builder keep generating for this project."
    );
    match ask_approval(ctx, inbox, Some(ws), &title, &detail, OVER_BUDGET)? {
        Verdict::Yes => Ok(()),
        Verdict::No(_) | Verdict::NoAnswer => Err(ToolError::new(ErrorCode::OverLimit, "The asset budget is used up and the person didn't raise it.")
            .with_remediation("Use simple shapes, colours and the assets already made for the rest.")),
    }
}

/// Save a generated file and list it with its cost.
fn keep(ws: &Workspace, path: &Path, rel: &str, kind: &str, made_from: &str, g: &Generated) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::io(parent.display(), e))?;
    }
    crate::project::write_atomic(path, &g.bytes)?;
    ws.record_asset(AssetRecord {
        file: rel.to_string(),
        kind: kind.into(),
        prompt: made_from.chars().take(1000).collect(),
        cost_usd: g.cost_usd,
        at_ms: crate::inbox::now_ms(),
    })
}

fn summary(ws: &Workspace, saved: &str, cost: f64, extra: Value) -> ToolOutput {
    let spent = ws.assets_spent();
    let mut v =
        json!({"saved": saved, "cost_usd": round(cost), "spent_usd": round(spent), "budget_left_usd": round((ws.manifest.asset_budget_usd - spent).max(0.0))});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    ToolOutput::Value(v)
}

fn round(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn prompt_ok(p: &str) -> Result<&str, ToolError> {
    let p = p.trim();
    if p.is_empty() || p.len() > 2000 {
        return Err(ToolError::invalid("prompt must be 1..=2000 characters"));
    }
    Ok(p)
}

fn assets_image_generate(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ImageParams = parse_params(params)?;
    let prompt = prompt_ok(&p.prompt)?;
    let shape = match p.shape.as_deref() {
        None => Shape::Square,
        Some(s) => Shape::parse(s).ok_or_else(|| ToolError::invalid("shape must be square, wide or tall"))?,
    };
    let mut j = job(ctx)?;
    let (path, rel) = save_path(&j.ws, &p.save_as, "png")?;
    let model = Catalogue::built_in().resolve_image(ctx.session.settings.assets.image_model.as_deref())?;
    let req = AssetRequest::Image { model: &model, prompt, shape };
    let provider = j.providers.for_request(&req)?;
    let key = j.key(provider.as_ref())?;
    within_budget(ctx, &j.inbox, &mut j.ws, provider.estimate(&req)?)?;
    ctx.report(None, "Making a picture…");
    let g = provider.generate(&key, &req)?;
    keep(&j.ws, &path, &rel, "image", prompt, &g)?;
    Ok(summary(&j.ws, &rel, g.cost_usd, json!({})))
}

fn assets_model_generate(_ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    // Accept the old parameters so a caller gets this explanation, not a schema error.
    let _p: ModelParams = parse_params(params)?;
    Err(ToolError::new(ErrorCode::FeatureMissing, "3D model generation isn't available: OpenAI-compatible APIs have no 3D endpoint.")
        .with_remediation("Make a picture with assets_image_generate, or build the object from simple shapes in the game engine."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::{Answer, InboxDir};
    use crate::workspace::TargetRef;
    use std::time::Duration;

    /// A game whose person allows generated assets.
    fn ws() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Workspace::create(dir.path(), "game", "A snake game", TargetRef::None).unwrap();
        w.set_person(|m| m.generate_assets = true).unwrap();
        (dir, w)
    }

    #[test]
    fn saved_inside_app_with_the_right_ending() {
        let (_d, w) = ws();
        let (p, rel) = save_path(&w, "Assets/Models/crate.glb", "glb").unwrap();
        assert_eq!(rel, "Assets/Models/crate.glb");
        assert!(p.starts_with(w.app_dir()));
        assert_eq!(save_path(&w, "app/Assets/sky.png", "png").unwrap().1, "Assets/sky.png", "paths from the workspace mean the same place");
        assert!(save_path(&w, "Assets/crate.png", "glb").is_err(), "the ending must match");
        assert!(save_path(&w, "../../evil.glb", "glb").is_err(), "nothing outside app/");
    }

    #[test]
    fn the_summaries_read_the_asset_table() {
        assert_eq!(
            summaries().0,
            "Make a picture with an OpenAI-compatible API (DALL-E 3) and save it as a PNG inside app/: a texture, a sky or background, an icon, a title screen or concept art. shape: square (1024x1024, default), wide (1792x1024, or 1536x1024 for a gpt-image model) or tall (1024x1792, or 1024x1536 for a gpt-image model). Costs about $0.04, counted against the project's asset budget. Describe original designs in the original's style; never ask for recognisable characters, logos or brand artwork. Returns where it was saved, the cost and the budget left."
        );
        assert!(summaries().1.contains("isn't available") && summaries().1.contains("assets_image_generate"), "{}", summaries().1);
    }

    /// Stands in for the image API: records each request's model name and answers with a few bytes.
    #[derive(Default)]
    struct StandIn(craft_anything_platform::sync::Mutex<Vec<String>>);

    impl craft_anything_engines::assets::AssetProvider for StandIn {
        fn id(&self) -> &'static str {
            "openai"
        }
        fn name(&self) -> &'static str {
            "stand-in"
        }
        fn generate(&self, key: &str, req: &AssetRequest) -> Result<Generated, ToolError> {
            assert_eq!(key, "k-1");
            let AssetRequest::Image { model, .. } = req else {
                return Err(ToolError::invalid("the stand-in makes pictures only"));
            };
            self.0.lock().unwrap().push(model.endpoint.clone());
            Ok(Generated { url: "https://stand-in/picture.png".into(), bytes: b"png".to_vec(), cost_usd: req.listed_cost() })
        }
    }

    #[test]
    fn a_picture_from_a_prompt_through_the_provider_registry() {
        let (_d, w) = ws();
        let stand_in = std::sync::Arc::new(StandIn::default());
        let mut providers = AssetProviders::built_in();
        providers.register(stand_in.clone());
        let mut session = crate::testing::session();
        // A name that isn't in the table is still sent as the model.
        session.settings.assets.image_model = Some("local-flux".into());
        let e = crate::Engine::new(session);
        e.session().ext.insert(std::sync::Arc::new(InboxDir(w.inbox_dir())));
        e.session().ext.insert(std::sync::Arc::new(AssetKeys([("openai".to_string(), "k-1".to_string())].into())));
        e.session().ext.insert(std::sync::Arc::new(providers));
        let out =
            e.call("assets_image_generate", json!({"prompt": "a crate", "save_as": "Assets/crate.png"}), crate::CallOptions::new(crate::Caller::Mcp)).unwrap();
        let ToolOutput::Value(v) = out else { panic!("{out:?}") };
        assert_eq!(v["saved"], "Assets/crate.png");
        assert_eq!(*stand_in.0.lock().unwrap(), vec!["local-flux".to_string()]);
        assert_eq!(std::fs::read(w.app_dir().join("Assets/crate.png")).unwrap(), b"png");
        let spent = w.assets_spent();
        assert!((spent - 0.04).abs() < 1e-9, "{spent}");
        assert_eq!(w.assets().len(), 1);
        let err = e
            .call("assets_model_generate", json!({"prompt": "a crate", "save_as": "Assets/crate.glb"}), crate::CallOptions::new(crate::Caller::Mcp))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::FeatureMissing);
        assert!(err.message.contains("no 3D endpoint"), "{}", err.message);
    }

    #[test]
    fn a_key_for_another_provider_doesnt_pay_this_one() {
        let (_d, w) = ws();
        let mut session = crate::testing::session();
        // A public base, so a missing key is an error even if OPENAI_BASE_URL points at localhost.
        session.settings.assets.base_url = Some("https://api.openai.com/v1".into());
        let e = crate::Engine::new(session);
        e.session().ext.insert(std::sync::Arc::new(InboxDir(w.inbox_dir())));
        e.session().ext.insert(std::sync::Arc::new(AssetKeys([("openai".to_string(), " ".to_string())].into())));
        let err =
            e.call("assets_image_generate", json!({"prompt": "a crate", "save_as": "crate.png"}), crate::CallOptions::new(crate::Caller::Mcp)).err().unwrap();
        assert!(err.code == ErrorCode::BadSettings, "a blank key is no key");
        let mut providers = AssetProviders::built_in();
        providers.register(std::sync::Arc::new(openai::OpenAiCompat::new("https://api.openai.com/v1")));
        providers.register(std::sync::Arc::new(Named("other")));
        e.session().ext.insert(std::sync::Arc::new(providers));
        e.session().ext.insert(std::sync::Arc::new(AssetKeys([("other".to_string(), "o-1".to_string())].into())));
        let err =
            e.call("assets_image_generate", json!({"prompt": "a crate", "save_as": "crate.png"}), crate::CallOptions::new(crate::Caller::Mcp)).err().unwrap();
        assert_eq!(err.code, ErrorCode::BadSettings);
        assert!(err.message.contains("No OpenAI-compatible key"), "{}", err.message);
        assert!(w.assets().is_empty() && Inbox::new(w.inbox_dir()).pending().is_empty(), "nothing asked or spent");
    }

    /// A provider that serves nothing in the catalogue.
    struct Named(&'static str);

    impl craft_anything_engines::assets::AssetProvider for Named {
        fn id(&self) -> &'static str {
            self.0
        }
        fn name(&self) -> &'static str {
            self.0
        }
        fn generate(&self, _key: &str, _req: &AssetRequest) -> Result<Generated, ToolError> {
            Err(ToolError::invalid("not here"))
        }
    }

    #[test]
    fn without_a_key_it_says_to_use_simple_shapes() {
        let (_d, w) = ws();
        let mut session = crate::testing::session();
        session.settings.assets.base_url = Some("https://api.openai.com/v1".into());
        let e = crate::Engine::new(session);
        e.session().ext.insert(std::sync::Arc::new(InboxDir(w.inbox_dir())));
        let err =
            e.call("assets_image_generate", json!({"prompt": "a crate", "save_as": "crate.png"}), crate::CallOptions::new(crate::Caller::Mcp)).err().unwrap();
        assert_eq!(err.code, ErrorCode::BadSettings);
        assert!(err.remediation.unwrap_or_default().contains("simple shapes"));
        let err =
            e.call("assets_model_generate", json!({"prompt": "a crate", "save_as": "crate.glb", "faces": 40_000}), crate::CallOptions::new(crate::Caller::Mcp));
        assert!(err.err().unwrap().message.contains("no 3D endpoint"), "3D is not an OpenAI-compatible API");
    }

    #[test]
    fn nothing_is_generated_unless_the_project_allows_it() {
        let (_d, mut w) = ws();
        w.set_person(|m| m.generate_assets = false).unwrap();
        let e = crate::Engine::new(crate::testing::session());
        e.session().ext.insert(std::sync::Arc::new(InboxDir(w.inbox_dir())));
        e.session().ext.insert(std::sync::Arc::new(AssetKeys([("openai".to_string(), "k-1".to_string())].into())));
        let err =
            e.call("assets_image_generate", json!({"prompt": "a crate", "save_as": "crate.png"}), crate::CallOptions::new(crate::Caller::Mcp)).err().unwrap();
        assert_eq!(err.code, ErrorCode::NoPermission, "a key alone isn't the person's choice");
        assert!(err.remediation.unwrap_or_default().contains("simple shapes"));
        assert!(w.assets().is_empty() && Inbox::new(w.inbox_dir()).pending().is_empty(), "nothing asked or spent");
    }

    #[test]
    fn the_budget_asks_before_going_over_and_remembers_always() {
        let (_d, mut w) = ws();
        w.record_asset(AssetRecord { file: "a.glb".into(), kind: "model".into(), prompt: "x".into(), cost_usd: 4.80, at_ms: 0 }).unwrap();
        let inbox = Inbox::new(w.inbox_dir());
        let mut session = crate::testing::session();
        // Auto mode stops at the budget without asking.
        session.settings.auto_mode = true;
        {
            let mut ctx = Ctx { tool: "test", session: &mut session, cancel: crate::CancelToken::default(), progress: &|_| {}, caller: crate::Caller::Mcp };
            assert_eq!(within_budget(&mut ctx, &inbox, &mut w, 0.48).unwrap_err().code, ErrorCode::OverLimit);
            assert!(inbox.pending().is_empty(), "no question in Auto mode");
        }
        // Asking is what Auto mode switches off.
        session.settings.auto_mode = false;
        let mut ctx = Ctx { tool: "test", session: &mut session, cancel: crate::CancelToken::default(), progress: &|_| {}, caller: crate::Caller::Mcp };
        // Under the budget: no question.
        within_budget(&mut ctx, &inbox, &mut w, 0.03).unwrap();
        assert!(inbox.pending().is_empty());
        // Over it: asked; "no" stops, "always" lifts the cap for the project.
        let dir = w.inbox_dir();
        let answer = |allow: bool, always: bool| {
            let inbox = Inbox::new(dir.clone());
            std::thread::spawn(move || {
                for _ in 0..200 {
                    if let Some(r) = inbox.pending().first() {
                        assert!(r.title.contains("$0.48"), "{}", r.title);
                        inbox.answer(&r.id, &Answer { allow: Some(allow), always, ..Default::default() }).unwrap();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                panic!("no question came");
            })
        };
        let t = answer(false, false);
        assert_eq!(within_budget(&mut ctx, &inbox, &mut w, 0.48).unwrap_err().code, ErrorCode::OverLimit);
        t.join().unwrap();
        let t = answer(true, true);
        within_budget(&mut ctx, &inbox, &mut w, 0.48).unwrap();
        t.join().unwrap();
        assert!(Workspace::open(&w.root).unwrap().manifest.allowed_actions.iter().any(|k| k == OVER_BUDGET));
        within_budget(&mut ctx, &inbox, &mut w, 10.0).unwrap();
        assert!(inbox.pending().is_empty(), "after \"always\" it doesn't ask again");
        assert!((w.assets_spent() - 4.80).abs() < 1e-9);
    }
}
