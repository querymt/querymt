use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(mut command: Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command.current_dir(root).args(args);
    run(command);
}

fn watched(binary: &Path, root: &Path) -> Vec<PathBuf> {
    let mut command = Command::new(binary);
    command.current_dir(root);
    let output = run(command);
    let paths: Vec<_> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            line.strip_prefix("cargo:rerun-if-changed=")
                .map(PathBuf::from)
        })
        .collect();
    assert!(paths.contains(&PathBuf::from("build.rs")));
    for path in &paths {
        assert!(root.join(path).exists(), "nonexistent watch path: {path:?}");
    }
    paths
}

#[test]
fn git_watch_paths_exist_in_regular_linked_and_archive_checkouts() {
    let fixture =
        Fixture(std::env::temp_dir().join(format!("qmcp-build-script-{}", uuid::Uuid::new_v4())));
    fs::create_dir_all(&fixture.0).unwrap();
    let binary = fixture
        .0
        .join(format!("build-script{}", std::env::consts::EXE_SUFFIX));
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs");
    let mut rustc = Command::new("rustc");
    rustc
        .arg("--edition=2024")
        .arg(&source)
        .arg("-o")
        .arg(&binary);
    run(rustc);

    let repo = fixture.0.join("repo");
    fs::create_dir(&repo).unwrap();
    fs::copy(&source, repo.join("build.rs")).unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    git(&repo, &["add", "build.rs"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    git(&repo, &["pack-refs", "--all"]);
    let regular = watched(&binary, &repo);
    assert!(regular.iter().any(|p| p.ends_with("HEAD")));
    assert!(regular.iter().any(|p| p.ends_with("refs/heads")));
    assert!(regular.iter().any(|p| p.ends_with("packed-refs")));

    let linked = fixture.0.join("linked");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(linked.join(".git").is_file());
    let paths = watched(&binary, &linked);
    assert!(paths.iter().any(|p| p.ends_with("HEAD")));
    assert!(paths.iter().any(|p| p.ends_with("refs/heads")));

    let archive = fixture.0.join("archive");
    fs::create_dir(&archive).unwrap();
    fs::copy(&source, archive.join("build.rs")).unwrap();
    assert_eq!(watched(&binary, &archive), vec![PathBuf::from("build.rs")]);
}
