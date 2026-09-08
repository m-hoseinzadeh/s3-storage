//! The incremental rule: given a source object and what we already hold, decide
//! whether to copy it or leave it alone.
//!
//! Kept pure and free of I/O so the rule can be exercised directly -- it is the
//! part of the sync that is easy to get subtly wrong and expensive to debug in
//! situ, because a mistake shows up as "the sync is slow" rather than as a
//! failure.
//!
//! # Why this compares size and mtime rather than ETags
//!
//! The obvious rule -- compare the source ETag against ours -- is both expensive
//! and wrong here:
//!
//! * Our own `ListObjectsV2` never populates `e_tag`, and
//!   [`FileSystem::object_etag`](crate::backend) only finds a stored value for
//!   multipart-completed objects. For anything written by `put_object` it falls
//!   through to `get_md5_sum`, which reads the whole file. A sync that heads
//!   every key would therefore re-read every local byte on every run, which is
//!   precisely what an incremental sync exists to avoid.
//! * An object uploaded to the source via multipart carries a *composite* ETag
//!   of the form `"<md5>-<parts>"`. Our `put_object` always produces a
//!   whole-body MD5, so that value can never match. Comparing against it would
//!   re-download every multipart-uploaded object on every run, forever.
//!
//! So the local side is a cheap `stat`. ETags are consulted only when the caller
//! opted into [`SyncRequest::verify_etag`](super::SyncRequest) *and* the source
//! ETag is a plain whole-object MD5, where the comparison is actually meaningful.

/// How aggressively an existing local object may be replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncMode {
    /// Copy when missing, when the size differs, or when the source looks newer.
    #[default]
    NewAndChanged,
    /// Never touch an object we already hold.
    SkipExisting,
    /// Copy everything the listing returns, unconditionally.
    OverwriteAll,
}

/// A source object as reported by the remote `ListObjectsV2`.
#[derive(Clone, Debug)]
pub(crate) struct SourceObject {
    pub key: String,
    pub size: u64,
    /// May be a plain MD5 or a composite `"<md5>-<parts>"`; quoting varies by
    /// implementation, so never compare it raw.
    pub etag: Option<String>,
    pub last_modified_unix: Option<i64>,
}

/// What a local `stat` (plus, only under `verify_etag`, the local MD5) reports.
#[derive(Clone, Debug)]
pub(crate) struct LocalObject {
    pub size: u64,
    pub modified_unix: i64,
    /// Populated only in `verify_etag` mode, because computing it reads the
    /// entire object off disk.
    pub etag: Option<String>,
}

/// Why an object is being copied. Surfaced in the preview so an operator can see
/// what the run intends to do before it does it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CopyReason {
    /// We do not hold this key at all.
    Missing,
    SizeDiffers,
    SourceNewer,
    EtagDiffers,
    /// The source reported no timestamp, so "unchanged" cannot be established.
    NoTimestamp,
    /// `overwrite_all` was requested.
    Forced,
}

/// Why an object is being left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkipReason {
    /// `skip_existing` was requested and we already hold the key.
    Exists,
    EtagMatches,
    UpToDate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Copy(CopyReason),
    Skip(SkipReason),
}

impl Decision {
    pub(crate) fn is_copy(self) -> bool {
        matches!(self, Decision::Copy(_))
    }

