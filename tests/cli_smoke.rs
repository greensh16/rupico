use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn help_runs_and_mentions_rupico() {
    let mut cmd = Command::cargo_bin("rupico").expect("binary rupico not found");
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(contains("Rust MicroPython helper for boards like the Pico"));
}

#[test]
fn ports_runs_without_port_flag() {
    let mut cmd = Command::cargo_bin("rupico").expect("binary rupico not found");
    cmd.arg("ports");
    cmd.assert().success();
}

#[test]
fn commands_requiring_port_fail_without_it() {
    let mut cmd = Command::cargo_bin("rupico").expect("binary rupico not found");
    cmd.args(["ls"]);
    cmd.assert()
        .failure()
        .stderr(contains("--port is required for this command"));
}
