//! A streaming `multipart/form-data` reader for file uploads. File parts are
//! written to disk as they arrive and hashed on the way; other parts are
//! small form fields kept in memory. Nothing holds a whole upload in memory.
use crate::ApiError;
use crate::contract::invalid;
use axum::body::Body;
use futures::StreamExt;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

/// Bytes of form fields and part headers an upload may carry besides its file.
const MAX_FIELD_BYTES: usize = 64 * 1024;

/// A received upload: its form fields, and the file part's name, size, and
/// SHA-256 digest.
pub(crate) struct Upload {
    pub(crate) fields: BTreeMap<String, String>,
    pub(crate) filename: String,
    pub(crate) size: u64,
    pub(crate) sha256: String,
}

/// One received file part of a multi-file upload: its filename, where its
/// contents were written, and its size.
pub(crate) struct Part {
    pub(crate) filename: String,
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
}

/// A received multi-file upload: its form fields and its file parts in order.
pub(crate) struct Parts {
    pub(crate) fields: BTreeMap<String, String>,
    pub(crate) files: Vec<Part>,
}

/// Which parts are files and where they go.
enum Target<'a> {
    /// Exactly one part named `file`, written to this path.
    One(&'a Path),
    /// Any number of parts with one of these names, up to `max`, written into
    /// this directory by position.
    Many {
        names: &'a [&'a str],
        directory: &'a Path,
        max: usize,
    },
}

enum Phase {
    /// Before the first delimiter.
    Preamble,
    /// After a delimiter: either the final `--` or a part's headers follow.
    Delimited,
    Headers,
    /// Inside a part: the file, or a field with this name.
    File,
    Field(String, Vec<u8>),
}

