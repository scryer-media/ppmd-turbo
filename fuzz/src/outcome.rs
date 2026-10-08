//! A decode reduced to its output and a verdict, and when two agree.

use std::io::{self, Read};

/// The class of an error, independent of who reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    /// The order or memory size was refused.
    InvalidParameters,
    /// The stream is corrupt.
    Corrupt,
    /// The input ended before the stream did.
    Truncated,
    /// Any other I/O error.
    Io,
    /// A variant this harness does not know yet.
    Other,
}

/// How a decode ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The known output size was produced in full.
    Complete,
    /// The decoder reported the end of its output (an end marker, or for a
    /// lenient reference the end of its input) before any known size.
    Ended,
    /// No size was known and the harness stopped at its output cap.
    Capped,
    /// The decoder returned an error.
    Failed(ErrKind),
}

/// The bytes a decode produced, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Every byte the decoder handed out before it ended.
    pub output: Vec<u8>,
    /// How it ended.
    pub verdict: Verdict,
}

impl Outcome {
    /// An outcome with no output.
    pub fn failed(kind: ErrKind) -> Self {
        Self {
            output: Vec::new(),
            verdict: Verdict::Failed(kind),
        }
    }
}

/// The class of a ppmd-turbo error. `ErrorKind` is non-exhaustive, so
/// this keeps compiling as kinds are added.
pub fn classify(e: &ppmd_turbo::Error) -> ErrKind {
    use ppmd_turbo::ErrorKind as K;
    match e.kind {
        K::InvalidParameters => ErrKind::InvalidParameters,
        K::Corrupt(_) => ErrKind::Corrupt,
        K::Truncated => ErrKind::Truncated,
        _ => ErrKind::Other,
    }
}

/// The class of an I/O error, looking through a wrapped ppmd-turbo error
/// first (the `Read` impls carry one inside `io::Error`).
pub fn classify_io(e: &io::Error) -> ErrKind {
    if let Some(inner) = e
        .get_ref()
        .and_then(|i| i.downcast_ref::<ppmd_turbo::Error>())
    {
        return classify(inner);
    }
    match e.kind() {
        io::ErrorKind::UnexpectedEof => ErrKind::Truncated,
        io::ErrorKind::InvalidData => ErrKind::Corrupt,
        io::ErrorKind::InvalidInput => ErrKind::InvalidParameters,
        _ => ErrKind::Io,
    }
}

/// Reads `reader` to its end, to `known` bytes, or to `cap` bytes, whichever
/// comes first, one bounded `read` at a time.
pub fn drain<R: Read>(mut reader: R, known: Option<usize>, cap: usize) -> Outcome {
    let limit = known.map_or(cap, |n| n.min(cap));
    let mut output = Vec::new();
    let mut buf = vec![0u8; 4096];
    loop {
        let want = (limit - output.len()).min(buf.len());
        if want == 0 {
            let verdict = if known.is_some_and(|n| n <= cap) {
                Verdict::Complete
            } else {
                Verdict::Capped
            };
            return Outcome { output, verdict };
        }
        match reader.read(&mut buf[..want]) {
            Ok(0) => {
                return Outcome {
                    output,
                    verdict: Verdict::Ended,
                };
            }
            Ok(n) if n <= want => output.extend_from_slice(&buf[..n]),
            Ok(n) => panic!("read returned {n} bytes into a {want}-byte buffer"),
            // A decoder never returns `Interrupted`; retrying could loop.
            Err(e) => {
                return Outcome {
                    output,
                    verdict: Verdict::Failed(classify_io(&e)),
                };
            }
        }
    }
}

/// A reference decode: its outcome, and whether it read to the end of its
/// input (a lenient decoder may stop there and report success).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// What the reference produced.
    pub outcome: Outcome,
    /// The reference asked for input past the end of the stream.
    pub hit_input_end: bool,
}

fn common_prefix_agrees(a: &[u8], b: &[u8]) -> bool {
    let n = a.len().min(b.len());
    a[..n] == b[..n]
}

/// Checks ppmd-turbo's outcome against the reference's, with the allowances
/// the backlog's F1 row grants (`docs/backlog.md`, Fuzzing):
///
/// - Error classes may differ; error versus success may not.
/// - ppmd-rust treats running out of input as the end of the data and
///   returns what it decoded, where 7-Zip (and ppmd-turbo) report the
///   overrun. When the reference hit the end of its input, ppmd-turbo may
///   either agree with it exactly or fail.
/// - The bytes handed out before an error depend on read granularity, so
///   after an error only the common prefix must match.
pub fn agree(reference: &Reference, ours: &Outcome) -> Result<(), String> {
    let r = &reference.outcome;
    let prefix = common_prefix_agrees(&r.output, &ours.output);
    let ok = match (r.verdict, ours.verdict) {
        (a, b) if a == b && !matches!(a, Verdict::Failed(_)) => r.output == ours.output,
        (Verdict::Failed(_), Verdict::Failed(_)) => prefix,
        (_, Verdict::Failed(ErrKind::Truncated | ErrKind::Corrupt)) if reference.hit_input_end => {
            prefix
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "reference {:?} ({} bytes, input end {}) vs ppmd-turbo {:?} ({} bytes)",
            r.verdict,
            r.output.len(),
            reference.hit_input_end,
            ours.verdict,
            ours.output.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_stops_at_known_and_cap() {
        let data = vec![7u8; 10_000];
        let o = drain(&data[..], Some(5000), 1 << 20);
        assert_eq!((o.output.len(), o.verdict), (5000, Verdict::Complete));
        let o = drain(&data[..], None, 6000);
        assert_eq!((o.output.len(), o.verdict), (6000, Verdict::Capped));
        let o = drain(&data[..], None, 1 << 20);
        assert_eq!((o.output.len(), o.verdict), (10_000, Verdict::Ended));
        let o = drain(&data[..], Some(20_000), 1 << 20);
        assert_eq!((o.output.len(), o.verdict), (10_000, Verdict::Ended));
    }

    #[test]
    fn classification_sees_through_io() {
        let wrapped = io::Error::from(ppmd_turbo::Error::new(ppmd_turbo::ErrorKind::Truncated));
        assert_eq!(classify_io(&wrapped), ErrKind::Truncated);
        let plain = io::Error::from(io::ErrorKind::InvalidData);
        assert_eq!(classify_io(&plain), ErrKind::Corrupt);
    }

    #[test]
    fn agreement_rules() {
        let out = |v: &[u8], verdict| Outcome {
            output: v.to_vec(),
            verdict,
        };
        let r = Reference {
            outcome: out(b"abc", Verdict::Ended),
            hit_input_end: true,
        };
        assert!(agree(&r, &out(b"abc", Verdict::Ended)).is_ok());
        assert!(agree(&r, &out(b"ab", Verdict::Failed(ErrKind::Truncated))).is_ok());
        assert!(agree(&r, &out(b"abd", Verdict::Failed(ErrKind::Truncated))).is_err());
        assert!(agree(&r, &out(b"abcd", Verdict::Ended)).is_err());
        let r = Reference {
            outcome: out(b"abc", Verdict::Complete),
            hit_input_end: false,
        };
        assert!(agree(&r, &out(b"abc", Verdict::Failed(ErrKind::Corrupt))).is_err());
        let r = Reference {
            outcome: out(b"a", Verdict::Failed(ErrKind::Corrupt)),
            hit_input_end: false,
        };
        assert!(agree(&r, &out(b"", Verdict::Failed(ErrKind::Io))).is_ok());
    }
}
