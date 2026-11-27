use super::*;

/// std's canonicalize gives `\\?\C:\…`: every grant was refused (CI run 42565812901). A dir
/// under %TEMP% (CI's is the 8.3 `RUNNER~1` spelling) and its long form are both accepted.
#[test]
fn extra_roots_accept_plain_and_temp_dirs() {
    let d = tempfile::tempdir().unwrap();
    assert!(d.path().starts_with(std::env::temp_dir()));
    let got = validate_root(d.path()).unwrap();
    assert!(!got.to_string_lossy().starts_with(r"\\?\"), "{got:?}");
    let long = dunce::canonicalize(d.path()).unwrap();
    assert_eq!(validate_root(&long).unwrap(), long);
    let upper = PathBuf::from(long.to_string_lossy().to_uppercase());
    assert!(validate_root(&upper).is_ok(), "case alone isn't a link");
    std::fs::create_dir(long.join("sub")).unwrap();
    assert!(validate_root(&long.join("sub")).is_ok());
}

/// A junction (no privilege needed to make one) is still refused, itself or as an ancestor.
#[test]
fn extra_roots_refuse_junctions() {
    let d = tempfile::tempdir().unwrap();
    let real = dunce::canonicalize(d.path()).unwrap();
    std::fs::create_dir_all(real.join("target").join("inner")).unwrap();
    let ok = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(real.join("link"))
        .arg(real.join("target"))
        .status()
        .unwrap();
    assert!(ok.success());
    for bad in [real.join("link"), real.join("link").join("inner")] {
        let e = validate_root(&bad).unwrap_err();
        assert!(e.starts_with(SANDBOX_INVALID_ROOT), "{bad:?}: {e}");
    }
    assert!(validate_root(&real.join("target").join("inner")).is_ok());
}
