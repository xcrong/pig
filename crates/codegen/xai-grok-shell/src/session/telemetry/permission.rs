/// Permission-mode label for the `session.permission_mode_changed` span.
pub(crate) fn permission_mode_label(is_yolo: bool) -> &'static str {
    if is_yolo {
        "bypassPermissions"
    } else {
        "default"
    }
}
