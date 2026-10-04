use super::*;
use se_core::{Config, Core, Input, Output, SourceFile};
use se_hub::{CoreMsg, Snapshot};

fn file(kind: &str, name: &str, text: &str) -> SourceFile {
    SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(text).unwrap() }
}

fn snapshot(core: &Core) -> Snapshot {
    let state = core.state();
    let signals = core.signals();
    Snapshot {
        tick: core.tick_index(), now: core.now(), generation: state.generation,
        index: Arc::new(state.params().iter().enumerate().map(|(i, p)| (p.addr.clone(), i)).collect()),
        values: state.params().iter().map(|p| p.resolved.clone()).collect(),
        priorities: state.params().iter().map(|p| p.overrides.iter().map(|o| o.priority).max().unwrap_or(0)).collect(),
        signal_names: Arc::new(signals.names().to_vec()),
        signal_index: Arc::new(signals.names().iter().enumerate().map(|(i, n)| (n.clone(), i)).collect()),
        signals: signals.values().to_vec(),
    }
}

fn ctl(ctx: EngineCtx) -> Ctl {
    let show = Arc::new(Show::load(&ctx.config.borrow(), &Show::empty()));
    assert!(show.errors.is_empty(), "{:?}", show.errors);
    let (plan, plan_errors) = Plan::build(show.rig.clone(), show.effects.clone());
    assert!(plan_errors.is_empty(), "{plan_errors:?}");
    let (_, monitor) = triple_buffer::TripleBuffer::new(&Monitor::default()).split();
    let (_, retired) = rtrb::RingBuffer::new(2);
    let shared = Arc::new(Shared {
        plan: arc_swap::ArcSwap::from_pointee(plan), monitor: parking_lot::Mutex::new(monitor),
        status: Default::default(), rdm: Default::default(), rdm_request: Default::default(),
        stop: Default::default(), scheduling: Default::default(), frames: Default::default(),
        rehearsal: Default::default(), output_sent: Default::default(), output_in_use: Default::default(),
        alloc_violations: Default::default(), alive: std::sync::atomic::AtomicBool::new(true),
        cid: [0; 16], retired: parking_lot::Mutex::new(retired),
    });
    let view = Arc::new(RwLock::new(View {
        show: show.clone(), plan_errors, playbacks: BTreeMap::new(), looks: BTreeMap::new(),
        prog: Programmer::default(), layers: Value::map(), layer_expiry: BTreeMap::new(),
    }));
    Ctl {
        hub: ctx.hub.clone(), ctx, show, shared, view, playbacks: BTreeMap::new(),
        operator_playbacks: BTreeSet::new(), looks: BTreeMap::new(), layers: BTreeMap::new(),
        pending_layers: BTreeMap::new(), layer_seq: 0, clock: Default::default(), clock_at: Instant::now(),
        prog: Default::default(), timers: BinaryHeap::new(), timer_data: HashMap::new(), timer_seq: 0,
        tokens: 0, prefixes: BTreeSet::new(), masters: HashMap::new(), master_pending: BTreeSet::new(),
        flash_token: 0, rdm_started: false, rdm_logged: 0, health: Value::Null, published: HashMap::new(),
        persisted: Value::Null, second: 0, live: Default::default(), panic_latched: false,
        picks: BTreeMap::new(), pick_rng: 1,
    }
}

fn act(ctl: &mut Ctl, text: &str) {
    let command = Command::new(Origin::Deck, Op::parse(text).unwrap());
    let Op::Action { name, args } = &command.op else { panic!("action: {text}") };
    ctl.handle(name, args, &command).unwrap();
}

