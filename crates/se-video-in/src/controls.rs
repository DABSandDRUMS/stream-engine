//! Camera controls as addressable parameters (`source.<name>.ctrl.<control>`): metadata with
//! the device's real ranges, and conversion between engine values and device values.

use se_devices::v4l2::{Control, ControlKind};
use se_proto::{Meta, Value};

/// Controls we expose (buttons and compound controls are not parameters).
pub fn exposed(c: &Control) -> bool {
    matches!(c.kind, ControlKind::Integer | ControlKind::Integer64 | ControlKind::Boolean | ControlKind::Menu | ControlKind::IntegerMenu)
}

/// Snap to the control's step grid and clamp into its range.
pub fn snap(c: &Control, v: i64) -> i64 {
    let v = v.clamp(c.min, c.max);
    let step = c.step.max(1);
    let snapped = c.min + ((v - c.min) as f64 / step as f64).round() as i64 * step;
    if snapped > c.max { snapped - step } else { snapped }
}

/// Device value → engine value.
pub fn from_device(c: &Control, raw: i64) -> Value {
    match c.kind {
        ControlKind::Boolean => Value::Bool(raw != 0),
        ControlKind::Menu | ControlKind::IntegerMenu => match c.menu.iter().find(|m| m.index == raw) {
            Some(m) => Value::Str(m.name.clone()),
            None => Value::Int(raw),
        },
        _ => Value::Int(raw),
    }
}

/// Engine value → device value, or None if it doesn't fit this control.
///
/// Menus accept the option name (`manual_mode`), its label (`Manual Mode`), a unique name
/// prefix (`manual`), or the raw menu index; integers accept numbers and numeric strings;
/// booleans accept anything truthy.
pub fn to_device(c: &Control, v: &Value) -> Option<i64> {
    match c.kind {
        ControlKind::Boolean => match v {
            Value::Str(s) => match s.to_ascii_lowercase().as_str() {
                "on" | "true" | "yes" | "1" => Some(1),
                "off" | "false" | "no" | "0" => Some(0),
                _ => None,
            },
            Value::Null => None,
            other => Some(other.truthy() as i64),
        },
        ControlKind::Menu | ControlKind::IntegerMenu => match v {
            Value::Str(s) => {
                let l = s.to_ascii_lowercase();
                if let Some(m) = c.menu.iter().find(|m| m.name == l || m.label.eq_ignore_ascii_case(s)) {
                    return Some(m.index);
                }
                let mut pre = c.menu.iter().filter(|m| m.name.starts_with(&l) && m.name.as_bytes().get(l.len()) == Some(&b'_'));
                if let (Some(m), None) = (pre.next(), pre.next()) {
                    return Some(m.index);
                }
                s.trim().parse::<i64>().ok().filter(|i| c.menu.iter().any(|m| m.index == *i))
            }
            other => {
                let i = other.as_f64()?.round() as i64;
                c.menu.iter().any(|m| m.index == i).then_some(i)
            }
        },
        _ => {
            let f = match v {
                Value::Str(s) => s.trim().parse::<f64>().ok()?,
                other => other.as_f64()?,
            };
            if !f.is_finite() {
                return None;
            }
            Some(snap(c, f.round() as i64))
        }
    }
}

