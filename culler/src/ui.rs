//! UI glue: pure view-model helpers + Slint model population. Wired by main (Task 11).

use crate::input::{Filter, passes};
use crate::pipeline::to_slint_image;
use culler_core::decode::DecodedImage;
use culler_core::model::{Decision, Session};
use slint::{ModelRc, VecModel};
use std::rc::Rc;

/// Filmstrip color bucket: 0 rest/grey, 1 keep/green, 2 pick/blue, 3 best/gold, 4 reject/red.
pub fn tier_color_code(d: &Decision) -> i32 {
    match d.tier {
        None => 0,
        Some(culler_core::model::Tier::Keep) => 1,
        Some(culler_core::model::Tier::Pick) => 2,
        Some(culler_core::model::Tier::Best) => 3,
        Some(culler_core::model::Tier::Reject) => 4,
    }
}

/// Unvisited residual shots render dimmer so progress is visible at a glance (§9).
pub fn dim_flag(d: &Decision) -> bool {
    d.tier.is_none() && !d.visited
}

/// Virtualized filmstrip window: indices (respecting `filter`) within `buffer` of current,
/// plus the offset of the current index inside that returned slice.
pub fn build_filmstrip_window(
    session: &Session,
    filter: Filter,
    buffer: usize,
) -> (Vec<usize>, usize) {
    let n = session.shots.len();
    if n == 0 {
        return (Vec::new(), 0);
    }
    // Collect all passing indices (cheap: it's a Vec walk, only the built VecModel is windowed).
    let passing: Vec<usize> = (0..n)
        .filter(|&i| passes(filter, session.decision(i)))
        .collect();
    if passing.is_empty() {
        return (Vec::new(), 0);
    }
    // Locate current (or nearest passing) in the passing list.
    let cur_pos = passing
        .iter()
        .position(|&i| i >= session.current)
        .unwrap_or(passing.len() - 1);
    let lo = cur_pos.saturating_sub(buffer);
    let hi = (cur_pos + buffer + 1).min(passing.len());
    let indices: Vec<usize> = passing[lo..hi].to_vec();
    let cur_off = cur_pos - lo;
    (indices, cur_off)
}

/// Rebuild the filmstrip VecModel to hold only the current window (virtualization).
/// `thumb_for` provides an already-decoded thumbnail per shot index (grey placeholder if
/// absent). It is `&mut dyn FnMut` (not `&dyn Fn`) so Task 11's caller closure can mutably
/// borrow an LRU cache while producing each thumbnail.
pub fn refresh_filmstrip(
    app: &crate::AppWindow,
    session: &Session,
    filter: Filter,
    buffer: usize,
    thumb_for: &mut dyn FnMut(usize) -> slint::Image,
) {
    let (indices, cur_off) = build_filmstrip_window(session, filter, buffer);
    let mut items: Vec<crate::FilmstripItem> = Vec::with_capacity(indices.len());
    for i in indices {
        let d = session.decision(i);
        items.push(crate::FilmstripItem {
            thumb: thumb_for(i),
            color_code: tier_color_code(d),
            dim: dim_flag(d),
        });
    }
    let model = Rc::new(VecModel::from(items));
    app.set_film_items(ModelRc::from(model));
    app.set_film_current(cur_off as i32);
}

/// Number of luma-histogram bars in the bottom-left HUD panel (DESIGN §4 1b).
pub const HISTOGRAM_BINS: usize = 30;

/// Marshal a decoded fit image onto the loupe (called from the event loop).
pub fn set_loupe(app: &crate::AppWindow, img: &DecodedImage) {
    app.set_current_image(to_slint_image(img));
    // Luma histogram (DESIGN §4 1b): recomputed from the same decoded buffer
    // we're displaying, so it always tracks the shot on screen. Cheap —
    // `luma_histogram` samples at most ~200k px — and set_loupe fires only for
    // the current shot (fresh delivery or cached repaint), not for prefetched
    // neighbors. Pushed as normalized [0,1] bar heights the .slint scales.
    let hist = culler_core::histogram::luma_histogram(img, HISTOGRAM_BINS);
    app.set_histogram(ModelRc::from(Rc::new(VecModel::from(hist))));
}

