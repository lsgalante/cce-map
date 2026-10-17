//! Where the map was, so it comes back there: the centre and zoom are saved
//! when the window closes (`on_exit` — which a restart, the remote's "Restart
//! focused app" or `ccectl restart-app`, runs as any close does), and a launch
//! opens on them — after a restart, at login, or when reopened.
//!
//! **A restart replays the same command line**, so the file also records what
//! the command line asked for (`launched`): the same request again, or none,
//! is this window reopening, and the saved view wins; a different one is a new
//! request, and it wins. The map takes no arguments today, so `launched` is
//! always null — the rule is here so one added later (a place to open on) is
//! not overridden by a stale file.
//!
//! `cce_state_dir()/map/state.json`, written whole to a temporary file and
//! renamed over, so a crash mid-write leaves the last one.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Saved {
    /// World coords of the window centre, as `MapApp::center`.
    center: (f64, f64),
    zoom: f64,
    /// What the command line asked for when this was saved, if anything.
    launched: Option<String>,
}

fn path() -> PathBuf {
    cce_ui::config::cce_state_dir().join("map").join("state.json")
}

/// The (centre, zoom) to open on: the saved view unless this launch asks for
/// something other than what it was saved under; else none (the default).
pub fn initial(max_zoom: f64) -> Option<((f64, f64), f64)> {
    let saved = std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str::<Saved>(&s).ok());
    choose(None, saved.as_ref(), max_zoom)
}

/// A view off the world or out of the zoom range (a hand-edited or damaged
/// file) is no view at all.
fn choose(launched: Option<&str>, saved: Option<&Saved>, max_zoom: f64) -> Option<((f64, f64), f64)> {
    let s = saved.filter(|s| launched.is_none() || s.launched.as_deref() == launched)?;
    let ((u, v), zoom) = (s.center, s.zoom);
    let on_map = (0.0..1.0).contains(&u) && (0.0..=1.0).contains(&v) && (0.0..=max_zoom).contains(&zoom);
    on_map.then_some(((u, v), zoom))
}

/// The window is closing.
pub fn save(center: (f64, f64), zoom: f64) {
    let saved = Saved { center, zoom, launched: None };
    let path = path();
    let Some(dir) = path.parent() else { return };
    let tmp = path.with_extension("json.tmp");
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&tmp, serde_json::to_vec(&saved).unwrap_or_default()))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        log::warn!("[map] cannot save the view to {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(center: (f64, f64), zoom: f64, launched: Option<&str>) -> Saved {
        Saved { center, zoom, launched: launched.map(Into::into) }
    }

    #[test]
    fn the_same_command_line_or_none_reopens_on_the_saved_view() {
        let view = ((0.52, 0.34), 11.5);
        assert_eq!(choose(None, Some(&saved(view.0, view.1, None)), 19.0), Some(view));
        assert_eq!(choose(Some("x"), Some(&saved(view.0, view.1, Some("x"))), 19.0), Some(view));
        assert_eq!(choose(None, Some(&saved(view.0, view.1, Some("x"))), 19.0), Some(view));
    }

    #[test]
    fn a_different_request_or_nothing_usable_opens_on_the_default() {
        // The command line asks for something else: it wins.
        assert_eq!(choose(Some("y"), Some(&saved((0.5, 0.5), 3.0, Some("x"))), 19.0), None);
        assert_eq!(choose(Some("y"), Some(&saved((0.5, 0.5), 3.0, None)), 19.0), None);
        // Nothing saved, or a view that is not on the map.
        assert_eq!(choose(None, None, 19.0), None);
        assert_eq!(choose(None, Some(&saved((1.5, 0.5), 3.0, None)), 19.0), None);
        assert_eq!(choose(None, Some(&saved((0.5, 0.5), 25.0, None)), 19.0), None);
        assert_eq!(choose(None, Some(&saved((f64::NAN, 0.5), 3.0, None)), 19.0), None);
    }
}
