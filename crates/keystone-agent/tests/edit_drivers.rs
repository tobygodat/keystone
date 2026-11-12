//! Both drivers against recordings of real file edits: Claude Code 2.1.322's Edit and
//! Write results (`tool_use_result.structuredPatch`) and codex-cli 0.167.1's `fileChange`
//! item (`changes[].diff`). `scripts/probe-edits.py` made the recordings and the fixtures
//! in `tests/fixtures/edits/`. The repository started with `a.txt` (one … five),
//! `b.txt` (red, green, blue) and `d.txt` (old), and each expected diff below is the edit
//! the probe asked for.

use keystone_agent::claude::ClaudeDriver;
use keystone_agent::codex::CodexDriver;
use keystone_agent::diff::{Change, FileDiff, Kind, Patch, shown_path};
use keystone_agent::{Command, Event, Item, ItemKind, ItemStatus, Outputs};
use keystone_core::ApprovalMode;

const SESSION: u64 = 5;
const SEED: u64 = 0x6b65_7973_746f_6e65;
const THREAD: &str = "01a1418d-549d-7553-bd08-4183fac44aa7";
const CLAUDE: &str = include_str!("fixtures/edits/claude-2.1.322-edits.jsonl");
const CODEX: &str = include_str!("fixtures/edits/codex-0.167.1-edits.jsonl");

fn send() -> Command {
    Command::Send {
        session: SESSION,
        text: "edit".into(),
        images: Vec::new(),
    }
}

/// Each file change the driver completed, in order.
fn file_changes(out: &Outputs) -> Vec<(Vec<String>, Option<Patch>, ItemStatus)> {
    out.events
        .iter()
        .filter_map(|(_, event)| match event {
            Event::ItemCompleted(Item {
                kind: ItemKind::FileChange { paths, patch },
                status,
                ..
            }) => Some((paths.clone(), patch.clone(), *status)),
            _ => None,
        })
        .collect()
}

/// A file's lines as marked text: ` a`, `-b`, `+c`.
fn marked(file: &FileDiff) -> Vec<String> {
    file.hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .map(|line| {
            let mark = match line.kind {
                Kind::Context => ' ',
                Kind::Added => '+',
                Kind::Removed => '-',
            };
            format!("{mark}{}", line.text)
        })
        .collect()
}

fn claude() -> Outputs {
    claude_replay(CLAUDE)
}

fn claude_replay(fixture: &str) -> Outputs {
    let mut out = Outputs::default();
    let mut driver = ClaudeDriver::new(SESSION, false, SEED, ApprovalMode::Provider, &mut out);
    driver.on_line(
        br#"{"type":"control_response","response":{"subtype":"success","request_id":"keystone-1","response":{"current_permission_mode":"default"}}}"#,
        &mut out,
    );
    for line in fixture.lines() {
        // Read as text, since a test line may hold JSON the driver must survive.
        if line.contains(r#""type":"command_lifecycle""#) && line.contains(r#""state":"queued""#) {
            driver.command(send(), &mut out);
        }
        let before = out.events.len();
        driver.on_line(line.as_bytes(), &mut out);
        let approvals: Vec<_> = out.events[before..]
            .iter()
            .filter_map(|(_, event)| match event {
                Event::ApprovalRequested(approval) => Some(approval.request.clone()),
                _ => None,
            })
            .collect();
        for request in approvals {
            let answer = Command::Answer {
                session: SESSION,
                request,
                allow: true,
            };
            driver.command(answer, &mut out);
        }
    }
    out
}

#[test]
fn each_claude_edit_and_write_keeps_its_own_diff() {
    let changes = file_changes(&claude());
    let summary: Vec<_> = changes
        .iter()
        .map(|(paths, patch, status)| {
            let patch = patch.as_ref().expect("every edit carries its patch");
            assert_eq!(patch.files.len(), 1);
            assert!(!patch.truncated);
            let file = &patch.files[0];
            assert_eq!(paths, std::slice::from_ref(&file.path));
            assert_eq!(*status, ItemStatus::Completed);
            (
                shown_path(&file.path, "/tmp/probe/claude-edits").into_owned(),
                file.change,
                marked(file),
            )
        })
        .collect();
    let lines = |lines: &[&str]| lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            (
                "a.txt".into(),
                Change::Modified,
                lines(&[" one", "-two", "+TWO", " three", " four", " five"])
            ),
            (
                "b.txt".into(),
                Change::Modified,
                lines(&["-red", "+RED", " green", " blue"])
            ),
            ("c.txt".into(), Change::Added, lines(&["+hello", "+world"])),
            ("d.txt".into(), Change::Modified, lines(&["-old", "+new"])),
            (
                "b.txt".into(),
                Change::Modified,
                lines(&[" RED", " green", "-blue", "+BLUE"])
            ),
        ]
    );
}

