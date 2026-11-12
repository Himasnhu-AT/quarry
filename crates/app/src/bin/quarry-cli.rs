//! `quarry-cli commands | exec <id> [json] | run <file> | mcp | serve | version`, and
//! `--connect PORT` to drive the running window. Human-only actions (approve, deny, run on the
//! human connection, commit) are not commands, so they are not reachable from here.

fn main() -> std::process::ExitCode {
    ashlar::app::cli_main(quarry::app())
}
