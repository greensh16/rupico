use predicates::str::contains;

#[test]
fn help_runs_and_mentions_rupico() {
    let mut cmd = assert_cmd::cargo_bin_cmd!("rupico");
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(contains("Rust MicroPython helper for boards like the Pico"));
}

#[test]
fn ports_runs_without_port_flag() {
    let mut cmd = assert_cmd::cargo_bin_cmd!("rupico");
    cmd.arg("ports");
    cmd.assert().success();
}

#[test]
fn commands_requiring_port_fail_without_it() {
    let mut cmd = assert_cmd::cargo_bin_cmd!("rupico");
    cmd.args(["ls"]);
    cmd.assert()
        .failure()
        .stderr(contains("--port is required for this command"));
}
