//! vue-native: an app's own `--shading` effects, as every renderer reads them. The CSS registry
//! (`sk-effects`) reads the parameter schemas from the same [`AppEffects::manifest`] and numbers the
//! effects the same way: `effect_id::COUNT + index`, in manifest order.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::bail;
use parking_lot::{Condvar, Mutex};

/// Names a directory of compiled app effects (`effects.json` and the shaders beside it).
/// A developer override: it wins over what the host installs with [`set_app_effects`].
pub const APP_EFFECTS_DIR_ENV: &str = "SK_EFFECTS_DIR";

/// The `effects.json` layout this build understands.
pub const APP_EFFECTS_SCHEMA: u32 = 14;

/// One generated shader of an [`AppEffect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEffectSource {
    /// `effects.json` names no file for this target.
    Unlisted,
    /// `effects.json` names a file the app's effects do not carry.
    Missing(String),
    /// The file and its text.
    Present {
        /// As `effects.json` names it.
        file: String,
        /// The generated source.
        text: String,
    },
}

impl AppEffectSource {
    /// The file name and its text; `language` names the target in the error.
    pub fn get(&self, language: &str) -> anyhow::Result<(&str, &str)> {
        match self {
            AppEffectSource::Unlisted => {
                bail!("effects.json lists no {language} for it; rebuild the app with sk build")
            }
            AppEffectSource::Missing(file) => bail!("the app's effects have no {file}"),
            AppEffectSource::Present { file, text } => Ok((file, text)),
        }
    }
}

/// One app effect as far as a renderer is concerned; parameter names, defaults and ranges belong
/// to the CSS layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEffect {
    /// The CSS function name.
    pub name: String,
    /// Decides the blend state and whether the renderer breaks the pass for a snapshot.
    pub needs_backdrop: bool,
    /// Whether it draws its element's painting (`[SkContent]`), and nothing without it.
    pub content: bool,
    /// Whether it keeps a history (`[SkHistory]`), which a second pipeline
    /// steps: blending off, a 16-bit float target.
    pub history: bool,
    /// For DirectX.
    pub hlsl: AppEffectSource,
    /// For wgpu.
    pub wgsl: AppEffectSource,
    /// For Metal.
    pub metal: AppEffectSource,
}

/// An app's effects: its `effects.json` and the generated shaders it names.
#[derive(Debug)]
pub struct AppEffects {
    /// Where they came from, for messages.
    pub origin: String,
    /// The `effects.json` text.
    pub manifest: String,
    effects: Vec<AppEffect>,
}

#[derive(serde::Deserialize)]
struct RawManifest {
    schema: u32,
    effects: Vec<RawEffect>,
}

#[derive(serde::Deserialize)]
struct RawEffect {
    name: String,
    backdrop: bool,
    #[serde(default)]
    content: bool,
    #[serde(default)]
    history: bool,
    #[serde(default)]
    hlsl: Option<String>,
    #[serde(default)]
    wgsl: Option<String>,
    #[serde(default)]
    metal: Option<String>,
}

impl AppEffects {
    /// `files` maps the names `manifest` gives the generated shaders to their source.
    pub fn new(
        origin: impl Into<String>,
        manifest: impl Into<String>,
        files: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, String> {
        let origin = origin.into();
        let manifest = manifest.into();
        let raw: RawManifest = serde_json::from_str(&manifest)
            .map_err(|e| format!("{origin}: effects.json is not readable ({e})"))?;
        if raw.schema != APP_EFFECTS_SCHEMA {
            return Err(format!(
                "{origin}: effects.json is schema {} but this build understands \
                 {APP_EFFECTS_SCHEMA} — rebuild the app with the sk that matches this host",
                raw.schema
            ));
        }
        let files: std::collections::HashMap<String, String> = files.into_iter().collect();
        let source = |file: Option<String>| match file {
            None => AppEffectSource::Unlisted,
            Some(file) => match files.get(&file).cloned() {
                Some(text) => AppEffectSource::Present { file, text },
                None => AppEffectSource::Missing(file),
            },
        };
        let effects = raw
            .effects
            .into_iter()
            .map(|effect| AppEffect {
                hlsl: source(effect.hlsl),
                wgsl: source(effect.wgsl),
                metal: source(effect.metal),
                needs_backdrop: effect.backdrop,
                content: effect.content,
                history: effect.history,
                name: effect.name,
            })
            .collect();
        Ok(Self {
            origin,
            manifest,
            effects,
        })
    }

    /// `<dir>/effects.json` and the `.hlsl`, `.wgsl` and `.metal` files beside it.
    pub fn from_dir(dir: &std::path::Path) -> Result<Self, String> {
        let manifest_path = dir.join("effects.json");
        let manifest = std::fs::read_to_string(&manifest_path)
            .map_err(|e| format!("{} could not be read ({e})", manifest_path.display()))?;
        let mut files = Vec::new();
        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("{} could not be listed ({e})", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let shader = path
                .extension()
                .is_some_and(|ext| ext == "hlsl" || ext == "wgsl" || ext == "metal");
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if shader {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("{} could not be read ({e})", path.display()))?;
                files.push((name.to_string(), text));
            }
        }
        Self::new(dir.display().to_string(), manifest, files)
    }

