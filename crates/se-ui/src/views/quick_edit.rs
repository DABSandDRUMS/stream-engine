//! New quick effect files made from other pages (Lights → "Put on a pad"). Quick effects are
//! made by developers; the operator only turns their knobs (the `preset.knob` action writes
//! those) — so this only knows the one shape the UI still creates and how to name its file.

use toml_edit::Value;

/// A quick effect that holds a light look until it's pressed again. It follows later edits of
/// the look.
pub fn look_preset(label: &str, look: &str) -> String {
    format!("# Made in Stream Engine from the Lights page.\nlabel = {}\ntoggle = true\nlights = {{ look = {} }}\n", Value::from(label), Value::from(look))
}

/// A file name for a new quick effect that no existing one uses: `Glitch hit` → `glitch_hit`,
/// then `glitch_hit_2`, … (`taken` = existing ids).
pub fn free_id(label: &str, taken: &[String]) -> String {
    let base = crate::views::scene_edit::slug(label);
    let base = if base.is_empty() { "effect".to_string() } else { base };
    if !taken.contains(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}_{n}")).find(|c| !taken.iter().any(|t| t == c)).expect("unbounded")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_look_quick_effect_is_a_valid_toggle_holding_the_look() {
        let t: toml::Table = toml::from_str(&look_preset("Warm \"stage\"", "warm")).expect("the engine can read it");
        let look: se_core::config::PresetDef = toml::Value::Table(t).try_into().expect("a valid quick effect");
        assert!(look.toggle);
        assert_eq!(look.label.as_deref(), Some("Warm \"stage\""));
        assert_eq!(look.lights.and_then(|l| l.look).as_deref(), Some("warm"));
    }

    #[test]
    fn new_names_never_collide() {
        let taken = vec!["glitch_hit".to_string(), "glitch_hit_2".to_string()];
        assert_eq!(free_id("Glitch hit", &taken), "glitch_hit_3");
        assert_eq!(free_id("Flash!", &taken), "flash");
        assert_eq!(free_id("!!", &taken), "effect");
    }
}
