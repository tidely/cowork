//! Text tooltips through GPUI's own tooltip, for any interactive element.
//!
//! gpui-component's `Button::tooltip` and `Switch::tooltip` show tooltips in
//! a window-level overlay, from the trigger's hover callbacks. A trigger
//! removed while the pointer is on it (a pending message's cancel button
//! when its message is sent, say) never reports that the hover ended, so its
//! tooltip stays until the pointer moves onto another trigger. GPUI's
//! tooltip is registered again by its element on every frame, so it goes
//! with the element. `clippy.toml` disallows the overlay methods.

use gpui::{InteractiveElement, SharedString, div, prelude::*};
use gpui_component::tooltip::Tooltip;

pub(crate) trait NativeTooltip: InteractiveElement + Sized {
    /// Shows `text` while the pointer rests on the element. The element
    /// needs an ID, as a `Button` always has.
    fn native_tooltip(mut self, text: impl Into<SharedString>) -> Self {
        let text = text.into();
        self.interactivity().tooltip(move |window, cx| {
            let text = text.clone();
            Tooltip::element(move |_, _| {
                let selector = format!("tooltip: {text}");
                div().debug_selector(move || selector).child(text.clone())
            })
            .build(window, cx)
        });
        self
    }
}

impl<E: InteractiveElement> NativeTooltip for E {}
