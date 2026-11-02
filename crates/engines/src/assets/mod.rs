//! Generated game assets: pictures (textures, skies, concept art) made by an OpenAI-compatible
//! image API. Each provider is an [`AssetProvider`]; an [`AssetProviders`] registry holds them
//! (built in: [`openai::OpenAiCompat`]). The models and their prices come from the `assets.toml`
//! table ([`Catalogue`]), so a cost is known before anything is spent, and every generation
//! reports what it cost. There is no standard 3D endpoint, so the built-in provider refuses
//! [`AssetRequest::Model3d`]. Audio would be one more [`AssetRequest`] variant and table.

pub mod openai;

use craft_anything_evidence::{ErrorCode, ToolError};
use serde::Deserialize;
use std::ops::RangeInclusive;
use std::sync::{Arc, OnceLock};

/// What kind of asset a request makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AssetKind {
    Image,
    Model3d,
}

/// Picture shapes. The images API is asked for 1024×1024, or a wider or taller size
/// (1792 on the long side, 1536 for a gpt-image model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// 1024×1024: objects, textures, icons.
    Square,
    /// Landscape: skies, backgrounds, title screens.
    Wide,
    /// Portrait: posters, tall props.
    Tall,
}

impl Shape {
    pub fn parse(s: &str) -> Option<Shape> {
        match s {
            "square" => Some(Shape::Square),
            "wide" => Some(Shape::Wide),
            "tall" => Some(Shape::Tall),
            _ => None,
        }
    }

    /// DALL-E 3's pixel count for this shape, for a per-megapixel price. gpt-image's wide and
    /// tall sizes are a little smaller; the estimate uses this.
    pub fn megapixels(self) -> f64 {
        let (w, h) = match self {
            Shape::Square => (1024.0, 1024.0),
            Shape::Wide => (1792.0, 1024.0),
            Shape::Tall => (1024.0, 1792.0),
        };
        w * h / 1e6
    }
}

/// A picture model from the table.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ImageModel {
    /// The table id (`flux-dev`), used in Settings.
    pub id: String,
    /// The provider that runs it (`fal`).
    pub provider: String,
    /// Shown to the person.
    pub name: String,
    /// The model's address at the provider.
    pub endpoint: String,
    /// The price by size; a model billed per picture leaves it at 0.
    #[serde(default)]
    pub usd_per_megapixel: f64,
    /// The price of a square picture.
    #[serde(default)]
    pub usd_per_image: f64,
    /// The price of a wide or tall picture. 0 means the same as [`Self::usd_per_image`].
    #[serde(default)]
    pub usd_large: f64,
    /// The model weights' licence, for the person choosing one.
    #[serde(default)]
    pub licence: String,
}

impl ImageModel {
    /// What a picture of this shape costs.
    pub fn cost(&self, shape: Shape) -> f64 {
        let flat = match shape {
            Shape::Square => self.usd_per_image,
            Shape::Wide | Shape::Tall => {
                if self.usd_large > 0.0 {
                    self.usd_large
                } else {
                    self.usd_per_image
                }
            }
        };
        flat + self.usd_per_megapixel * shape.megapixels()
    }
}

/// A 3D model generator from the table (a picture in, a GLB out).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Model3d {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub endpoint: String,
    /// Per model, untextured and textured.
    pub usd: f64,
    pub textured_usd: f64,
    /// The face counts it accepts, `[fewest, most]`.
    pub faces: [u32; 2],
    #[serde(default)]
    pub licence: String,
}

impl Model3d {
    pub fn cost(&self, textured: bool) -> f64 {
        if textured { self.textured_usd } else { self.usd }
    }

    pub fn faces(&self) -> RangeInclusive<u32> {
        let [low, high] = self.faces;
        low..=high
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Defaults {
    image: String,
    /// Empty when the table offers no 3D generator.
    #[serde(default)]
    model_3d: String,
}

/// The models assets are made with and their prices (`assets.toml`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Catalogue {
    default: Defaults,
    #[serde(default)]
    image: Vec<ImageModel>,
    #[serde(default)]
    model_3d: Vec<Model3d>,
}

fn price_ok(usd: f64) -> bool {
    usd.is_finite() && usd >= 0.0
}

impl Catalogue {
    /// The built-in table. One that doesn't parse (a broken edit) gives [`Catalogue::fallback`],
    /// so assets keep working at the known prices; a test keeps the table parsing.
    pub fn built_in() -> &'static Catalogue {
        static TABLE: OnceLock<Catalogue> = OnceLock::new();
        TABLE.get_or_init(|| {
            Catalogue::parse(include_str!("assets.toml")).unwrap_or_else(|e| {
                log::warn!("assets.toml is unreadable ({e}); using the built-in asset models");
                Catalogue::fallback()
            })
        })
    }

