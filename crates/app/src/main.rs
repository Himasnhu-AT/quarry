//! Quarry in a window. `--headless --snap out.png --tree tree.json` renders without one;
//! `--scene NAME` opens a named screen (`--list-scenes`).

fn main() -> std::process::ExitCode {
    match quarry::app().run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("quarry: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
