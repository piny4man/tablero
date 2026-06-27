use std::process::ExitCode;

use tablero_wayland::{SurfaceConfig, run};

fn main() -> ExitCode {
    env_logger::init();

    match run(SurfaceConfig::default()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tablero: {e}");
            ExitCode::FAILURE
        }
    }
}