    /// A table from TOML text. Entries with a price that isn't a positive number, an empty
    /// endpoint or an empty face range are left out (asking for them gives a clear error).
    pub fn parse(text: &str) -> Result<Catalogue, toml::de::Error> {
        let mut c: Catalogue = toml::from_str(text)?;
        c.image.retain(|m| price_ok(m.usd_per_megapixel) && price_ok(m.usd_per_image) && price_ok(m.usd_large) && !m.endpoint.trim().is_empty());
        c.model_3d.retain(|m| {
            price_ok(m.usd) && price_ok(m.textured_usd) && !m.endpoint.trim().is_empty() && {
                let [low, high] = m.faces;
                low <= high
            }
        });
        Ok(c)
    }

    /// The defaults the table ships with, in code: DALL-E 3 on an OpenAI-compatible server.
    /// 3D generation has no standard endpoint, so none is listed.
    pub fn fallback() -> Catalogue {
        Catalogue {
            default: Defaults { image: "dall-e-3".into(), model_3d: String::new() },
            image: vec![ImageModel {
                id: "dall-e-3".into(),
                provider: openai::ID.into(),
                name: "DALL-E 3".into(),
                endpoint: "dall-e-3".into(),
                usd_per_megapixel: 0.0,
                usd_per_image: 0.04,
                usd_large: 0.08,
                licence: "Generated through the image API; the provider's terms apply".into(),
            }],
            model_3d: Vec::new(),
        }
    }

    pub fn images(&self) -> &[ImageModel] {
        &self.image
    }

    pub fn models_3d(&self) -> &[Model3d] {
        &self.model_3d
    }

    /// The picture model to send. A table id uses that row. Any other non-empty name is sent
    /// as the API `model`, priced like the default (the server's own price may differ).
    pub fn resolve_image(&self, id: Option<&str>) -> Result<ImageModel, ToolError> {
        if let Ok(model) = self.image_model(id) {
            return Ok(model.clone());
        }
        let raw = id.map(str::trim).filter(|s| !s.is_empty());
        let Some(raw) = raw else {
            return self.image_model(None).cloned();
        };
        let mut model = self.image_model(None)?.clone();
        model.id = raw.to_string();
        model.name = raw.to_string();
        model.endpoint = raw.to_string();
        Ok(model)
    }

    /// The picture model `id` names, or the default one when `id` is `None` or empty.
    pub fn image_model(&self, id: Option<&str>) -> Result<&ImageModel, ToolError> {
        let id = id.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&self.default.image);
        self.image.iter().find(|m| m.id == id).ok_or_else(|| {
            let known: Vec<&str> = self.image.iter().map(|m| m.id.as_str()).collect();
            missing("picture model", id, &known)
        })
    }

    /// The 3D model generator `id` names, or the default one.
    pub fn model_3d(&self, id: Option<&str>) -> Result<&Model3d, ToolError> {
        let id = id.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&self.default.model_3d);
        self.model_3d.iter().find(|m| m.id == id).ok_or_else(|| {
            let known: Vec<&str> = self.model_3d.iter().map(|m| m.id.as_str()).collect();
            missing("3D model generator", id, &known)
        })
    }
}

fn missing(what: &str, id: &str, known: &[&str]) -> ToolError {
    let remediation = if known.is_empty() {
        "The asset table lists none; generate assets with simple shapes and colours instead.".to_string()
    } else {
        format!("Choose one of: {} (Settings → AI → Game assets, or [assets] in config.toml).", known.join(", "))
    };
    ToolError::new(ErrorCode::BadSettings, format!("There is no {what} called \"{id}\" in the asset table.")).with_remediation(remediation)
}

