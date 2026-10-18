//! The live Omarchy theme, as Sealwax follows it: `~/.local/state/omarchy/current/theme/colors.toml`
//! sets libadwaita's colours and the week view's palette, Hyprland's `decoration:rounding` sets
//! every corner, and a change of either applies at once.

use gtk4::{gdk, gio, glib, prelude::*};
use libadwaita as adw;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

pub type Rgb = (f64, f64, f64);

#[derive(Clone, Debug)]
pub struct Palette {
    pub dark: bool,
    pub background: Rgb,
    pub surface: Rgb,
    pub foreground: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub now: Rgb,
    /// For calendars with no colour of their own, in turn.
    pub series: Vec<Rgb>,
    /// Surfaces' corner radius in px, Hyprland's rounding; controls take [`Palette::small_radius`].
    pub radius: f64,
}

impl Palette {
    pub fn fallback(dark: bool) -> Self {
        let hex = |text| parse_hex(text).expect("a built-in colour");
        if dark {
            Self {
                dark,
                background: hex("#242424"),
                surface: hex("#303030"),
                foreground: hex("#ffffff"),
                muted: hex("#9a9a9a"),
                accent: hex("#78aeed"),
                now: hex("#ff7b63"),
                series: ["#78aeed", "#8ff0a4", "#dc8add", "#f8e45c", "#99c1f1", "#ffbe6f"].map(hex).to_vec(),
                radius: 0.0,
            }
        } else {
            Self {
                dark,
                background: hex("#fafafb"),
                surface: hex("#ffffff"),
                foreground: hex("#1e1e1e"),
                muted: hex("#77767b"),
                accent: hex("#3584e4"),
                now: hex("#e01b24"),
                series: ["#3584e4", "#2ec27e", "#9141ac", "#e5a50a", "#1c71d8", "#e66100"].map(hex).to_vec(),
                radius: 0.0,
            }
        }
    }

    fn from_colors(colors: &BTreeMap<String, String>) -> Self {
        let dark = colors.get("mode").is_none_or(|mode| mode != "light");
        let base = Self::fallback(dark);
        let get = |key: &str, fallback: Rgb| colors.get(key).and_then(|v| parse_hex(v)).unwrap_or(fallback);
        let series: Vec<Rgb> = ["blue", "green", "magenta", "yellow", "cyan", "orange", "red"]
            .iter()
            .filter_map(|key| colors.get(*key).and_then(|v| parse_hex(v)))
            .collect();
        Self {
            dark,
            background: get("background", base.background),
            surface: get("lighter_background", base.surface),
            foreground: get("foreground", base.foreground),
            muted: get("muted", get("dark_foreground", base.muted)),
            accent: get("accent", base.accent),
            now: get("red", base.now),
            series: if series.is_empty() { base.series } else { series },
            radius: base.radius,
        }
    }

    /// Controls' radius: three quarters of the surfaces', as in Sealwax.
    pub fn small_radius(&self) -> f64 {
        small_radius(self.radius as u32) as f64
    }
}

/// The largest radius applied, in px (Sealwax's limit).
const MAX_RADIUS: u32 = 64;

fn small_radius(radius: u32) -> u32 {
    (radius * 3 + 2) / 4
}

/// Hyprland's `decoration:rounding` in px, as Sealwax and omarchy-shell read it; square (0) when
/// Hyprland isn't running or doesn't answer within 300 ms.
fn hyprland_rounding() -> u32 {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return 0;
    }
    let Ok(mut child) = Command::new("hyprctl")
        .args(["-j", "getoption", "decoration:rounding"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return 0;
    };
    let deadline = Instant::now() + Duration::from_millis(300);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!("hyprctl didn't answer; corners are square");
                return 0;
            }
        }
    }
    let mut json = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut out, &mut json);
    }
    parse_rounding(&json).unwrap_or(0).min(MAX_RADIUS)
}

/// The `int` field of `{"option": "decoration:rounding", "int": 8, "set": true }`.
fn parse_rounding(json: &str) -> Option<u32> {
    let after_key = &json[json.find("\"int\"")? + "\"int\"".len()..];
    let value = after_key.trim_start().strip_prefix(':')?.trim_start();
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    value[..digits].parse().ok()
}

/// Corners for libadwaita's widgets: surfaces take `radius`, controls three quarters of it, and
/// joined buttons keep square inner corners. Round things (switches, dots) go square at 0, as
/// Omarchy's do.
fn radius_css(radius: u32) -> String {
    let small = small_radius(radius);
    format!(
        "* {{ border-radius: {small}px; }}
window, window.csd, dialog.floating sheet, dialog > sheet, popover > contents, .card, toast,
list.boxed-list, list.boxed-list > row:first-child:last-child {{ border-radius: {radius}px; }}
list.boxed-list > row, list.boxed-list > row > box, spinbutton > button, banner > revealer > widget,
headerbar, .top-bar, .bottom-bar, scrolledwindow, undershoot, overshoot {{ border-radius: 0; }}
list.boxed-list > row:first-child {{ border-top-left-radius: {radius}px; border-top-right-radius: {radius}px; }}
list.boxed-list > row:last-child {{ border-bottom-left-radius: {radius}px; border-bottom-right-radius: {radius}px; }}
.linked > :not(:first-child) {{ border-top-left-radius: 0; border-bottom-left-radius: 0; }}
.linked > :not(:last-child) {{ border-top-right-radius: 0; border-bottom-right-radius: 0; }}
"
    )
}

