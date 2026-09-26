//! Local Twitch stand-in for smoke runs without an account, next to the Twitch CLI's mock
//! EventSub server (`twitch event websocket start-server`).
//!
//! ```text
//! mock_twitch serve [addr] [client_id] [user_id] [login]
//!     id.twitch.tv (/oauth2/{device,token,validate}) + Helix (/helix/…) on addr
//!     (default 127.0.0.1:18090, client id `mock-client`, broadcaster 1337 `streamer`);
//!     GET /activate approves a pending device code, GET /_mock/calls lists Helix calls.
//! mock_twitch fire <payload.json>…
//!     forward EventSub payloads ({subscription, event} objects or arrays of them) to the
//!     running CLI mock server, for types `twitch event trigger` cannot generate.
//! ```

use se_twitch::mock::{Mock, cli_forward};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("fire") => {
            for path in &args[1..] {
                let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
                let items = match v {
                    serde_json::Value::Array(a) => a,
                    one => vec![one],
                };
                for item in items {
                    cli_forward(&item.to_string()).await?;
                }
                println!("forwarded {path}");
            }
            Ok(())
        }
        Some("serve") | None => {
            let addr = args.get(1).map(String::as_str).unwrap_or("127.0.0.1:18090").parse()?;
            let mock = Mock::new(
                args.get(2).map(String::as_str).unwrap_or("mock-client"),
                args.get(3).map(String::as_str).unwrap_or("1337"),
                args.get(4).map(String::as_str).unwrap_or("streamer"),
            );
            let bound = mock.serve(addr).await?;
            println!("mock Twitch on http://{bound} (auth_url http://{bound}/oauth2, helix_url http://{bound}/helix)");
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}` (serve | fire)").into()),
    }
}
