//! UI glue: pure view-model helpers + Slint model population. Wired by main (Task 11).
#![allow(dead_code)] // TODO(Task 11): remove once main wires the event loop

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
            selected: i == session.current,
        });
    }
    let model = Rc::new(VecModel::from(items));
    app.set_film_items(ModelRc::from(model));
    app.set_film_current(cur_off as i32);
}

/// Marshal a decoded fit image onto the loupe (called from the event loop).
pub fn set_loupe(app: &crate::AppWindow, img: &DecodedImage) {
    app.set_current_image(to_slint_image(img));
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
