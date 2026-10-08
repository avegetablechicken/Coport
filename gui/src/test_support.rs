//! Isolated credentials for data API tests on every supported platform.

pub const DATA_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
pub const DATA_KEY_ENV: &str = "COPORT_TEST_EXTERNAL_DATA_KEY";

/// Re-run exactly one test with a synthetic environment key. Setting it on the
/// child avoids mutating the environment of the parallel, multithreaded test
/// runner. Windows intentionally cannot use unchecked private token files.
/// Returns true in the parent after the child has completed all assertions.
pub fn data_key_in_subprocess(test: &str) -> bool {
    const CASE_ENV: &str = "COPORT_TEST_EXTERNAL_DATA_CASE";
    if std::env::var(CASE_ENV).as_deref() == Ok(test) {
        assert_eq!(std::env::var(DATA_KEY_ENV).unwrap(), DATA_KEY);
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CASE_ENV, test)
        .env(DATA_KEY_ENV, DATA_KEY)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{test}:\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("test result: ok. 1 passed;"),
        "The child must execute exactly one test: {stdout}"
    );
    true
}
