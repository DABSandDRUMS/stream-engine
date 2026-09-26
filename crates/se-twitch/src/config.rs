//! `[twitch]` in `project.toml`.

use se_core::config::Dur;
use serde::Deserialize;

/// OAuth scopes requested for the broadcaster account (§11, confirmed against the EventSub and
/// Helix reference: `channel.ban`/`channel.unban` additionally need `channel:moderate`).
pub const SCOPES: &[&str] = &[
    "user:read:chat",
    "user:write:chat",
    "bits:read",
    "channel:read:subscriptions",
    "channel:read:hype_train",
    "channel:read:redemptions",
    "channel:manage:redemptions",
    "channel:manage:polls",
    "channel:manage:predictions",
    "channel:manage:broadcast",
    "channel:manage:raids",
    "channel:read:ads",
    "channel:manage:ads",
    "channel:moderate",
    "moderator:read:followers",
    "moderator:manage:shoutouts",
    "moderator:manage:banned_users",
    "moderator:manage:chat_messages",
    "moderator:manage:automod",
    "moderator:manage:blocked_terms",
];

/// Scopes for the optional bot account (it only reads and writes chat).
pub const BOT_SCOPES: &[&str] = &["user:read:chat", "user:write:chat"];

/// Where chat replies (`twitch.chat.send`) come from by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChatAs {
    #[default]
    Broadcaster,
    Bot,
}

/// Source of sub/resub/gift events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SubSource {
    /// `channel.chat.notification`: links every gifted sub to its gift bomb (`gift_id`).
    #[default]
    Chat,
    /// `channel.subscribe` / `.subscription.gift` / `.subscription.message` (gift recipients are
    /// attributed to the latest open gift bomb of the same tier).
    Eventsub,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TwitchCfg {
    /// App Client ID from dev.twitch.tv/console (public client, device code flow).
    pub client_id: String,
    /// Authorize a second account for chat replies.
    pub bot: bool,
    pub chat_as: ChatAs,
    pub sub_source: SubSource,
    /// Third-party emote providers for overlays: `7tv`, `bttv`, `ffz`.
    pub emote_providers: Vec<String>,
    pub viewer_poll_ms: u64,
    pub ads_poll_ms: u64,
    /// Emit `twitch.ad.upcoming` this many seconds before a scheduled ad.
    pub ad_warning_s: i64,
    /// Viewer-side video latency estimate added to the measured delay (low-latency ≈ 2–3 s).
    pub latency_ms: u64,
    /// Messages per 30 s the chat sender allows itself (Twitch: 20 for users, 100 for mods/broadcaster).
    pub chat_rate: u32,
    pub sync_rewards: bool,
    /// Keyring name prefix for the tokens (`<prefix>.refresh_token`, `<prefix>.bot_refresh_token`).
    pub secrets: String,
    pub eventsub_url: String,
    pub helix_url: String,
    pub auth_url: String,
    /// EventSub subscription endpoint (default `<helix_url>/eventsub/subscriptions`; the Twitch
    /// CLI mock server serves its own).
    pub subscriptions_url: String,
}

