use gitbuddy::git::{ExternalEditor, Repository};
use std::{
    fs,
    time::{Duration, Instant},
};

#[cfg(unix)]
#[test]
fn editor_passes_literal_file_path_without_shell() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(&dir.path().join("repo")).unwrap();
    let file = repo.root.join("file $(touch SHOULD_NOT_EXIST).txt");
    fs::write(&file, "content").unwrap();
    let output = dir.path().join("result");
    let program = dir.path().join("editor script");
    fs::write(&program, "#!/bin/sh\nprintf '%s' \"$2\" > \"$1\"\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let tool = ExternalEditor {
        program: program.to_string_lossy().into(),
        args: vec![output.to_string_lossy().into(), "$FILE".into()],
    };
    repo.launch_editor(file.file_name().unwrap().as_ref(), &tool)
        .unwrap();
    let start = Instant::now();
    while !output.exists() && start.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(fs::read_to_string(output).unwrap(), file.to_string_lossy());
    assert!(!repo.root.join("SHOULD_NOT_EXIST").exists());
    assert!(
        ExternalEditor {
            program: "editor".into(),
            args: vec!["prefix$FILE".into()]
        }
        .validate()
        .is_err()
    );
    assert!(repo.launch_editor("../outside".as_ref(), &tool).is_err());
}
