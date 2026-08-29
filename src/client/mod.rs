pub(crate) mod http;
pub(crate) mod repl;

/// Run the client connecting to a remote daemon.
pub(crate) fn run_client(
    daemon_url: &str,
    args: &crate::cli::Args,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = http::DaemonClient::new(daemon_url)?;

    if !args.rest.is_empty() {
        // One-shot mode: single prompt, then exit.
        let prompt = args.rest.join(" ");
        repl::one_shot(&client, &prompt)?;
    } else {
        // Interactive REPL.
        repl::run_repl(&client)?;
    }

    Ok(())
}
