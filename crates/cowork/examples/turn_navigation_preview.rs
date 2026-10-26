//! The conversation navigation rail and its turn cards in their states,
//! without a model server. Run with
//! `cargo run -p cowork --example turn_navigation_preview`.
fn main() -> anyhow::Result<()> {
    cowork::component_preview::run_turn_navigation()
}
