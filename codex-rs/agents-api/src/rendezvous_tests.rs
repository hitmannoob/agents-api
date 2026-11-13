use super::*;
use pretty_assertions::assert_eq;

#[test]
fn frames_route_by_stream_and_resets_are_recognized() {
    // RelayMessageFrame{version: 1, stream_id: "s-1", resume: {next_seq: 0}}.
    let resume = [0x08, 0x01, 0x12, 0x03, b's', b'-', b'1', 0x3a, 0x00];
    assert_eq!(stream_of(&resume), Some(("s-1".to_string(), false)));
    assert_eq!(
        stream_of(&reset("s-1", "harness_disconnected")),
        Some(("s-1".to_string(), true))
    );
    // A frame without a stream, and a truncated frame, are not routed.
    assert_eq!(stream_of(&[0x08, 0x01]), None);
    assert_eq!(stream_of(&[0x08, 0x01, 0x12, 0x09, b's']), None);
}

#[test]
fn the_stream_table_is_bounded() {
    let (owner, other) = (1, 2);
    let full: HashMap<String, u64> = (0..MAX_STREAMS)
        .map(|index| (format!("stream-{index}"), owner))
        .collect();
    // A full table refuses new streams but keeps serving its existing ones,
    // and still accepts the resets that shrink it.
    assert_eq!(
        refusal(&full, true, "new", /*resets*/ false, owner),
        Some("stream_limit")
    );
    assert_eq!(refusal(&full, true, "stream-0", false, owner), None);
    assert_eq!(refusal(&full, true, "new", /*resets*/ true, owner), None);
    // Overlong IDs never enter the table.
    let long = "s".repeat(MAX_STREAM_ID_BYTES + 1);
    assert_eq!(
        refusal(&HashMap::new(), true, &long, false, owner),
        Some("stream_limit")
    );
    // Ownership and executor checks still come first.
    assert_eq!(
        refusal(&full, true, "stream-0", false, other),
        Some("stream_in_use")
    );
    assert_eq!(
        refusal(&HashMap::new(), false, "new", false, owner),
        Some("environment_offline")
    );
}
