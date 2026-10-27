//! whatsapp-gpui patch: AccessKit text semantics for the editor.
//!
//! A text input only supports text ranges in AccessKit (macOS
//! `AXNumberOfCharacters`, `AXSelectedTextRange`, `AXStringForRange`, …) when
//! it has `TextRun` children and a text selection. Each hard line becomes one
//! run whose value keeps its trailing `\n`; the last run may be empty. An
//! AccessKit "character" is a grapheme cluster, so an emoji is one character
//! that the platform adapter maps to its UTF-16 length.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::accesskit::{Node, NodeId, Role, TextPosition, TextSelection};
use unicode_segmentation::UnicodeSegmentation as _;

/// One synthetic `TextRun`: its byte range in the text and per-character
/// UTF-8 lengths (the AccessKit character unit).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct A11yTextRun {
    pub(super) range: Range<usize>,
    pub(super) character_lengths: Vec<u8>,
}

pub(super) fn text_runs(text: &str) -> Vec<A11yTextRun> {
    let mut runs = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        runs.push(run(text, start..start + line.len()));
        start += line.len();
    }
    if runs.is_empty() || text.ends_with('\n') {
        runs.push(run(text, start..start));
    }
    runs
}

fn run(text: &str, range: Range<usize>) -> A11yTextRun {
    let mut character_lengths = Vec::new();
    for grapheme in text[range.clone()].graphemes(true) {
        match u8::try_from(grapheme.len()) {
            Ok(len) => character_lengths.push(len),
            // Lengths are a u8; split pathological clusters into scalars.
            Err(_) => character_lengths.extend(grapheme.chars().map(|c| c.len_utf8() as u8)),
        }
    }
    A11yTextRun {
        range,
        character_lengths,
    }
}

/// The run index and character index of a byte offset. An offset right after a
/// line's `\n` belongs to the start of the next run.
pub(super) fn position(runs: &[A11yTextRun], offset: usize) -> (usize, usize) {
    let ix = runs
        .iter()
        .rposition(|run| run.range.start <= offset)
        .unwrap_or(0);
    let run = &runs[ix];
    let mut byte = run.range.start;
    let mut character = 0;
    for len in &run.character_lengths {
        if byte >= offset {
            break;
        }
        byte += *len as usize;
        character += 1;
    }
    (ix, character)
}

/// The byte offset of an AccessKit text position.
pub(super) fn offset(runs: &[A11yTextRun], ix: usize, character_index: usize) -> Option<usize> {
    let run = runs.get(ix)?;
    let bytes: usize = run
        .character_lengths
        .iter()
        .take(character_index)
        .map(|len| *len as usize)
        .sum();
    Some(run.range.start + bytes)
}

/// The runs last written for an editor, so a `SetTextSelection` request (which
/// names synthetic run nodes) can be mapped back to byte offsets.
#[derive(Clone, Default)]
pub(super) struct A11yTextRuns(Rc<RefCell<(Vec<NodeId>, Vec<A11yTextRun>)>>);

impl A11yTextRuns {
    /// Byte offsets `(anchor, focus)` of a requested selection.
    pub(super) fn selection_offsets(&self, selection: &TextSelection) -> Option<(usize, usize)> {
        let (ids, runs) = &*self.0.borrow();
        let to_offset = |position: &TextPosition| {
            let ix = ids.iter().position(|id| *id == position.node)?;
            offset(runs, ix, position.character_index)
        };
        Some((to_offset(&selection.anchor)?, to_offset(&selection.focus)?))
    }
}

/// Pushes the runs under the editor node and sets its selection (`anchor` and
/// `focus` are byte offsets).
pub(super) fn write_text_runs(
    builder: &mut gpui::A11ySubtreeBuilder,
    text: &str,
    anchor: usize,
    focus: usize,
    written: &A11yTextRuns,
) {
    let runs = text_runs(text);
    let ids: Vec<NodeId> = (0..runs.len())
        .map(|ix| builder.synthetic_node_id(("text-run", ix)))
        .collect();
    for (ix, run) in runs.iter().enumerate() {
        let mut node = Node::new(Role::TextRun);
        node.set_value(&text[run.range.clone()]);
        node.set_character_lengths(run.character_lengths.clone());
        builder.push_child(ids[ix], node);
    }
    let to_position = |offset: usize| {
        let (ix, character_index) = position(&runs, offset.min(text.len()));
        TextPosition {
            node: ids[ix],
            character_index,
        }
    };
    builder.parent_node().set_text_selection(TextSelection {
        anchor: to_position(anchor),
        focus: to_position(focus),
    });
    *written.0.borrow_mut() = (ids, runs);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values<'a>(text: &'a str, runs: &[A11yTextRun]) -> Vec<&'a str> {
        runs.iter().map(|run| &text[run.range.clone()]).collect()
    }

    #[test]
    fn empty_text_has_one_empty_run() {
        let runs = text_runs("");
        assert_eq!(values("", &runs), vec![""]);
        assert_eq!(position(&runs, 0), (0, 0));
    }

    #[test]
    fn lines_keep_their_newlines() {
        let text = "ab\ncd\n";
        let runs = text_runs(text);
        assert_eq!(values(text, &runs), vec!["ab\n", "cd\n", ""]);
        assert_eq!(position(&runs, 2), (0, 2));
        assert_eq!(position(&runs, 3), (1, 0));
        assert_eq!(position(&runs, 5), (1, 2));
        assert_eq!(position(&runs, 6), (2, 0));
        for offset in 0..=text.len() {
            let (ix, character) = position(&runs, offset);
            assert_eq!(super::offset(&runs, ix, character), Some(offset));
        }
    }

    #[test]
    fn emoji_is_one_character() {
        let text = "Hi 👍🏽 there";
        let runs = text_runs(text);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].character_lengths[3], "👍🏽".len() as u8);
        assert_eq!(runs[0].character_lengths.len(), 10);
        let after_emoji = "Hi 👍🏽".len();
        assert_eq!(position(&runs, after_emoji), (0, 4));
        assert_eq!(position(&runs, text.len()), (0, 10));
    }

    #[test]
    fn oversized_cluster_splits_into_scalars() {
        let text = format!("a{}", "\u{301}".repeat(200));
        let runs = text_runs(&text);
        assert_eq!(runs[0].character_lengths.len(), 201);
        assert_eq!(position(&runs, text.len()), (0, 201));
    }
}
