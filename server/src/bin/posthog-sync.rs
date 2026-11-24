use cja::{
    color_eyre::{self, eyre::WrapErr as _},
    observability::posthog,
};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> color_eyre::Result<ExitCode> {
    let declarations = match arena::observability_declarations::declarations() {
        Ok(declarations) => declarations,
        Err(error) => {
            eprintln!("invalid observability declarations: {error}");
            return Ok(ExitCode::from(2));
        }
    };
    let client = posthog::SyncClient::new().wrap_err("initialize the PostHog HTTP client")?;
    Ok(posthog::run_from_env("arena", &declarations, &client).await)
}