#[test]
fn claude_numbers_each_side_of_an_edit_from_its_hunk() {
    let changes = file_changes(&claude());
    let a = &changes[0].1.as_ref().unwrap().files[0].hunks[0];
    assert_eq!(a.header, "@@ -1,5 +1,5 @@");
    let numbers: Vec<_> = a.lines.iter().map(|line| (line.old, line.new)).collect();
    assert_eq!(
        numbers,
        [
            (Some(1), Some(1)),
            (Some(2), None),
            (None, Some(2)),
            (Some(3), Some(3)),
            (Some(4), Some(4)),
            (Some(5), Some(5)),
        ]
    );
    let created = &changes[2].1.as_ref().unwrap().files[0].hunks[0];
    assert_eq!(created.header, "@@ -0,0 +1,2 @@");
}

fn codex() -> Outputs {
    codex_replay(CODEX)
}

fn codex_replay(fixture: &str) -> Outputs {
    let mut out = Outputs::default();
    let mut driver = CodexDriver::new("0.0.0-test", &mut out);
    driver.command(
        Command::Open {
            session: SESSION,
            cwd: "/tmp/probe".into(),
            resume: None,
        },
        &mut out,
    );
    driver.on_line(br#"{"id":1,"result":{"userAgent":"test"}}"#, &mut out);
    let response = format!(r#"{{"id":2,"result":{{"thread":{{"id":"{THREAD}"}}}}}}"#);
    driver.on_line(response.as_bytes(), &mut out);
    driver.command(send(), &mut out);
    let mut out = Outputs::default();
    for line in fixture.lines() {
        driver.on_line(line.as_bytes(), &mut out);
    }
    out
}

#[test]
fn a_codex_file_change_keeps_each_files_diff() {
    let changes = file_changes(&codex());
    assert_eq!(changes.len(), 1);
    let (paths, patch, status) = &changes[0];
    assert_eq!(*status, ItemStatus::Completed);
    let patch = patch.as_ref().expect("the change carries its patch");
    assert!(!patch.truncated);
    let summary: Vec<_> = patch
        .files
        .iter()
        .map(|file| {
            let name = shown_path(&file.path, "/tmp/probe/codex-edits");
            (name.into_owned(), file.change, marked(file))
        })
        .collect();
    let lines = |lines: &[&str]| lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            (
                "a.txt".into(),
                Change::Modified,
                lines(&[" one", "-two", "+TWO", " three"])
            ),
            ("c.txt".into(), Change::Added, lines(&["+hello", "+world"])),
            ("d.txt".into(), Change::Deleted, lines(&["-old"])),
        ]
    );
    let files: Vec<_> = patch.files.iter().map(|f| f.path.clone()).collect();
    assert_eq!(paths, &files);
    assert_eq!(patch.files[0].hunks[0].header, "@@ -1,3 +1,3 @@");
}

