//! Freeze the top-level CLI surface described in
//! `docs/v0.3.0-frozen-api.md` §10.

use std::process::Command;

fn run_cli(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_aerosync"))
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run aerosync {:?}: {err}", args));

    assert!(
        output.status.success(),
        "aerosync {:?} failed\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn extract_command_names(help: &str) -> Vec<String> {
    let mut in_commands = false;
    let mut names = Vec::new();

    for line in help.lines() {
        let trimmed = line.trim();
        if trimmed == "Commands:" {
            in_commands = true;
            continue;
        }

        if !in_commands {
            continue;
        }

        if trimmed.is_empty() {
            if !names.is_empty() {
                break;
            }
            continue;
        }

        let candidate = trimmed.split_whitespace().next().unwrap_or_default();
        if candidate.starts_with('-') {
            break;
        }

        if candidate
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch == '-')
        {
            names.push(candidate.to_string());
        }
    }

    names
}

#[test]
fn top_level_help_lists_frozen_commands() {
    let help = run_cli(&["--help"]);
    let mut commands = extract_command_names(&help);
    commands.retain(|name| name != "help");

    let expected = [
        "send", "receive", "token", "status", "resume", "history", "receipt", "watch", "discover",
    ];

    for name in expected {
        assert!(
            commands.iter().any(|cmd| cmd == name),
            "missing top-level command `{name}` in help output:\n{help}"
        );
    }

    assert_eq!(
        commands.len(),
        expected.len(),
        "unexpected top-level command set: {commands:?}"
    );
}

#[test]
fn subcommand_help_still_resolves() {
    for subcommand in [
        "send", "receive", "token", "status", "resume", "history", "watch", "discover",
    ] {
        let help = run_cli(&[subcommand, "--help"]);
        assert!(
            help.contains("Usage:"),
            "expected clap usage block in `{subcommand} --help` output:\n{help}"
        );
    }
}

#[test]
fn version_matches_package_version() {
    let version = run_cli(&["--version"]);
    assert!(
        version.contains(env!("CARGO_PKG_VERSION")),
        "expected CLI version output to contain {}, got:\n{}",
        env!("CARGO_PKG_VERSION"),
        version
    );
}
