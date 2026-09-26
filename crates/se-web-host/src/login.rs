//! Sign-in mode (`--se-login=<url>`): a normal, windowed Chrome-style browser on the same
//! persistent profile as the off-screen host, so the owner can sign into YouTube/Google once
//! (§13.3). The process exits when the last window is closed; the cookies stay in the profile.

use cef::*;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicUsize, Ordering};

static OPEN: AtomicUsize = AtomicUsize::new(0);

wrap_life_span_handler! {
    struct LoginLifeSpan {}

    impl LifeSpanHandler {
        fn on_after_created(&self, _browser: Option<&mut Browser>) {
            let n = OPEN.fetch_add(1, Ordering::SeqCst) + 1;
            eprintln!("stream-engine-web: sign-in window open ({n})");
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            let n = OPEN.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
            if n == 0 {
                eprintln!("stream-engine-web: sign-in window closed");
                quit_message_loop();
            }
        }
    }
}

wrap_client! {
    struct LoginClient {
        life_span: LifeSpanHandler,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }
    }
}

/// Open the sign-in window (UI thread, from `on_context_initialized`).
pub fn open(url: &str) {
    let mut client = LoginClient::new(LoginLifeSpan::new());
    // Chrome style: a regular browser window with an address bar, so any Google sign-in flow
    // (2FA, account chooser, passkeys) works as in Chrome.
    let window_info = WindowInfo { runtime_style: RuntimeStyle::CHROME, ..Default::default() };
    let settings = BrowserSettings::default();
    let ok: c_int = browser_host_create_browser(Some(&window_info), Some(&mut client), Some(&url.into()), Some(&settings), None, None);
    if ok != 1 {
        eprintln!("stream-engine-web: could not open the sign-in window");
        quit_message_loop();
    }
}
