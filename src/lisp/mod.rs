//! Lisp config DSL. The interpreter is the configuration frontend;
//! simulation state lives on `World`. The Config object holds:
//!   - the tulisp context (interior-mutable for hot eval),
//!   - a Metadata block (socket addr, etc.) shared with the runtime,
//!   - per-component config specs (gridpools, market-makers) the
//!     binary reads at startup before constructing the World.
//!
//! Defuns are registered against an `Arc<RwLock<...>>`-fronted shared
//! state, so a `(set-*)` call from inside an `(every …)` callback —
//! which fires under the interpreter lock at tick time — can mutate
//! a value that the gRPC service or the market-maker task is reading
//! concurrently.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tulisp::{SharedMut, TulispContext};

/// Top-level identity + transport settings, set via lisp defuns.
#[derive(Clone, Debug)]
pub struct Metadata {
    pub socket_addr: String,
    pub physics_tick: Duration,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            socket_addr: "[::1]:8810".to_string(),
            physics_tick: Duration::from_millis(100),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    #[allow(dead_code)]
    filename: String,
    #[allow(dead_code)]
    pub(crate) ctx: SharedMut<TulispContext>,
    metadata: Arc<RwLock<Metadata>>,
}

impl Config {
    /// Build a config from `filename`. Returns the formatted lisp
    /// error on parse/eval failure — the caller (binary boot)
    /// decides whether to panic or fall back to defaults.
    pub fn new(filename: &str) -> Result<Self, String> {
        let mut ctx = TulispContext::new();
        let metadata = Arc::new(RwLock::new(Metadata::default()));

        // `Path::parent()` returns Some("") for bare names; tulisp
        // rejects empty paths, so fall back to "." in that case.
        let load_dir: PathBuf = match Path::new(filename).parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        ctx.set_load_path(Some(&load_dir))
            .map_err(|e| format!("set_load_path({}): {e}", load_dir.display()))?;

        register_runtime(&mut ctx, metadata.clone());

        if let Err(e) = ctx.eval_file(filename) {
            return Err(e.format(&ctx));
        }

        Ok(Self {
            filename: filename.to_string(),
            ctx: SharedMut::new(ctx),
            metadata,
        })
    }

    pub fn metadata(&self) -> Metadata {
        self.metadata.read().clone()
    }

    pub fn socket_addr(&self) -> String {
        self.metadata.read().socket_addr.clone()
    }
}

fn register_runtime(ctx: &mut TulispContext, metadata: Arc<RwLock<Metadata>>) {
    add_log_functions(ctx);
    register_metadata(ctx, metadata);
}

fn register_metadata(ctx: &mut TulispContext, metadata: Arc<RwLock<Metadata>>) {
    let m = metadata.clone();
    ctx.defun("set-socket-addr", move |addr: String| -> String {
        m.write().socket_addr = addr.clone();
        addr
    });
    let m = metadata.clone();
    ctx.defun("set-physics-tick-ms", move |ms: i64| -> i64 {
        let ms = ms.max(1) as u64;
        m.write().physics_tick = Duration::from_millis(ms);
        ms as i64
    });
    let m = metadata;
    ctx.defun("get-socket-addr", move || -> String {
        m.read().socket_addr.clone()
    });
}

fn add_log_functions(ctx: &mut TulispContext) {
    ctx.defun("log.info", |msg: String| log::info!("{msg}"))
        .defun("log.warn", |msg: String| log::warn!("{msg}"))
        .defun("log.error", |msg: String| log::error!("{msg}"))
        .defun("log.debug", |msg: String| log::debug!("{msg}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(body: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::Builder::new()
            .suffix(".lisp")
            .tempfile()
            .unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn empty_config_yields_defaults() {
        let f = write_tmp(";; empty\n");
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        assert_eq!(cfg.socket_addr(), "[::1]:8810");
        assert_eq!(cfg.metadata().physics_tick, Duration::from_millis(100));
    }

    #[test]
    fn set_socket_addr_takes_effect() {
        let f = write_tmp(r#"(set-socket-addr "[::]:9000")"#);
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        assert_eq!(cfg.socket_addr(), "[::]:9000");
    }

    #[test]
    fn set_physics_tick_ms_takes_effect() {
        let f = write_tmp("(set-physics-tick-ms 200)");
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        assert_eq!(cfg.metadata().physics_tick, Duration::from_millis(200));
    }

    #[test]
    fn lisp_error_surfaces_with_trace() {
        let f = write_tmp("(this-is-not-a-defun 1 2 3)");
        // Config doesn't impl Debug (SharedMut<TulispContext> doesn't),
        // so test the error path via match instead of unwrap_err.
        let path = f.path().to_str().unwrap().to_string();
        match Config::new(&path) {
            Ok(_) => panic!("expected error from {path}"),
            Err(e) => assert!(e.contains("this-is-not-a-defun"), "got: {e}"),
        }
    }
}
