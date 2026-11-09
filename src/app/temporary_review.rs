use std::collections::HashSet;
use std::fmt::Write;
use std::time::Instant;

use crossterm::event::KeyEvent;

use crate::diff::{LineType, PatchIndex};

use super::{hash_string, App, AppState, InputMode};

#[derive(Debug, Clone)]
pub struct TemporaryComment {
    pub path: String,
    pub location: String,
    pub context: String,
    pub body: String,
    start: usize,
    end: usize,
}

#[derive(Default)]
pub(super) struct TemporaryReview {
    scope: Option<ReviewScope>,
    notes: Vec<TemporaryComment>,
}

#[derive(PartialEq)]
struct ReviewScope {
    repo: String,
    pr_number: Option<u32>,
    revision: String,
    local: bool,
    path: String,
    patch_hash: u64,
}

impl App {
    fn current_review_scope(&self) -> Option<ReviewScope> {
        let file = self.files().get(self.selected_file)?;
        Some(ReviewScope {
            repo: self.repo.clone(),
            pr_number: self.pr_number,
            revision: self.pr()?.head.sha.clone(),
            local: self.local_mode,
            path: file.filename.clone(),
            patch_hash: hash_string(file.patch.as_deref()?),
        })
    }

    pub(crate) fn reconcile_temporary_review(&mut self) {
        if self.temporary_review.scope.is_none() {
            return;
        }
        let in_pane = match self.state {
            AppState::DiffView | AppState::SplitViewDiff => true,
            AppState::TextInput => matches!(
                self.preview_return_state,
                AppState::DiffView | AppState::SplitViewDiff
            ),
            AppState::Help => matches!(
                self.previous_state,
                AppState::DiffView | AppState::SplitViewDiff
            ),
            _ => false,
        };
        if !in_pane || self.temporary_review.scope != self.current_review_scope() {
            self.temporary_review = TemporaryReview::default();
            if matches!(self.input_mode, Some(InputMode::TemporaryComment(_))) {
                self.cancel_input();
                self.review_message(false, "Diff changed; temporary note discarded".into());
            }
        }
    }

    fn review_message(&mut self, success: bool, message: String) {
        self.cmt.submission_result = Some((success, message));
        self.cmt.submission_result_time = Some(Instant::now());
    }

    fn selected_diff_range(&self) -> (usize, usize) {
        self.multiline_selection.as_ref().map_or(
            (
                self.diff_scroll.selected_line,
                self.diff_scroll.selected_line,
            ),
            |selection| (selection.start(), selection.end()),
        )
    }

    pub(crate) fn selected_diff_text(&self) -> Option<String> {
        let patch = self.files().get(self.selected_file)?.patch.as_deref()?;
        let index = PatchIndex::build(patch);
        let (start, end) = self.selected_diff_range();
        let mut text = String::new();
        for line in (start..=end).filter_map(|i| index.get(i)) {
            if matches!(
                line.line_type,
                LineType::Added | LineType::Removed | LineType::Context
            ) {
                text.push_str(line.content);
                text.push('\n');
            }
        }
        (!text.is_empty()).then_some(text)
    }

    pub(crate) fn jump_change_hunk(&mut self, forward: bool, visible_lines: usize) {
        let Some(patch) = self
            .files()
            .get(self.selected_file)
            .and_then(|file| file.patch.as_deref())
        else {
            return;
        };
        let mut classifier = crate::diff::PatchClassifier::new();
        let mut changes: Vec<(usize, usize)> = Vec::new();
        let mut in_change = false;
        for (i, raw) in patch.lines().enumerate() {
            let (kind, _) = classifier.classify(raw);
            let changed = matches!(kind, LineType::Added | LineType::Removed);
            if changed && !in_change {
                changes.push((i, i));
            } else if changed {
                if let Some((_, end)) = changes.last_mut() {
                    *end = i;
                }
            }
            if kind != LineType::Meta {
                in_change = changed;
            }
        }
        let current = self.diff_scroll.selected_line;
        let target = if forward {
            changes.into_iter().find(|&(start, _)| start > current)
        } else {
            changes.into_iter().rev().find(|&(_, end)| end < current)
        };
        if let Some((target, _)) = target {
            self.multiline_selection = None;
            self.cmt.comment_panel_open = false;
            self.diff_scroll.selected_line = target;
            self.adjust_scroll(visible_lines);
        }
    }

    pub(crate) fn enter_temporary_comment(&mut self) {
        self.reconcile_temporary_review();
        let Some(scope) = self.current_review_scope() else {
            return;
        };
        let Some(file) = self.files().get(self.selected_file) else {
            return;
        };
        let Some(patch) = file.patch.as_deref() else {
            return;
        };
        let (start, end) = self.selected_diff_range();
        let Some(mut note) = note_context(&file.filename, patch, start, end) else {
            self.review_message(false, "Select code lines for a temporary note".into());
            return;
        };
        if let Some(existing) = self
            .temporary_review
            .notes
            .iter()
            .find(|note| note.start == start && note.end == end)
        {
            note.body.clone_from(&existing.body);
        }
        self.input_text_area.set_content(&note.body);
        self.input_mode = Some(InputMode::TemporaryComment(note));
        self.temporary_review.scope = Some(scope);
        self.multiline_selection = None;
        self.preview_return_state = self.state;
        self.state = AppState::TextInput;
    }