    /// In manifest order: effect id `effect_id::COUNT + index`.
    pub fn effects(&self) -> &[AppEffect] {
        &self.effects
    }
}

struct Installed {
    read: bool,
    effects: Option<Arc<AppEffects>>,
}

static INSTALLED: Mutex<Installed> = Mutex::new(Installed {
    read: false,
    effects: None,
});
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn app_effects_dir() -> Option<std::path::PathBuf> {
    std::env::var_os(APP_EFFECTS_DIR_ENV)
        .filter(|dir| !dir.is_empty())
        .map(std::path::PathBuf::from)
}

/// Hands the renderers and the CSS registry the app's effects (from its payload). Call it before
/// the first window; false when [`APP_EFFECTS_DIR_ENV`] is set, which wins, or when the effects were
/// already read.
pub fn set_app_effects(effects: AppEffects) -> bool {
    if app_effects_dir().is_some() {
        return false;
    }
    let mut installed = INSTALLED.lock();
    if installed.read {
        return false;
    }
    installed.read = true;
    installed.effects = Some(Arc::new(effects));
    true
}

/// Replaces the app's effects in a running app (a dev rebuild). Every renderer rebuilds the changed
/// pipelines from its next frame on, on a background thread, and the CSS registry renumbers on its
/// next read.
pub fn reload_app_effects(effects: AppEffects) {
    let mut installed = INSTALLED.lock();
    installed.read = true;
    installed.effects = Some(Arc::new(effects));
    GENERATION.fetch_add(1, Ordering::Release);
}

/// Moves on every [`reload_app_effects`]; a renderer compares it once per frame.
pub fn app_effects_generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// The app's effects: the last reload, else `$SK_EFFECTS_DIR` when set, else what the host
/// installed, else none.
pub fn app_effects() -> Option<Arc<AppEffects>> {
    let mut installed = INSTALLED.lock();
    if !installed.read {
        installed.read = true;
        installed.effects = app_effects_dir().and_then(|dir| {
            AppEffects::from_dir(&dir)
                .map_err(|error| {
                    let message = format!(
                        "{APP_EFFECTS_DIR_ENV} is set to `{}` but {error} — the app's own \
                         `--shading` effects are unavailable",
                        dir.display()
                    );
                    log::warn!("{message}");
                    eprintln!("skathi: {message}");
                })
                .ok()
                .map(Arc::new)
        });
    }
    installed.effects.clone()
}

/// Which app effects the renderers build ahead of the frame that first draws them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AppEffectsPrewarm {
    /// Each is built in the first frame that draws it.
    #[default]
    Off,
    All,
    /// These, by name; the rest as with [`Self::Off`].
    Only(Vec<String>),
}

impl AppEffectsPrewarm {
    fn includes(&self, name: &str) -> bool {
        match self {
            AppEffectsPrewarm::Off => false,
            AppEffectsPrewarm::All => true,
            AppEffectsPrewarm::Only(names) => names.iter().any(|n| n == name),
        }
    }
}

static PREWARM: Mutex<AppEffectsPrewarm> = Mutex::new(AppEffectsPrewarm::Off);

/// Sets which app effects are built off the frame path; call it before the first window.
pub fn set_app_effects_prewarm(prewarm: AppEffectsPrewarm) {
    *PREWARM.lock() = match prewarm {
        AppEffectsPrewarm::Only(names) if names.is_empty() => AppEffectsPrewarm::Off,
        prewarm => prewarm,
    };
}

/// Whether any effect is built off the frame path. A renderer whose built-in effects are compiled
/// on first use builds them ahead too when this is set.
pub fn app_effects_prewarm_enabled() -> bool {
    *PREWARM.lock() != AppEffectsPrewarm::Off
}

