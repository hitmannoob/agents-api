use super::*;
use pretty_assertions::assert_eq;

fn message(bytes: usize) -> Vec<Value> {
    vec![json!({"type": "input_text", "text": "x".repeat(bytes)})]
}

fn wait(inputs: Vec<Queued>) -> Wait {
    Wait {
        inputs: inputs.into(),
        generation: 0,
    }
}

#[test]
fn waiting_input_is_bounded_per_session_and_released_when_dropped() {
    let waits = Waits::default();
    let refused = |result: Result<Queued, ApiError>| result.err().map(|error| (error.0, error.1));
    let too_much = Some((
        StatusCode::TOO_MANY_REQUESTS,
        "too much input is waiting for this session's environment to connect".to_owned(),
    ));

    // A session holds at most MAX_WAITING_INPUTS batches.
    let full = wait(
        (0..MAX_WAITING_INPUTS)
            .map(|_| {
                waits
                    .admit(/*waiting*/ None, message(1))
                    .ok()
                    .expect("admit")
            })
            .collect(),
    );
    assert_eq!(refused(waits.admit(Some(&full), message(1))), too_much);

    // And at most MAX_WAITING_BYTES, counted with what it already holds.
    let half = MAX_WAITING_BYTES / 2;
    let first = waits
        .admit(/*waiting*/ None, message(half))
        .ok()
        .expect("first half");
    let holding = wait(vec![first]);
    assert_eq!(
        refused(waits.admit(Some(&holding), message(half))),
        too_much
    );
    assert_eq!(
        refused(waits.admit(/*waiting*/ None, message(MAX_WAITING_BYTES))),
        too_much
    );

    // Everything queued returns its bytes when the queue lets go of it, and a
    // refused batch leaves no reservation behind.
    drop(full);
    drop(holding);
    assert_eq!(waits.total_bytes.load(Ordering::Relaxed), 0);
}

#[test]
fn waiting_input_is_bounded_across_sessions() {
    let waits = Waits::default();
    // Other sessions already hold nearly the whole service-wide budget.
    let others = MAX_TOTAL_WAITING_BYTES - 64;
    waits.total_bytes.store(others, Ordering::Relaxed);
    assert!(waits.admit(/*waiting*/ None, message(128)).is_err());
    assert_eq!(waits.total_bytes.load(Ordering::Relaxed), others);
    let small = waits
        .admit(/*waiting*/ None, message(1))
        .ok()
        .expect("fits");
    assert!(waits.total_bytes.load(Ordering::Relaxed) > others);
    drop(small);
    assert_eq!(waits.total_bytes.load(Ordering::Relaxed), others);
}