#[test]
fn an_edit_survives_the_log_round_trip() {
    let mut changes = file_changes(&claude());
    changes.extend(file_changes(&codex()));
    for (paths, patch, status) in changes {
        let item = Item {
            id: "edit".into(),
            kind: ItemKind::FileChange { paths, patch },
            text: String::new(),
            status,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert_eq!(serde_json::from_str::<Item>(&json).unwrap(), item);
    }
}

#[test]
fn a_file_change_logged_before_patches_reads_without_one() {
    let old = r#"{"id":"e1","kind":{"type":"file_change","paths":["src/lib.rs"]},"text":"","status":"completed"}"#;
    let item: Item = serde_json::from_str(old).unwrap();
    assert_eq!(
        item.kind,
        ItemKind::FileChange {
            paths: vec!["src/lib.rs".into()],
            patch: None
        }
    );
}

/// The recording with the first Edit result's `structuredPatch` swapped for `patch`, raw
/// JSON text.
fn claude_with_patch(patch: &str) -> String {
    let mut swapped = false;
    CLAUDE
        .lines()
        .map(|line| {
            let start = line.find("\"structuredPatch\":[{");
            match start {
                Some(start) if !swapped => {
                    swapped = true;
                    let end = start + line[start..].find("}]").unwrap() + 2;
                    format!(
                        "{}\"structuredPatch\":{patch}{}",
                        &line[..start],
                        &line[end..]
                    )
                }
                _ => line.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_claude_patch_that_does_not_parse_costs_only_the_patch() {
    for (case, patch) in [
        (
            "a lone surrogate",
            r#"[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-\ud800","+ok"]}]"#,
        ),
        (
            "a start past u32::MAX",
            r#"[{"oldStart":4294967295,"oldLines":2,"newStart":1,"newLines":2,"lines":[" a"," b"]}]"#,
        ),
        ("not a list", r#""patch""#),
        ("a hunk with no lines", r#"[{"oldStart":1,"newStart":1}]"#),
    ] {
        let changes = file_changes(&claude_replay(&claude_with_patch(patch)));
        assert_eq!(changes.len(), 5, "{case}: every edit still completes");
        let (paths, patch, status) = &changes[0];
        assert!(
            paths[0].ends_with("/a.txt"),
            "{case}: the edit keeps its path"
        );
        assert_eq!(*status, ItemStatus::Completed, "{case}");
        assert_eq!(*patch, None, "{case}: only the patch is dropped");
        assert!(
            changes[1].1.is_some(),
            "{case}: the next edit keeps its patch"
        );
    }
}

#[test]
fn a_codex_diff_that_is_not_text_or_numbers_past_the_end_costs_only_that_diff() {
    let item = |changes: &str| {
        format!(
            r#"{{"method":"item/completed","params":{{"threadId":"{THREAD}","turnId":"t","item":{{"type":"fileChange","id":"fc","status":"completed","changes":{changes}}}}}}}"#
        )
    };
    let good = r#"{"path":"/r/a.txt","kind":{"type":"add"},"diff":"hi\n"}"#;
    for (case, bad, kept) in [
        (
            "a number",
            r#"{"path":"/r/b.txt","kind":{"type":"update"},"diff":42}"#,
            1,
        ),
        (
            "an object",
            r#"{"path":"/r/b.txt","kind":"update","diff":{"x":1}}"#,
            1,
        ),
        (
            "a start past u32::MAX",
            r#"{"path":"/r/b.txt","kind":{"type":"update"},"diff":"@@ -4294967295,2 +1,2 @@\n a\n b\n"}"#,
            2,
        ),
    ] {
        let changes = file_changes(&codex_replay(&item(&format!("[{good},{bad}]"))));
        assert_eq!(changes.len(), 1, "{case}: the item completes");
        let (paths, patch, _) = &changes[0];
        assert_eq!(
            paths,
            &["/r/a.txt".to_owned(), "/r/b.txt".to_owned()],
            "{case}"
        );
        let files = &patch.as_ref().expect("the good file keeps its diff").files;
        assert_eq!(files.len(), kept, "{case}");
        assert_eq!(marked(&files[0]), ["+hi"], "{case}");
        if kept == 2 {
            assert!(
                files[1].hunks.is_empty(),
                "{case}: the malformed hunk is left out"
            );
        }
        let changes = file_changes(&codex_replay(&item(&format!("[{bad}]"))));
        assert_eq!(changes.len(), 1, "{case}: alone, the item still completes");
        assert_eq!(changes[0].0, ["/r/b.txt".to_owned()], "{case}");
    }
}

#[test]
fn an_edit_copies_as_unified_diff_text_with_paths_from_its_folder() {
    let changes = file_changes(&codex());
    let patch = changes[0].1.as_ref().unwrap();
    let text = patch.shown_from("/tmp/probe/codex-edits").text();
    assert_eq!(
        text,
        "--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n\
         --- /dev/null\n+++ b/c.txt\n@@ -0,0 +1,2 @@\n+hello\n+world\n\
         --- a/d.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-old\n"
    );
}

#[test]
fn a_path_is_shown_from_its_folder_a_whole_component_at_a_time() {
    assert_eq!(shown_path("/srv/app/src/a.rs", "/srv/app"), "src/a.rs");
    assert_eq!(shown_path("/srv/app/src/a.rs", "/srv/app/"), "src/a.rs");
    assert_eq!(shown_path("/srv/apple/a.rs", "/srv/app"), "/srv/apple/a.rs");
    assert_eq!(shown_path("/srv/app", "/srv/app"), "/srv/app");
    assert_eq!(shown_path("/etc/hosts", "/srv/app"), "/etc/hosts");
    assert_eq!(shown_path("a.rs", "/srv/app"), "a.rs");
    assert_eq!(shown_path("/srv/a.rs", "/"), "srv/a.rs");
}

#[test]
fn a_path_outside_the_folder_is_copied_as_it_is_never_after_a_prefix() {
    let changes = file_changes(&codex());
    let text = changes[0]
        .1
        .as_ref()
        .unwrap()
        .shown_from("/elsewhere")
        .text();
    assert!(
        text.contains("--- /tmp/probe/codex-edits/a.txt\n"),
        "{text}"
    );
    assert!(!text.contains("a//") && !text.contains("b//"), "{text}");
}

#[test]
fn a_cut_edit_copies_with_headers_for_the_lines_kept_and_says_it_was_cut() {
    let lines: Vec<String> = (1..=1500)
        .map(|k| match k % 3 {
            0 => format!(" context {k}"),
            _ => format!("+added {k}"),
        })
        .collect();
    let hunk = keystone_agent::diff::Hunk::new(1, 1, lines).unwrap();
    assert_eq!(hunk.header, "@@ -1,500 +1,1500 @@");
    let patch = Patch::edit(vec![FileDiff {
        path: "long.txt".into(),
        hunks: vec![hunk],
        ..FileDiff::default()
    }]);
    let text = patch.text();
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some(
            "Keystone kept only the first 1000 lines of this edit, so this diff is partial and will not apply as it is."
        )
    );
    assert_eq!(lines.next(), Some("--- a/long.txt"));
    assert_eq!(lines.next(), Some("+++ b/long.txt"));
    assert_eq!(lines.next(), Some("@@ -1,333 +1,1000 @@"));
    assert_eq!(lines.count(), 1000);
}

/// The hunk Claude Code's jsdiff writes for `two` changed in `one\ntwo\nthree`, a file
/// with no final newline.
const JSDIFF_NO_NEWLINE: [&str; 5] = [
    " one",
    "-two",
    "+TWO",
    " three",
    "\\ No newline at end of file",
];

fn hunk(old: u32, new: u32, lines: &[&str]) -> keystone_agent::diff::Hunk {
    keystone_agent::diff::Hunk::new(old, new, lines.iter().copied()).unwrap()
}

fn one_file(path: &str, change: Change, hunks: Vec<keystone_agent::diff::Hunk>) -> Patch {
    Patch::edit(vec![FileDiff {
        path: path.into(),
        change,
        hunks,
        ..FileDiff::default()
    }])
}

#[test]
fn a_missing_final_newline_marks_the_line_it_follows_and_copies_back() {
    let jsdiff = hunk(1, 1, &JSDIFF_NO_NEWLINE);
    let marked: Vec<bool> = jsdiff.lines.iter().map(|line| line.no_newline).collect();
    assert_eq!(marked, [false, false, false, true], "after context");
    assert_eq!(jsdiff.header, "@@ -1,3 +1,3 @@", "the marker is not a line");
    assert_eq!(jsdiff.lines[3].new, Some(3));
    let text = one_file("a.txt", Change::Modified, vec![jsdiff]).text();
    assert!(
        text.ends_with(" three\n\\ No newline at end of file\n"),
        "{text}"
    );

    let both = hunk(
        1,
        1,
        &[
            "-old",
            "\\ No newline at end of file",
            "+new",
            "\\ No newline at end of file",
        ],
    );
    let marked: Vec<(Kind, bool)> = both
        .lines
        .iter()
        .map(|line| (line.kind, line.no_newline))
        .collect();
    assert_eq!(
        marked,
        [(Kind::Removed, true), (Kind::Added, true)],
        "both sides"
    );
    let added = hunk(1, 1, &[" keep", "+last", "\\ No newline at end of file"]);
    assert!(added.lines[1].no_newline, "after an added line");
    let removed = hunk(
        1,
        1,
        &[" keep", "-last", "\\ No newline at end of file", "+last"],
    );
    assert!(
        removed.lines[1].no_newline && !removed.lines[2].no_newline,
        "after a removed line"
    );
    let text = one_file("x", Change::Modified, vec![both]).text();
    assert!(
        text.ends_with("-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n"),
        "{text}"
    );
}

#[test]
fn a_file_created_without_a_final_newline_copies_with_the_marker() {
    let patch = Patch::edit(vec![FileDiff::created("c.txt".into(), "hello\nworld")]);
    assert!(patch.files[0].hunks[0].lines[1].no_newline);
    assert!(
        patch
            .text()
            .ends_with("+world\n\\ No newline at end of file\n")
    );
    let whole = Patch::edit(vec![FileDiff::created("c.txt".into(), "hello\nworld\n")]);
    assert!(!whole.text().contains("No newline"));
}

#[test]
fn the_marker_survives_the_log_and_an_older_hunk_still_reads() {
    let patch = one_file(
        "a.txt",
        Change::Modified,
        vec![hunk(1, 1, &JSDIFF_NO_NEWLINE)],
    );
    let json = serde_json::to_string(&patch).unwrap();
    assert_eq!(serde_json::from_str::<Patch>(&json).unwrap(), patch);
    let older = r#"{"files":[{"path":"a.txt","hunks":[{"header":"@@ -1,1 +1,1 @@","lines":["-a","+b"]}]}]}"#;
    let read: Patch = serde_json::from_str(older).unwrap();
    assert!(
        read.files[0].hunks[0]
            .lines
            .iter()
            .all(|line| !line.no_newline)
    );
}

#[test]
fn codex_hunk_text_with_the_marker_reads_the_same_way() {
    // Codex writes an update with the `similar` crate, whose unified diff adds the same
    // marker line after a file's last line when it has no newline.
    let hunks = keystone_agent::diff::hunks(
        "@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n\\ No newline at end of file\n",
    );
    assert_eq!(hunks, [hunk(1, 1, &JSDIFF_NO_NEWLINE)]);
}

/// `git apply --check`, then `git apply`, of `patch` in `repo`.
fn applies(repo: &std::path::Path, patch: &str) -> bool {
    use std::io::Write;
    ["--check", "--verbose"].iter().all(|flag| {
        let mut git = std::process::Command::new("git")
            .args(["apply", flag])
            .current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        git.stdin
            .take()
            .unwrap()
            .write_all(patch.as_bytes())
            .unwrap();
        let output = git.wait_with_output().unwrap();
        if !output.status.success() {
            eprintln!(
                "git apply {flag}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        output.status.success()
    })
}

#[test]
fn a_copy_of_an_edit_to_a_file_with_no_final_newline_applies_byte_for_byte() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("a.txt"), "one\ntwo\nthree").unwrap();
    let edit = one_file(
        "a.txt",
        Change::Modified,
        vec![hunk(1, 1, &JSDIFF_NO_NEWLINE)],
    );
    assert!(applies(repo.path(), &edit.text()), "{}", edit.text());
    assert_eq!(
        std::fs::read(repo.path().join("a.txt")).unwrap(),
        b"one\nTWO\nthree"
    );

    let created = Patch::edit(vec![FileDiff::created("c.txt".into(), "hello\nworld")]);
    assert!(applies(repo.path(), &created.text()), "{}", created.text());
    assert_eq!(
        std::fs::read(repo.path().join("c.txt")).unwrap(),
        b"hello\nworld"
    );
}

#[test]
fn a_shortened_line_makes_the_copy_say_it_is_partial() {
    let long = format!("+{}", "x".repeat(5000));
    let patch = one_file(
        "wide.txt",
        Change::Modified,
        vec![hunk(1, 1, &[" a", long.as_str()])],
    );
    let text = patch.text();
    assert_eq!(
        text.lines().next(),
        Some(
            "Keystone shortened lines longer than 4 KiB, so this diff is partial and will not apply as it is."
        )
    );
    let json = serde_json::to_string(&patch).unwrap();
    let read: Patch = serde_json::from_str(&json).unwrap();
    assert!(
        read.text().starts_with("Keystone shortened"),
        "the log keeps that a line was cut"
    );
    let short = one_file(
        "a.txt",
        Change::Modified,
        vec![hunk(1, 1, &JSDIFF_NO_NEWLINE)],
    );
    assert!(short.text().starts_with("--- a/a.txt"));
}

#[test]
fn a_doubled_slash_in_a_path_is_one_separator() {
    assert_eq!(shown_path("/srv/app//x.txt", "/srv/app"), "x.txt");
    assert_eq!(shown_path("/srv//app/src//x.txt", "/srv/app/"), "src/x.txt");
    assert_eq!(shown_path("/srv/app/x.txt", "/srv//app"), "x.txt");
    assert_eq!(
        shown_path("/srv/apple//x.txt", "/srv/app"),
        "/srv/apple/x.txt"
    );
}
