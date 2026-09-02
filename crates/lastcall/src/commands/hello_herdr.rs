pub fn run(
    _socket: Option<std::path::PathBuf>,
    _exit_after: Option<u64>,
) -> Result<std::process::ExitCode, Box<dyn std::error::Error>> {
    Ok(std::process::ExitCode::SUCCESS)
}