impl Default for TwitchCfg {
    fn default() -> Self {
        TwitchCfg {
            client_id: String::new(),
            bot: false,
            chat_as: ChatAs::Broadcaster,
            sub_source: SubSource::Chat,
            emote_providers: vec!["7tv".into(), "bttv".into(), "ffz".into()],
            viewer_poll_ms: 30_000,
            ads_poll_ms: 60_000,
            ad_warning_s: 60,
            latency_ms: 2_500,
            chat_rate: 20,
            sync_rewards: true,
            secrets: "twitch".into(),
            eventsub_url: "wss://eventsub.wss.twitch.tv/ws".into(),
            helix_url: "https://api.twitch.tv/helix".into(),
            auth_url: "https://id.twitch.tv/oauth2".into(),
            subscriptions_url: "https://api.twitch.tv/helix/eventsub/subscriptions".into(),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct File {
    client_id: Option<String>,
    bot: Option<bool>,
    chat_as: Option<ChatAs>,
    sub_source: Option<SubSource>,
    emotes: Option<Vec<String>>,
    viewer_poll: Option<Dur>,
    ads_poll: Option<Dur>,
    ad_warning: Option<Dur>,
    latency: Option<Dur>,
    chat_rate: Option<u32>,
    sync_rewards: Option<bool>,
    secrets: Option<String>,
    eventsub_url: Option<String>,
    helix_url: Option<String>,
    auth_url: Option<String>,
    subscriptions_url: Option<String>,
}

impl TwitchCfg {
    pub fn parse(v: Option<&toml::Value>) -> Result<TwitchCfg, String> {
        let Some(v) = v else { return Ok(TwitchCfg::default()) };
        let f: File = v.clone().try_into().map_err(|e: toml::de::Error| format!("project.toml [twitch]: {}", e.message()))?;
        let d = TwitchCfg::default();
        let helix_url = f.helix_url.map(|u| u.trim_end_matches('/').to_string()).unwrap_or(d.helix_url);
        let providers = f.emotes.unwrap_or(d.emote_providers);
        if let Some(p) = providers.iter().find(|p| !matches!(p.as_str(), "7tv" | "bttv" | "ffz")) {
            return Err(format!("project.toml [twitch] emotes: unknown provider `{p}` (7tv, bttv, ffz)"));
        }
        let secrets = f.secrets.unwrap_or(d.secrets);
        if secrets.is_empty() || !secrets.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err("project.toml [twitch] secrets: use letters, digits, `-`, `_`".into());
        }
        Ok(TwitchCfg {
            client_id: f.client_id.unwrap_or_default().trim().to_string(),
            bot: f.bot.unwrap_or(false),
            chat_as: f.chat_as.unwrap_or_default(),
            sub_source: f.sub_source.unwrap_or_default(),
            emote_providers: providers,
            viewer_poll_ms: f.viewer_poll.map(Dur::ms).unwrap_or(d.viewer_poll_ms).max(5_000),
            ads_poll_ms: f.ads_poll.map(Dur::ms).unwrap_or(d.ads_poll_ms).max(10_000),
            ad_warning_s: f.ad_warning.map(|x| (x.ms() / 1000) as i64).unwrap_or(d.ad_warning_s),
            latency_ms: f.latency.map(Dur::ms).unwrap_or(d.latency_ms),
            chat_rate: f.chat_rate.unwrap_or(d.chat_rate).clamp(1, 100),
            sync_rewards: f.sync_rewards.unwrap_or(true),
            secrets,
            eventsub_url: f.eventsub_url.unwrap_or(d.eventsub_url),
            subscriptions_url: f.subscriptions_url.unwrap_or_else(|| format!("{helix_url}/eventsub/subscriptions")),
            helix_url,
            auth_url: f.auth_url.map(|u| u.trim_end_matches('/').to_string()).unwrap_or(d.auth_url),
        })
    }

    pub fn refresh_secret(&self, bot: bool) -> String {
        if bot { format!("{}.bot_refresh_token", self.secrets) } else { format!("{}.refresh_token", self.secrets) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        assert_eq!(TwitchCfg::parse(None).unwrap(), TwitchCfg::default());
        let v: toml::Value =
            toml::from_str("client_id = \" abc \"\nbot = true\nchat_as = \"bot\"\nhelix_url = \"http://127.0.0.1:9/helix/\"\nviewer_poll = \"10s\"").unwrap();
        let c = TwitchCfg::parse(Some(&v)).unwrap();
        assert_eq!(c.client_id, "abc");
        assert_eq!(c.chat_as, ChatAs::Bot);
        assert_eq!(c.subscriptions_url, "http://127.0.0.1:9/helix/eventsub/subscriptions");
        assert_eq!(c.viewer_poll_ms, 10_000);
        // the default secret names match the engine's well-known keyring entries
        assert_eq!(TwitchCfg::default().refresh_secret(false), se_store::secrets::names::TWITCH_REFRESH);
        assert_eq!(TwitchCfg::default().refresh_secret(true), se_store::secrets::names::TWITCH_BOT_REFRESH);
        let bad: toml::Value = toml::from_str("clientid = \"x\"").unwrap();
        assert!(TwitchCfg::parse(Some(&bad)).is_err());
    }
}
