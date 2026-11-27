use super::*;

#[test]
fn enhanced_station_requirement_is_case_insensitive_and_reports_actual_station() {
    for station in ["WinSta0", "winsta0", "WINSTA0"] {
        assert_eq!(check_enhanced_station(station), Ok(()));
    }
    for station in ["Service-0x0-12345$", "OtherStation", ""] {
        let why = check_enhanced_station(station).unwrap_err();
        assert!(why.contains("enhanced sandbox requires WinSta0"));
        assert!(why.contains(&format!("parent window station is {station};")));
        assert!(why.contains("starting the enhanced helper in this station is unsupported"));
    }
}

struct UnsupportedStation;

impl SelfTester for UnsupportedStation {
    fn setup(&self) -> Result<Option<SetupState>, String> {
        // Deliberately stale/nonexistent helper: environment must win over setup validation.
        Ok(Some(SetupState {
            version: SETUP_VERSION,
            offline: Account { name: "offline".into(), sid: "sid".into(), password: "sealed".into() },
            online: Account { name: "online".into(), sid: "sid".into(), password: "sealed".into() },
            group_sid: "group".into(),
            helper: PathBuf::from("nonexistent-helper"),
            helper_sha256: "stale".into(),
            deny_read: vec![],
            deny_read_user: None,
            canary: PathBuf::from("canary"),
            proxy_range: PROXY_PORT_RANGE,
            wfp_filters: 4,
        }))
    }

    fn check_environment(&self) -> Result<(), String> {
        check_enhanced_station("Service-0x0-12345$")
    }

    fn current_exe_sha256(&self) -> Option<String> {
        panic!("unsupported environment must be checked before helper validation")
    }

    fn run(&self, _: &SetupState) -> Result<SelfTest, String> {
        panic!("unsupported station must never launch the enhanced helper")
    }
}

#[test]
fn unsupported_station_enters_existing_capability_fallback_without_launching_helper() {
    let Gate::Fallback(why) = evaluate(&UnsupportedStation) else {
        panic!("unsupported station must make enhanced capabilities unavailable");
    };
    assert_eq!(why, format!("enhanced tier unusable: {}", check_enhanced_station("Service-0x0-12345$").unwrap_err()));
}

#[cfg(windows)]
#[test]
fn real_parent_station_preflight_matches_the_reported_station() {
    let station = crate::winsandbox::desktop::process_station_name().expect("query parent station");
    assert_eq!(win::RealTester.check_environment(), check_enhanced_station(&station));
}
