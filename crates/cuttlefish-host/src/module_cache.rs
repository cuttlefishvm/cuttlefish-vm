//! A content-hash-keyed cache of compiled `wasmtime::Module`s.
//!
//! `wasmtime::Module::new` measurably costs ~1.5s for a ~3.3MB module (the
//! shared Rhai interpreter this feature adds) vs. ~125ms for a small
//! example block — over 10x — and today nothing in this codebase caches a
//! compiled module at all: `Guest::new` recompiles from scratch on every
//! single job run. Every `Script`-kind node, across every spec and every
//! job ever run against it, shares byte-identical `module_bytes` (the one
//! embedded interpreter), so this cache turns an otherwise-repeated ~1.5s
//! tax into a one-time cost per process lifetime.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// **Load-bearing invariant this cache does not check**: every `compile`
/// call through one `ModuleCache` must pass the *same* `wasmtime::Engine`
/// every time. A `Module` is only valid for the `Engine` that compiled it —
/// mixing engines through one cache would silently return a `Module`
/// compiled for the wrong `Engine`. This codebase constructs exactly one
/// `Engine` per process (`cuttlefishd`'s and `cuttlefish build`'s `main`),
/// so a `ModuleCache` constructed once alongside it, and never shared
/// across processes, upholds this automatically. A test that constructs
/// its own throwaway `Engine` should also construct its own throwaway
/// `ModuleCache` — never reuse one across two different `Engine`s.
pub struct ModuleCache {
    modules: Mutex<HashMap<String, Arc<wasmtime::Module>>>,
}

impl ModuleCache {
    /// A fresh, empty cache.
    pub fn new() -> Self {
        Self {
            modules: Mutex::new(HashMap::new()),
        }
    }

    /// Compile `module_bytes` against `engine`, reusing a cached
    /// compilation if these exact bytes were compiled before through this
    /// same cache.
    pub fn compile(
        &self,
        engine: &wasmtime::Engine,
        module_bytes: &[u8],
    ) -> anyhow::Result<Arc<wasmtime::Module>> {
        use sha2::{Digest, Sha256};
        let key = format!("{:x}", Sha256::digest(module_bytes));

        if let Some(cached) = self.modules.lock().unwrap().get(&key) {
            return Ok(cached.clone());
        }

        let module = Arc::new(wasmtime::Module::new(engine, module_bytes)?);
        self.modules.lock().unwrap().insert(key, module.clone());
        Ok(module)
    }
}

impl Default for ModuleCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trivial_wasm() -> Vec<u8> {
        wat::parse_str("(module)").unwrap()
    }

    #[test]
    fn compiling_the_same_bytes_twice_returns_the_same_arc() {
        let engine = wasmtime::Engine::default();
        let cache = ModuleCache::new();
        let bytes = trivial_wasm();

        let a = cache.compile(&engine, &bytes).unwrap();
        let b = cache.compile(&engine, &bytes).unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "second call should hit the cache, not recompile"
        );
    }

    #[test]
    fn compiling_different_bytes_returns_different_modules() {
        let engine = wasmtime::Engine::default();
        let cache = ModuleCache::new();
        let a = cache.compile(&engine, &trivial_wasm()).unwrap();
        let other_wasm = wat::parse_str("(module (func))").unwrap();
        let b = cache.compile(&engine, &other_wasm).unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
    }
}
