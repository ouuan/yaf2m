use color_eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    color_eyre::install()?;
    yaf2m::run().await
}