/// Autocomplete rows for the tag entry (DESIGN §4 2f). Given the global
/// tag→count list (`Session::tag_counts`, already count-desc), returns rows
/// matching `prefix` (case-insensitive), or **all** tags when `prefix` is empty
/// ("show all by default"). Tags already committed earlier in the same entry
/// (`exclude`) and an exact-prefix match are dropped. Each row is
/// `(matched-prefix, remainder, count)` so the popup can bold the prefix and
/// right-align the count; the show-all case yields an empty prefix. Capped for a
/// tidy popup.
pub fn suggest_tags_counted(
    counts: &[(String, usize)],
    prefix: &str,
    exclude: &[String],
) -> Vec<(String, String, i32)> {
    let p = prefix.trim().to_lowercase();
    let plen = prefix.trim().chars().count();
    counts
        .iter()
        .filter(|(t, _)| {
            if exclude.iter().any(|e| e.eq_ignore_ascii_case(t)) {
                return false;
            }
            if p.is_empty() {
                true
            } else {
                let lt = t.to_lowercase();
                lt.starts_with(&p) && lt != p
            }
        })
        .take(8)
        .map(|(t, c)| {
            let pre: String = t.chars().take(plen).collect();
            let rest: String = t.chars().skip(plen).collect();
            (pre, rest, *c as i32)
        })
        .collect()
}

/// Split tag-entry text into (the segment currently being typed, the tags
/// already committed before it). The last comma-separated field is the live
/// segment feeding the autocomplete; earlier non-empty fields are tags already
/// entered (excluded from suggestions).
pub fn tag_segments(text: &str) -> (String, Vec<String>) {
    let mut parts: Vec<&str> = text.split(',').collect();
    let last = parts.pop().unwrap_or("").trim().to_string();
    let exclude: Vec<String> = parts
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (last, exclude)
}

/// Pre-rendered HUD strings (DESIGN.md §4 screen 1b: top-left filename/RAW/position,
/// top-right counts pill/tier badge, bottom-left tags/filter/progress).
pub struct HudText {
    pub tier: String,
    pub tags: String,
    pub counts: String,
    pub progress: String,
    pub filter_label: String,
    /// Current shot's display (jpeg) file name, lossy-decoded. Empty for an empty session.
    pub filename: String,
    /// Whether the current shot has a RAW sibling (drives the top-left RAW badge).
    pub has_raw: bool,
    /// "{current+1}/{len}", 1-based for display; "0/0" when the session has no shots.
    pub position: String,
    /// EXIF line "1/250s · ƒ2.8 · ISO 400 · 85mm" (DESIGN §4 1b); empty when the
    /// current shot carries no exposure EXIF.
    pub exif: String,
}

