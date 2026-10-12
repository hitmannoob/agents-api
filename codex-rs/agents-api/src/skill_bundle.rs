//! Skill bundles: the files of one skill version, uploaded as a single ZIP or
//! as a directory (one multipart part per file, each named by its path).
//!
//! Either way the bundle holds one top-level folder with exactly one
//! `SKILL.md` (matched case-insensitively) directly inside it. Its frontmatter
//! is read with the harness's own parser, so a stored version is one Codex can
//! load, and `name` and `description` are then held to the Agent Skills
//! specification the guide names. A valid bundle is rewritten as a canonical
//! ZIP: sorted entries, regular files only, and only the executable bit of
//! each file's mode kept.
use crate::ApiError;
use crate::contract::invalid;
use crate::upload::Part;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

/// The largest skill upload.
pub(crate) const MAX_UPLOAD_BYTES: u64 = 50 * 1024 * 1024;
/// The most files one skill version may hold.
pub(crate) const MAX_FILES: usize = 500;
/// The largest file in a skill version, uncompressed.
const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
/// The most a whole version may hold uncompressed. The guide bounds each file
/// and the count; this local bound keeps a small ZIP from expanding without
/// limit.
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 1024;
/// Folders that archivers add beside the bundle's own, which are dropped.
const ARCHIVER_FOLDERS: [&str; 1] = ["__MACOSX"];

/// A validated bundle, written as a canonical ZIP.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Bundle {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

/// Where one file of the bundle is read from.
enum Source {
    /// An entry of the uploaded ZIP, by index.
    Entry(usize),
    /// A directory-upload part on disk.
    Part(PathBuf),
}

struct File {
    source: Source,
    executable: bool,
}