/// Whether app effect `name` is built off the frame path.
pub fn app_effect_prewarmed(name: &str) -> bool {
    PREWARM.lock().includes(name)
}

/// The built-ins: ready without a warmup, or warmed with the renderer's backdrop.
const BUILTIN_EFFECT_NAMES: [&str; 3] = ["frost", "noise", "glow"];

/// [`app_effects`], without settling them before the host had its chance to [`set_app_effects`].
fn app_effects_if_settled() -> Option<Arc<AppEffects>> {
    let settled = INSTALLED.lock().read || app_effects_dir().is_some();
    settled.then(app_effects).flatten()
}

/// A renderer's pipelines for the app's effects, `effect_id::COUNT + index`. [`Self::sync`]
/// rebuilds only the effects that changed since the last call; one whose new shader is unusable
/// keeps its previous pipeline, and only an effect that never built is a hole.
pub struct AppEffectPipelines<P> {
    generation: Option<u64>,
    slots: Vec<AppEffectSlot<P>>,
}

struct AppEffectSlot<P> {
    /// What the slot was last asked to build; `pipeline` may come from an earlier version.
    effect: AppEffect,
    pipeline: Option<P>,
}

impl<P> Default for AppEffectPipelines<P> {
    fn default() -> Self {
        Self {
            generation: None,
            slots: Vec::new(),
        }
    }
}

impl<P> AppEffectPipelines<P> {
    /// Catches up with [`reload_app_effects`]; false when nothing changed.
    pub fn sync(&mut self, mut build: impl FnMut(&AppEffect) -> anyhow::Result<P>) -> bool {
        let generation = app_effects_generation();
        if self.generation == Some(generation) {
            return false;
        }
        self.generation = Some(generation);
        let app = app_effects();
        let effects = app.as_deref().map(AppEffects::effects).unwrap_or_default();
        let mut previous = std::mem::take(&mut self.slots);
        for effect in effects {
            let old = previous
                .iter()
                .position(|slot| slot.effect.name == effect.name)
                .map(|index| previous.swap_remove(index));
            let pipeline = match old {
                Some(old) if old.effect == *effect => old.pipeline,
                old => match (build(effect), old.and_then(|old| old.pipeline)) {
                    (Ok(pipeline), _) => Some(pipeline),
                    (Err(error), Some(previous)) => {
                        advisory(
                            generation,
                            format!(
                                "`--shading: {}(…)` keeps its previous shader — the new one is \
                                 unusable: {error:#}",
                                effect.name
                            ),
                        );
                        Some(previous)
                    }
                    (Err(error), None) => {
                        advisory(
                            generation,
                            format!("`--shading: {}(…)` is unavailable — {error:#}", effect.name),
                        );
                        None
                    }
                },
            };
            self.slots.push(AppEffectSlot {
                effect: effect.clone(),
                pipeline,
            });
        }
        true
    }

    /// [`Self::sync`], taking what `warmup` built and building the rest here.
    pub fn sync_warmed(
        &mut self,
        warmup: &AppEffectWarmup<P>,
        mut build: impl FnMut(&AppEffect) -> anyhow::Result<P>,
    ) -> bool {
        self.sync(|effect| warmup.take(effect).unwrap_or_else(|| build(effect)))
    }

    /// The pipeline of app effect `index`; None for a hole or past the end.
    pub fn get(&self, index: usize) -> Option<&P> {
        self.slots.get(index)?.pipeline.as_ref()
    }

    /// [`Self::get`], mutably.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut P> {
        self.slots.get_mut(index)?.pipeline.as_mut()
    }

    /// Every pipeline built, holes skipped.
    pub fn pipelines(&self) -> impl Iterator<Item = &P> {
        self.slots.iter().filter_map(|slot| slot.pipeline.as_ref())
    }
}

/// A renderer's app effects built on a background thread ahead of the frame that first draws
/// them; [`AppEffectPipelines::sync_warmed`] takes them. That frame builds an effect the thread
/// has not started yet itself, and waits for one the thread is building.
pub struct AppEffectWarmup<P> {
    shared: Arc<WarmShared<P>>,
    started: Option<u64>,
}

struct WarmShared<P> {
    state: Mutex<WarmState<P>>,
    finished: Condvar,
}

struct WarmState<P> {
    /// Moves on every start; a thread of an earlier one stops.
    job: u64,
    entries: Vec<WarmEntry<P>>,
}

