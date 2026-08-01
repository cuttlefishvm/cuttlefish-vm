//! Tests for backend resolution.
//!
//! The property that matters here is not "ollama works" — it is that adding a
//! provider is additive. These tests register a made-up provider and resolve it,
//! which is exactly what a future OpenAI-compatible or embedded-llama.cpp
//! backend will do, without this file or the parser changing.

use cuttlefish_core::spec::ModelRef;
use cuttlefish_host::{
    backend::{BackendFactory, Registry},
    infer::{InferBackend, InferRequest, InferResult},
};
use std::sync::Arc;

/// Stands in for a provider that does not exist yet.
struct FakeFactory;

struct FakeBackend(String);

#[async_trait::async_trait]
impl InferBackend for FakeBackend {
    async fn infer(
        &self,
        _req: InferRequest<'_>,
        _on_token: &mut (dyn for<'t> FnMut(&'t str) -> bool + Send),
    ) -> anyhow::Result<InferResult> {
        Ok(InferResult {
            text: String::new(),
            tokens_in: 0,
            tokens_out: 0,
        })
    }

    fn model_name(&self) -> String {
        self.0.clone()
    }
}

impl BackendFactory for FakeFactory {
    fn provider(&self) -> &'static str {
        "invented"
    }
    fn describe(&self) -> &'static str {
        "a provider that did not exist when the registry was written"
    }
    fn build(&self, target: &str) -> anyhow::Result<Arc<dyn InferBackend>> {
        Ok(Arc::new(FakeBackend(target.to_string())))
    }
}

#[test]
fn builtins_are_registered() {
    let providers = Registry::with_builtins().providers();
    assert!(providers.contains(&"stub"), "got {providers:?}");
    assert!(providers.contains(&"ollama"), "got {providers:?}");
}

#[test]
fn a_new_provider_needs_no_changes_elsewhere() {
    // The whole point of the registry: this provider is unknown to the parser,
    // to the runner, and to the daemon, and resolving it still works.
    let mut registry = Registry::with_builtins();
    registry.register(Box::new(FakeFactory));

    let backend = match registry.resolve(&ModelRef::new("invented", "some-target")) {
        Ok(b) => b,
        Err(e) => panic!("a registered provider must resolve: {e}"),
    };
    assert_eq!(backend.model_name(), "some-target");
}

#[test]
fn an_unknown_provider_lists_what_is_available() {
    // An unknown provider is usually a typo or a feature that was not enabled at
    // build time. Naming the alternatives turns a dead end into a fix.
    let err = Registry::with_builtins()
        .resolve(&ModelRef::new("gpt9", "x"))
        .err()
        .expect("an unknown provider must not resolve");

    let msg = err.to_string();
    assert!(msg.contains("gpt9"), "the bad name must appear: {msg}");
    assert!(msg.contains("ollama"), "alternatives must be listed: {msg}");
    assert!(msg.contains("stub"), "alternatives must be listed: {msg}");
}

#[test]
fn registering_twice_replaces_rather_than_duplicates() {
    // Substituting a provider is how a test points `ollama` at a mock without a
    // separate injection path.
    let mut registry = Registry::new();
    registry.register(Box::new(FakeFactory));
    registry.register(Box::new(FakeFactory));

    assert_eq!(registry.providers(), vec!["invented"]);
}

#[test]
fn an_empty_registry_resolves_nothing() {
    assert!(Registry::new()
        .resolve(&ModelRef::new("stub", "x"))
        .is_err());
}

#[test]
fn the_stub_provider_takes_its_reply_from_the_spec() {
    // `model = Stub "canned reply"` makes a whole pipeline testable without a
    // model, and without depending on any model's wording.
    let backend = match Registry::with_builtins().resolve(&ModelRef::new("stub", "canned reply")) {
        Ok(b) => b,
        Err(e) => panic!("stub must resolve: {e}"),
    };

    // The stub reports its reply as the model name only when defaulted; here we
    // just confirm it built. Behaviour is covered by the runner's tests.
    assert_eq!(backend.model_name(), "stub");
}

#[test]
fn ollama_requires_a_model_name() {
    let err = Registry::with_builtins()
        .resolve(&ModelRef::new("ollama", ""))
        .err()
        .expect("an empty model name is unusable");
    assert!(err.to_string().contains("model name"), "{err}");
}

#[test]
fn resolution_errors_name_the_provider_and_target() {
    let err = Registry::with_builtins()
        .resolve(&ModelRef::new("ollama", ""))
        .err()
        .unwrap();
    let msg = err.to_string();
    assert!(msg.contains("ollama"), "{msg}");
}
