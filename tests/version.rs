use std::process::Command;

#[test]
fn version_prints_package_version() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_aequimuta"))
        .arg("version")
        .output()?;

    assert!(
        output.status.success(),
        "version command failed with status {} and stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let expected = format!("Aequimuta {}\n", env!("CARGO_PKG_VERSION"));
    assert_eq!(output.stdout, expected.as_bytes());
    assert!(
        output.stderr.is_empty(),
        "version command wrote to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(())
}