/// The boundary of a `multipart/form-data` content type.
pub(crate) fn boundary(content_type: &str) -> Option<String> {
    if !content_type.starts_with("multipart/form-data") {
        return None;
    }
    content_type
        .split(';')
        .map(str::trim)
        .find_map(|parameter| parameter.strip_prefix("boundary="))
        .map(|boundary| boundary.trim_matches('"').to_owned())
        .filter(|boundary| !boundary.is_empty() && boundary.len() <= 70)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Read an upload, writing its `file` part to `destination` (at most
/// `max_file_bytes`). The caller removes `destination` if this fails.
pub(crate) async fn receive(
    body: Body,
    boundary: &str,
    destination: &Path,
    max_file_bytes: u64,
) -> Result<Upload, ApiError> {
    let (fields, mut files) =
        parse(body, boundary, Target::One(destination), max_file_bytes).await?;
    let (part, sha256) = files.pop().ok_or_else(|| invalid("file is required"))?;
    Ok(Upload {
        fields,
        filename: part.filename,
        size: part.size,
        sha256,
    })
}

/// Read an upload whose file parts are named one of `names`, writing each into
/// `directory`: at most `max_files` parts and `max_total_bytes` across them.
/// The caller removes `directory` whether or not this succeeds.
pub(crate) async fn receive_many(
    body: Body,
    boundary: &str,
    names: &[&str],
    directory: &Path,
    max_files: usize,
    max_total_bytes: u64,
) -> Result<Parts, ApiError> {
    let target = Target::Many {
        names,
        directory,
        max: max_files,
    };
    let (fields, files) = parse(body, boundary, target, max_total_bytes).await?;
    Ok(Parts {
        fields,
        files: files.into_iter().map(|(part, _)| part).collect(),
    })
}

/// A file part being written.
struct Open {
    handle: tokio::fs::File,
    part: Part,
    hasher: Sha256,
}

impl Open {
    async fn finish(mut self) -> Result<(Part, String), ApiError> {
        self.handle.flush().await.map_err(anyhow::Error::from)?;
        self.handle.sync_all().await.map_err(anyhow::Error::from)?;
        Ok((self.part, format!("{:x}", self.hasher.finalize())))
    }
}

/// The streaming parser behind [`receive`] and [`receive_many`]: form fields,
/// and each file part with its SHA-256 digest. `max_bytes` bounds the file
/// parts together.
async fn parse(
    body: Body,
    boundary: &str,
    target: Target<'_>,
    max_bytes: u64,
) -> Result<(BTreeMap<String, String>, Vec<(Part, String)>), ApiError> {
    let malformed = || invalid("malformed multipart body");
    // A leading CRLF lets the first delimiter match the same separator as
    // the ones between parts.
    let separator = format!("\r\n--{boundary}").into_bytes();
    let mut buffer = b"\r\n".to_vec();
    let mut chunks = body.into_data_stream();
    let mut phase = Phase::Preamble;
    let mut fields = BTreeMap::new();
    let mut files: Vec<(Part, String)> = Vec::new();
    let mut open: Option<Open> = None;
    let mut total = 0u64;
    let mut side_bytes = 0usize;
    loop {
        // Consume what the buffer holds; `None` asks for more input.
        let progressed = match &mut phase {
            // Bounded like a part's headers, so text before the first
            // boundary cannot grow the buffer without limit.
            Phase::Preamble => match find(&buffer, &separator) {
                None if buffer.len() > MAX_FIELD_BYTES => return Err(malformed()),
                found => found.map(|at| {
                    buffer.drain(..at + separator.len());
                    phase = Phase::Delimited;
                }),
            },
            Phase::Delimited if buffer.len() < 2 => None,
            Phase::Delimited => {
                match &buffer[..2] {
                    b"--" => break,
                    b"\r\n" => {
                        buffer.drain(..2);
                        phase = Phase::Headers;
                    }
                    _ => return Err(malformed()),
                }
                Some(())
            }
            Phase::Headers => match find(&buffer, b"\r\n\r\n") {
                None if buffer.len() > MAX_FIELD_BYTES => return Err(malformed()),
                None => None,
                Some(end) => {
                    let headers = std::str::from_utf8(&buffer[..end]).map_err(|_| malformed())?;
                    let disposition = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.trim()
                                .eq_ignore_ascii_case("content-disposition")
                                .then_some(value.to_owned())
                        })
                        .ok_or_else(malformed)?;
                    let parameter = |key: &str| {
                        disposition.split(';').map(str::trim).find_map(|parameter| {
                            parameter
                                .strip_prefix(key)
                                .and_then(|rest| rest.strip_prefix('='))
                                .map(|value| value.trim_matches('"').to_owned())
                        })
                    };
                    let name = parameter("name").ok_or_else(malformed)?;
                    let filename = parameter("filename");
                    buffer.drain(..end + 4);
                    let destination = match (&target, filename) {
                        (Target::One(destination), Some(filename))
                            if name == "file" && files.is_empty() =>
                        {
                            Some((destination.to_path_buf(), filename))
                        }
                        (Target::One(_), _) if name == "file" => {
                            return Err(invalid("upload exactly one file"));
                        }
                        (
                            Target::Many {
                                names,
                                directory,
                                max,
                            },
                            Some(filename),
                        ) if names.contains(&name.as_str()) => {
                            if files.len() == *max {
                                return Err(invalid(format!("upload at most {max} files")));
                            }
                            Some((directory.join(files.len().to_string()), filename))
                        }
                        _ => None,
                    };
                    phase = match destination {
                        Some((path, filename)) => {
                            let handle = tokio::fs::File::create(&path)
                                .await
                                .map_err(anyhow::Error::from)?;
                            open = Some(Open {
                                handle,
                                part: Part {
                                    filename,
                                    path,
                                    size: 0,
                                },
                                hasher: Sha256::new(),
                            });
                            Phase::File
                        }
                        // File parts are bounded by count and size instead.
                        None => {
                            side_bytes += end;
                            Phase::Field(name, Vec::new())
                        }
                    };
                    Some(())
                }
            },
            Phase::File | Phase::Field(..) => {
                let (content, end) = match find(&buffer, &separator) {
                    Some(at) => (at, Some(at + separator.len())),
                    // Keep a tail that may begin the separator.
                    None => (buffer.len().saturating_sub(separator.len() - 1), None),
                };
                if content > 0 {
                    match &mut phase {
                        Phase::File => {
                            total += content as u64;
                            if total > max_bytes {
                                return Err(invalid(match target {
                                    Target::One(_) => {
                                        format!("files are limited to {max_bytes} bytes")
                                    }
                                    Target::Many { .. } => {
                                        format!("uploads are limited to {max_bytes} bytes")
                                    }
                                }));
                            }
                            if let Some(open) = &mut open {
                                open.part.size += content as u64;
                                open.hasher.update(&buffer[..content]);
                                open.handle
                                    .write_all(&buffer[..content])
                                    .await
                                    .map_err(anyhow::Error::from)?;
                            }
                        }
                        Phase::Field(_, value) => {
                            side_bytes += content;
                            if side_bytes > MAX_FIELD_BYTES {
                                return Err(invalid("upload form fields are too large"));
                            }
                            value.extend_from_slice(&buffer[..content]);
                        }
                        Phase::Preamble | Phase::Delimited | Phase::Headers => {}
                    }
                    buffer.drain(..content);
                }
                match end {
                    Some(end) => {
                        buffer.drain(..end - content);
                        match std::mem::replace(&mut phase, Phase::Delimited) {
                            Phase::File => {
                                if let Some(open) = open.take() {
                                    files.push(open.finish().await?);
                                }
                            }
                            Phase::Field(name, value) => {
                                fields.insert(name, String::from_utf8_lossy(&value).into_owned());
                            }
                            Phase::Preamble | Phase::Delimited | Phase::Headers => {}
                        }
                        Some(())
                    }
                    None => None,
                }
            }
        };
        if progressed.is_some() {
            continue;
        }
        match chunks.next().await {
            Some(chunk) => buffer.extend_from_slice(&chunk.map_err(|_| malformed())?),
            None => return Err(malformed()),
        }
    }
    Ok((fields, files))
}

#[cfg(test)]
#[path = "upload_tests.rs"]
mod tests;
