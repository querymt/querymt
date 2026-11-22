//! Pins the Ajika 0.2.1 behaviours that the code cache design relies on (design D2, D5, D6).
//!
//! If one of these fails after an Ajika upgrade, the matching design decision must be revisited.

use std::{fs, path::PathBuf, time::Duration};

use ajika::{
    Database, DatabaseOptions, ErrorKind, InputInit, Inputs, PersistenceErrorKind, Retention,
};

type Db<'a> = ajika::Db<'a, ()>;

struct WorkspaceRoot(PathBuf);

// (e) The input declaration syntax used by the cache: one `Arc`-shared optional text, `None`
// when the file cannot be read, with a lazy initializer reading under `Inputs::with_state`.
// Readability and text come from one read, exactly as in production.
#[ajika::input(key = String)]
struct ContractFile {
    #[input(arc, initialize = read_text)]
    text: Option<String>,
}

fn read_text(key: &String, cx: &dyn InputInit) -> ajika::Result<Option<String>> {
    cx.check_cancelled()?;
    let root = cx.state::<WorkspaceRoot>()?;
    Ok(fs::read_to_string(root.0.join(key)).ok())
}

#[ajika::query]
fn line_count(db: &Db, path: String) -> ajika::Result<usize> {
    let text = ContractFile::at(path).text(db)?;
    Ok(text.as_deref().map_or(0, |text| text.lines().count()))
}

fn options() -> DatabaseOptions {
    DatabaseOptions::new(Retention {
        max_bytes: 16 * 1024 * 1024,
        max_entries: 1024,
        max_idle: Duration::from_secs(3600),
    })
}

fn inputs(root: &std::path::Path) -> Inputs {
    Inputs::with_state(WorkspaceRoot(root.to_owned()))
}

fn open(storage: &std::path::Path, root: &std::path::Path, epoch: &str) -> ajika::Result<Database> {
    Database::open(storage, epoch, inputs(root), (), options())
}

fn workspace(content: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    fs::create_dir_all(&root).expect("create root");
    fs::write(root.join("a.txt"), content).expect("write file");
    let storage = dir.path().join("cache");
    (dir, root, storage)
}

/// (a) Opening an already-open directory fails with `PersistenceErrorKind::InUse`.
/// Design D5 maps exactly this kind to the in-memory fallback.
#[test]
fn ajika_contract_second_open_reports_in_use() {
    let (_dir, root, storage) = workspace("one\ntwo\n");
    let _first = open(&storage, &root, "epoch").expect("first open");

    let Err(error) = open(&storage, &root, "epoch") else {
        panic!("a second owner must not open the same directory");
    };
    match error.kind() {
        ErrorKind::Persistence(persistence) => {
            assert_eq!(persistence.kind(), PersistenceErrorKind::InUse)
        }
        other => panic!("expected a persistence error, got {other:?}"),
    }
}

/// (b) After the host forgets a text, re-acquiring equal text through the initializer
/// validates the stored result without executing the query.
#[test]
fn ajika_contract_forget_then_reacquire_equal_text_validates_without_executing() {
    let (_dir, root, storage) = workspace("one\ntwo\n");
    let mut db = open(&storage, &root, "epoch").expect("open");
    let file = ContractFile::at("a.txt".to_owned());

    db.update_inputs(|update| file.set_text(update, Some("one\ntwo\n".to_owned())))
        .expect("set inputs");
    assert_eq!(
        *db.run(|db| line_count(db, "a.txt".into())).expect("run"),
        2
    );
    assert_eq!(db.statistics().executed, 1);

    db.update_inputs(|update| {
        update.forget(ContractFile::TEXT, "a.txt".to_owned());
        Ok(())
    })
    .expect("forget");
    assert_eq!(
        *db.run(|db| line_count(db, "a.txt".into())).expect("run"),
        2
    );
    assert_eq!(
        db.statistics().executed,
        0,
        "equal re-acquired text must validate, not execute"
    );

    // Control: a changed file executes again after forgetting.
    fs::write(root.join("a.txt"), "one\ntwo\nthree\n").expect("rewrite");
    db.update_inputs(|update| {
        update.forget(ContractFile::TEXT, "a.txt".to_owned());
        Ok(())
    })
    .expect("forget");
    assert_eq!(
        *db.run(|db| line_count(db, "a.txt".into())).expect("run"),
        3
    );
    assert_eq!(db.statistics().executed, 1);
}

