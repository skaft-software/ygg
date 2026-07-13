#![allow(missing_docs)]

mod tui;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    println!("ygg scaffold");
    Ok(())
}
