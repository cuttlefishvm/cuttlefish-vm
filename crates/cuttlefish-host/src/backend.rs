//! Resolving a spec's model reference into something that can generate.
//!
//! Inference will come from several places over this project's life: a local
//! Ollama, an OpenAI-compatible HTTP endpoint (which covers llama.cpp's own
//! server, vLLM, LM Studio, and most hosted providers), an embedded llama.cpp,
//! and others. [`InferBackend`] is the interface they share; this module is how
//! a spec picks one *without* anything in the call chain knowing the list.
//!
//! # Why a registry rather than a match
//!
//! The obvious implementation is a `match` on the provider name in the daemon.
//! It works, and it means every new backend edits the daemon, the parser, and an
//! enum — three places that have nothing to do with the new backend, touched
//! only because they enumerate. That is the shape that makes a fourth or fifth
//! provider progressively less attractive to add.
//!
//! Instead a backend supplies a [`BackendFactory`], registers it under a
//! provider name, and nothing else changes. The spec parser already accepts any
//! `Provider "target"`; the runner only ever sees [`InferBackend`]. Adding one
//! is genuinely additive.
//!
//! A backend that needs heavy or platform-specific dependencies — embedded
//! llama.cpp being the obvious case — can live behind a cargo feature and
//! register itself only when enabled. Nothing here has to change to allow that.
//!
//! ```
//! use cuttlefish_core::spec::ModelRef;
//! use cuttlefish_host::backend::Registry;
//!
//! let registry = Registry::with_builtins();
//!
//! let backend = registry.resolve(&ModelRef::new("stub", "anything")).unwrap_or_else(|e| {
//!     panic!("stub is always registered: {e}")
//! });
//! assert_eq!(backend.model_name(), "stub");
//!
//! // An unknown provider explains what is available rather than panicking.
//! let err = registry.resolve(&ModelRef::new("nope", "x")).err().unwrap();
//! assert!(err.to_string().contains("stub"));
//! ```

use crate::infer::InferBackend;
use cuttlefish_core::spec::ModelRef;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Builds one kind of [`InferBackend`] from a spec's model target.
pub trait BackendFactory: Send + Sync {
    /// The provider name this handles, lowercase — `ollama`, `stub`.
    fn provider(&self) -> &'static str;

    /// One line describing what this serves, shown when resolution fails.
    fn describe(&self) -> &'static str;

    /// Build a backend for `target`, whose meaning is this provider's own: a
    /// model tag, a filesystem path, a URL.
    ///
    /// Returning an error here should mean the target is unusable — malformed,
    /// or naming something that cannot exist. Whether the *service* is reachable
    /// is deliberately not checked: that would make constructing a backend
    /// fallible for reasons that change minute to minute, and the failure is
    /// better reported when a job actually runs, where it lands in that job's
    /// envelope instead of preventing the daemon from starting.
    fn build(&self, target: &str) -> anyhow::Result<Arc<dyn InferBackend>>;
}

/// The providers this host knows how to serve.
#[derive(Default)]
pub struct Registry {
    // BTreeMap rather than HashMap so that the "available providers" list in an
    // error message comes out in a stable order — an error that reorders itself
    // between runs is harder to recognise as the same error.
    factories: BTreeMap<&'static str, Box<dyn BackendFactory>>,
}

impl Registry {
    /// An empty registry, which resolves nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with every backend compiled into this build.
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(crate::infer::StubFactory));
        registry.register(Box::new(crate::ollama::OllamaFactory));
        // Present only when the `llamacpp` feature is on. A spec naming
        // `llamacpp` in a build without it gets the normal unknown-provider
        // error listing what *is* available, which is a far better outcome than
        // a link failure or a silent fallback to something else.
        #[cfg(feature = "llamacpp")]
        registry.register(Box::new(crate::llamacpp::LlamaCppFactory));
        registry
    }

    /// Add a factory, replacing any previous one for the same provider.
    ///
    /// Replacing rather than refusing is deliberate: it lets a test or an
    /// embedder substitute a provider — pointing `ollama` at a mock, say —
    /// without needing a separate injection path.
    pub fn register(&mut self, factory: Box<dyn BackendFactory>) {
        self.factories.insert(factory.provider(), factory);
    }

    /// Provider names this registry can resolve, in stable order.
    pub fn providers(&self) -> Vec<&'static str> {
        self.factories.keys().copied().collect()
    }

    /// Resolve a spec's model reference into a backend.
    pub fn resolve(&self, model: &ModelRef) -> anyhow::Result<Arc<dyn InferBackend>> {
        let factory = self.factories.get(model.provider.as_str()).ok_or_else(|| {
            // Listing what *is* available turns "unknown provider" from a dead
            // end into a correctable mistake — usually a typo or a feature that
            // was not enabled at build time.
            let available = self
                .factories
                .values()
                .map(|f| format!("  {} — {}", f.provider(), f.describe()))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::anyhow!(
                "unknown model provider `{}`. Available providers:\n{available}",
                model.provider
            )
        })?;

        factory.build(&model.target).map_err(|e| {
            anyhow::anyhow!(
                "provider `{}` could not serve `{}`: {e}",
                model.provider,
                model.target
            )
        })
    }
}