/// A file that was unreadable (`None`) and is created later: after the host forgets the
/// observation, the query runs again and sees the new text.
#[test]
fn ajika_contract_forget_after_unreadable_then_created_executes_again() {
    let (_dir, root, storage) = workspace("one\ntwo\n");
    let mut db = open(&storage, &root, "epoch").expect("open");
    let forget = |db: &mut Database| {
        db.update_inputs(|update| {
            update.forget(ContractFile::TEXT, "late.txt".to_owned());
            Ok(())
        })
        .expect("forget");
    };

    // Acquired through the initializer while the file does not exist: `None`.
    assert_eq!(
        *db.run(|db| line_count(db, "late.txt".into())).expect("run"),
        0
    );
    assert_eq!(db.statistics().executed, 1);
    forget(&mut db);

    fs::write(root.join("late.txt"), "a\nb\nc\n").expect("create file");
    assert_eq!(
        *db.run(|db| line_count(db, "late.txt".into())).expect("run"),
        3
    );
    assert_eq!(db.statistics().executed, 1, "the new text must be seen");

    // Control: equal text after forgetting still validates without executing.
    forget(&mut db);
    assert_eq!(
        *db.run(|db| line_count(db, "late.txt".into())).expect("run"),
        3
    );
    assert_eq!(db.statistics().executed, 0);
}

/// (c) Setting equal text neither executes nor invalidates the stored result
/// (`validated == 0`), i.e. it does not create a revision that anything depends on.
/// Design D6 keeps the owner-side comparison anyway; this records the observed behaviour.
#[test]
fn ajika_contract_setting_equal_text_does_not_invalidate() {
    let (_dir, root, storage) = workspace("one\ntwo\n");
    let mut db = open(&storage, &root, "epoch").expect("open");
    let file = ContractFile::at("a.txt".to_owned());
    let set = |db: &mut Database, text: &str| {
        let text = text.to_owned();
        db.update_inputs(|update| file.set_text(update, Some(text)))
            .expect("set inputs");
    };

    set(&mut db, "one\ntwo\n");
    db.run(|db| line_count(db, "a.txt".into())).expect("run");

    set(&mut db, "one\ntwo\n");
    db.run(|db| line_count(db, "a.txt".into())).expect("run");
    let stats = db.statistics();
    assert_eq!(stats.executed, 0);
    assert_eq!(stats.validated, 0, "an equal assignment preserves validity");

    set(&mut db, "one\ntwo\nthree\n");
    assert_eq!(
        *db.run(|db| line_count(db, "a.txt".into())).expect("run"),
        3
    );
    assert_eq!(db.statistics().executed, 1);
}

/// (d) Reopening with a different epoch discards the stored results; the same epoch keeps them.
#[test]
fn ajika_contract_epoch_change_executes_again() {
    let (_dir, root, storage) = workspace("one\ntwo\n");
    {
        let mut db = open(&storage, &root, "epoch-1").expect("open");
        assert_eq!(
            *db.run(|db| line_count(db, "a.txt".into())).expect("run"),
            2
        );
        assert_eq!(db.statistics().executed, 1);
    }
    {
        let mut db = open(&storage, &root, "epoch-1").expect("reopen same epoch");
        assert_eq!(
            *db.run(|db| line_count(db, "a.txt".into())).expect("run"),
            2
        );
        assert_eq!(db.statistics().executed, 0, "same epoch reuses the result");
    }
    {
        let mut db = open(&storage, &root, "epoch-2").expect("reopen new epoch");
        assert_eq!(
            *db.run(|db| line_count(db, "a.txt".into())).expect("run"),
            2
        );
        assert_eq!(db.statistics().executed, 1, "new epoch recomputes");
    }
}
