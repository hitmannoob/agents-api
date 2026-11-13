use super::*;
use pretty_assertions::assert_eq;

const BOUNDARY: &str = "test-boundary";

fn body(parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut body = b"preamble".to_vec();
    for (name, filename, value) in parts {
        let filename = filename
            .map(|name| format!("; filename=\"{name}\""))
            .unwrap_or_default();
        body.extend(format!("\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"{filename}\r\n\r\n").bytes());
        body.extend_from_slice(value);
    }
    body.extend(format!("\r\n--{BOUNDARY}--\r\n").bytes());
    body
}

/// Feed the body in chunks of `size` bytes, so delimiters straddle chunks.
async fn receive_in_chunks(
    body: Vec<u8>,
    size: usize,
    max: u64,
) -> (Result<Upload, ApiError>, Vec<u8>) {
    let directory = tempfile::tempdir().expect("tempdir");
    let destination = directory.path().join("upload");
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        body.chunks(size).map(|chunk| Ok(chunk.to_vec())).collect();
    let result = receive(
        Body::from_stream(futures::stream::iter(chunks)),
        BOUNDARY,
        &destination,
        max,
    )
    .await;
    let written = std::fs::read(&destination).unwrap_or_default();
    (result, written)
}

#[tokio::test]
async fn uploads_stream_whatever_the_chunking() {
    // Contents that contain a near-miss of the separator.
    let contents = format!("line one\r\n--{}x\r\n--test-boundar", "test-boundar").into_bytes();
    let upload = body(&[
        ("purpose", None, b"user_data"),
        ("file", Some("a.txt"), &contents),
        ("note", None, b"n"),
    ]);
    for size in [1, 3, 7, 64, upload.len()] {
        let (result, written) = receive_in_chunks(upload.clone(), size, /*max*/ 1024).await;
        let received = result.unwrap_or_else(|error| panic!("chunk size {size}: {}", error.1));
        assert_eq!(
            (
                received.fields,
                received.filename,
                received.size,
                received.sha256,
                written
            ),
            (
                BTreeMap::from([
                    ("note".to_owned(), "n".to_owned()),
                    ("purpose".to_owned(), "user_data".to_owned())
                ]),
                "a.txt".to_owned(),
                contents.len() as u64,
                format!("{:x}", Sha256::digest(&contents)),
                contents.clone(),
            ),
            "chunk size {size}"
        );
    }
}

#[tokio::test]
async fn malformed_and_oversized_uploads_are_refused() {
    let messages = |result: Result<Upload, ApiError>| result.err().map(|error| error.1);
    let truncated = body(&[("file", Some("a.txt"), b"abc")]);
    let truncated = truncated[..truncated.len() - 10].to_vec();
    assert_eq!(
        messages(receive_in_chunks(truncated, 4, 1024).await.0),
        Some("malformed multipart body".to_owned())
    );
    let oversized = body(&[("file", Some("a.txt"), &[7; 100])]);
    assert_eq!(
        messages(receive_in_chunks(oversized, 16, 99).await.0),
        Some("files are limited to 99 bytes".to_owned())
    );
    let twice = body(&[("file", Some("a.txt"), b"a"), ("file", Some("b.txt"), b"b")]);
    assert_eq!(
        messages(receive_in_chunks(twice, 5, 1024).await.0),
        Some("upload exactly one file".to_owned())
    );
    let none = body(&[("purpose", None, b"user_data")]);
    assert_eq!(
        messages(receive_in_chunks(none, 5, 1024).await.0),
        Some("file is required".to_owned())
    );
    // An otherwise valid upload is refused once text before the first
    // boundary outgrows the header limit, instead of being buffered.
    let mut padded = vec![b'x'; 2 * MAX_FIELD_BYTES];
    padded.extend(body(&[("file", Some("a.txt"), b"abc")]));
    assert_eq!(
        messages(receive_in_chunks(padded, 4096, 1024).await.0),
        Some("malformed multipart body".to_owned())
    );
}

#[tokio::test]
async fn multi_file_uploads_keep_each_part_and_bound_them_together() {
    let receive_parts = |body: Vec<u8>, max_files: usize, max_total: u64| async move {
        let directory = tempfile::tempdir().expect("tempdir");
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
            body.chunks(5).map(|chunk| Ok(chunk.to_vec())).collect();
        let result = receive_many(
            Body::from_stream(futures::stream::iter(chunks)),
            BOUNDARY,
            &["files[]", "files"],
            directory.path(),
            max_files,
            max_total,
        )
        .await;
        result.map(|parts| {
            let files: Vec<(String, Vec<u8>, u64)> = parts
                .files
                .iter()
                .map(|part| {
                    let contents = std::fs::read(&part.path).expect("part contents");
                    (part.filename.clone(), contents, part.size)
                })
                .collect();
            (parts.fields, files)
        })
    };
    let upload = body(&[
        ("files[]", Some("skill/SKILL.md"), b"manifest"),
        ("default", None, b"true"),
        ("files", Some("skill/a.txt"), b"a"),
        // A part without a filename is a form field, even under a file name.
        ("files[]", None, b"field"),
    ]);
    let (fields, files) = receive_parts(upload.clone(), 2, 9)
        .await
        .unwrap_or_else(|error| panic!("{}", error.1));
    assert_eq!(
        (fields, files),
        (
            BTreeMap::from([
                ("default".to_owned(), "true".to_owned()),
                ("files[]".to_owned(), "field".to_owned()),
            ]),
            vec![
                ("skill/SKILL.md".to_owned(), b"manifest".to_vec(), 8),
                ("skill/a.txt".to_owned(), b"a".to_vec(), 1),
            ]
        )
    );
    let message = |result: Result<_, ApiError>| result.err().map(|error| error.1);
    assert_eq!(
        message(receive_parts(upload.clone(), 1, 9).await),
        Some("upload at most 1 files".to_owned())
    );
    assert_eq!(
        message(receive_parts(upload, 2, 8).await),
        Some("uploads are limited to 8 bytes".to_owned())
    );
}
