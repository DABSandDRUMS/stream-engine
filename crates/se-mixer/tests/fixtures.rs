//! Decoding real traffic recorded from the owner's StudioLive 16R (firmware 3.2.0.108461).
//! A firmware update that changes the protocol shows up here first (PLAN §27).

mod support;

use se_mixer::map::{Control, Coverage, Param, Strip, fader_to_db};
use se_mixer::sync::{Remote, SyncConfig, SyncEngine};
use se_mixer::ucnet::meters::{LevelFrame, MeterKind, group, to_db};
use se_mixer::ucnet::msg::{Decoder, Incoming};
use se_mixer::ucnet::packet::{self, Code};
use std::time::Instant;
use support::{fixture, handshake_state, tcp_packets, udp_datagrams};

fn decode_all(name: &str) -> Vec<Incoming> {
    let mut d = Decoder::default();
    tcp_packets(&fixture(name)).iter().map(|p| d.decode(packet::parse(p).unwrap()).unwrap()).collect()
}

#[test]
fn handshake_yields_state_and_subscription_reply() {
    let msgs = decode_all("handshake.tcp");
    assert!(msgs.iter().any(|m| matches!(m, Incoming::Json(j) if j["id"] == "SubscriptionReply")));
    let states = msgs.iter().filter(|m| matches!(m, Incoming::State(_))).count();
    assert_eq!(states, 1, "the chunked (CK) state payload reassembles exactly once");
    let st = handshake_state();
    let info = st.info();
    assert_eq!(info.model, "StudioLive 16R");
    assert_eq!(info.firmware, "3.2.0.108461");
    assert_eq!(info.serial, "RA1E24110101");
    assert_eq!(st.text("line/ch1/username"), Some("Kick"));
    assert_eq!(st.text("line/ch10/username"), Some("VocalMic"));
}

#[test]
fn coverage_matches_the_16r() {
    let cov = Coverage::measure(&handshake_state());
    assert_eq!(cov.lines, (1..=16).collect::<Vec<u16>>());
    assert_eq!(cov.auxes, (1..=6).collect::<Vec<u16>>());
    assert_eq!(cov.fxbuses, vec![1, 2]);
    assert_eq!(cov.returns, vec![1]);
    assert_eq!(cov.fxreturns, vec![1, 2]);
    assert!(cov.talkback && cov.main);
    let has = |s: Strip, p: Param| cov.controls.contains(&Control::new(s, p));
    assert!(has(Strip::Line(16), Param::Fader));
    assert!(has(Strip::Line(10), Param::SendAux(1)));
    assert!(has(Strip::Line(10), Param::SendFx(2)));
    assert!(has(Strip::Main, Param::Fader));
    assert!(has(Strip::Aux(6), Param::Mute));
    assert!(has(Strip::FxBus(1), Param::FxParam("predelay".into())));
    // the payload lists aux1..aux32 / FXA..FXH on every strip; only real buses count
    assert!(!has(Strip::Line(1), Param::SendAux(7)));
    assert!(!has(Strip::Line(1), Param::SendFx(3)));
    let sends = cov.controls.iter().filter(|c| matches!(c.param, Param::SendAux(_))).count();
    assert_eq!(sends, 6 * (16 + 1 + 2 + 1));
}

#[test]
fn fader_echo_is_a_full_fdrs_packet() {
    let msgs = decode_all("fader_set.tcp");
    let Some(Incoming::Faders(groups)) = msgs.iter().find(|m| matches!(m, Incoming::Faders(_))) else { panic!("no fdrs") };
    let line = groups.iter().find(|g| g.group_name() == Some("line")).unwrap();
    assert_eq!(line.values.len(), 16);
    assert!((line.values[15] - 0.5).abs() < 1e-4, "ch16 at 0.5: {:?}", line.values);
    assert!((line.values[0] - 0.752809).abs() < 2e-5, "ch1 unchanged");
    let names: Vec<_> = groups.iter().filter_map(|g| g.group_name()).collect();
    assert_eq!(names, ["line", "return", "fxreturn", "talkback", "aux", "fxbus", "main"]);
    let restore = decode_all("fader_restore.tcp");
    let Some(Incoming::Faders(g2)) = restore.iter().find(|m| matches!(m, Incoming::Faders(_))) else { panic!() };
    assert_eq!(g2[0].values[15], 0.0);
}

