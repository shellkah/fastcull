slint::include_modules!();

mod applyflow; // Task 12
mod input;
mod pipeline;
mod startup;
mod ui;

// Bundled IBM Plex fonts (culler/ui/fonts/) need no registration call here: Slint
// 1.17 has no public `register_font_from_path`/`register_font_from_data` free
// function (only a per-renderer trait method used internally by the compiler).
// The supported mechanism is compile-time — `theme.slint` does `import
// "fonts/IBMPlexMono-Regular.ttf";` (etc.) for all 6 weights, and slint-build's
// default `EmbedResourcesKind::EmbedAllResources` embeds the bytes and emits a
// `RegisterCustomFontByMemory` call into the generated component's init code
// automatically, before the window is constructed. See task-1b-report.md.

use clap::Parser;
use culler_core::decode::TargetSize;
use culler_core::model::SESSION_FILE;
use culler_core::persist::{load_or_fresh, save};
use culler_core::scan::scan;
use input::{
    Action, Filter, InputContext, apply_action, key_to_action, next_filter, parse_tags, to_key,
};
use pipeline::{FullSlot, Pipeline, prefetch_set, to_slint_image};
use startup::{
    BreadcrumbProbe, Cli, find_crashed_apply, journal_report, probe_breadcrumb, reattach,
    resolve_buckets, validate_buckets,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

const PREFETCH_N: usize = 4;
const FILMSTRIP_BUFFER: usize = 8;
const CACHE_BUDGET: usize = 512 * 1024 * 1024; // 512 MB of fit-size textures

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let source = cli.source.clone();
    let auto_advance = !cli.no_auto_advance;
    let buckets = resolve_buckets(&cli);
    if let Err(msg) = validate_buckets(&buckets) {
        eprintln!("{msg}");
        return Err(msg.into());
    }

    // Startup: load-or-fresh, always rescan, reattach by stem.
    let prev = load_or_fresh(&source)?; // corrupt -> .bad + Ok(None), reported by core
    let scanned = scan(&source)?;
    let mut session = reattach(&source, scanned, prev);

    // Crash detection (spec §6 rev 3): the session breadcrumb points at the
    // in-flight dest of a crashed apply — that is what makes "detected on next
    // launch" work, since the journal lives in DEST while launches open SOURCE.
    // A breadcrumb whose dest carries no journal is stale (crash before the
    // first journal write, or a vanished dest) and is cleared rather than
    // haunting every future launch. UI surfaces resume-or-report (Task 12
    // dialog / Task 13 banner) — for now this is a launch-time eprintln.
    match probe_breadcrumb(&mut session) {
        BreadcrumbProbe::CrashedJournal(j) => {
            eprintln!(
                "{}",
                journal_report(&j).unwrap_or_else(|e| format!(
                    "Interrupted apply found at {} but the journal could not be read: {e}",
                    j.display()
                ))
            );
        }
        BreadcrumbProbe::StaleCleared(dest) => {
            // Autosave the cleared breadcrumb immediately so a repeat crash
            // right after launch can't resurrect it from the on-disk session.
            let _ = save(&session, &source.join(SESSION_FILE));
            eprintln!(
                "stale pending_apply breadcrumb cleared (no journal at {}); proceeding",
                dest.display()
            );
        }
        BreadcrumbProbe::NoBreadcrumb => {}
    }
    // The source dir itself is also probed (it may have been a prior run's
    // DESTINATION). Done before wrapping `session` in Rc/RefCell — this is a
    // plain startup check, not part of the live event-loop state.
    if let Some(j) = find_crashed_apply(&source) {
        eprintln!(
            "{}",
            journal_report(&j).unwrap_or_else(|e| format!(
                "Interrupted apply found at {} but the journal could not be read: {e}",
                j.display()
            ))
        );
    }

    let app = AppWindow::new()?;
    let session = Rc::new(RefCell::new(session));
    let filter = Rc::new(RefCell::new(Filter::All));
    let zoom = Rc::new(RefCell::new(ui::ZoomState::default()));
    let cache = Arc::new(Mutex::new(pipeline::LruCache::new(CACHE_BUDGET)));
    let full_slot = Arc::new(Mutex::new(FullSlot::default()));
    // The shot the loupe is showing — the delivery-time freshness check reads
    // this on the event loop before painting any decode result.
    let current_shot = Arc::new(std::sync::atomic::AtomicUsize::new(
        session.borrow().current,
    ));
    // Mirrors `pipeline.generation` at the moment `request_current` last
    // bumped it (I2). `Pipeline::bump()` happens before the current-shot
    // `enqueue`, so this is the SAME generation stamped onto that request's
    // `Request.generation` — a delivery whose `req.generation` is older than
    // this is a superseded decode (e.g. a `Z`-toggle's stale `Fit`) and must
    // not paint even if its index still matches the current shot.
    let current_gen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Debounced-autosave dirty flag (spec §12: never a sync fsync per keypress).
    let dirty = Rc::new(std::cell::Cell::new(false));
    // Set by applyflow's on_apply_finished once a successful apply retires
    // source/.fastcull.json into dest. Post-apply, the session record lives
    // in dest; the autosave timer and exit-flush below must not resurrect
    // the retired source copy.
    let applied = Rc::new(std::cell::Cell::new(false));

    // Decode pipeline. on_ready DROPS STALE RESULTS AT DELIVERY (§12): results
    // land in completion order, so a prefetched NEIGHBOR or a superseded decode
    // must never repaint the loupe — cache everything, paint only the current
    // shot's result. (This check is load-bearing: without it, holding → paints
    // whichever of the ±N prefetches decodes last.)
    let weak = app.as_weak();
    let cache_w = cache.clone();
    let full_w = full_slot.clone();
    let cur_w = current_shot.clone();
    let gen_w = current_gen.clone();
    let pipeline = Arc::new(Pipeline::spawn(3, move |res| {
        let weak = weak.clone();
        let cache_w = cache_w.clone();
        let full_w = full_w.clone();
        let cur_w = cur_w.clone();
        let gen_w = gen_w.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                // I2: freshness gates the PAINT only — index alone is not
                // enough (a still-in-flight OLD Fit for the current index
                // must not overwrite a fresh Full after a `Z` toggle bumps
                // the generation). Caching stays unconditional below: a
                // decoded image is valid for its index regardless of
                // staleness.
                let is_fresh = pipeline::is_fresh_delivery(
                    &res.req,
                    cur_w.load(Ordering::SeqCst),
                    gen_w.load(Ordering::SeqCst),
                );
                match res.target {
                    TargetSize::Full => {
                        full_w.lock().unwrap().set(res.req.index, res.image.clone());
                        if is_fresh {
                            ui::set_loupe(&app, &res.image);
                        }
                    }
                    _ => {
                        cache_w
                            .lock()
                            .unwrap()
                            .put(res.req.index, res.image.clone());
                        if is_fresh {
                            ui::set_loupe(&app, &res.image);
                        }
                        // Neighbor prefetches land here too (spec §12 "tiles
                        // refine lazily") — hop through the Send-safe slint
                        // callback rather than capturing refresh_view (an
                        // Rc-based closure) directly in this Send closure.
                        app.invoke_thumbs_updated();
                    }
                }
            }
        });
    }));

    // Helper: (re)request current + prefetch neighbors after any navigation.
    let request_current = {
        let session = session.clone();
        let zoom = zoom.clone();
        let cache = cache.clone();
        let full_slot = full_slot.clone();
        let pipeline = pipeline.clone();
        let current_shot = current_shot.clone();
        let current_gen = current_gen.clone();
        let app_w = app.as_weak();
        move || {
            let s = session.borrow();
            if s.shots.is_empty() {
                return;
            }
            let (fw, fh) = (1600u32, 1000u32);
            // I2: bump BEFORE storing/enqueuing, and store the SAME value
            // that `enqueue` below will stamp onto this request's
            // `Request.generation` — so the freshly-enqueued current
            // request can never be seen as stale by `on_ready`.
            let new_gen = pipeline.bump(); // latest-wins: supersede in-flight requests
            current_gen.store(new_gen, Ordering::SeqCst);
            let cur = s.current;
            current_shot.store(cur, Ordering::SeqCst); // delivery freshness anchor
            let z = *zoom.borrow();
            // Show the best already-cached scale immediately.
            if let Some(app) = app_w.upgrade() {
                if z.zoomed {
                    if let Some(img) = full_slot.lock().unwrap().get(cur) {
                        ui::set_loupe(&app, &img);
                    }
                } else if let Some(img) = cache.lock().unwrap().get(cur) {
                    ui::set_loupe(&app, &img);
                }
            }
            // Request the exact target for current.
            pipeline.enqueue(cur, s.shots[cur].jpeg.clone(), z.target(fw, fh), false);
            // Prefetch neighbors (fit-size only). thumb_first: true — an
            // embedded EXIF thumbnail paints the filmstrip tile instantly
            // (spec §12), refined by the real decode moments later. The
            // current-shot request above stays `false`: the loupe wants the
            // real decode, not a low-res thumbnail flash.
            for idx in prefetch_set(cur, PREFETCH_N, s.shots.len()) {
                if idx != cur && !cache.lock().unwrap().contains(idx) {
                    pipeline.enqueue(
                        idx,
                        s.shots[idx].jpeg.clone(),
                        TargetSize::Fit(fw, fh),
                        true,
                    );
                }
            }
        }
    };

    // Refresh HUD + filmstrip from current state.
    let refresh_view = {
        let session = session.clone();
        let filter = filter.clone();
        let cache = cache.clone();
        let app_w = app.as_weak();
        move || {
            let Some(app) = app_w.upgrade() else { return };
            let s = session.borrow();
            let h = ui::hud_text(&s, *filter.borrow());
            app.set_hud_tier(h.tier.into());
            app.set_hud_tags(h.tags.into());
            app.set_hud_counts(h.counts.into());
            app.set_hud_progress(h.progress.into());
            app.set_filter_label(h.filter_label.into());
            app.set_hud_filename(h.filename.into());
            app.set_hud_has_raw(h.has_raw);
            app.set_hud_position(h.position.into());
            let c = s.counts();
            app.set_count_best(c.bests as i32);
            app.set_count_pick(c.picks as i32);
            app.set_count_keep(c.keep as i32);
            app.set_count_rest(c.rest as i32);
            app.set_count_reject(c.rejected as i32);
            let mut cache = cache.lock().unwrap();
            let grey = pipeline::grey_thumb();
            let mut thumb_for = |i: usize| {
                cache
                    .get(i)
                    .map(|im| to_slint_image(&im))
                    .unwrap_or_else(|| grey.clone())
            };
            ui::refresh_filmstrip(&app, &s, *filter.borrow(), FILMSTRIP_BUFFER, &mut thumb_for);
        }
    };

    // Lazy tile refinement (spec §12): the decode pipeline's event-loop hop
    // (on_ready, above) invokes this slint callback after a prefetched
    // neighbor's FIT-target result is cached, so the filmstrip repaints from
    // grey without waiting for the next user event. Registered here (after
    // refresh_view exists) rather than captured in the pipeline closure
    // itself, since that closure must stay Send and refresh_view holds Rc
    // state.
    {
        let refresh_view = refresh_view.clone();
        app.on_thumbs_updated(move || {
            refresh_view();
        });
    }

    // Toast (Task 9b, DESIGN §4 2g): a single transient pill, auto-cleared by
    // a restartable SingleShot timer — repeated calls just push the clear
    // time back (`Timer::start`'s doc: "If the timer has been started
    // previously, then it will be restarted"). `Timer` itself isn't `Clone`,
    // so it's wrapped in `Rc` — that lets this closure be `.clone()`d for the
    // `show-toast` Slint callback below without a second timer/duplicated
    // clear logic.
    let show_toast = {
        let app_w = app.as_weak();
        let timer: Rc<slint::Timer> = Rc::new(slint::Timer::default());
        move |text: String, code: i32| {
            let Some(app) = app_w.upgrade() else { return };
            app.set_toast_text(text.into());
            app.set_toast_code(code);
            let app_w2 = app_w.clone();
            timer.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(2500),
                move || {
                    if let Some(app) = app_w2.upgrade() {
                        app.set_toast_text("".into());
                    }
                },
            );
        }
    };
    // `show-toast` callback: lets applyflow (Task 12, a separate module) reach
    // the same helper on apply completion without threading an extra Rc
    // closure through `wire_apply_dialog`'s signature (Task 9b amendment —
    // "expose as an AppWindow callback ... pick the simpler").
    {
        let show_toast = show_toast.clone();
        app.on_show_toast(move |text, code| show_toast(text.to_string(), code));
    }

    // Key dispatch: pure map -> action -> mutate model or UI state, then refresh.
    {
        let session = session.clone();
        let filter = filter.clone();
        let zoom = zoom.clone();
        let request_current = request_current.clone();
        let refresh_view = refresh_view.clone();
        let app_w = app.as_weak();
        let source = source.clone();
        let dirty = dirty.clone();
        let show_toast = show_toast.clone();
        let applied = applied.clone();
        app.on_key_pressed(move |text, ctrl| {
            let Some(app) = app_w.upgrade() else {
                return false;
            };
            // Help overlay gating (Task 9b) FIRST and separate from the pure
            // §9 keymap below: while it's open, only `?`/Escape do anything
            // (close it) and every other key is swallowed outright, so none
            // of the loupe/tag/apply keymaps can leak through underneath the
            // sheet.
            if app.get_help_open() {
                if text == "?" || text == "Escape" {
                    app.set_help_open(false);
                }
                return true;
            }
            // M-1: Apply dialog keyboard dismiss, mirroring the help_open
            // gate above — Escape closes it, but only when the worker
            // thread isn't mid-apply (abandoning the progress view would
            // orphan the journal-polling UI with no way back to see the
            // outcome). The pure §9 keymap stays inert for ApplyDialog
            // either way (key_to_action returns None for ctx != Loupe).
            if app.get_apply_open() && text == "Escape" && !app.get_apply_running() {
                app.set_apply_open(false);
                return true;
            }
            let ctx = if app.get_help_open() {
                // Unreachable given the early return above — kept so the pure
                // layer stays inert here too if that gate is ever changed
                // (belt and braces, key_to_action already treats Help like
                // any other non-Loupe modal context).
                InputContext::Help
            } else if app.get_tag_open() {
                InputContext::TagEntry
            } else if app.get_apply_open() {
                InputContext::ApplyDialog
            } else {
                InputContext::Loupe
            };
            let Some(key) = to_key(&text) else {
                return false;
            };
            let mods = input::Modifiers {
                control: ctrl,
                ..Default::default()
            };
            let Some(action) = key_to_action(key, mods, ctx) else {
                return false;
            };
            match action {
                Action::CycleFilter => {
                    let nf = next_filter(*filter.borrow());
                    *filter.borrow_mut() = nf;
                    refresh_view();
                    // Toast (DESIGN §4 2g): the new filter's label + a dot in
                    // its tier color; `All` has no ladder floor, so no dot.
                    let (label, code) = match nf {
                        Filter::All => ("filter: All", -1),
                        Filter::Keep => ("filter: >=Keep", 1),
                        Filter::Pick => ("filter: >=Pick", 2),
                        Filter::Best => ("filter: >=Best", 3),
                        Filter::Rejects => ("filter: Rejects", 4),
                    };
                    show_toast(label.to_string(), code);
                }
                Action::ToggleZoom => {
                    zoom.borrow_mut().toggle();
                    app.set_zoomed(zoom.borrow().zoomed);
                    request_current();
                }
                Action::OpenTagEntry => {
                    let s = session.borrow();
                    app.set_tag_text(s.decision(s.current).tags.join(", ").into());
                    app.set_tag_open(true);
                }
                Action::OpenApply => {
                    app.set_apply_open(true);
                }
                Action::ForceSave => {
                    // Ctrl+S: immediate, and the debounce flag is satisfied.
                    // I4: post-apply, the session record lives in dest; do
                    // not resurrect the retired source copy (mirrors the
                    // autosave timer + exit-flush guards on `applied.get()`
                    // above/below).
                    dirty.set(false);
                    if applied.get() {
                        show_toast(
                            "session already applied (record is in the destination)".to_string(),
                            -1,
                        );
                    } else {
                        let _ = save(&session.borrow(), &source.join(SESSION_FILE));
                        show_toast("session saved".to_string(), -1);
                    }
                }
                Action::ToggleHelp => {
                    app.set_help_open(true);
                }
                Action::Undo => {
                    // Session::undo() (culler-core, untouched here) only
                    // returns whether it reverted anything — not which stem
                    // or tier. The toast approximates with the CURRENT shot
                    // (the shot a tier/tag edit almost always targets) rather
                    // than threading a richer return type back through core.
                    let reverted = session.borrow_mut().undo();
                    let (text, code) = if reverted {
                        dirty.set(true);
                        refresh_view();
                        let s = session.borrow();
                        let cur = s.current;
                        let text = match s.shots.get(cur) {
                            Some(shot) => format!("undo {}", shot.stem),
                            None => "undo".to_string(),
                        };
                        (text, ui::tier_color_code(s.decision(cur)))
                    } else {
                        ("nothing to undo".to_string(), -1)
                    };
                    show_toast(text, code);
                }
                other => {
                    let before = session.borrow().current;
                    apply_action(
                        other,
                        &mut session.borrow_mut(),
                        auto_advance,
                        *filter.borrow(),
                    );
                    if session.borrow().current != before {
                        zoom.borrow_mut().on_navigate();
                        request_current();
                    }
                    // Autosave is DEBOUNCED (flag + timer below) — a synchronous
                    // serialize+fsync per keypress fights §12 sub-frame navigation.
                    dirty.set(true);
                    refresh_view();
                }
            }
            true
        });
    }

    // Filmstrip click -> jump.
    {
        let session = session.clone();
        let filter = filter.clone();
        let request_current = request_current.clone();
        let refresh_view = refresh_view.clone();
        let dirty = dirty.clone();
        app.on_film_clicked(move |offset| {
            let (indices, _) =
                ui::build_filmstrip_window(&session.borrow(), *filter.borrow(), FILMSTRIP_BUFFER);
            if let Some(&idx) = indices.get(offset as usize) {
                session.borrow_mut().current = idx;
                session.borrow_mut().mark_visited(idx);
                dirty.set(true); // picked up by the debounced autosave
                request_current();
                refresh_view();
            }
        });
    }

    // Tag entry commit / cancel.
    {
        let session = session.clone();
        let refresh_view = refresh_view.clone();
        let dirty = dirty.clone();
        let app_w = app.as_weak();
        app.on_tag_committed(move |text| {
            let Some(app) = app_w.upgrade() else { return };
            let idx = session.borrow().current;
            session.borrow_mut().set_tags(idx, parse_tags(&text));
            app.set_tag_open(false);
            dirty.set(true); // picked up by the debounced autosave
            refresh_view();
        });
    }
    {
        let app_w = app.as_weak();
        app.on_tag_cancelled(move || {
            if let Some(a) = app_w.upgrade() {
                a.set_tag_open(false);
            }
        });
    }
    {
        let session = session.clone();
        let app_w = app.as_weak();
        app.on_tag_changed(move |text| {
            if let Some(app) = app_w.upgrade() {
                let all = session.borrow().all_tags();
                let last = text.rsplit(',').next().unwrap_or("").to_string();
                let sugg: Vec<slint::SharedString> = ui::suggest_tags(&all, &last)
                    .into_iter()
                    .map(Into::into)
                    .collect();
                app.set_tag_suggestions(Rc::new(slint::VecModel::from(sugg)).into());
            }
        });
    }

    // Apply dialog callbacks are wired in Task 12 (applyflow).
    applyflow::wire_apply_dialog(&app, session.clone(), buckets, applied.clone());

    // Debounced autosave: flush the dirty flag at most every 2s, off the hot
    // key path. The Timer binding must outlive run() or it is cancelled.
    let autosave_timer = slint::Timer::default();
    {
        let session = session.clone();
        let source = source.clone();
        let dirty = dirty.clone();
        let applied = applied.clone();
        autosave_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(2),
            move || {
                // I-1: post-apply, the session record lives in dest; do not
                // resurrect the retired source copy.
                if !applied.get() && dirty.replace(false) {
                    let _ = save(&session.borrow(), &source.join(SESSION_FILE));
                }
            },
        );
    }

    // First paint.
    {
        let mut s = session.borrow_mut();
        if !s.shots.is_empty() {
            let cur = s.current;
            s.mark_visited(cur);
        }
    }
    request_current();
    refresh_view();
    app.run()?;
    // Flush any still-debounced state on the way out. I-1: post-apply, the
    // session record lives in dest; do not resurrect the retired source copy.
    if !applied.get() {
        let _ = save(&session.borrow(), &source.join(SESSION_FILE));
    }
    Ok(())
}
