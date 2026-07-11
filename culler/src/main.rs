slint::include_modules!();

mod input;

// Bundled IBM Plex fonts (culler/ui/fonts/) need no registration call here: Slint
// 1.17 has no public `register_font_from_path`/`register_font_from_data` free
// function (only a per-renderer trait method used internally by the compiler).
// The supported mechanism is compile-time — `theme.slint` does `import
// "fonts/IBMPlexMono-Regular.ttf";` (etc.) for all 6 weights, and slint-build's
// default `EmbedResourcesKind::EmbedAllResources` embeds the bytes and emits a
// `RegisterCustomFontByMemory` call into the generated component's init code
// automatically, before the window is constructed. See task-1b-report.md.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = AppWindow::new()?;
    app.run()?;
    Ok(())
}