struct WarmEntry<P> {
    effect: AppEffect,
    slot: WarmSlot<P>,
}

enum WarmSlot<P> {
    Queued,
    Building,
    Built(anyhow::Result<P>),
    Taken,
}

impl<P> Default for AppEffectWarmup<P> {
    fn default() -> Self {
        Self {
            shared: Arc::new(WarmShared {
                state: Mutex::new(WarmState {
                    job: 0,
                    entries: Vec::new(),
                }),
                finished: Condvar::new(),
            }),
            started: None,
        }
    }
}

impl<P: Send + 'static> AppEffectWarmup<P> {
    /// Builds the app's effects that `pipelines` has not synced yet, with `build` on a thread
    /// named `thread`. Once per reload: a later call before the next one does nothing.
    pub fn start(
        &mut self,
        thread: &str,
        pipelines: &AppEffectPipelines<P>,
        mut build: impl FnMut(&AppEffect) -> anyhow::Result<P> + Send + 'static,
    ) {
        let generation = app_effects_generation();
        if pipelines.generation == Some(generation) || self.started == Some(generation) {
            return;
        }
        let prewarm = PREWARM.lock().clone();
        if prewarm == AppEffectsPrewarm::Off {
            return;
        }
        let Some(app) = app_effects_if_settled() else {
            return;
        };
        self.started = Some(generation);
        if let AppEffectsPrewarm::Only(names) = &prewarm {
            for name in names {
                let known = BUILTIN_EFFECT_NAMES.contains(&name.as_str())
                    || app.effects().iter().any(|effect| effect.name == *name);
                if !known {
                    advisory(
                        generation,
                        format!(
                            "sk.config.json \"effects.prewarm\" names `{name}`, which is not one of \
                             the app's `--shading` effects — it is ignored; use the name its \
                             [SkEffect(\"…\")] declares"
                        ),
                    );
                }
            }
        }
        let effects: Vec<WarmEntry<P>> = app
            .effects()
            .iter()
            .filter(|effect| prewarm.includes(&effect.name))
            .filter(|effect| !pipelines.slots.iter().any(|slot| slot.effect == **effect))
            .map(|effect| WarmEntry {
                effect: effect.clone(),
                slot: WarmSlot::Queued,
            })
            .collect();
        let job = {
            let mut state = self.shared.state.lock();
            state.job += 1;
            state.entries = effects;
            if state.entries.is_empty() {
                return;
            }
            state.job
        };
        self.shared.finished.notify_all();
        let shared = Arc::downgrade(&self.shared);
        let spawned = std::thread::Builder::new()
            .name(thread.into())
            .spawn(move || {
                // Ends with the renderer, or when a reload started another job.
                while let Some(shared) = shared.upgrade() {
                    let effect = {
                        let mut state = shared.state.lock();
                        if state.job != job {
                            return;
                        }
                        let Some(entry) = state
                            .entries
                            .iter_mut()
                            .find(|entry| matches!(entry.slot, WarmSlot::Queued))
                        else {
                            return;
                        };
                        entry.slot = WarmSlot::Building;
                        entry.effect.clone()
                    };
                    // A frame may be waiting for this one.
                    let built =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build(&effect)))
                            .unwrap_or_else(|_| Err(anyhow::anyhow!("building it panicked")));
                    let mut state = shared.state.lock();
                    if state.job == job
                        && let Some(entry) = state.entries.iter_mut().find(|entry| {
                            entry.effect == effect && matches!(entry.slot, WarmSlot::Building)
                        })
                    {
                        entry.slot = WarmSlot::Built(built);
                    }
                    drop(state);
                    shared.finished.notify_all();
                }
            });
        if let Err(error) = spawned {
            log::debug!("app effects are built on their first frame: {error}");
            self.shared.state.lock().entries.clear();
        }
    }
}

impl<P> AppEffectWarmup<P> {
    /// What the thread built for `effect`, waiting while it builds it; None when it has not
    /// started on it (and now will not) or has no such effect.
    fn take(&self, effect: &AppEffect) -> Option<anyhow::Result<P>> {
        let mut state = self.shared.state.lock();
        loop {
            let entry = state
                .entries
                .iter_mut()
                .find(|entry| entry.effect == *effect)?;
            match std::mem::replace(&mut entry.slot, WarmSlot::Taken) {
                WarmSlot::Building => {
                    entry.slot = WarmSlot::Building;
                    self.shared.finished.wait(&mut state);
                }
                WarmSlot::Built(built) => return Some(built),
                WarmSlot::Queued | WarmSlot::Taken => return None,
            }
        }
    }

