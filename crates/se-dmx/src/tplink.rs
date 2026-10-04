//! Native TP-Link legacy local protocol (EP10, TCP port 9999; no credentials).
//!
//! Framing follows python-kasa's `transports/xortransport.py`: an unencrypted
//! big-endian u32 payload length, then rolling XOR starting with key 0xab.
//! Each operation is bounded and uses one connection so the identity check and
//! any subsequent relay command address the same peer. No automatic retries:
//! a lost acknowledgement leaves outcome unknown; callers re-evaluate ownership.

use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PAYLOAD: usize = 64 * 1024;
const SYSINFO: &str = r#"{"system":{"get_sysinfo":{}}}"#;
const POWER_ON: &str = r#"{"system":{"set_relay_state":{"state":1}}}"#;
const POWER_OFF: &str = r#"{"system":{"set_relay_state":{"state":0}}}"#;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Device {
    pub mac: [u8; 6],
    pub alias: String,
    pub model: String,
    pub on: bool,
}

/// Change the relay only after matching the expected MAC, then verify its state.
/// An error after sending the relay command may mean the device changed state;
/// callers must not treat a failed acknowledgement as proof that nothing happened.
pub(crate) async fn set_power(
    host: &str,
    port: u16,
    expected_mac: [u8; 6],
    on: bool,
) -> Result<Device, String> {
    timeout(OPERATION_TIMEOUT, async {
        let mut stream = connect(host, port).await?;
        let before = sysinfo(&mut stream).await?;
        require_identity(&before, expected_mac)?;
        if before.on == on { return Ok(before); }

        let response = exchange(&mut stream, if on { POWER_ON } else { POWER_OFF }).await?;
        let acknowledgement = command_result(&response, "set_relay_state")?;
        if acknowledgement.get("err_code").and_then(Value::as_i64) != Some(0) {
            return Err("TP-Link set_relay_state acknowledgement is missing err_code=0".into());
        }

        let after = sysinfo(&mut stream).await?;
        require_identity(&after, expected_mac)?;
        if after.on != on {
            return Err(format!(
                "TP-Link relay verification failed: requested {}, observed {}",
                on, after.on
            ));
        }
        Ok(after)
    })
    .await
    .map_err(|_| "TP-Link set_power timed out (relay state may be unknown)".to_owned())?
}

async fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let stream = TcpStream::connect((host, port))
        .await
        .map_err(|error| format!("TP-Link connect to {host}:{port}: {error}"))?;
    stream
        .set_nodelay(true)
        .map_err(|error| format!("TP-Link TCP configuration: {error}"))?;
    Ok(stream)
}

async fn exchange(stream: &mut TcpStream, request: &str) -> Result<Value, String> {
    let mut frame = [0u8; 4 + POWER_OFF.len()];
    frame[..4].copy_from_slice(&(request.len() as u32).to_be_bytes());
    let mut key = 0xab;
    for (out, byte) in frame[4..].iter_mut().zip(request.bytes()) {
        key ^= byte;
        *out = key;
    }
    stream
        .write_all(&frame[..4 + request.len()])
        .await
        .map_err(|error| format!("TP-Link request write: {error}"))?;

    let mut header = [0; 4];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|error| format!("TP-Link response header: {error}"))?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_PAYLOAD {
        return Err(format!("TP-Link invalid response length: {length}"));
    }
    let mut payload = vec![0; length];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|error| format!("TP-Link response body: {error}"))?;
    decrypt(&mut payload);
    serde_json::from_slice(&payload).map_err(|error| format!("TP-Link response JSON: {error}"))
}

fn decrypt(payload: &mut [u8]) {
    let mut key = 0xab;
    for byte in payload {
        let encrypted = *byte;
        *byte ^= key;
        key = encrypted;
    }
}

fn check_error(value: &Value, context: &str) -> Result<(), String> {
    if let Some(code) = value.get("err_code") {
        match code.as_i64() {
            Some(0) => {}
            Some(code) => {
                return Err(format!(
                    "TP-Link {context} refused (err_code={code}, err_msg={})",
                    value.get("err_msg").and_then(Value::as_str).unwrap_or("unspecified")
                ));
            }
            None => return Err(format!("TP-Link {context} has invalid err_code")),
        }
    }
    Ok(())
}

fn command_result<'a>(response: &'a Value, command: &str) -> Result<&'a Value, String> {
    check_error(response, command)?;
    let system = response
        .get("system")
        .filter(|value| value.is_object())
        .ok_or_else(|| "TP-Link response is missing system object".to_owned())?;
    check_error(system, command)?;
    let result = system
        .get(command)
        .filter(|value| value.is_object())
        .ok_or_else(|| format!("TP-Link response is missing {command} object"))?;
    check_error(result, command)?;
    Ok(result)
}

async fn sysinfo(stream: &mut TcpStream) -> Result<Device, String> {
    let response = exchange(stream, SYSINFO).await?;
    let info = command_result(&response, "get_sysinfo")?;
    let mac = info
        .get("mac")
        .or_else(|| info.get("mic_mac"))
        .and_then(Value::as_str)
        .ok_or_else(|| "TP-Link get_sysinfo is missing MAC".to_owned())?;
    let on = match info.get("relay_state").and_then(Value::as_i64) {
        Some(0) => false,
        Some(1) => true,
        _ => return Err("TP-Link get_sysinfo has invalid relay_state (expected 0 or 1)".into()),
    };
    Ok(Device {
        mac: parse_mac(mac)?,
        alias: string_field(info, "alias")?,
        model: string_field(info, "model")?,
        on,
    })
}