pub fn parse_hex(text: &str) -> Option<Rgb> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() < 6 || !hex.is_char_boundary(6) {
        return None;
    }
    let channel = |range| u8::from_str_radix(&hex[range], 16).ok().map(|v| f64::from(v) / 255.0);
    Some((channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

fn omarchy_dir() -> PathBuf {
    let state = match std::env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => std::env::home_dir().unwrap_or_default().join(".local/state"),
    };
    state.join("omarchy/current")
}

/// `key = "value"` lines, leniently, as `omarchy-theme-color` reads them.
fn read_colors() -> Option<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(omarchy_dir().join("theme/colors.toml")).ok()?;
    Some(
        text.lines()
            .filter_map(|line| {
                let (key, value) = line.split_once('=')?;
                let key = key.trim().trim_matches('"');
                let value = value.trim().split(" #").next()?.trim().trim_matches(['"', '\'']);
                (!key.is_empty() && !key.starts_with('#')).then(|| (key.to_owned(), value.to_owned()))
            })
            .collect(),
    )
}

fn css(colors: &BTreeMap<String, String>) -> String {
    let pick = |keys: &[&str]| keys.iter().find_map(|key| colors.get(*key).filter(|v| parse_hex(v).is_some()));
    let pairs: [(&str, &[&str]); 12] = [
        ("accent_bg_color", &["accent"]),
        ("accent_color", &["accent"]),
        ("window_bg_color", &["background"]),
        ("window_fg_color", &["foreground"]),
        ("view_bg_color", &["background"]),
        ("view_fg_color", &["foreground"]),
        ("headerbar_bg_color", &["dark_background", "background"]),
        ("headerbar_fg_color", &["foreground"]),
        ("sidebar_bg_color", &["dark_background", "background"]),
        ("card_bg_color", &["lighter_background"]),
        ("dialog_bg_color", &["background"]),
        ("popover_bg_color", &["lighter_background", "background"]),
    ];
    let mut named = String::new();
    let mut variables = String::from(":root {\n");
    for (name, keys) in pairs {
        if let Some(value) = pick(keys) {
            named.push_str(&format!("@define-color {name} {value};\n"));
            variables.push_str(&format!("  --{}: {value};\n", name.replace('_', "-")));
        }
    }
    variables.push_str("}\n");
    named + &variables
}

/// Applies the theme now and whenever Omarchy switches it; calls `changed` with each palette.
pub fn follow(changed: impl Fn(Palette) + 'static) -> Palette {
    let provider = gtk4::CssProvider::new();
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(&display, &provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    let apply = move || {
        let manager = adw::StyleManager::default();
        let radius = hyprland_rounding();
        let mut palette = match read_colors() {
            Some(colors) => {
                let palette = Palette::from_colors(&colors);
                manager.set_color_scheme(if palette.dark {
                    adw::ColorScheme::ForceDark
                } else {
                    adw::ColorScheme::ForceLight
                });
                provider.load_from_string(&(css(&colors) + &radius_css(radius)));
                palette
            }
            None => {
                provider.load_from_string(&radius_css(radius));
                manager.set_color_scheme(adw::ColorScheme::Default);
                Palette::fallback(manager.is_dark())
            }
        };
        palette.radius = f64::from(radius);
        palette
    };
    let palette = apply();
    let changed = Rc::new(changed);
    let apply = Rc::new(apply);
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    // `omarchy-theme-set` replaces the `current/theme` directory and rewrites `theme.name`;
    // Hyprland reloads its config, rounding included, when a file in `~/.config/hypr` changes.
    let hypr = match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => std::env::home_dir().unwrap_or_default().join(".config"),
    }
    .join("hypr");
    let watched = [
        gio::File::for_path(omarchy_dir().join("theme.name")).monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE),
        gio::File::for_path(hypr).monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE),
    ];
    for monitor in watched.into_iter().flatten() {
        let (pending, changed, apply) = (Rc::clone(&pending), Rc::clone(&changed), Rc::clone(&apply));
        monitor.connect_changed(move |_, _, _, _| {
            if let Some(source) = pending.borrow_mut().take() {
                source.remove();
            }
            let (pending_inner, changed, apply) = (Rc::clone(&pending), Rc::clone(&changed), Rc::clone(&apply));
            // Let the theme swap or Hyprland's reload finish before reading.
            let source = glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                pending_inner.borrow_mut().take();
                changed(apply());
            });
            *pending.borrow_mut() = Some(source);
        });
        // The monitors must live as long as the app.
        std::mem::forget(monitor);
    }
    palette
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_omarchy_colours() {
        let colors: BTreeMap<String, String> = [("mode", "light"), ("accent", "#7daea3"), ("blue", "#112233")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let palette = Palette::from_colors(&colors);
        assert!(!palette.dark);
        assert_eq!(palette.series, vec![parse_hex("#112233").unwrap()]);
        assert!(css(&colors).contains("@define-color accent_bg_color #7daea3;"));
        assert_eq!(parse_hex("#ff0000"), Some((1.0, 0.0, 0.0)));
        assert_eq!(parse_hex("red"), None);
    }

    #[test]
    fn corners_follow_hyprland() {
        assert_eq!(parse_rounding(r#"{"option": "decoration:rounding", "int": 8, "set": true }"#), Some(8));
        assert_eq!(parse_rounding("garbage"), None);
        assert_eq!((small_radius(0), small_radius(8), small_radius(10)), (0, 6, 8));
        let square = radius_css(0);
        assert!(square.contains("* { border-radius: 0px; }"));
        assert!(!square.contains("border-radius: 1"));
        assert!(radius_css(8).contains("window, window.csd"));
    }
}
