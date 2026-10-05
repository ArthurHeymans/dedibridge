use super::*;
use serde_json::json;

fn artifact(package: &str, binary: &str, executable: Option<&str>, test: bool) -> String {
    json!({
        "reason": "compiler-artifact",
        "package_id": package,
        "manifest_path": "/workspace/boards/example/Cargo.toml",
        "target": {
            "kind": ["bin"],
            "crate_types": ["bin"],
            "name": binary,
            "src_path": "/workspace/boards/example/src/main.rs",
            "edition": "2024",
            "doc": true,
            "doctest": false,
            "test": true
        },
        "profile": {
            "opt_level": "s",
            "debuginfo": 2,
            "debug_assertions": false,
            "overflow_checks": false,
            "test": test
        },
        "features": [],
        "filenames": [],
        "executable": executable,
        "fresh": true
    })
    .to_string()
}

#[test]
fn selects_the_reported_binary_path_even_for_cached_custom_target_builds() {
    let package = PackageId {
        repr: "path+file:///workspace/boards/example#dedi-example@0.1.0".into(),
    };
    let path = "/custom target directory/custom-triple/release/dedi-example";
    let messages = [
        artifact(
            "other-package",
            "dedi-example",
            Some("/wrong-package"),
            false,
        ),
        artifact(&package.repr, "other-bin", Some("/wrong-bin"), false),
        artifact(&package.repr, "dedi-example", Some("/test-harness"), true),
        artifact(&package.repr, "dedi-example", None, false),
        artifact(&package.repr, "dedi-example", Some(path), false),
        r#"{"reason":"build-finished","success":true}"#.into(),
    ]
    .join("\n");
    assert_eq!(
        read_executable(messages.as_bytes(), &package, "dedi-example").unwrap(),
        Some(PathBuf::from(path))
    );
}

#[test]
fn missing_or_ambiguous_executables_are_not_guessed() {
    let package = PackageId {
        repr: "example".into(),
    };
    assert!(
        read_executable(&b""[..], &package, "example")
            .unwrap()
            .is_none()
    );
    let messages = [
        artifact("example", "example", Some("/first"), false),
        artifact("example", "example", Some("/second"), false),
    ]
    .join("\n");
    assert!(read_executable(messages.as_bytes(), &package, "example").is_err());
}

#[test]
fn host_arguments_are_forwarded_without_parsing_or_shell_expansion() {
    let cli = Cli::try_parse_from([
        "xtask",
        "host",
        "--socket",
        "/tmp/board a.sock",
        "console",
        "--baud",
        "115200",
    ])
    .unwrap();
    let Task::Host { args } = cli.task else {
        panic!("expected host task");
    };
    assert_eq!(
        args,
        [
            "--socket",
            "/tmp/board a.sock",
            "console",
            "--baud",
            "115200"
        ]
        .map(OsString::from)
    );
    let cli = Cli::try_parse_from(["xtask", "host", "--", "--help"]).unwrap();
    let Task::Host { args } = cli.task else {
        panic!("expected host task");
    };
    assert_eq!(args, [OsString::from("--help")]);
}

#[test]
fn firmware_tasks_require_a_known_board_and_fmt_supports_check() {
    assert!(Cli::try_parse_from(["xtask", "flash"]).is_err());
    assert!(Cli::try_parse_from(["xtask", "build", "unknown"]).is_err());
    assert!(matches!(
        Cli::try_parse_from(["xtask", "build", "ch32v307"])
            .unwrap()
            .task,
        Task::Build {
            board: Board::Ch32v307
        }
    ));
    assert!(matches!(
        Cli::try_parse_from(["xtask", "fmt", "--check"])
            .unwrap()
            .task,
        Task::Fmt { check: true }
    ));
}