/// Validate uploaded parts as one skill bundle and write it to `output` as a
/// canonical ZIP. Blocking; run it off the async runtime.
pub(crate) fn package(parts: &[Part], output: &Path) -> Result<Bundle, ApiError> {
    let mut archive = None;
    let mut files = BTreeMap::new();
    match parts {
        [] => return Err(invalid("files is required")),
        [part] if part.filename.to_ascii_lowercase().ends_with(".zip") => {
            let opened = std::fs::File::open(&part.path).map_err(anyhow::Error::from)?;
            let mut zip = zip::ZipArchive::new(opened)
                .map_err(|_| invalid("the uploaded skill is not a readable ZIP archive"))?;
            for index in 0..zip.len() {
                let entry = zip
                    .by_index_raw(index)
                    .map_err(|_| invalid("the uploaded skill is not a readable ZIP archive"))?;
                if entry.is_dir() {
                    continue;
                }
                if entry.encrypted() {
                    return Err(invalid("skill archives must not be encrypted"));
                }
                if entry.is_symlink() {
                    return Err(invalid(format!(
                        "skill archives must not contain links: {}",
                        entry.name()
                    )));
                }
                let executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
                let path = entry.name().to_owned();
                insert(&mut files, path, Source::Entry(index), executable)?;
            }
            archive = Some(zip);
        }
        parts => {
            for part in parts {
                let source = Source::Part(part.path.clone());
                insert(
                    &mut files,
                    part.filename.clone(),
                    source,
                    /*executable*/ false,
                )?;
            }
        }
    }
    files.retain(|path, _| {
        !ARCHIVER_FOLDERS.contains(&path.split('/').next().unwrap_or_default())
            && path.rsplit('/').next() != Some(".DS_Store")
    });
    if files.is_empty() {
        return Err(invalid("the skill bundle holds no files"));
    }
    if files.len() > MAX_FILES {
        return Err(invalid(format!(
            "a skill version holds at most {MAX_FILES} files"
        )));
    }
    let mut folders = files.keys().filter_map(|path| path.split_once('/'));
    let folder = folders
        .next()
        .map(|(folder, _)| folder.to_owned())
        .filter(|folder| folders.all(|(other, _)| other == folder))
        .filter(|_| files.keys().all(|path| path.contains('/')))
        .ok_or_else(|| invalid("a skill bundle must contain a single top-level folder"))?;
    let manifests: Vec<&String> = files
        .keys()
        .filter(|path| {
            path.rsplit('/')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
        })
        .collect();
    let manifest = match manifests.as_slice() {
        [manifest] if manifest.matches('/').count() == 1 => (*manifest).clone(),
        [] | [_] => {
            return Err(invalid(format!(
                "the skill bundle needs a SKILL.md directly inside {folder}/"
            )));
        }
        _ => return Err(invalid("exactly one SKILL.md is allowed in a skill bundle")),
    };

    let written = std::fs::File::create(output).map_err(anyhow::Error::from)?;
    let mut writer = zip::ZipWriter::new(written);
    let mut contents = None;
    let mut total = 0u64;
    for (path, file) in &files {
        let mut reader: Box<dyn Read> = match (&file.source, archive.as_mut()) {
            (Source::Entry(index), Some(zip)) => Box::new(
                zip.by_index(*index)
                    .map_err(|_| invalid(format!("could not read {path} from the archive")))?,
            ),
            (Source::Part(part), _) => {
                Box::new(std::fs::File::open(part).map_err(anyhow::Error::from)?)
            }
            (Source::Entry(_), None) => {
                return Err(anyhow::anyhow!("archive entry without an archive").into());
            }
        };
        let mode = if file.executable { 0o755 } else { 0o644 };
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(mode);
        writer
            .start_file(path, options)
            .map_err(anyhow::Error::from)?;
        // Read one byte past the limit to tell a file at the limit from one over it.
        let mut limited = (&mut reader).take(MAX_FILE_BYTES + 1);
        let mut copied = 0u64;
        let mut manifest_bytes = (*path == manifest).then(Vec::new);
        let mut chunk = vec![0; 64 * 1024];
        loop {
            let read = limited
                .read(&mut chunk)
                .map_err(|_| invalid(format!("could not read {path} from the upload")))?;
            if read == 0 {
                break;
            }
            copied += read as u64;
            total += read as u64;
            if copied > MAX_FILE_BYTES {
                return Err(invalid(format!(
                    "{path} exceeds the {MAX_FILE_BYTES}-byte limit for a skill file"
                )));
            }
            if total > MAX_TOTAL_BYTES {
                return Err(invalid(format!(
                    "a skill version holds at most {MAX_TOTAL_BYTES} bytes uncompressed"
                )));
            }
            if let Some(bytes) = &mut manifest_bytes {
                bytes.extend_from_slice(&chunk[..read]);
            }
            writer
                .write_all(&chunk[..read])
                .map_err(anyhow::Error::from)?;
        }
        if let Some(bytes) = manifest_bytes {
            contents = Some(bytes);
        }
    }
    writer
        .finish()
        .map_err(anyhow::Error::from)?
        .sync_all()
        .map_err(anyhow::Error::from)?;

    let contents = String::from_utf8(contents.unwrap_or_default())
        .map_err(|_| invalid("SKILL.md must be UTF-8 text"))?;
    let (name, description) = frontmatter(&contents)?;
    let mut hasher = Sha256::new();
    let mut stored = std::fs::File::open(output).map_err(anyhow::Error::from)?;
    let bytes = std::io::copy(&mut stored, &mut hasher).map_err(anyhow::Error::from)?;
    Ok(Bundle {
        name,
        description,
        bytes,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

/// Record one file of the bundle by its validated relative path.
fn insert(
    files: &mut BTreeMap<String, File>,
    path: String,
    source: Source,
    executable: bool,
) -> Result<(), ApiError> {
    let unsafe_path = || {
        invalid(format!(
            "skill file paths must be relative and stay inside the bundle: {path}"
        ))
    };
    if path.is_empty()
        || path.len() > 1024
        || path.starts_with('/')
        || path.contains(['\\', '\0', ':'])
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(unsafe_path());
    }
    if files
        .insert(path.clone(), File { source, executable })
        .is_some()
    {
        return Err(invalid(format!("the skill bundle names {path} twice")));
    }
    Ok(())
}

/// The `name` and `description` of a `SKILL.md`.
fn frontmatter(contents: &str) -> Result<(String, String), ApiError> {
    let parsed = codex_skills::parse_skill_frontmatter_metadata(contents, String::new)
        .map_err(|error| invalid(format!("invalid SKILL.md frontmatter: {error}")))?;
    let name = parsed.name;
    let valid_name = name.chars().count() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|character| matches!(character, 'a'..='z' | '0'..='9' | '-'))
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if !valid_name {
        return Err(invalid(format!(
            "invalid SKILL.md frontmatter: name must be 1-{MAX_NAME_CHARS} lowercase letters, digits, and single hyphens, not starting or ending with a hyphen"
        )));
    }
    if parsed.description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(invalid(format!(
            "invalid SKILL.md frontmatter: description exceeds {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    Ok((name, parsed.description))
}

#[cfg(test)]
#[path = "skill_bundle_tests.rs"]
mod tests;