fn string_field(info: &Value, field: &str) -> Result<String, String> {
    info.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("TP-Link get_sysinfo is missing {field} string"))
}

/// Parse a colon-separated, dash-separated, or compact six-byte hardware MAC.
pub(crate) fn parse_mac(value: &str) -> Result<[u8; 6], String> {
    let bytes = value.as_bytes();
    let separated = bytes.len() == 17 && matches!(bytes[2], b':' | b'-');
    if bytes.len() != 12 && !separated {
        return Err("TP-Link get_sysinfo has invalid MAC".into());
    }
    let mut mac = [0; 6];
    for (index, byte) in mac.iter_mut().enumerate() {
        let offset = index * if separated { 3 } else { 2 };
        if separated && index < 5 && bytes[offset + 2] != bytes[2] {
            return Err("TP-Link get_sysinfo has invalid MAC separators".into());
        }
        let nibble = |digit: u8| match digit {
            b'0'..=b'9' => Some(digit - b'0'),
            b'a'..=b'f' => Some(digit - b'a' + 10),
            b'A'..=b'F' => Some(digit - b'A' + 10),
            _ => None,
        };
        let high = nibble(bytes[offset]);
        let low = nibble(bytes[offset + 1]);
        *byte = match (high, low) {
            (Some(high), Some(low)) => high << 4 | low,
            _ => return Err("TP-Link get_sysinfo has invalid MAC digits".into()),
        };
    }
    Ok(mac)
}

fn require_identity(device: &Device, expected: [u8; 6]) -> Result<(), String> {
    if device.mac != expected {
        return Err(format!(
            "TP-Link identity mismatch: expected {expected:02X?}, received {:02X?}",
            device.mac
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    const MAC: [u8; 6] = [0xb4, 0xb0, 0x24, 0x69, 0xf2, 0x7b];

    fn info(on: bool) -> Value {
        json!({"system": {"get_sysinfo": {
            "err_code": 0,
            "mac": "B4:B0:24:69:F2:7B",
            "alias": "Main",
            "model": "EP10(US)",
            "relay_state": u8::from(on)
        }}})
    }

    async fn simulator(exchanges: Vec<(&'static str, Value)>) -> (u16, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            timeout(Duration::from_secs(2), async {
                let (mut stream, _) = listener.accept().await.unwrap();
                for (expected, response) in exchanges {
                    let mut header = [0; 4];
                    stream.read_exact(&mut header).await.unwrap();
                    let length = u32::from_be_bytes(header) as usize;
                    assert!(length <= MAX_PAYLOAD);
                    let mut request = vec![0; length];
                    stream.read_exact(&mut request).await.unwrap();
                    decrypt(&mut request);
                    assert_eq!(
                        serde_json::from_slice::<Value>(&request).unwrap(),
                        serde_json::from_str::<Value>(expected).unwrap()
                    );
                    let payload = serde_json::to_vec(&response).unwrap();
                    let mut frame = Vec::with_capacity(4 + payload.len());
                    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                    let mut key = 0xab;
                    for byte in payload {
                        key ^= byte;
                        frame.push(key);
                    }
                    stream.write_all(&frame).await.unwrap();
                }
                // No relay command after an identity rejection, and no retry or
                // verification query after a rejected acknowledgement.
                let mut unexpected = [0; 1];
                assert_eq!(stream.read(&mut unexpected).await.unwrap(), 0);
            })
            .await
            .expect("simulator timed out");
        });
        (port, task)
    }

    #[tokio::test]
    async fn wrong_mac_never_sends_relay_command() {
        let (port, server) = simulator(vec![(SYSINFO, info(true))]).await;
        let error = set_power("127.0.0.1", port, [0; 6], false).await.unwrap_err();
        assert!(error.contains("identity mismatch"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn device_refusal_stops_before_verification() {
        let (port, server) = simulator(vec![
            (SYSINFO, info(true)),
            (POWER_OFF, json!({"system": {"set_relay_state": {"err_code": -1, "err_msg": "denied"}}})),
        ])
        .await;
        let error = set_power("127.0.0.1", port, MAC, false).await.unwrap_err();
        assert!(error.contains("err_code=-1"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn acknowledged_but_unchanged_relay_is_an_error() {
        let (port, server) = simulator(vec![
            (SYSINFO, info(true)),
            (POWER_OFF, json!({"system": {"set_relay_state": {"err_code": 0}}})),
            (SYSINFO, info(true)),
        ])
        .await;
        let error = set_power("127.0.0.1", port, MAC, false).await.unwrap_err();
        assert!(error.contains("relay verification failed"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn missing_acknowledgement_code_is_not_success() {
        let (port, server) = simulator(vec![
            (SYSINFO, info(true)),
            (POWER_OFF, json!({"system": {"set_relay_state": {}}})),
        ])
        .await;
        let error = set_power("127.0.0.1", port, MAC, false).await.unwrap_err();
        assert!(error.contains("missing err_code=0"), "{error}");
        server.await.unwrap();
    }
}