/// One thing to generate, with the table entry that says where and at what price.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum AssetRequest<'a> {
    /// A picture (PNG) from a description.
    Image { model: &'a ImageModel, prompt: &'a str, shape: Shape },
    /// A 3D model (GLB) of at most `faces` faces from a picture: a web address, or a `data:` URI.
    Model3d { model: &'a Model3d, image_url: &'a str, textured: bool, faces: u32 },
}

impl AssetRequest<'_> {
    pub fn kind(&self) -> AssetKind {
        match self {
            AssetRequest::Image { .. } => AssetKind::Image,
            AssetRequest::Model3d { .. } => AssetKind::Model3d,
        }
    }

    /// The provider the table entry names.
    pub fn provider(&self) -> &str {
        match self {
            AssetRequest::Image { model, .. } => &model.provider,
            AssetRequest::Model3d { model, .. } => &model.provider,
        }
    }

    /// The table's price for it.
    pub fn listed_cost(&self) -> f64 {
        match self {
            AssetRequest::Image { model, shape, .. } => model.cost(*shape),
            AssetRequest::Model3d { model, textured, .. } => model.cost(*textured),
        }
    }
}

/// A generated file: where the provider keeps it, its bytes, and what it cost.
#[derive(Debug, Clone)]
pub struct Generated {
    pub url: String,
    pub bytes: Vec<u8>,
    pub cost_usd: f64,
}

/// A paid service that generates assets.
pub trait AssetProvider: Send + Sync {
    /// Stable id, named by the table's `provider` (`"openai"`).
    fn id(&self) -> &'static str;
    /// Shown to the person ("OpenAI-compatible").
    fn name(&self) -> &'static str;
    /// The environment variable the provider's own tools read its key from (`OPENAI_API_KEY`),
    /// used when no key is stored.
    fn key_env(&self) -> Option<&'static str> {
        None
    }
    /// A loopback server can be called with no key.
    fn allows_empty_key(&self) -> bool {
        false
    }
    /// Where the person gets a key (shown as a link in Settings → AI).
    fn keys_page(&self) -> Option<&'static str> {
        None
    }
    /// What `req` will cost, before anything is spent (the budget is checked against it).
    fn estimate(&self, req: &AssetRequest) -> Result<f64, ToolError> {
        Ok(req.listed_cost())
    }
    /// Make it with the person's `key`; the result carries what it actually cost.
    fn generate(&self, key: &str, req: &AssetRequest) -> Result<Generated, ToolError>;
}

/// The asset providers a project can use.
#[derive(Clone)]
pub struct AssetProviders {
    providers: Vec<Arc<dyn AssetProvider>>,
}

impl std::fmt::Debug for AssetProviders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.providers.iter().map(|p| p.id())).finish()
    }
}

impl Default for AssetProviders {
    fn default() -> Self {
        AssetProviders::built_in()
    }
}

impl AssetProviders {
    /// The OpenAI-compatible image API.
    pub fn built_in() -> AssetProviders {
        AssetProviders { providers: vec![Arc::new(openai::OpenAiCompat::default())] }
    }

    /// Add a provider, or replace the one with the same id.
    pub fn register(&mut self, provider: Arc<dyn AssetProvider>) {
        self.providers.retain(|p| p.id() != provider.id());
        self.providers.push(provider);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn AssetProvider>> {
        self.providers.iter().find(|p| p.id() == id).cloned()
    }

    pub fn ids(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.id()).collect()
    }

    /// The provider that makes `req`.
    pub fn for_request(&self, req: &AssetRequest) -> Result<Arc<dyn AssetProvider>, ToolError> {
        let id = req.provider();
        self.get(id).ok_or_else(|| {
            ToolError::new(ErrorCode::FeatureMissing, format!("No asset provider called \"{id}\" is available."))
                .with_remediation("Check the asset table, or generate assets with simple shapes and colours instead.")
        })
    }
}

/// A price in prompt text: "$0.53", or "under $0.01" for a fraction of a cent.
pub fn usd(x: f64) -> String {
    if x > 0.0 && x < 0.005 { "under $0.01".into() } else { format!("${x:.2}") }
}

