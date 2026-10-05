use crate::app::types::{StatusView, TaskDetail};
use agentos_engine::export::{Manifest, ReviewContents};
use agentos_store::read::EventPage;
use askama::Template;
pub(super) struct Preview {
    pub text: String,
    pub bytes: usize,
    pub total: usize,
    pub truncated: bool,
}
pub(super) fn preview(bytes: &[u8], limit: usize) -> Preview {
    let mut end = bytes.len().min(limit);
    if let Ok(text) = std::str::from_utf8(bytes) {
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
    Preview {
        text: String::from_utf8_lossy(&bytes[..end]).into_owned(),
        bytes: end,
        total: bytes.len(),
        truncated: end < bytes.len(),
    }
}
struct DiffLine {
    kind: &'static str,
    text: String,
}
#[derive(Template)]
#[template(path = "ui/patch.html")]
struct Patch<'a> {
    preview: &'a Preview,
    lines: Vec<DiffLine>,
}
pub(super) fn render_patch(preview: &Preview) -> Result<String, askama::Error> {
    Patch {
        preview,
        lines: preview
            .text
            .lines()
            .map(|line| DiffLine {
                kind: if line.starts_with('+') {
                    "added"
                } else if line.starts_with('-') {
                    "removed"
                } else {
                    "context"
                },
                text: line.to_owned(),
            })
            .collect(),
    }
    .render()
}
pub(super) struct ResultView {
    pub manifest: Manifest,
    pub patch: Preview,
    pub verified_final: bool,
    pub evidence: Vec<Preview>,
}
impl From<ReviewContents> for ResultView {
    fn from(contents: ReviewContents) -> Self {
        let manifest = contents.manifest;
        let verified_final = manifest.state == "SUCCEEDED"
            && manifest.final_workspace_digest.is_some()
            && manifest.final_workspace_digest == manifest.verified_digest
            && manifest.verification_results.iter().any(|v| {
                v.completed
                    && v.passed
                    && v.accepted_for_final_workspace
                    && v.workspace_digest == manifest.verified_digest
            });
        Self {
            manifest,
            patch: preview(&contents.patch_diff, 256 * 1024),
            verified_final,
            evidence: contents
                .evidence
                .values()
                .map(|b| preview(b, 64 * 1024))
                .collect(),
        }
    }
}
#[derive(Template)]
#[template(path = "ui/task.html")]
pub(super) struct TaskPage<'a> {
    pub actions: String,
    pub detail: &'a TaskDetail,
    pub contract: String,
    pub csrf: &'a str,
    pub active: bool,
}
#[derive(Template)]
#[template(path = "ui/status.html")]
pub(super) struct StatusPage<'a> {
    pub status: &'a StatusView,
    pub active: bool,
}
#[derive(Template)]
#[template(path = "ui/result.html")]
struct ResultPage<'a> {
    result: &'a ResultView,
    manifest: String,
}
pub(super) fn render_result(view: &ResultView) -> Result<String, askama::Error> {
    // Each component is independently escaped by Askama. Only rendered markup is joined.
    let mut html = ResultPage {
        result: view,
        manifest: serde_json::to_string_pretty(&view.manifest).unwrap(),
    }
    .render()?;
    html.push_str(&render_patch(&view.patch)?);
    Ok(html)
}
#[derive(Template)]
#[template(path = "ui/events.html")]
pub(super) struct EventsPage<'a> {
    pub page: &'a EventPage,
}
pub(super) fn active(status: &StatusView) -> bool {
    !matches!(status.state.as_str(), "SUCCEEDED" | "FAILED" | "CANCELLED")
        || !status.outstanding_effects.is_empty()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_and_html_do_not_split_or_execute_untrusted_text() {
        let shown = preview("é<script>alert(1)</script>".as_bytes(), 3);
        assert!(shown.truncated);
        assert_eq!(shown.text, "é<");
        assert_eq!(shown.bytes, 3);
        let html = render_patch(&preview(b"<script>alert(1)</script>", 256 * 1024)).unwrap();
        assert!(!html.contains("<script>"));
        assert!(html.contains("alert(1)"));
        assert_eq!(preview("é!".as_bytes(), 1).text, "");
        assert_eq!(preview(&[0xff, b'x'], 1).text, "�");
    }
}
