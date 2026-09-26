//! Tracing setup: stderr (journald) plus a layer that forwards log lines to the engine bus
//! so the UI console shows them.

use se_hub::Hub;
use std::sync::{Arc, OnceLock};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

static HUB: OnceLock<Arc<Hub>> = OnceLock::new();

pub fn attach_hub(hub: Arc<Hub>) {
    let _ = HUB.set(hub);
}

struct BusLayer {
    min: Level,
}

struct Msg(String);

impl Visit for Msg {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        } else if self.0.is_empty() {
            self.0 = format!("{}={value:?}", field.name());
        } else {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        } else {
            self.record_debug(field, &value);
        }
    }
}

impl<S: Subscriber> Layer<S> for BusLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() > self.min {
            return;
        }
        // core log lines are already published by the core itself
        if meta.target() == "core" {
            return;
        }
        let Some(hub) = HUB.get() else { return };
        let mut m = Msg(String::new());
        event.record(&mut m);
        hub.log(&meta.level().to_string().to_lowercase(), meta.target(), m.0);
    }
}

pub fn init(dev: bool) {
    let default =
        if dev { "debug,wgpu_core=warn,wgpu_hal=warn,naga=warn,notify=warn,hyper=info,tower_http=info" } else { "info,wgpu_core=warn,wgpu_hal=warn,naga=warn" };
    let filter = EnvFilter::try_from_env("STREAM_ENGINE_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let fmt = if journald {
        tracing_subscriber::fmt::layer().with_target(true).with_ansi(false).without_time().boxed()
    } else {
        tracing_subscriber::fmt::layer().with_target(true).boxed()
    };
    let _ = tracing_subscriber::registry().with(filter).with(fmt).with(BusLayer { min: if dev { Level::INFO } else { Level::WARN } }).try_init();
}
