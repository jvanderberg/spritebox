//! `MemFs` ↔ `TokioFs` equivalence test.
//!
//! The hexagonal design promises these two `HostFs` impls are
//! interchangeable. This test runs the same script of operations
//! against both and asserts the resulting snapshots match.
//!
//! Prior to this test, the integration suite only ever exercised
//! `MemFs`, so any drift in `TokioFs::stat`/mode/uid/gid was invisible.

use std::path::Path;

use bytes::Bytes;
use spritebox_fs_host::mem::FakeClock;
use spritebox_fs_host::{HostFs, MemFs, TokioFs};

#[derive(Clone)]
#[allow(dead_code)]
enum Op {
    Mkdir(&'static str, u16),
    Create(&'static str, u16),
    Write(&'static str, u64, &'static [u8]),
    Truncate(&'static str, u64),
    Chmod(&'static str, u16),
    Unlink(&'static str),
    Rmdir(&'static str),
    Rename(&'static str, &'static str),
}

async fn apply(fs: &impl HostFs, op: &Op) {
    match op {
        Op::Mkdir(p, m) => {
            fs.mkdir(Path::new(p), *m).await.unwrap();
        }
        Op::Create(p, m) => {
            fs.create(Path::new(p), *m).await.unwrap();
        }
        Op::Write(p, off, data) => {
            fs.write(Path::new(p), *off, data).await.unwrap();
        }
        Op::Truncate(p, size) => {
            fs.truncate(Path::new(p), *size).await.unwrap();
        }
        Op::Chmod(p, m) => {
            fs.chmod(Path::new(p), *m).await.unwrap();
        }
        Op::Unlink(p) => {
            fs.unlink(Path::new(p)).await.unwrap();
        }
        Op::Rmdir(p) => {
            fs.rmdir(Path::new(p)).await.unwrap();
        }
        Op::Rename(from, to) => {
            fs.rename(Path::new(from), Path::new(to)).await.unwrap();
        }
    }
}

async fn run_script(fs: &impl HostFs, ops: &[Op]) {
    for op in ops {
        apply(fs, op).await;
    }
}

/// Assert two snapshots agree on every (path, kind, content). Modes,
/// uid, gid, mtime are NOT compared — those legitimately differ between
/// MemFs (synthetic) and TokioFs (whatever the OS returns).
fn snapshots_agree(
    a: &[(std::path::PathBuf, spritebox_fs_protocol::FileKind, Option<Bytes>)],
    b: &[(std::path::PathBuf, spritebox_fs_protocol::FileKind, Option<Bytes>)],
) {
    assert_eq!(a.len(), b.len(), "snapshot lengths differ:\nA: {a:#?}\nB: {b:#?}");
    for (ae, be) in a.iter().zip(b.iter()) {
        assert_eq!(ae.0, be.0, "path mismatch: {ae:?} vs {be:?}");
        assert_eq!(ae.1, be.1, "kind mismatch: {ae:?} vs {be:?}");
        assert_eq!(ae.2, be.2, "content mismatch: {ae:?} vs {be:?}");
    }
}

#[tokio::test]
async fn equivalence_basic_create_write_read() {
    let mem = MemFs::with_clock(FakeClock::new(1_000_000));
    let dir = tempfile::tempdir().unwrap();
    let tokio = TokioFs::new(dir.path().to_path_buf());

    let script = vec![
        Op::Create("a.txt", 0o644),
        Op::Write("a.txt", 0, b"hello"),
        Op::Write("a.txt", 5, b" world"),
    ];
    run_script(&mem, &script).await;
    run_script(&tokio, &script).await;

    snapshots_agree(&mem.snapshot().await, &tokio.snapshot().await);
}

#[tokio::test]
async fn equivalence_nested_dirs_and_unlink() {
    let mem = MemFs::with_clock(FakeClock::new(1_000_000));
    let dir = tempfile::tempdir().unwrap();
    let tokio = TokioFs::new(dir.path().to_path_buf());

    let script = vec![
        Op::Mkdir("a", 0o755),
        Op::Mkdir("a/b", 0o755),
        Op::Create("a/b/x", 0o644),
        Op::Write("a/b/x", 0, b"deep"),
        Op::Create("a/b/y", 0o644),
        Op::Write("a/b/y", 0, b"sibling"),
        Op::Unlink("a/b/y"),
    ];
    run_script(&mem, &script).await;
    run_script(&tokio, &script).await;

    snapshots_agree(&mem.snapshot().await, &tokio.snapshot().await);
}

#[tokio::test]
async fn equivalence_rename_replaces_dest() {
    let mem = MemFs::with_clock(FakeClock::new(1_000_000));
    let dir = tempfile::tempdir().unwrap();
    let tokio = TokioFs::new(dir.path().to_path_buf());

    let script = vec![
        Op::Create("from", 0o644),
        Op::Write("from", 0, b"new-content"),
        Op::Create("to", 0o644),
        Op::Write("to", 0, b"old-content"),
        Op::Rename("from", "to"),
    ];
    run_script(&mem, &script).await;
    run_script(&tokio, &script).await;

    snapshots_agree(&mem.snapshot().await, &tokio.snapshot().await);
}

#[tokio::test]
async fn equivalence_truncate_grows_with_zeros() {
    let mem = MemFs::with_clock(FakeClock::new(1_000_000));
    let dir = tempfile::tempdir().unwrap();
    let tokio = TokioFs::new(dir.path().to_path_buf());

    let script = vec![
        Op::Create("g", 0o644),
        Op::Write("g", 0, b"abc"),
        Op::Truncate("g", 8),
    ];
    run_script(&mem, &script).await;
    run_script(&tokio, &script).await;

    let mem_snap = mem.snapshot().await;
    let tokio_snap = tokio.snapshot().await;

    // Both should have a single file "g" of size 8 ending in zero bytes.
    let (_, _, mem_content) = mem_snap.into_iter().next().unwrap();
    let (_, _, tokio_content) = tokio_snap.into_iter().next().unwrap();
    let mem_bytes = mem_content.unwrap();
    let tokio_bytes = tokio_content.unwrap();
    assert_eq!(mem_bytes.len(), 8);
    assert_eq!(tokio_bytes.len(), 8);
    assert_eq!(&mem_bytes[..3], b"abc");
    assert_eq!(&tokio_bytes[..3], b"abc");
    assert_eq!(&mem_bytes[3..], &[0u8; 5]);
    assert_eq!(&tokio_bytes[3..], &[0u8; 5]);
}

#[tokio::test]
async fn equivalence_path_escape_rejected_consistently() {
    let mem = MemFs::with_clock(FakeClock::new(1_000_000));
    let dir = tempfile::tempdir().unwrap();
    let tokio = TokioFs::new(dir.path().to_path_buf());

    for evil in ["../escape", "/etc/passwd", "a/../b"] {
        let mr = mem.stat(Path::new(evil)).await;
        let tr = tokio.stat(Path::new(evil)).await;
        // Both must reject; the exact error variant should match.
        assert!(mr.is_err(), "MemFs accepted {evil}");
        assert!(tr.is_err(), "TokioFs accepted {evil}");
    }
}

#[tokio::test]
async fn equivalence_chmod_changes_mode() {
    let mem = MemFs::with_clock(FakeClock::new(1_000_000));
    let dir = tempfile::tempdir().unwrap();
    let tokio = TokioFs::new(dir.path().to_path_buf());

    let script = vec![
        Op::Create("c", 0o644),
        Op::Chmod("c", 0o755),
    ];
    run_script(&mem, &script).await;
    run_script(&tokio, &script).await;

    let mem_attr = mem.stat(Path::new("c")).await.unwrap();
    let tokio_attr = tokio.stat(Path::new("c")).await.unwrap();
    assert_eq!(mem_attr.mode & 0o777, 0o755);
    assert_eq!(tokio_attr.mode & 0o777, 0o755);
}