/// "about $0.53", or "under $0.01".
pub fn about_usd(x: f64) -> String {
    if x > 0.0 && x < 0.005 { usd(x) } else { format!("about {}", usd(x)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_parses_and_its_defaults_match_the_code() {
        let t = Catalogue::parse(include_str!("assets.toml")).unwrap();
        let f = Catalogue::fallback();
        assert_eq!(t.image_model(None).unwrap(), f.image_model(None).unwrap());
        assert!(t.model_3d(None).is_err() && f.model_3d(None).is_err(), "no 3D model on an OpenAI-compatible API");
        assert_eq!(Catalogue::built_in(), &t);
        let gpt = t.image_model(Some("gpt-image-1")).unwrap();
        assert_eq!(gpt.endpoint, "gpt-image-1");
        assert!(gpt.cost(Shape::Wide) < t.image_model(None).unwrap().cost(Shape::Wide));
        let dalle = t.image_model(None).unwrap();
        assert_eq!((dalle.id.as_str(), dalle.endpoint.as_str(), dalle.usd_per_image, dalle.usd_large), ("dall-e-3", "dall-e-3", 0.04, 0.08));
        let custom = t.resolve_image(Some("local-flux")).unwrap();
        assert_eq!(custom.endpoint, "local-flux");
        assert_eq!(custom.cost(Shape::Square), dalle.cost(Shape::Square));
        for m in t.images() {
            assert!(AssetProviders::built_in().get(&m.provider).is_some(), "{} names an unknown provider", m.id);
        }
    }

    #[test]
    fn prices_follow_the_shape() {
        let c = Catalogue::built_in();
        let dalle = c.image_model(None).unwrap();
        assert_eq!((dalle.cost(Shape::Square), dalle.cost(Shape::Wide), dalle.cost(Shape::Tall)), (0.04, 0.08, 0.08));
        assert!(c.model_3d(Some(" ")).is_err());
        assert_eq!(about_usd(dalle.cost(Shape::Square)), "about $0.04");
        assert_eq!(about_usd(0.0031), "under $0.01");
        assert_eq!(usd(0.0262144), "$0.03");
    }

    #[test]
    fn a_broken_or_odd_table_never_panics() {
        assert!(Catalogue::parse("not = [toml").is_err());
        assert!(Catalogue::parse("").is_err(), "the defaults are required");
        let odd = Catalogue::parse(
            r#"
            [default]
            image = "free"
            model_3d = "upside-down"
            [[image]]
            id = "free"
            provider = "openai"
            name = "x"
            endpoint = "x"
            usd_per_image = -1.0
            [[model_3d]]
            id = "upside-down"
            provider = "openai"
            name = "x"
            endpoint = "x"
            usd = 0.1
            textured_usd = 0.2
            faces = [500, 10]
            "#,
        )
        .unwrap();
        let e = odd.image_model(None).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadSettings);
        assert!(e.message.contains("\"free\""), "{}", e.message);
        assert!(odd.model_3d(None).is_err());
        let e = Catalogue::fallback().image_model(Some("no-such-model")).unwrap_err();
        assert!(e.remediation.unwrap_or_default().contains("dall-e-3"));
    }

    #[test]
    fn the_registry_finds_the_provider_a_model_names() {
        struct Stub;
        impl AssetProvider for Stub {
            fn id(&self) -> &'static str {
                "stub"
            }
            fn name(&self) -> &'static str {
                "Stub"
            }
            fn generate(&self, _key: &str, req: &AssetRequest) -> Result<Generated, ToolError> {
                Ok(Generated { url: String::new(), bytes: Vec::new(), cost_usd: req.listed_cost() })
            }
        }
        let mut r = AssetProviders::built_in();
        assert_eq!(r.ids(), vec!["openai"]);
        let mut m = Catalogue::fallback().image_model(None).unwrap().clone();
        m.provider = "stub".into();
        let req = AssetRequest::Image { model: &m, prompt: "a crate", shape: Shape::Square };
        assert_eq!(r.for_request(&req).err().map(|e| e.code), Some(ErrorCode::FeatureMissing));
        r.register(Arc::new(Stub));
        let p = r.for_request(&req).unwrap();
        assert_eq!(p.estimate(&req).unwrap(), m.cost(Shape::Square));
        assert_eq!(p.generate("k", &req).unwrap().cost_usd, m.cost(Shape::Square));
        assert_eq!(req.kind(), AssetKind::Image);
    }
}