pub fn hud_text(session: &Session, filter: Filter) -> HudText {
    let d = session.decision(session.current);
    let tier = match d.tier {
        None => "Rest".to_string(),
        Some(culler_core::model::Tier::Keep) => "Keep".to_string(),
        Some(culler_core::model::Tier::Pick) => "Pick".to_string(),
        Some(culler_core::model::Tier::Best) => "Best".to_string(),
        Some(culler_core::model::Tier::Reject) => "Reject".to_string(),
    };
    let c = session.counts();
    let counts = format!(
        "reject {}  rest {}  keep {}  pick {}  best {}",
        c.rejected, c.rest, c.keep, c.picks, c.bests
    );
    let progress = format!("seen {}/{}", session.visited_count(), session.shots.len());
    let filter_label = match filter {
        Filter::All => "filter: All",
        Filter::Keep => "filter: >=Keep",
        Filter::Pick => "filter: >=Pick",
        Filter::Best => "filter: >=Best",
        Filter::Rejects => "filter: Rejects",
    }
    .to_string();
    let current_shot = session.shots.get(session.current);
    let filename = current_shot
        .and_then(|s| s.jpeg.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let has_raw = current_shot.is_some_and(|s| s.raw.is_some());
    let position = if session.shots.is_empty() {
        "0/0".to_string()
    } else {
        format!("{}/{}", session.current + 1, session.shots.len())
    };
    let exif = current_shot
        .and_then(|s| s.exif.as_ref())
        .map(|e| e.hud_line())
        .unwrap_or_default();
    HudText {
        tier,
        tags: d.tags.join(", "),
        counts,
        progress,
        filter_label,
        filename,
        has_raw,
        position,
        exif,
    }
}

/// Sticky loupe zoom/pan. Both `zoomed` and pan persist across prev/next so the
/// same crop can be compared through a burst (§9).
#[derive(Clone, Copy, Debug, Default)]
pub struct ZoomState {
    pub zoomed: bool,
    // Exercised by `zoom_tests` (pan persists across toggle/navigate); the
    // live pan offset is actually owned by app.slint's own two-way-bound
    // `pan-x`/`pan-y` properties (see Loupe's drag gesture), so these fields
    // are unread by production Rust code.
    #[allow(dead_code)]
    pub pan_x: f32,
    #[allow(dead_code)]
    pub pan_y: f32,
}

impl ZoomState {
    /// `Z`: flip 1:1 zoom, leaving pan untouched.
    pub fn toggle(&mut self) {
        self.zoomed = !self.zoomed;
    }

    /// Navigating to another shot keeps zoom + pan exactly as they were.
    pub fn on_navigate(&mut self) {
        // Intentionally a no-op: persistence IS the behavior. Kept explicit so a
        // future "reset pan on navigate" option has an obvious home.
    }

    /// Which decode target the loupe needs right now.
    pub fn target(&self, fit_w: u32, fit_h: u32) -> culler_core::decode::TargetSize {
        if self.zoomed {
            culler_core::decode::TargetSize::Full // bypasses the LRU cache (dedicated slot)
        } else {
            culler_core::decode::TargetSize::Fit(fit_w, fit_h)
        }
    }
}

#[cfg(test)]
mod hud_tests {
    use super::*;
    use crate::input::Filter;
    use culler_core::model::{CaptureTime, Decision, Session, Shot, Tier};

    fn mk(tiers: &[Option<Tier>]) -> Session {
        let mut shots = Vec::new();
        let mut decisions = std::collections::HashMap::new();
        for (i, t) in tiers.iter().enumerate() {
            let stem = format!("IMG_{i:04}");
            shots.push(Shot {
                stem: stem.clone(),
                jpeg: format!("/s/{stem}.JPG").into(),
                raw: None,
                sidecar: None,
                capture: CaptureTime::default(),
                exif: None,
            });
            decisions.insert(
                stem,
                Decision {
                    tier: *t,
                    tags: vec![],
                    visited: t.is_some(),
                },
            );
        }
        Session {
            source_dir: "/s".into(),
            shots,
            decisions,
            current: 0,
            pending_apply: None,
            undo: Vec::new(),
        }
    }

    #[test]
    fn suggest_tags_counted_filters_shows_all_and_splits_prefix() {
        let counts = vec![
            ("sky".to_string(), 142usize),
            ("skyline".to_string(), 12),
            ("sea".to_string(), 30),
            ("Sunset".to_string(), 7),
        ];
        // prefix filter (case-insensitive), each row split into (prefix, rest, count)
        assert_eq!(
            suggest_tags_counted(&counts, "sk", &[]),
            vec![
                ("sk".to_string(), "y".to_string(), 142),
                ("sk".to_string(), "yline".to_string(), 12),
            ]
        );
        // case-insensitive match preserves the tag's own casing in the split
        assert_eq!(
            suggest_tags_counted(&counts, "su", &[]),
            vec![("Su".to_string(), "nset".to_string(), 7)]
        );
        // empty prefix -> ALL tags in the given (count-desc) order, nothing bolded
        let all = suggest_tags_counted(&counts, "", &[]);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0], ("".to_string(), "sky".to_string(), 142));
        // exact-prefix match is not re-suggested; excluded tags are dropped
        assert!(
            suggest_tags_counted(&counts, "sky", &[])
                .iter()
                .all(|(p, r, _)| format!("{p}{r}") != "sky")
        );
        assert!(
            suggest_tags_counted(&counts, "", &["sky".to_string()])
                .iter()
                .all(|(p, r, _)| format!("{p}{r}") != "sky")
        );
    }

    #[test]
    fn tag_segments_splits_current_from_committed() {
        // typing the first tag: no committed tags yet
        assert_eq!(tag_segments("sk"), ("sk".to_string(), vec![]));
        // trailing comma+space -> empty live segment (show-all), one committed
        assert_eq!(
            tag_segments("street, "),
            ("".to_string(), vec!["street".to_string()])
        );
        // mid-typing a later tag
        assert_eq!(
            tag_segments("street, maria, mar"),
            ("mar".to_string(), vec!["street".to_string(), "maria".to_string()])
        );
        assert_eq!(tag_segments(""), ("".to_string(), vec![]));
    }

    #[test]
    fn hud_text_reports_tier_counts_and_progress() {
        let s = mk(&[Some(Tier::Keep), Some(Tier::Reject), None]);
        let h = hud_text(&s, Filter::All);
        assert_eq!(h.tier, "Keep"); // current @0
        assert!(h.counts.contains("keep 1"));
        assert!(h.counts.contains("reject 1"));
        assert!(h.progress.contains("seen 2/3")); // two tiered => visited
        assert_eq!(h.filter_label, "filter: All");
        assert_eq!(h.filename, "IMG_0000.JPG");
        assert!(!h.has_raw);
        assert_eq!(h.position, "1/3");
    }

    #[test]
    fn hud_text_shows_rest_for_undecided_current() {
        let mut s = mk(&[None, None]);
        s.current = 0;
        let h = hud_text(&s, Filter::Keep);
        assert_eq!(h.tier, "Rest");
        assert_eq!(h.filter_label, "filter: >=Keep");
    }

    #[test]
    fn hud_text_empty_session_is_safe() {
        let s = Session {
            source_dir: "/s".into(),
            shots: Vec::new(),
            decisions: std::collections::HashMap::new(),
            current: 0,
            pending_apply: None,
            undo: Vec::new(),
        };
        let h = hud_text(&s, Filter::All);
        assert_eq!(h.position, "0/0");
        assert_eq!(h.tier, "Rest");
    }
}

