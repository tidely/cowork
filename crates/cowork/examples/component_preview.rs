//! Shows Cowork's components in their states, side by side in a fullscreen
//! grid, to review them before anything in the app puts them in those
//! states: `cargo run -p cowork --example component_preview`.

fn main() -> anyhow::Result<()> {
    cowork::component_preview::run()
}