/// Metadata for `source.<n>.ctrl.<control>`; `initial` (from `[controls]`) becomes the default
/// when valid, else the device default.
pub fn meta(c: &Control, initial: Option<&Value>, owner: &str) -> Meta {
    let default_raw = initial.and_then(|v| to_device(c, v)).unwrap_or(c.default);
    let default = from_device(c, default_raw);
    let mut m = match c.kind {
        ControlKind::Boolean => Meta::boolean(default.truthy()),
        ControlKind::Menu | ControlKind::IntegerMenu => {
            let opts: Vec<&str> = c.menu.iter().map(|m| m.name.as_str()).collect();
            let d = default.as_str().map(str::to_string).unwrap_or_default();
            Meta::enumeration(&d, &opts)
        }
        _ => Meta::int(default_raw, [c.min as f64, c.max as f64]),
    };
    m = m.owner(owner).describe(&c.label);
    if c.read_only() {
        m = m.readonly();
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_devices::v4l2::MenuItem;

    fn int(min: i64, max: i64, step: i64, default: i64) -> Control {
        Control {
            id: 1,
            name: "brightness".into(),
            label: "Brightness".into(),
            kind: ControlKind::Integer,
            min,
            max,
            step,
            default,
            flags: 0,
            menu: vec![],
            value: Some(default),
        }
    }

    fn auto_exposure() -> Control {
        Control {
            id: 2,
            name: "auto_exposure".into(),
            label: "Auto Exposure".into(),
            kind: ControlKind::Menu,
            min: 0,
            max: 3,
            step: 1,
            default: 3,
            flags: 0,
            menu: vec![
                MenuItem { index: 1, label: "Manual Mode".into(), name: "manual_mode".into() },
                MenuItem { index: 3, label: "Aperture Priority Mode".into(), name: "aperture_priority_mode".into() },
            ],
            value: Some(3),
        }
    }

    #[test]
    fn integer_ranges_clamp_and_snap_to_step() {
        let c = int(-64, 64, 1, 0);
        assert_eq!(to_device(&c, &Value::Int(10)), Some(10));
        assert_eq!(to_device(&c, &Value::Float(10.6)), Some(11));
        assert_eq!(to_device(&c, &Value::Int(500)), Some(64));
        assert_eq!(to_device(&c, &Value::Int(-500)), Some(-64));
        assert_eq!(to_device(&c, &Value::Str(" 7 ".into())), Some(7));
        assert_eq!(to_device(&c, &Value::Str("x".into())), None);
        let stepped = int(2800, 6500, 50, 4600);
        assert_eq!(to_device(&stepped, &Value::Int(4624)), Some(4600));
        assert_eq!(to_device(&stepped, &Value::Int(4626)), Some(4650));
        assert_eq!(to_device(&stepped, &Value::Int(6499)), Some(6500));
        // a max that isn't on the grid never snaps past it
        let odd = int(0, 10, 4, 0);
        assert_eq!(snap(&odd, 10), 8);
        assert_eq!(snap(&odd, 9), 8);
    }

    #[test]
    fn integer_meta_uses_device_range_and_config_default() {
        let c = int(-64, 64, 1, 0);
        let m = meta(&c, Some(&Value::Int(20)), "video-in");
        assert_eq!(m.range, Some([-64.0, 64.0]));
        assert_eq!(m.default, Value::Int(20));
        assert_eq!(m.description.as_deref(), Some("Brightness"));
        // invalid initial value falls back to the device default
        assert_eq!(meta(&c, Some(&Value::Str("loud".into())), "video-in").default, Value::Int(0));
        assert_eq!(meta(&c, None, "video-in").default, Value::Int(0));
        // coercion through the metadata keeps values in range
        assert_eq!(m.coerce(&Value::Int(99)), Value::Int(64));
    }

    #[test]
    fn menus_map_names_labels_prefixes_and_indices() {
        let c = auto_exposure();
        assert_eq!(to_device(&c, &Value::Str("manual_mode".into())), Some(1));
        assert_eq!(to_device(&c, &Value::Str("Aperture Priority Mode".into())), Some(3));
        assert_eq!(to_device(&c, &Value::Str("manual".into())), Some(1));
        assert_eq!(to_device(&c, &Value::Int(3)), Some(3));
        assert_eq!(to_device(&c, &Value::Int(2)), None, "index 2 is not offered by the device");
        assert_eq!(from_device(&c, 1), Value::Str("manual_mode".into()));
        let m = meta(&c, Some(&Value::Str("manual".into())), "video-in");
        assert_eq!(m.options, vec!["manual_mode", "aperture_priority_mode"]);
        assert_eq!(m.default, Value::Str("manual_mode".into()));
    }

    #[test]
    fn booleans() {
        let c = Control { kind: ControlKind::Boolean, min: 0, max: 1, default: 1, ..int(0, 1, 1, 1) };
        assert_eq!(to_device(&c, &Value::Bool(false)), Some(0));
        assert_eq!(to_device(&c, &Value::Str("on".into())), Some(1));
        assert_eq!(to_device(&c, &Value::Int(0)), Some(0));
        assert_eq!(to_device(&c, &Value::Null), None);
        assert_eq!(meta(&c, None, "x").default, Value::Bool(true));
        assert_eq!(from_device(&c, 0), Value::Bool(false));
    }

    #[test]
    fn read_only_controls_are_readonly_params() {
        let mut c = int(0, 10, 1, 0);
        c.flags = se_devices::v4l2::sys::V4L2_CTRL_FLAG_READ_ONLY;
        assert!(meta(&c, None, "x").readonly);
        let mut b = c.clone();
        b.kind = ControlKind::Button;
        assert!(!exposed(&b));
    }
}
