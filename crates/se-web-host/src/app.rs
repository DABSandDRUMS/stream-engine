//! The CEF application object: Chromium switches and browser-process setup for both modes.

use crate::{Mode, Opts, ipc};
use cef::*;
use std::sync::Arc;

/// Sites whose cookies must survive as third-party cookies inside the player page's iframe
/// (YouTube Premium is recognized only with the signed-in cookies, §13.3).
const COOKIE_SITES: &[&str] = &[
    "https://www.youtube.com/",
    "https://youtube.com/",
    "https://m.youtube.com/",
    "https://www.youtube-nocookie.com/",
    "https://accounts.google.com/",
    "https://www.google.com/",
    "https://google.com/",
];

/// Chromium switches for the browser process (children inherit what they need).
pub fn switches(opts: &Opts) -> Vec<(&'static str, Option<String>)> {
    let mut s: Vec<(&'static str, Option<String>)> = vec![
        // No setuid sandbox helper is installed (no root); pages are our own + YouTube.
        ("no-sandbox", None),
        // The player and alert pages must start audio/video without a click.
        ("autoplay-policy", Some("no-user-gesture-required".into())),
        // Cookies are stored with Chromium's built-in key instead of a desktop keyring, so the
        // engine never blocks on (or loses the session to) a locked keyring at boot. The profile
        // directory is private (0700).
        ("password-store", Some("basic".into())),
        // Off-screen pages are never "visible" to Chromium; keep timers, rAF, and media running.
        ("disable-background-timer-throttling", None),
        ("disable-renderer-backgrounding", None),
        ("disable-backgrounding-occluded-windows", None),
        // No MPRIS/media-key integration (the desktop must not control show audio), and treat the
        // player iframe's storage like first-party youtube.com storage.
        ("disable-features", Some("HardwareMediaKeyHandling,MediaSessionService,ThirdPartyStoragePartitioning".into())),
        ("disable-component-update", None),
        ("no-first-run", None),
        ("no-default-browser-check", None),
        ("noerrdialogs", None),
        ("hide-crash-restore-bubble", None),
    ];
    match &opts.mode {
        Some(Mode::Osr { .. }) => {
            // Off-screen rendering needs no display server: the host keeps working when the
            // engine runs without a graphical session.
            s.push(("ozone-platform", Some("headless".into())));
            s.push(("no-startup-window", None));
            if opts.gpu {
                // Headless ozone has no X11/EGL display, so WebGL/compositing go through ANGLE on
                // Vulkan (the RTX 3070 via the NVIDIA ICD). Frames still come back through the
                // CPU paint path (`on_paint`). The future zero-copy path is accelerated OSR
                // (`shared_texture_enabled` + `on_accelerated_paint` dmabufs), blocked on NVIDIA
                // until CEF ships chromiumembedded/cef PR #4238 (PLAN §4.2).
                s.push(("use-angle", Some("vulkan".into())));
                // ANGLE alone leaves the compositor on an incompatible shared-image path:
                // decoded native video frames fail in MailboxVideoFrameConverter and reset
                // the GPU process. Share Vulkan with the compositor as well.
                s.push(("enable-features", Some("Vulkan,VulkanFromANGLE".into())));
            } else {
                s.push(("disable-gpu", None));
                s.push(("disable-gpu-compositing", None));
            }
        }
        Some(Mode::Login { .. }) => {
            let platform = if std::env::var_os("DISPLAY").is_some() { "x11" } else { "wayland" };
            s.push(("ozone-platform", Some(platform.into())));
        }
        Some(Mode::Version) | None => {}
    }
    s
}

wrap_app! {
    pub struct HostApp {
        opts: Arc<Opts>,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefStringUtf16>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(cl) = command_line else { return };
            // Only the browser process (empty type); children get their switches from it.
            if process_type.is_some_and(|p| !p.to_string().is_empty()) {
                return;
            }
            for (name, value) in switches(&self.opts) {
                match value {
                    Some(v) => cl.append_switch_with_value(Some(&name.into()), Some(&v.as_str().into())),
                    None => cl.append_switch(Some(&name.into())),
                }
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(HostBrowserProcessHandler::new(self.opts.clone()))
        }
    }
}

wrap_browser_process_handler! {
    struct HostBrowserProcessHandler {
        opts: Arc<Opts>,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            allow_youtube_cookies();
            match &self.opts.mode {
                Some(Mode::Osr { .. }) => crate::osr::ready(self.opts.gpu),
                Some(Mode::Login { url }) => crate::login::open(url),
                Some(Mode::Version) | None => {}
            }
        }
    }
}

pub fn build(opts: Arc<Opts>) -> App {
    HostApp::new(opts)
}

fn set_pref(ctx: &RequestContext, name: &str, value: impl FnOnce(&Value)) {
    let Some(mut v) = value_create() else { return };
    value(&v);
    let mut err = CefString::default();
    if ctx.set_preference(Some(&name.into()), Some(&mut v), Some(&mut err)) != 1 {
        ipc::log("warn", format!("could not set preference {name}: {err}"));
    }
}

/// Persistent profile with third-party cookies allowed for YouTube/Google (the player page on
/// `http://127.0.0.1` embeds `https://www.youtube.com`).
fn allow_youtube_cookies() {
    let Some(ctx) = request_context_get_global_context() else {
        ipc::log("error", "no global request context; cookie settings not applied".to_string());
        return;
    };
    // 0 = allow third-party cookies (CookieControlsMode::kOff).
    set_pref(&ctx, "profile.cookie_controls_mode", |v| {
        v.set_int(0);
    });
    set_pref(&ctx, "profile.block_third_party_cookies", |v| {
        v.set_bool(0);
    });
    ctx.set_content_setting(None, None, ContentSettingTypes::COOKIES, ContentSettingValues::ALLOW);
    for site in COOKIE_SITES {
        ctx.set_content_setting(Some(&(*site).into()), None, ContentSettingTypes::COOKIES, ContentSettingValues::ALLOW);
    }
    let mode = ctx.preference(Some(&"profile.cookie_controls_mode".into())).map(|v| v.int());
    let block = ctx.preference(Some(&"profile.block_third_party_cookies".into())).map(|v| v.bool());
    ipc::log("info", format!("cookie prefs: cookie_controls_mode={mode:?} block_third_party_cookies={block:?}"));
}