    /// Whether the thread has nothing left to build.
    pub fn is_idle(&self) -> bool {
        self.shared
            .state
            .lock()
            .entries
            .iter()
            .all(|entry| !matches!(entry.slot, WarmSlot::Queued | WarmSlot::Building))
    }
}

/// Once per message per reload: every window's renderer meets the same failure.
fn advisory(generation: u64, message: String) {
    static SEEN: Mutex<(u64, Vec<String>)> = Mutex::new((0, Vec::new()));
    let mut seen = SEEN.lock();
    if seen.0 != generation {
        *seen = (generation, Vec::new());
    }
    if seen.1.contains(&message) {
        return;
    }
    log::warn!("{message}");
    eprintln!("skathi: {message}");
    seen.1.push(message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_resolves_each_effect_to_its_shader_per_target() -> Result<(), String> {
        let app = AppEffects::new(
            "test",
            r#"{"schema": 14, "kind": "user", "effects": [
                {"name": "ripple", "backdrop": true, "hlsl": "ripple.hlsl", "wgsl": "ripple.wgsl"},
                {"name": "plain", "backdrop": false, "hlsl": "plain.hlsl"}
            ]}"#,
            [("ripple.wgsl".to_string(), "// ripple".to_string())],
        )?;
        let [ripple, plain] = app.effects() else {
            return Err(format!("{:?}", app.effects()));
        };
        assert!(ripple.needs_backdrop && !plain.needs_backdrop);
        assert_eq!(
            ripple.wgsl,
            AppEffectSource::Present {
                file: "ripple.wgsl".into(),
                text: "// ripple".into()
            }
        );
        assert_eq!(ripple.hlsl, AppEffectSource::Missing("ripple.hlsl".into()));
        assert_eq!(plain.wgsl, AppEffectSource::Unlisted);
        Ok(())
    }

    #[test]
    fn prewarm_builds_off_the_frame_what_it_selects_and_a_reload_only_what_changed()
    -> Result<(), String> {
        let install = |ripple: &str, plain: &str| -> Result<(), String> {
            reload_app_effects(AppEffects::new(
                "test",
                r#"{"schema": 14, "effects": [
                    {"name": "ripple", "backdrop": true, "wgsl": "ripple.wgsl"},
                    {"name": "plain", "backdrop": false, "wgsl": "plain.wgsl"}
                ]}"#,
                [
                    ("ripple.wgsl".to_string(), ripple.to_string()),
                    ("plain.wgsl".to_string(), plain.to_string()),
                ],
            )?);
            Ok(())
        };
        let source = |effect: &AppEffect| Ok(effect.wgsl.get("WGSL")?.1.to_string());
        let mut pipelines = AppEffectPipelines::<String>::default();
        let mut warmup = AppEffectWarmup::default();
        let only_plain = AppEffectsPrewarm::Only(vec!["plain".into()]);
        for (prewarm, ripple, plain, warmed, on_frame) in [
            (
                AppEffectsPrewarm::Off,
                "// one",
                "// one",
                vec![],
                vec!["ripple", "plain"],
            ),
            (
                AppEffectsPrewarm::All,
                "// two",
                "// one",
                vec!["ripple"],
                vec![],
            ),
            (
                only_plain,
                "// three",
                "// three",
                vec!["plain"],
                vec!["ripple"],
            ),
        ] {
            set_app_effects_prewarm(prewarm);
            install(ripple, plain)?;
            let built = Arc::new(Mutex::new(Vec::new()));
            let off_frame = built.clone();
            warmup.start("test-warmup", &pipelines, move |effect| {
                off_frame.lock().push(effect.name.clone());
                source(effect)
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !warmup.is_idle() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the warmup did not finish"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let mut built_on_frame = Vec::new();
            assert!(pipelines.sync_warmed(&warmup, |effect| {
                built_on_frame.push(effect.name.clone());
                source(effect)
            }));
            assert_eq!(*built.lock(), warmed);
            assert_eq!(built_on_frame, on_frame);
            assert_eq!(pipelines.get(0).map(String::as_str), Some(ripple));
            assert_eq!(pipelines.get(1).map(String::as_str), Some(plain));
        }
        set_app_effects_prewarm(AppEffectsPrewarm::Off);
        Ok(())
    }
}
