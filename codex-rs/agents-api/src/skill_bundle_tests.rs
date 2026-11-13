use super::*;
use pretty_assertions::assert_eq;

/// Upload parts as (filename, contents).
type Files<'a> = Vec<(&'a str, &'a [u8])>;

const MANIFEST: &str = "---\nname: pdf-tools\ndescription: Fill and merge PDFs. Use when handling PDFs.\n---\n# PDF tools\n";

/// A ZIP of these entries; a `None` mode leaves the entry's mode unset.
fn zip_of(entries: &[(&str, &[u8], Option<u32>)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (path, contents, mode) in entries {
        let mut options = zip::write::SimpleFileOptions::default();
        if let Some(mode) = mode {
            options = options.unix_permissions(*mode);
        }
        writer.start_file(*path, options).expect("start entry");
        writer.write_all(contents).expect("write entry");
    }
    writer.finish().expect("finish zip").into_inner()
}

/// Upload parts written into `directory`, as the multipart reader leaves them.
fn parts(directory: &Path, files: &[(&str, &[u8])]) -> Vec<Part> {
    files
        .iter()
        .enumerate()
        .map(|(index, (filename, contents))| {
            let path = directory.join(index.to_string());
            std::fs::write(&path, contents).expect("write part");
            Part {
                filename: (*filename).to_owned(),
                path,
                size: contents.len() as u64,
            }
        })
        .collect()
}

/// The canonical ZIP's entries as (path, contents, mode).
fn entries(path: &Path) -> Vec<(String, Vec<u8>, Option<u32>)> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).expect("open")).expect("zip");
    (0..zip.len())
        .map(|index| {
            let mut entry = zip.by_index(index).expect("entry");
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents).expect("read");
            (
                entry.name().to_owned(),
                contents,
                entry.unix_mode().map(|mode| mode & 0o777),
            )
        })
        .collect()
}

fn accepted(result: Result<Bundle, ApiError>) -> Bundle {
    result.unwrap_or_else(|error| panic!("bundle refused: {}", error.1))
}

fn message(result: Result<Bundle, ApiError>) -> String {
    result.err().map(|error| error.1).unwrap_or_default()
}

#[test]
fn zip_and_directory_uploads_become_the_same_canonical_archive() {
    let directory = tempfile::tempdir().expect("tempdir");
    let archive = zip_of(&[
        ("pdf-tools/scripts/merge.py", b"print(1)", Some(0o100755)),
        ("pdf-tools/SKILL.md", MANIFEST.as_bytes(), None),
        ("__MACOSX/pdf-tools/._SKILL.md", b"junk", None),
        ("pdf-tools/.DS_Store", b"junk", None),
    ]);
    let zipped = parts(directory.path(), &[("pdf-tools.zip", &archive)]);
    let from_zip = directory.path().join("from-zip");
    let bundle = accepted(package(&zipped, &from_zip));
    assert_eq!(
        (bundle.name.as_str(), bundle.description.as_str()),
        ("pdf-tools", "Fill and merge PDFs. Use when handling PDFs.")
    );
    assert_eq!(
        entries(&from_zip),
        vec![
            (
                "pdf-tools/SKILL.md".to_owned(),
                MANIFEST.as_bytes().to_vec(),
                Some(0o644)
            ),
            (
                "pdf-tools/scripts/merge.py".to_owned(),
                b"print(1)".to_vec(),
                Some(0o755)
            ),
        ]
    );

    let uploaded = tempfile::tempdir().expect("tempdir");
    let directory_parts = parts(
        uploaded.path(),
        &[
            ("pdf-tools/SKILL.md", MANIFEST.as_bytes()),
            ("pdf-tools/notes/a.txt", b"a"),
        ],
    );
    let from_directory = directory.path().join("from-directory");
    let bundle = accepted(package(&directory_parts, &from_directory));
    assert_eq!(bundle.name, "pdf-tools");
    assert_eq!(
        entries(&from_directory)
            .into_iter()
            .map(|(path, _, mode)| (path, mode))
            .collect::<Vec<_>>(),
        vec![
            ("pdf-tools/SKILL.md".to_owned(), Some(0o644)),
            ("pdf-tools/notes/a.txt".to_owned(), Some(0o644)),
        ]
    );
    // The digest and size describe the stored archive.
    let stored = std::fs::read(&from_directory).expect("stored");
    assert_eq!(
        (bundle.bytes, bundle.sha256),
        (
            stored.len() as u64,
            format!("{:x}", Sha256::digest(&stored))
        )
    );
}

