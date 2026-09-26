//! Prints the Markdown effect reference (`docs/audio-effects.md`) from the registry.
//! `cargo run -p se-dsp --example fx_reference > docs/audio-effects.md`

use se_dsp::registry::{default_mix, describe};
use se_dsp::{ParamKind, create, kinds, params_of};

fn main() {
    println!("# Audio effect reference\n");
    println!("Generated from the `se-dsp` registry — regenerate with");
    println!("`cargo run -p se-dsp --example fx_reference > docs/audio-effects.md`.\n");
    println!("Use a kind in an effect chain (`{{ name = \"x\", kind = \"<kind>\", <param> = … }}`, see");
    println!("[audio.md](audio.md)); every parameter is live at `audio.bus.<bus>.fx.<name>.<param>`.");
    println!("Effects output the processed signal; the slot mixes it with the dry signal using `wet`/`dry`.\n");
    println!("| Kind | Description | Default wet/dry | Latency @ 48 kHz |\n|---|---|---|---|");
    for k in kinds() {
        let (w, d) = default_mix(k);
        let lat = create(k, 48000.0).map(|f| f.latency()).unwrap_or(0);
        println!("| `{k}` | {} | {w} / {d} | {} |", describe(k), if lat == 0 { "0".to_string() } else { format!("{lat} samples") });
    }
    for k in kinds() {
        println!("\n## `{k}`\n\n{}\n", describe(k));
        println!("| Param | Range | Default | Description |\n|---|---|---|---|");
        for p in params_of(k).unwrap() {
            let (range, def) = match p.kind {
                ParamKind::Float => (format!("{} … {} {}", p.min, p.max, p.unit).trim_end().to_string(), format!("{}", p.default)),
                ParamKind::Bool => ("true / false".to_string(), format!("{}", p.default >= 0.5)),
                ParamKind::Choice(o) => (o.iter().map(|x| format!("`{x}`")).collect::<Vec<_>>().join(" "), format!("`{}`", o[p.default as usize])),
            };
            println!("| `{}` | {range} | {def} | {} |", p.name, p.description);
        }
    }
}
