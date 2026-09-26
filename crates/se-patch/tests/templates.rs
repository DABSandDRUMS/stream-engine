//! Every shipped template creates a valid patch; shader and particles templates compile
//! against the generated header exactly as the renderer assembles them.

use se_patch::templates;
use se_patch::wgsl::{Layout, particles_prelude};
use se_patch::{Kind, Manifest};
use std::path::PathBuf;

fn share() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn validate(name: &str, src: &str, entries: &[(&str, naga::ShaderStage)]) {
    let module = naga::front::wgsl::parse_str(src).unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(src)));
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(src)));
    for (entry, stage) in entries {
        assert!(module.entry_points.iter().any(|ep| ep.name == *entry && ep.stage == *stage), "{name}: missing {stage:?} entry `{entry}`");
    }
}

#[test]
fn every_template_creates_a_working_patch() {
    let all = templates::list(&share());
    for kind in templates::KINDS {
        assert!(all.iter().any(|t| t.kind == *kind && t.name == "default"), "no default template for {kind}");
    }
    let proj = tempfile::tempdir().unwrap();
    for t in &all {
        let id = format!("{}_{}", t.kind, t.name);
        let m: Manifest = templates::create(&share(), proj.path(), &id, &t.kind, &t.name).unwrap_or_else(|e| panic!("{}/{}: {e}", t.kind, t.name));
        assert_eq!(m.kind.as_str(), t.kind);
        assert!(!t.description.is_empty(), "{}/{} needs a description", t.kind, t.name);
        let dir = proj.path().join("patches").join(&id);
        // placeholders are all substituted
        for f in walk(&dir) {
            if let Ok(s) = std::fs::read_to_string(&f) {
                assert!(!s.contains("{{"), "{} keeps a placeholder", f.display());
            }
        }
        let layout = Layout::new(&m);
        match m.kind {
            Kind::Shader => {
                let src = layout.header() + &std::fs::read_to_string(m.entry_path()).unwrap();
                validate(&id, &src, &[("fs", naga::ShaderStage::Fragment), ("se_vs", naga::ShaderStage::Vertex)]);
            }
            Kind::Particles => {
                let p = m.particles.as_ref().unwrap();
                let sim = layout.header() + &particles_prelude(true, p.count) + &std::fs::read_to_string(dir.join(&p.sim)).unwrap();
                validate(&format!("{id}/sim"), &sim, &[("sim", naga::ShaderStage::Compute)]);
                let draw = layout.header() + &particles_prelude(false, p.count) + &std::fs::read_to_string(dir.join(&p.draw)).unwrap();
                validate(&format!("{id}/draw"), &draw, &[("vs", naga::ShaderStage::Vertex), ("fs", naga::ShaderStage::Fragment)]);
            }
            Kind::Web => {
                let html = std::fs::read_to_string(m.entry_path()).unwrap();
                assert!(html.contains("/engine.js") && html.contains("Engine.connect"), "{id}: web template must use engine.js");
            }
            Kind::Dsp => {
                // ships a prebuilt module + its source and build script (executable)
                let wasm = std::fs::read(m.entry_path()).unwrap();
                assert_eq!(&wasm[..4], b"\0asm", "{id}: main.wasm is not WebAssembly");
                use std::os::unix::fs::PermissionsExt;
                assert!(std::fs::metadata(dir.join("build.sh")).unwrap().permissions().mode() & 0o111 != 0, "{id}: build.sh must stay executable");
                assert_eq!(templates::edit_target(&m), dir.join("src/lib.rs"));
            }
            Kind::Script => assert!(m.entry_path().is_file()),
        }
    }
}

fn walk(d: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(d).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
