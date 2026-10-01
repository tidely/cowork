//! Caret and selection geometry in draft editors: moving between
//! editors by visual line, and drawing other participants' selections and
//! caret labels.

use std::ops::Range;

use gpui::{App, Bounds, SharedString, TextRun, Window, point, px, rgb, size};
use gpui_base::input::TextareaState;

/// When the caret is on the first visual line of `editor` (or the last, with
/// `last`), returns its horizontal position, so moving to the neighboring
/// editor can keep it.
pub(crate) fn caret_x_on_edge_line(editor: &TextareaState, last: bool) -> Option<gpui::Pixels> {
    let caret = editor.cursor();
    let edge = if last { editor.value().len() } else { 0 };
    // Without a layout there is nothing to compare, and the edge is as good a
    // guess as any.
    let Some(caret_bounds) = editor.range_to_bounds(&(caret..caret)) else {
        return Some(px(0.));
    };
    let Some(edge_bounds) = editor.range_to_bounds(&(edge..edge)) else {
        return Some(caret_bounds.left());
    };
    same_visual_line(caret_bounds.top(), edge_bounds.top()).then_some(caret_bounds.left())
}

/// The offset on the last visual line of `editor` (or the first, without
/// `last`) horizontally closest to `x`.
pub(crate) fn offset_near_x(editor: &TextareaState, x: gpui::Pixels, last: bool) -> usize {
    let text = editor.value();
    let edge = if last { text.len() } else { 0 };
    let Some(edge_top) = editor
        .range_to_bounds(&(edge..edge))
        .map(|bounds| bounds.top())
    else {
        return edge;
    };
    let boundaries = text
        .char_indices()
        .map(|(offset, _)| offset)
        .chain([text.len()])
        .collect::<Vec<_>>();
    let on_line = |offset: &usize| {
        editor
            .range_to_bounds(&(*offset..*offset))
            .map(|bounds| (bounds.left(), same_visual_line(bounds.top(), edge_top)))
    };
    let distance = |left: gpui::Pixels| if left > x { left - x } else { x - left };
    // Walk inwards from the edge and stop at the first offset on another line.
    let candidates: Box<dyn Iterator<Item = &usize>> = if last {
        Box::new(boundaries.iter().rev())
    } else {
        Box::new(boundaries.iter())
    };
    candidates
        .map_while(|offset| {
            on_line(offset)
                .filter(|(_, same_line)| *same_line)
                .map(|(left, _)| (*offset, distance(left)))
        })
        .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map_or(edge, |(offset, _)| offset)
}

/// The rectangles covering `selection` of `editor`'s text, one per visual
/// line, in window coordinates.
pub(crate) fn selection_rects(
    editor: &TextareaState,
    text: &str,
    selection: Range<usize>,
    text_bounds: Bounds<gpui::Pixels>,
) -> Vec<Bounds<gpui::Pixels>> {
    if selection.is_empty() {
        return Vec::new();
    }
    let mut rects = Vec::new();
    let mut line_start = selection.start;
    loop {
        let line_end = text[line_start..selection.end]
            .find('\n')
            .map_or(selection.end, |newline| line_start + newline);
        if let (Some(start), Some(end)) = (
            editor.range_to_bounds(&(line_start..line_start)),
            editor.range_to_bounds(&(line_end..line_end)),
        ) && start.size.height > px(0.)
        {
            let height = start.size.height;
            // A selected empty line still shows as selected.
            let min_width = if line_end < selection.end {
                px(4.)
            } else {
                px(0.)
            };
            if same_visual_line(start.top(), end.top()) {
                let width = (end.left() - start.left()).max(min_width);
                rects.push(Bounds::new(start.origin, size(width, height)));
            } else {
                // The line wraps: the rest of its first row, whole rows in
                // between, and its last row up to the end.
                rects.push(Bounds::new(
                    start.origin,
                    size(text_bounds.right() - start.left(), height),
                ));
                let mut top = start.top() + height;
                while top + px(1.) < end.top() {
                    rects.push(Bounds::new(
                        point(text_bounds.left(), top),
                        size(text_bounds.size.width, height),
                    ));
                    top += height;
                }
                rects.push(Bounds::new(
                    point(text_bounds.left(), end.top()),
                    size((end.left() - text_bounds.left()).max(min_width), height),
                ));
            }
        }
        if line_end >= selection.end {
            return rects;
        }
        line_start = line_end + 1;
    }
}

/// A participant's name in their color, just above their caret at `origin`.
pub(crate) fn paint_caret_label(
    color: u32,
    name: SharedString,
    origin: gpui::Point<gpui::Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    const FONT_SIZE: gpui::Pixels = px(10.);
    const HEIGHT: gpui::Pixels = px(14.);

    let run = TextRun {
        len: name.len(),
        font: window.text_style().font(),
        // The label sits on a participant identity color, not a themed surface.
        color: rgb(0xffffff).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .shape_line(name, FONT_SIZE, &[run], None);
    let label = Bounds::new(
        point(origin.x, origin.y - HEIGHT),
        size(line.width + px(8.), HEIGHT),
    );
    window.paint_quad(gpui::fill(label, rgb(color)).corner_radii(px(3.)));
    _ = line.paint(
        point(label.left() + px(4.), label.top()),
        HEIGHT,
        gpui::TextAlign::Left,
        None,
        window,
        cx,
    );
}

fn same_visual_line(a: gpui::Pixels, b: gpui::Pixels) -> bool {
    a <= b + px(1.) && b <= a + px(1.)
}
