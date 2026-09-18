use gpui::{App, AppContext, Context, IntoElement, Render, Window, WindowOptions, actions, div};

struct Sidebar {}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

actions!(window, [Quit]);

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        cx.open_window(WindowOptions::default(), |_window, cx| {
            cx.new(|_cx| Sidebar {})
        })
        .unwrap();

        cx.activate(true);
        cx.on_action(|_: &Quit, cx| cx.quit());
    });
}