#[cfg(test)]
mod color_tests {
    use super::*;
    use crate::input::Filter;
    use culler_core::model::{CaptureTime, Decision, Session, Shot, Tier};

    #[test]
    fn tier_color_code_maps_every_tier() {
        assert_eq!(tier_color_code(&Decision::default()), 0); // rest/grey
        assert_eq!(
            tier_color_code(&Decision {
                tier: Some(Tier::Keep),
                ..Default::default()
            }),
            1
        );
        assert_eq!(
            tier_color_code(&Decision {
                tier: Some(Tier::Pick),
                ..Default::default()
            }),
            2
        );
        assert_eq!(
            tier_color_code(&Decision {
                tier: Some(Tier::Best),
                ..Default::default()
            }),
            3
        );
        assert_eq!(
            tier_color_code(&Decision {
                tier: Some(Tier::Reject),
                ..Default::default()
            }),
            4
        );
    }

    #[test]
    fn only_unvisited_rest_is_dim() {
        assert!(dim_flag(&Decision {
            tier: None,
            visited: false,
            ..Default::default()
        }));
        assert!(!dim_flag(&Decision {
            tier: None,
            visited: true,
            ..Default::default()
        }));
        assert!(!dim_flag(&Decision {
            tier: Some(Tier::Keep),
            visited: false,
            ..Default::default()
        }));
    }

    fn mk(n: usize) -> Session {
        let mut shots = Vec::new();
        let mut decisions = std::collections::HashMap::new();
        for i in 0..n {
            let stem = format!("IMG_{i:04}");
            shots.push(Shot {
                stem: stem.clone(),
                jpeg: format!("/s/{stem}.JPG").into(),
                raw: None,
                sidecar: None,
                capture: CaptureTime::default(),
                exif: None,
            });
            decisions.insert(stem, Decision::default());
        }
        Session {
            source_dir: "/s".into(),
            shots,
            decisions,
            current: 0,
            pending_apply: None,
            undo: Vec::new(),
        }
    }

    #[test]
    fn filmstrip_window_is_buffered_and_reports_current_offset() {
        let mut s = mk(100);
        s.current = 50;
        let (indices, cur_off) = build_filmstrip_window(&s, Filter::All, 5);
        assert_eq!(indices, vec![45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55]);
        assert_eq!(indices[cur_off], 50);
    }

    #[test]
    fn filmstrip_window_clamps_at_edges() {
        let mut s = mk(4);
        s.current = 0;
        let (indices, cur_off) = build_filmstrip_window(&s, Filter::All, 5);
        assert_eq!(indices, vec![0, 1, 2, 3]);
        assert_eq!(cur_off, 0);
    }

    #[test]
    fn filmstrip_window_respects_filter() {
        // Only even indices are Keep; a >=Keep filter keeps only those in the window.
        let mut s = mk(10);
        for i in (0..10).step_by(2) {
            let stem = format!("IMG_{i:04}");
            s.decisions.get_mut(&stem).unwrap().tier = Some(Tier::Keep);
        }
        s.current = 4;
        let (indices, cur_off) = build_filmstrip_window(&s, Filter::Keep, 5);
        assert_eq!(indices, vec![0, 2, 4, 6, 8]);
        assert_eq!(indices[cur_off], 4);
    }
}

#[cfg(test)]
mod zoom_tests {
    use super::*;

    #[test]
    fn toggle_flips_zoom_but_keeps_pan() {
        let mut z = ZoomState::default();
        assert!(!z.zoomed);
        z.pan_x = 120.0;
        z.pan_y = -40.0;
        z.toggle();
        assert!(z.zoomed);
        // toggling zoom must NOT reset the pan
        assert_eq!(z.pan_x, 120.0);
        assert_eq!(z.pan_y, -40.0);
    }

    #[test]
    fn navigation_preserves_zoom_and_pan() {
        let mut z = ZoomState {
            zoomed: true,
            pan_x: 200.0,
            pan_y: 55.0,
        };
        z.on_navigate(); // moving to another shot
        assert!(z.zoomed); // still zoomed
        assert_eq!(z.pan_x, 200.0); // pan sticky across prev/next
        assert_eq!(z.pan_y, 55.0);
    }
}
