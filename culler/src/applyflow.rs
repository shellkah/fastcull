//! Apply dialog flow — implemented in Task 12.

/// Wire the apply dialog's callbacks (open/confirm/cancel, plan preview,
/// progress) to `app`. Task 11 only lands the `apply-open` property and this
/// stub call site; the real dialog flow is Task 12.
pub fn wire_apply_dialog(
    _app: &crate::AppWindow,
    _session: std::rc::Rc<std::cell::RefCell<culler_core::model::Session>>,
    _buckets: [String; 5],
) {
}