#[test]
fn default_lighting_restores_idle_after_all_owned_and_direct_contributions() {
    let config = Config::build(&[
        file("project", "project", "schema=1"),
        file("lights", "rig", "[fixtures.par]\nprofile='generic_rgb'\nmode='3ch'\naddress=1\nlayout_verified=true\n[groups]\nfront=['par']\n[idle]\ntarget='par'\ncolor='#ff69b4'\nintensity=0.3\n[output]\narmed=false\n[safety]\nmax_intensity=0.35\nstrobe='block'"),
        file("lights/palettes", "red", "[set]\nall={intensity=0.3,color='#ff0000'}"),
        file("lights/palettes", "blue", "[set]\nall={intensity=0.3,color='#0000ff'}"),
        file("lights/cuelists", "delayed", "[[cue]]\ndelay='5s'\nset={all={intensity=0.3,color='#0000ff'}}"),
        file("lights/effects", "pulse", "kind='dimmer_sine'\nrate=1\nsize=1\norder='index'"),
    ]);
    assert!(config.errors.is_empty(), "{:?}", config.errors);
    let (hub, rx) = Hub::new(Arc::new(se_clock::Clock::new()));
    let mut core = Core::new(config.clone(), 0);
    let (_tx, config) = tokio::sync::watch::channel(Arc::new(config));
    let dir = tempfile::tempdir().unwrap();
    let mut ctl = ctl(EngineCtx {
        hub: hub.clone(), db: se_store::Db::memory().unwrap(), project_root: dir.path().into(),
        data_dir: dir.path().into(), share_dir: dir.path().into(), config,
        http: "127.0.0.1:0".parse().unwrap(), dev: true,
    });
    // No worker, output thread, socket or wall-clock wait: deliver inputs and actions explicitly.
    let pump = |ctl: &mut Ctl, core: &mut Core| {
        for _ in 0..8 {
            for message in rx.try_iter() {
                if let CoreMsg::Input(input) = message { core.submit(input); }
            }
            core.step();
            for output in core.drain_outputs() {
                if let Output::Action(command) = output { ctl.action(command); }
            }
            hub.snapshot.store(Arc::new(snapshot(core)));
        }
    };
    ctl.declare();
    pump(&mut ctl, &mut core);
    let plan = ctl.shared.plan.load_full();
    let mut engine = crate::engine::Engine::new(plan);
    assert_eq!(&engine.render(&hub.snapshot.load(), 1_000_000_000).universes[0][..3], &[77, 32, 54]);

    act(&mut ctl, "lights.layer.select layer=base palette=red fade=0");
    act(&mut ctl, "lights.layer.select layer=base palette=blue fade=5s");
    act(&mut ctl, "lights.layer.select layer=accent palette=red quantize=4 duration=5s");
    act(&mut ctl, "lights.cue delayed");
    act(&mut ctl, "lights.cue look=red fade=0");
    act(&mut ctl, "lights.release look=red fade=5s");
    act(&mut ctl, "lights.programmer.select targets=all");
    act(&mut ctl, "lights.programmer.set attr=intensity value=0.3");
    act(&mut ctl, "lights.programmer.highlight on=true");
    act(&mut ctl, "lights.effect.start pulse");
    act(&mut ctl, "lights.flash ms=5s");
    for (origin, address, value) in [
        (Origin::System, "lights.par.intensity", Value::Float(0.2)),
        (Origin::Ui, "lights.par.intensity", Value::Float(0.3)),
        (Origin::Ui, "lights.group.front.color", Value::from("#0000ff")),
        (Origin::Ui, "lights.group.front.master", Value::Float(0.0)),
        (Origin::Ui, "lights.master", Value::Float(0.0)),
        (Origin::Ui, "lights.blackout", Value::Bool(true)),
        (Origin::Ui, "unrelated", Value::Float(0.8)),
    ] {
        hub.command(Command::new(origin, Op::Set { address: address.into(), value }));
    }
    pump(&mut ctl, &mut core);
    assert!(!ctl.layers.is_empty() && !ctl.pending_layers.is_empty());
    assert!(ctl.timer_data.values().any(|t| matches!(t, Timer::Release { list, .. } if list.starts_with("layer:"))));
    assert!(ctl.timer_data.values().any(|t| matches!(t, Timer::Release { list, .. } if list.starts_with("look:"))));
    assert!(ctl.timer_data.values().any(|t| matches!(t, Timer::Entry { .. })));
    let timers = ctl.timer_data.len();
    let spoof = Command::new(Origin::Rule, Op::Action { name: "lights.default".into(), args: Value::Null }).with_priority(Some(u16::MAX));
    assert!(ctl.handle("lights.default", &Value::Null, &spoof).is_err());
    assert_eq!(ctl.timer_data.len(), timers, "refusal cannot mutate the controller");

    for panic in [false, true, false] {
        if panic { act(&mut ctl, "lights.panic"); pump(&mut ctl, &mut core); assert!(ctl.panic_latched); }
        hub.command(Command::new(Origin::Deck, Op::Action { name: "lights.default".into(), args: Value::Null }));
        pump(&mut ctl, &mut core);
        assert!(ctl.layers.is_empty() && ctl.pending_layers.is_empty() && ctl.looks.is_empty());
        assert!(ctl.timer_data.is_empty() && ctl.timers.is_empty() && ctl.master_pending.is_empty());
        assert!(ctl.playbacks.values().all(|p| p.current.is_none() && p.applied.is_empty()));
        assert!(ctl.prog.selection.is_empty() && ctl.prog.values.is_empty() && ctl.prog.highlight.is_empty());
        assert!(!ctl.panic_latched);
        assert_eq!(core.get("lights.auto"), Some(&Value::Bool(false)));
        assert_eq!(core.get("lights.master"), Some(&Value::Float(1.0)));
        assert_eq!(core.get("lights.blackout"), Some(&Value::Bool(false)));
        assert_eq!(core.get("lights.effect.pulse.active"), Some(&Value::Bool(false)));
        assert_eq!(core.get("unrelated"), Some(&Value::Float(0.8)));
        assert_eq!(&engine.render(&hub.snapshot.load(), 2_000_000_000 + core.now()).universes[0][..3], &[77, 32, 54]);
        assert_eq!(engine.intended(), &[false], "idle uses the existing Main handoff, not authored output");
        assert!(!ctl.show.rig.output_armed, "output arming is unchanged");
        assert_eq!(ctl.show.rig.safety.max_intensity, 0.35);
    }
    // Stale tokens cannot issue a flash fade after default, and effects remain usable afterward.
    ctl.timer(Timer::FlashFade { addrs: vec!["lights.par.intensity".into()], ms: 100,
        token: 1, origin: Origin::Deck, actor: None, priority: PRIORITY_MANUAL });
    act(&mut ctl, "lights.effect.start pulse");
    pump(&mut ctl, &mut core);
    assert_eq!(core.get("lights.effect.pulse.active"), Some(&Value::Bool(true)));
}
