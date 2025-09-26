use std::{fs, path::Path, process::Command};

use insta::assert_snapshot;
use tempfile::tempdir;

use crate::rojo_test::io_util::{get_working_dir_path, PATCH_TESTS_PATH, ROJO_PATH};

macro_rules! patch_tests {
    ( $($test_name: ident,)* ) => {
        $(
            #[test]
            fn $test_name() {
                let _ = env_logger::try_init();

                run_patch_test(stringify!($test_name));
            }
        )*
    };
}

patch_tests! {
    baseplate,
    script_update,
}

fn run_patch_test(test_name: &str) {
    let working_dir = get_working_dir_path();

    let test_dir = Path::new(PATCH_TESTS_PATH).join(test_name);

    let project_path = test_dir.join("project");
    let mut is_place = true;
    let mut input_path = test_dir.join("input.rbxl");

    if !input_path.exists() {
        input_path.set_extension("rbxm");
        is_place = false;
    }

    let output_dir = tempdir().expect("couldn't create temporary directory");
    let output_path = output_dir.path().join(if is_place {
        "output.rbxlx"
    } else {
        "output.rbxmx"
    });

    let output = Command::new(ROJO_PATH)
        .args([
            "patch",
            project_path.to_str().unwrap(),
            "--input",
            input_path.to_str().unwrap(),
            "--output",
            output_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "error")
        .current_dir(working_dir)
        .output()
        .expect("Couldn't start Rojo");

    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));

    assert!(output.status.success(), "Rojo did not exit successfully");

    let contents = fs::read_to_string(&output_path).expect("Couldn't read output file");

    let mut settings = insta::Settings::new();

    let snapshot_path = Path::new(PATCH_TESTS_PATH)
        .parent()
        .unwrap()
        .join("patch-test-snapshots");

    settings.set_snapshot_path(snapshot_path);

    settings.bind(|| {
        assert_snapshot!(test_name, contents);
    });
}
