//! `aerosync receipt` with an isolated XDG home (no real `receipts.db`).

use assert_cmd::prelude::*;
use std::process::Command;
use tempfile::TempDir;

fn isolated_aerosync(home: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("aerosync").expect("aerosync binary");
    cmd.env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"));
    cmd
}

#[test]
fn receipt_list_succeeds_with_no_journal_file() {
    let home = TempDir::new().unwrap();
    isolated_aerosync(&home)
        .args(["receipt", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("No receipt journal"));
}

#[test]
fn receipt_subcommand_help() {
    let home = TempDir::new().unwrap();
    isolated_aerosync(&home)
        .args(["receipt", "list", "--help"])
        .assert()
        .success();
}