    pub(crate) fn save_temporary_comment(&mut self, mut note: TemporaryComment, body: String) {
        if self.temporary_review.scope != self.current_review_scope() {
            self.temporary_review = TemporaryReview::default();
            self.review_message(false, "Diff changed; temporary note discarded".into());
            return;
        }
        note.body = body;
        if let Some(existing) = self
            .temporary_review
            .notes
            .iter_mut()
            .find(|existing| existing.start == note.start && existing.end == note.end)
        {
            *existing = note;
        } else {
            self.temporary_review.notes.push(note);
        }
        let key = self.config.keybindings.copy_temporary_comments.display();
        self.review_message(
            true,
            format!("Temporary note saved · {key} Copy notes · cleared on pane change"),
        );
    }

    pub fn temporary_comment_count(&self) -> usize {
        self.temporary_review.notes.len()
    }

    pub(crate) fn diff_comment_lines(&self) -> HashSet<usize> {
        let mut lines = self.cmt.file_comment_lines.clone();
        for note in &self.temporary_review.notes {
            lines.extend(note.start..=note.end);
        }
        lines
    }

    pub(crate) fn temporary_comments_text(&self) -> Option<String> {
        if self.temporary_review.notes.is_empty()
            || self.temporary_review.scope != self.current_review_scope()
        {
            return None;
        }
        let mut text = String::from("# Review feedback\n\n");
        let revision = self
            .pr()
            .map(|pr| pr.head.sha.as_str())
            .unwrap_or("unknown");
        if self.local_mode {
            let _ = writeln!(
                text,
                "Repository: {}\nWorking tree changes against HEAD ({revision})\n",
                self.repo
            );
        } else {
            let _ = writeln!(
                text,
                "Repository: {}\nPR: #{}\nRevision: {revision}\n",
                self.repo,
                self.pr_number.unwrap_or(0)
            );
        }
        text.push_str("Address the feedback below in this checkout.\n\n");
        for (i, note) in self.temporary_review.notes.iter().enumerate() {
            let fence = "`".repeat(
                note.context
                    .split(|c| c != '`')
                    .map(str::len)
                    .max()
                    .unwrap_or(0)
                    .max(2)
                    + 1,
            );
            let _ = writeln!(text, "## {}. {} — {}\n\nContext (old/new line numbers; > marks the selection):\n{fence}diff\n{}{fence}\n\nFeedback:\n{}\n", i + 1, note.path, note.location, note.context, note.body);
        }
        Some(text)
    }

    pub(super) async fn handle_private_review_key(
        &mut self,
        key: &KeyEvent,
        visible_lines: usize,
    ) -> bool {
        let kb = &self.config.keybindings;
        if self.matches_single_key(key, &kb.copy_selection) {
            if let Some(text) = self.selected_diff_text() {
                if self.copy_review_text(&text, "Code copied").await {
                    self.multiline_selection = None;
                }
            } else {
                self.review_message(false, "Select code lines to copy".into());
            }
        } else if self.matches_single_key(key, &kb.temporary_comment) {
            self.enter_temporary_comment();
        } else if self.matches_single_key(key, &kb.copy_temporary_comments) {
            self.reconcile_temporary_review();
            if let Some(text) = self.temporary_comments_text() {
                self.copy_review_text(&text, "Temporary notes copied with context")
                    .await;
            } else {
                self.review_message(false, "No temporary notes in this pane".into());
            }
        } else if self.matches_single_key(key, &kb.next_hunk) {
            self.jump_change_hunk(true, visible_lines);
        } else if self.matches_single_key(key, &kb.previous_hunk) {
            self.jump_change_hunk(false, visible_lines);
        } else {
            return false;
        }
        true
    }

    async fn copy_review_text(&mut self, text: &str, success: &str) -> bool {
        match crate::clipboard::copy(text).await {
            Ok(()) => {
                self.review_message(true, success.into());
                true
            }
            Err(error) => {
                self.review_message(false, format!("Copy failed: {error:#}"));
                false
            }
        }
    }
}

fn note_context(path: &str, patch: &str, start: usize, end: usize) -> Option<TemporaryComment> {
    let index = PatchIndex::build(patch);
    let mut old_line = None;
    let mut context = String::new();
    let mut locations = Vec::new();
    for (i, raw) in patch.lines().enumerate() {
        let line = index.get(i)?;
        if let Some(header) = raw.strip_prefix("@@ -") {
            old_line = header.split([',', ' ']).next()?.parse::<u32>().ok();
        }
        let old = matches!(line.line_type, LineType::Removed | LineType::Context)
            .then_some(old_line)
            .flatten();
        if i >= start.saturating_sub(3) && i <= end.saturating_add(3) {
            if let Some(marker) = line.line_type.marker() {
                let selected = i >= start && i <= end;
                if selected {
                    let location = if line.line_type == LineType::Removed {
                        old.map(|n| format!("old line {n}"))
                    } else {
                        line.new_line_number.map(|n| format!("new line {n}"))
                    };
                    if let Some(location) = location {
                        locations.push(location);
                    }
                }
                let _ = writeln!(
                    context,
                    "{} {:>5} {:>5} | {}{}",
                    if selected { '>' } else { ' ' },
                    old.map(|n| n.to_string()).unwrap_or_default(),
                    line.new_line_number
                        .map(|n| n.to_string())
                        .unwrap_or_default(),
                    marker,
                    line.content
                );
            }
        }
        if old.is_some() {
            old_line = old_line.map(|n| n + 1);
        }
        if i > end.saturating_add(3) {
            break;
        }
    }
    let first = locations.first()?;
    let last = locations.last()?;
    let location = if first == last {
        first.clone()
    } else {
        format!("{first} – {last}")
    };
    Some(TemporaryComment {
        path: path.into(),
        location,
        context,
        body: String::new(),
        start,
        end,
    })
}