#[test]
fn bundles_that_break_the_layout_are_refused() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manifest = MANIFEST.as_bytes();
    let cases: Vec<(Files, &str)> = vec![
        (vec![], "files is required"),
        (
            vec![("SKILL.md", manifest)],
            "a skill bundle must contain a single top-level folder",
        ),
        (
            vec![("a/SKILL.md", manifest), ("b/x.txt", b"x")],
            "a skill bundle must contain a single top-level folder",
        ),
        (
            vec![("a/x.txt", b"x")],
            "the skill bundle needs a SKILL.md directly inside a/",
        ),
        (
            vec![("a/sub/SKILL.md", manifest)],
            "the skill bundle needs a SKILL.md directly inside a/",
        ),
        (
            vec![("a/SKILL.md", manifest), ("a/sub/skill.md", manifest)],
            "exactly one SKILL.md is allowed in a skill bundle",
        ),
        (
            vec![("a/SKILL.md", manifest), ("a/../escape", b"x")],
            "skill file paths must be relative and stay inside the bundle: a/../escape",
        ),
        (
            vec![("/a/SKILL.md", manifest)],
            "skill file paths must be relative and stay inside the bundle: /a/SKILL.md",
        ),
        (
            vec![("a/SKILL.md", manifest), ("a\\x", b"x")],
            "skill file paths must be relative and stay inside the bundle: a\\x",
        ),
        (
            vec![("a/SKILL.md", manifest), ("a/SKILL.md", manifest)],
            "the skill bundle names a/SKILL.md twice",
        ),
    ];
    for (index, (files, expected)) in cases.into_iter().enumerate() {
        let part_directory = directory.path().join(format!("case-{index}"));
        std::fs::create_dir(&part_directory).expect("case directory");
        let output = directory.path().join(format!("out-{index}"));
        assert_eq!(
            message(package(&parts(&part_directory, &files), &output)),
            expected,
            "case {index}"
        );
    }

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer.start_file("a/SKILL.md", options).expect("manifest");
    writer.write_all(manifest).expect("manifest contents");
    writer
        .add_symlink("a/link", "/etc/passwd", options)
        .expect("link");
    let symlink = writer.finish().expect("finish zip").into_inner();
    let output = directory.path().join("symlink-out");
    assert_eq!(
        message(package(
            &parts(directory.path(), &[("a.zip", &symlink)]),
            &output
        )),
        "skill archives must not contain links: a/link"
    );
    // Decoders other than stored and deflated may buffer without bound.
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let bzip2 =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Bzip2);
    writer
        .start_file("a/SKILL.md", zip::write::SimpleFileOptions::default())
        .expect("manifest");
    writer.write_all(manifest).expect("manifest contents");
    writer.start_file("a/data.bin", bzip2).expect("bzip2 entry");
    writer.write_all(&[0; 1024]).expect("bzip2 contents");
    let compressed = writer.finish().expect("finish zip").into_inner();
    let output = directory.path().join("bzip2-out");
    assert_eq!(
        message(package(
            &parts(directory.path(), &[("a.zip", &compressed)]),
            &output
        )),
        "skill archives must use stored or deflated entries: a/data.bin"
    );
    let output = directory.path().join("not-zip-out");
    assert_eq!(
        message(package(
            &parts(directory.path(), &[("a.zip", b"not a zip")]),
            &output
        )),
        "the uploaded skill is not a readable ZIP archive"
    );
}

#[test]
fn frontmatter_follows_the_agent_skills_specification() {
    let directory = tempfile::tempdir().expect("tempdir");
    let long_manifest = format!("---\nname: ok\ndescription: {}\n---\n", "d".repeat(1025));
    let cases = [
        ("---\nname: Bad-Name\ndescription: d\n---\n", "name must be"),
        ("---\nname: -lead\ndescription: d\n---\n", "name must be"),
        (
            "---\nname: two--hyphens\ndescription: d\n---\n",
            "name must be",
        ),
        ("---\ndescription: d\n---\n", "missing field `name`"),
        ("---\nname: ok\n---\n", "missing field `description`"),
        ("no frontmatter", "missing YAML frontmatter"),
        (
            long_manifest.as_str(),
            "description exceeds 1024 characters",
        ),
    ];
    for (index, (manifest, expected)) in cases.into_iter().enumerate() {
        let part_directory = directory.path().join(format!("case-{index}"));
        std::fs::create_dir(&part_directory).expect("case directory");
        let output = directory.path().join(format!("out-{index}"));
        let error = message(package(
            &parts(&part_directory, &[("ok/SKILL.md", manifest.as_bytes())]),
            &output,
        ));
        assert!(
            error.starts_with("invalid SKILL.md frontmatter: ") && error.contains(expected),
            "case {index}: {error}"
        );
    }
    let longest = "a".repeat(64);
    let output = directory.path().join("longest");
    let manifest = format!("---\nname: {longest}\ndescription: d\n---\n");
    let accepted = accepted(package(
        &parts(directory.path(), &[("x/SKILL.md", manifest.as_bytes())]),
        &output,
    ));
    assert_eq!(accepted.name, longest);
}

#[test]
fn files_over_the_limits_are_refused() {
    let directory = tempfile::tempdir().expect("tempdir");
    // Highly compressible, so the archive itself stays small.
    let large = vec![0u8; MAX_FILE_BYTES as usize + 1];
    let archive = zip_of(&[
        ("a/SKILL.md", MANIFEST.as_bytes(), None),
        ("a/large.bin", &large, None),
    ]);
    assert!(archive.len() < 1024 * 1024);
    let output = directory.path().join("large-out");
    assert_eq!(
        message(package(
            &parts(directory.path(), &[("a.zip", &archive)]),
            &output
        )),
        format!("a/large.bin exceeds the {MAX_FILE_BYTES}-byte limit for a skill file")
    );

    let many: Vec<(String, &[u8])> = (0..MAX_FILES)
        .map(|index| (format!("a/{index}.txt"), b"x".as_slice()))
        .chain([("a/SKILL.md".to_owned(), MANIFEST.as_bytes())])
        .collect();
    let many: Vec<(&str, &[u8])> = many
        .iter()
        .map(|(path, contents)| (path.as_str(), *contents))
        .collect();
    let part_directory = directory.path().join("many");
    std::fs::create_dir(&part_directory).expect("many directory");
    let output = directory.path().join("many-out");
    assert_eq!(
        message(package(&parts(&part_directory, &many), &output)),
        format!("a skill version holds at most {MAX_FILES} files")
    );
}