    /// A short machine-readable tag for the preview JSON.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Decision::Copy(_) => "copy",
            Decision::Skip(_) => "skip",
        }
    }

    pub(crate) fn reason(self) -> serde_json::Value {
        match self {
            Decision::Copy(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
            Decision::Skip(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
        }
    }
}

/// Decide what to do with one source object.
///
/// `skew_secs` is the allowance for the source clock running ahead of ours; it
/// widens the window in which equal-sized objects count as already synced. Set
/// it negative to be more eager instead.
pub(crate) fn decide(
    mode: SyncMode,
    src: &SourceObject,
    local: Option<&LocalObject>,
    skew_secs: i64,
) -> Decision {
    if mode == SyncMode::OverwriteAll {
        return Decision::Copy(CopyReason::Forced);
    }
    let Some(local) = local else {
        return Decision::Copy(CopyReason::Missing);
    };
    if mode == SyncMode::SkipExisting {
        return Decision::Skip(SkipReason::Exists);
    }

    // Size is the one field both sides always report and always agree on.
    if local.size != src.size {
        return Decision::Copy(CopyReason::SizeDiffers);
    }

    // ETags are decisive only when the source ETag is a plain whole-object MD5
    // *and* the caller paid for the local hash. A composite "<md5>-<parts>"
    // ETag comes from a multipart upload and is not reproducible by our
    // single-shot writes, so it must fall through to the size+time rule below
    // rather than being treated as a mismatch.
    if let (Some(remote), Some(ours)) = (src.etag.as_deref(), local.etag.as_deref())
        && is_plain_md5(remote)
    {
        return if normalize_etag(remote).eq_ignore_ascii_case(normalize_etag(ours)) {
            Decision::Skip(SkipReason::EtagMatches)
        } else {
            Decision::Copy(CopyReason::EtagDiffers)
        };
    }

    // Same size, and our copy is at least as new as the source version, so it is
    // already synced. Our mtime is stamped by the atomic rename that ends a
    // copy, and so is always >= the moment we fetched the object.
    match src.last_modified_unix {
        None => Decision::Copy(CopyReason::NoTimestamp),
        Some(remote_ts) if local.modified_unix.saturating_add(skew_secs) >= remote_ts => {
            Decision::Skip(SkipReason::UpToDate)
        }
        Some(_) => Decision::Copy(CopyReason::SourceNewer),
    }
}

/// Strip the quoting that S3 implementations apply inconsistently to ETags.
pub(crate) fn normalize_etag(etag: &str) -> &str {
    etag.trim().trim_matches('"')
}

/// Whether an ETag is a whole-object MD5 (32 hex digits) rather than a composite
/// `"<md5>-<parts>"` produced by a multipart upload.
pub(crate) fn is_plain_md5(etag: &str) -> bool {
    let e = normalize_etag(etag);
    e.len() == 32 && e.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MD5_A: &str = "0123456789abcdef0123456789abcdef";
    const MD5_B: &str = "fedcba9876543210fedcba9876543210";

    fn src(size: u64, etag: Option<&str>, ts: Option<i64>) -> SourceObject {
        SourceObject {
            key: "k".to_owned(),
            size,
            etag: etag.map(ToOwned::to_owned),
            last_modified_unix: ts,
        }
    }

    fn local(size: u64, ts: i64, etag: Option<&str>) -> LocalObject {
        LocalObject { size, modified_unix: ts, etag: etag.map(ToOwned::to_owned) }
    }

    #[test]
    fn an_object_we_do_not_hold_is_copied() {
        let d = decide(SyncMode::NewAndChanged, &src(10, None, Some(100)), None, 0);
        assert_eq!(d, Decision::Copy(CopyReason::Missing));
    }

    #[test]
    fn a_differing_size_is_copied_even_when_our_copy_is_newer() {
        let d = decide(
            SyncMode::NewAndChanged,
            &src(20, None, Some(100)),
            Some(&local(10, 999, None)),
            0,
        );
        assert_eq!(d, Decision::Copy(CopyReason::SizeDiffers));
    }

    #[test]
    fn same_size_and_our_copy_is_newer_is_up_to_date() {
        let d = decide(
            SyncMode::NewAndChanged,
            &src(10, None, Some(100)),
            Some(&local(10, 100, None)),
            0,
        );
        assert_eq!(d, Decision::Skip(SkipReason::UpToDate));
    }

    #[test]
    fn same_size_but_a_newer_source_is_copied() {
        let d = decide(
            SyncMode::NewAndChanged,
            &src(10, None, Some(200)),
            Some(&local(10, 100, None)),
            0,
        );
        assert_eq!(d, Decision::Copy(CopyReason::SourceNewer));
    }

    #[test]
    fn a_source_without_a_timestamp_is_always_copied() {
        let d = decide(
            SyncMode::NewAndChanged,
            &src(10, None, None),
            Some(&local(10, 100, None)),
            0,
        );
        assert_eq!(d, Decision::Copy(CopyReason::NoTimestamp));
    }

    /// The regression test for the bug this whole module exists to avoid: a
    /// multipart-uploaded source object carries a composite ETag we can never
    /// reproduce. It must fall through to size+time, not be read as a mismatch,
    /// or every such object is re-downloaded on every run forever.
    #[test]
    fn a_composite_source_etag_does_not_force_a_recopy() {
        let composite = format!("{MD5_A}-4");
        let d = decide(
            SyncMode::NewAndChanged,
            &src(10, Some(&composite), Some(100)),
            Some(&local(10, 150, Some(MD5_B))),
            0,
        );
        assert_eq!(d, Decision::Skip(SkipReason::UpToDate));
    }

    #[test]
    fn a_plain_md5_etag_is_decisive_when_we_have_ours() {
        let same = decide(
            SyncMode::NewAndChanged,
            &src(10, Some(MD5_A), Some(999)),
            Some(&local(10, 1, Some(MD5_A))),
            0,
        );
        assert_eq!(same, Decision::Skip(SkipReason::EtagMatches));

        let differs = decide(
            SyncMode::NewAndChanged,
            &src(10, Some(MD5_A), Some(1)),
            Some(&local(10, 999, Some(MD5_B))),
            0,
        );
        assert_eq!(differs, Decision::Copy(CopyReason::EtagDiffers));
    }

    #[test]
    fn a_quoted_etag_compares_equal_to_its_unquoted_form() {
        let quoted = format!("\"{MD5_A}\"");
        let d = decide(
            SyncMode::NewAndChanged,
            &src(10, Some(&quoted), Some(999)),
            Some(&local(10, 1, Some(MD5_A))),
            0,
        );
        assert_eq!(d, Decision::Skip(SkipReason::EtagMatches));
    }

    #[test]
    fn without_verify_etag_the_local_hash_is_absent_and_time_decides() {
        // `local.etag == None` is what `verify_etag: false` produces.
        let d = decide(
            SyncMode::NewAndChanged,
            &src(10, Some(MD5_A), Some(100)),
            Some(&local(10, 150, None)),
            0,
        );
        assert_eq!(d, Decision::Skip(SkipReason::UpToDate));
    }

    #[test]
    fn skip_existing_short_circuits_every_other_check() {
        let d = decide(
            SyncMode::SkipExisting,
            &src(999, Some(MD5_A), Some(9999)),
            Some(&local(1, 1, Some(MD5_B))),
            0,
        );
        assert_eq!(d, Decision::Skip(SkipReason::Exists));
    }

    #[test]
    fn skip_existing_still_copies_what_is_missing() {
        let d = decide(SyncMode::SkipExisting, &src(10, None, Some(1)), None, 0);
        assert_eq!(d, Decision::Copy(CopyReason::Missing));
    }

    #[test]
    fn overwrite_all_copies_an_identical_object() {
        let d = decide(
            SyncMode::OverwriteAll,
            &src(10, Some(MD5_A), Some(1)),
            Some(&local(10, 999, Some(MD5_A))),
            0,
        );
        assert_eq!(d, Decision::Copy(CopyReason::Forced));
    }

    #[test]
    fn skew_widens_the_up_to_date_window_in_both_directions() {
        // Source is 30s ahead of our copy: without an allowance, it looks newer.
        let s = src(10, None, Some(130));
        let l = local(10, 100, None);
        assert_eq!(decide(SyncMode::NewAndChanged, &s, Some(&l), 0), Decision::Copy(CopyReason::SourceNewer));
        assert_eq!(decide(SyncMode::NewAndChanged, &s, Some(&l), 30), Decision::Skip(SkipReason::UpToDate));
        // A negative allowance makes the rule stricter.
        let equal = src(10, None, Some(100));
        assert_eq!(
            decide(SyncMode::NewAndChanged, &equal, Some(&l), -1),
            Decision::Copy(CopyReason::SourceNewer)
        );
    }

    #[test]
    fn skew_does_not_overflow_at_the_extremes() {
        let s = src(10, None, Some(i64::MAX));
        let l = local(10, i64::MAX, None);
        // Must not panic in debug builds.
        let _ = decide(SyncMode::NewAndChanged, &s, Some(&l), i64::MAX);
    }

    #[test]
    fn plain_md5_detection_rejects_composites_and_malformed_tags() {
        assert!(is_plain_md5(MD5_A));
        assert!(is_plain_md5(&format!("\"{MD5_A}\"")));
        assert!(!is_plain_md5(&format!("{MD5_A}-4")));
        assert!(!is_plain_md5(""));
        assert!(!is_plain_md5("abc"));
        // Right length, not hex.
        assert!(!is_plain_md5("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"));
    }

    #[test]
    fn etag_normalisation_strips_quotes_and_whitespace() {
        assert_eq!(normalize_etag("  \"abc\" "), "abc");
        assert_eq!(normalize_etag("abc"), "abc");
    }
}
