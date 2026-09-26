//! OAuth device code flow and token refresh against the local id.twitch.tv mock.

use se_twitch::auth::{Account, Auth, AuthError, MemoryStore, Poll, SecretStore};
use se_twitch::config::TwitchCfg;
use se_twitch::mock::Mock;
use std::sync::Arc;

fn auth(url: &str, store: &Arc<MemoryStore>) -> Auth {
    let cfg = TwitchCfg { client_id: "cid".into(), auth_url: url.into(), secrets: "t".into(), ..Default::default() };
    Auth::new(reqwest::Client::new(), cfg, store.clone())
}

#[tokio::test]
async fn device_flow_then_rotating_refresh_then_revocation() {
    let mock = Mock::new("cid", "1337", "streamer");
    let addr = mock.clone().serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let url = format!("http://{addr}/oauth2");
    let store = Arc::new(MemoryStore::default());
    let a = auth(&url, &store);

    let dc = a.device_start(Account::Broadcaster).await.unwrap();
    assert_eq!(dc.user_code, "MOCK-CODE");
    assert_eq!(a.device_poll(Account::Broadcaster, &dc).await.unwrap(), Poll::Pending, "user has not entered the code yet");
    mock.approve();
    let Poll::Done(t) = a.device_poll(Account::Broadcaster, &dc).await.unwrap() else { panic!("expected tokens") };
    assert_eq!((t.user_id.as_str(), t.login.as_str()), ("1337", "streamer"));
    assert!(a.missing_scopes(Account::Broadcaster).is_empty());
    assert_eq!(store.get("t.refresh_token").unwrap(), Some(t.refresh.clone()), "refresh token in the keyring");

    // refresh rotates the single-use refresh token and keeps the keyring current
    let t2 = a.refresh(Account::Broadcaster).await.unwrap();
    assert_ne!(t2.refresh, t.refresh);
    assert_eq!(store.get("t.refresh_token").unwrap(), Some(t2.refresh.clone()));
    assert_eq!(a.access(Account::Broadcaster).await.unwrap(), t2.access);
    a.validate(Account::Broadcaster).await.unwrap();

    // another process (engine restart) loads the stored grant, which rotates it again …
    let b = auth(&url, &store);
    assert_eq!(b.load(Account::Broadcaster).await, Ok(true));
    // … so the first holder's refresh token is spent: it must re-authorize, without deleting the
    // newer grant from the keyring
    let err = a.refresh(Account::Broadcaster).await.unwrap_err();
    assert!(matches!(err, AuthError::Revoked(_)), "{err}");
    assert!(a.tokens(Account::Broadcaster).is_none());
    assert_eq!(store.get("t.refresh_token").unwrap(), b.tokens(Account::Broadcaster).map(|t| t.refresh));
    // nothing stored for the bot → not authorized, no error
    assert_eq!(b.load(Account::Bot).await, Ok(false));
    // a wrong client id is refused at the device step
    let wrong = Auth::new(reqwest::Client::new(), TwitchCfg { client_id: "other".into(), auth_url: url, ..Default::default() }, store);
    assert!(wrong.device_start(Account::Broadcaster).await.is_err());
}
