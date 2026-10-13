//! Interactive compaction settings and context UI without a model server.
//! Run with `cargo run -p cowork --example compaction_preview`.
fn main() -> anyhow::Result<()> {
    cowork::component_preview::run_compaction()
}
