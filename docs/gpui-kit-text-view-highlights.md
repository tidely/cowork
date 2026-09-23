# gpui-kit request: source-range highlights on `TextView`

A proposed addition to our gpui-kit fork (`tidely/gpui-kit`, branch
`selected-source-range`) that Cowork needs to show other participants'
selections in agent messages.

## Motivation

Presence already shows other participants' carets and selections in draft
editors (see [collaboration.md](collaboration.md#presence)). Selecting text in
an agent message, which is how a comment starts, is not shared: participants
only see the comment once someone starts typing it.

Sending such a selection is simple: the message ID and the selected byte range
in the message's Markdown source, which `TextViewState::selected_source_range`
already provides. Showing it is not. `TextView` can map the local selection to
source offsets, but not a source range back to what is on screen, so Cowork
has nothing to paint another participant's selection with.

The alternatives available without changing gpui-kit are poor:

- **Reusing the comment highlight annotations** (rewriting the Markdown with
  `[...](#inline-comment)` links) splits and reparses the message while the
  other participant drags, resets the local user's selection in that message,
  and colors text like a comment highlight.
- **Showing only a marker** next to the message hides what is selected.

## Proposed API

```rust
impl TextViewState {
    /// Highlights ranges of the Markdown source with background colors,
    /// replacing any previous highlights.
    ///
    /// Ranges address the source passed to this view, like
    /// `selected_source_range`. They are painted under the glyphs like the
    /// local selection, without changing text, layout, or the local
    /// selection. Out-of-range or non-char-boundary ranges are clamped;
    /// overlapping highlights are painted in order.
    pub fn set_source_highlights(
        &mut self,
        highlights: Vec<SourceHighlight>,
        cx: &mut Context<Self>,
    );
}

pub struct SourceHighlight {
    pub range: Range<usize>,
    /// Usually translucent, since it is painted under the text.
    pub background: Hsla,
}
```

A smaller alternative that would also work: a query mapping a source range to
the bounds of the rendered text it covers, one rectangle per visual line,
leaving the painting to the caller:

```rust
impl TextViewState {
    pub fn source_range_bounds(&self, range: Range<usize>) -> Vec<Bounds<Pixels>>;
}
```

`set_source_highlights` is preferable: painting inside the view keeps
highlights in step with the view's own layout, scrolling and clamping, where a
caller painting from last frame's bounds lags by a frame.

## Semantics to get right

- **Streaming.** Agent messages grow through `push_str`. Highlights must
  survive appends, since a range in the existing source stays valid.
- **Delimiters.** Ranges may include Markdown syntax (e.g. `**`) or span
  inline code and several blocks, as `selected_source_range` results do.
  Unrendered delimiters should simply not be painted.
- **Atomic content.** Images or other inline objects inside a range may be
  highlighted whole, like the local selection does.
- **Performance.** A highlight changes every frame while someone drags, so
  setting one must not reparse the document or invalidate text layout; a
  repaint is enough.

## Implementation hints

- Inline nodes already keep source segments, used by `selected_source_range`
  to map rendered selections to source offsets. The reverse mapping for a
  highlight can use the same data.
- Background painting already exists for `<mark>` (`TextMark::highlight`) and
  for the local selection, so no new painting code should be needed.

## How Cowork would use it

1. Presence gains an optional excerpt selection: message ID plus source range,
   sent while the local user selects in an agent message or a comment reply
   (`Cowork::selected_message_source_range` already computes it).
2. When rendering an agent message, Cowork collects the excerpt selections of
   everyone else in that message and sets them as highlights in each
   participant's color on the message's text views (for annotated messages,
   mapped into each segment's source offsets).
