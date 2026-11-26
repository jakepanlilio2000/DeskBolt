#![cfg_attr(
    all(target_os = "windows", not(feature = "dev-mcp")),
    windows_subsystem = "windows"
)]

#[cfg(not(target_os = "android"))]
fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help" | "-h") => {
            println!(
                "DeskBolt Viewer {}\n\
                 Usage: deskbolt-viewer [--connect-id <id>] [--mode <full|view|files>]",
                env!("CARGO_PKG_VERSION")
            );
            #[cfg(feature = "dev-mcp")]
            println!("       deskbolt-viewer --mcp [--loopback-only]");
            std::process::ExitCode::SUCCESS
        }
        Some("--version" | "-V") => {
            println!("DeskBolt Viewer {}", env!("CARGO_PKG_VERSION"));
            std::process::ExitCode::SUCCESS
        }
        #[cfg(feature = "dev-mcp")]
        Some("--mcp") if args.len() == 1 || args[1..] == ["--loopback-only"] => report(serve_mcp()),
        None | Some("--connect-id" | "--mode") => report(deskbolt_viewer_lib::run_desktop()),
        Some(arg) if arg.starts_with("--connect-id=") || arg.starts_with("--mode=") => {
            report(deskbolt_viewer_lib::run_desktop())
        }
        _ => {
            eprintln!("Unknown option. Use --help for available options.");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(not(target_os = "android"))]
fn report(result: Result<(), impl std::fmt::Display>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(all(feature = "dev-mcp", not(target_os = "android")))]
#[tokio::main]
async fn serve_mcp() -> Result<(), Box<dyn std::error::Error>> {
    use rmcp::ServiceExt;

    deskbolt_server_lib::loopback::ProtocolChecks
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(target_os = "android")]
fn main() {}