#[test]
fn mute_echo_is_a_pv() {
    let on = decode_all("mute_on.tcp");
    assert!(matches!(&on[..], [Incoming::Param { path, value }] if path == "line/ch16/mute" && *value == 1.0));
    let off = decode_all("mute_restore.tcp");
    assert!(matches!(&off[..], [Incoming::Param { path, value }] if path == "line/ch16/mute" && *value == 0.0));
}

#[test]
fn keepalive_replies_carry_our_request_ids() {
    let ids: Vec<u16> = decode_all("keepalive.tcp")
        .into_iter()
        .filter_map(|m| match m {
            Incoming::FileData { id } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![0x5e01, 0x5e02]);
}

#[test]
fn meter_capture_decodes() {
    let datagrams = udp_datagrams(&fixture("meters.udp"));
    assert!(datagrams.len() > 50);
    let mut f = LevelFrame::default();
    let (mut levels, mut other) = (0, 0);
    let mut peak_ch6 = 0f32;
    for d in &datagrams {
        assert_eq!(packet::parse_lenient(d).unwrap().code, Code::METER16);
        match f.parse(d).unwrap() {
            MeterKind::Level => {
                levels += 1;
                peak_ch6 = peak_ch6.max(f.level(group::INPUT, 5));
                // nothing is plugged into 13–16
                for ch in 12..16 {
                    assert_eq!(f.level(group::INPUT, ch), 0.0);
                }
            }
            _ => other += 1,
        }
    }
    assert!(levels > 20 && other > 20, "levl {levels}, redu {other}");
    // ch6 carries room noise from its mic at roughly −60 dBFS
    let db = to_db(peak_ch6);
    assert!((-75.0..-45.0).contains(&db), "{db}");
}

#[test]
fn recorded_echoes_drive_the_sync_engine() {
    // replay: we set ch16 to 0.5 and back; the console's own packets are echoes, not changes
    let st = handshake_state();
    let mut s = SyncEngine::new(SyncConfig::default());
    let ctl = Control::new(Strip::Line(16), Param::Fader);
    let mute = Control::new(Strip::Line(16), Param::Mute);
    s.track(ctl.clone(), "mixer.16r.ch.16.fader".into(), true, st.num(&ctl.path()));
    s.track(mute.clone(), "mixer.16r.ch.16.mute".into(), true, st.num(&mute.path()));
    s.track(Control::new(Strip::Line(1), Param::Fader), "mixer.16r.ch.1.fader".into(), true, st.num("line/ch1/volume"));
    let now = Instant::now();
    assert!(s.resolved("mixer.16r.ch.16.mute", 1.0, now));
    assert!(s.resolved("mixer.16r.ch.16.fader", 0.5, now));
    assert_eq!(s.due(now).len(), 2);
    let mut seen = Vec::new();
    for name in ["mute_on.tcp", "fader_set.tcp"] {
        for m in decode_all(name) {
            match m {
                Incoming::Param { path, value } => seen.push((path.clone(), s.remote(&path, value as f64, now).unwrap().1)),
                Incoming::Faders(groups) => {
                    for g in &groups {
                        for (i, v) in g.values.iter().enumerate() {
                            let path = format!("{}/ch{}/volume", g.group_name().unwrap(), i + 1);
                            if let Some((_, r)) = s.remote(&path, *v, now) {
                                seen.push((path, r));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    assert!(seen.contains(&("line/ch16/mute".into(), Remote::Echo)));
    assert!(seen.contains(&("line/ch16/volume".into(), Remote::Echo)));
    assert!(seen.contains(&("line/ch1/volume".into(), Remote::Unchanged)), "the 16-bit fdrs value matches the float from the state");
    assert!(!seen.iter().any(|(_, r)| *r == Remote::External), "{seen:?}");
    assert!((fader_to_db(0.5) + 10.1).abs() < 0.5, "half travel ≈ −10 dB on this law");
}
