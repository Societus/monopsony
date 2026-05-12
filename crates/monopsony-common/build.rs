use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Check for protoc before calling tonic_build, so we can give a helpful
    // error message instead of the generic "Could not find `protoc`".
    if std::env::var("PROTOC").is_err() {
        // Try common locations
        let candidates: Vec<PathBuf> = vec![
            PathBuf::from("protoc"), // relies on PATH resolution
            dirs_home().join(".local/bin/python-grpc-tools-protoc"),
        ];

        let found = candidates.iter().any(|p| {
            if p.is_relative() {
                // Check if protoc is on PATH
                std::process::Command::new("protoc")
                    .arg("--version")
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            } else {
                p.exists()
            }
        });

        if !found {
            eprintln!();
            eprintln!("  ERROR: protoc not found. Install it one of these ways:");
            eprintln!();
            eprintln!("    1. System package:  sudo pacman -S protobuf");
            eprintln!("                       sudo apt install protobuf-compiler");
            eprintln!("    2. uv tool:         uv tool install grpcio-tools");
            eprintln!("    3. Manual:          Set PROTOC=/path/to/protoc env var");
            eprintln!();
            eprintln!("  If using uv's grpcio-tools, the protoc binary is typically at:");
            eprintln!("    ~/.local/bin/python-grpc-tools-protoc");
            eprintln!();
            eprintln!("  Example: PROTOC=~/.local/bin/python-grpc-tools-protoc cargo check");
            eprintln!();
            std::process::exit(1);
        }
    }

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["../../proto/monopsony.proto"], &["../../proto"])?;
    Ok(())
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}
