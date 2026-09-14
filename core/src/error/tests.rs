use super::*;

#[test]
fn error_codes_are_stable() {
    // Verify every variant has a known stable code
    let cases: &[(Error, &str, i32)] = &[
        (
            Error::StoreNotFound { id: "x".into() },
            "store_not_found",
            3,
        ),
        (
            Error::SourceNotFound { id: "x".into() },
            "source_not_found",
            3,
        ),
        (
            Error::ResourceNotFound { id: "x".into() },
            "resource_not_found",
            3,
        ),
        (Error::JobNotFound { id: "x".into() }, "job_not_found", 3),
        (Error::RuntimeStateLocked, "runtime_state_locked", 4),
        (Error::DaemonRunning, "daemon_running", 4),
        (Error::DaemonUnreachable, "daemon_unreachable", 5),
        (
            Error::InvalidConfig {
                message: "m".into(),
            },
            "invalid_config",
            2,
        ),
        (
            Error::InvalidRequest {
                message: "m".into(),
            },
            "invalid_request",
            2,
        ),
        (
            Error::UnsupportedFormat {
                format: "pdf".into(),
            },
            "unsupported_format",
            2,
        ),
        (
            Error::ExtractionFailed {
                format: "office/docx".into(),
                reason: "zip error".into(),
            },
            "extraction_failed",
            2,
        ),
        (
            Error::ProviderUnavailable {
                message: "m".into(),
            },
            "provider_unavailable",
            5,
        ),
        (
            Error::ModelMissing {
                message: "m".into(),
            },
            "model_missing",
            5,
        ),
        (Error::IndexInProgress, "index_in_progress", 4),
        (Error::JobCancelled, "job_cancelled", 4),
        (Error::JobAlreadyTerminal, "job_already_terminal", 4),
        (
            Error::Internal {
                message: "bug".into(),
                correlation_id: "abc123".into(),
            },
            "internal",
            1,
        ),
        (
            Error::Unauthorized {
                message: "m".into(),
            },
            "unauthorized",
            6,
        ),
        (
            Error::Forbidden {
                message: "m".into(),
            },
            "forbidden",
            6,
        ),
        (
            Error::RateLimited {
                message: "m".into(),
            },
            "rate_limited",
            5,
        ),
    ];

    for (err, expected_code, expected_exit) in cases {
        assert_eq!(err.code(), *expected_code, "code mismatch for {:?}", err);
        assert_eq!(
            err.exit_code(),
            *expected_exit,
            "exit_code mismatch for {:?}",
            err
        );
    }
}

#[test]
fn error_display_contains_context() {
    let err = Error::StoreNotFound {
        id: "my-store".into(),
    };
    assert!(err.to_string().contains("my-store"));

    let err = Error::Internal {
        message: "something broke".into(),
        correlation_id: "corr-1".into(),
    };
    assert!(err.to_string().contains("corr-1"));
    assert!(err.to_string().contains("something broke"));
}

#[test]
fn all_not_found_variants_exit_3() {
    assert_eq!(Error::StoreNotFound { id: "s".into() }.exit_code(), 3);
    assert_eq!(Error::SourceNotFound { id: "s".into() }.exit_code(), 3);
    assert_eq!(Error::ResourceNotFound { id: "s".into() }.exit_code(), 3);
    assert_eq!(Error::JobNotFound { id: "s".into() }.exit_code(), 3);
}

// -- from_code: round trip with code(), and the two documented gaps -----

#[test]
fn from_code_round_trips_every_code_with_a_message_field() {
    // Every variant whose `code()` output `from_code` claims to
    // recognize must decode back to an equal value when fed its own
    // `code()` + a representative message — this is what makes it safe
    // for `finish_job`/`decode_daemon_error` to reconstruct the typed
    // error a job or an HTTP error body only carries as a string pair.
    let cases: &[Error] = &[
        Error::StoreNotFound { id: "x".into() },
        Error::SourceNotFound { id: "x".into() },
        Error::ResourceNotFound { id: "x".into() },
        Error::JobNotFound { id: "x".into() },
        Error::RuntimeStateLocked,
        Error::DaemonRunning,
        Error::DaemonUnreachable,
        Error::InvalidConfig {
            message: "x".into(),
        },
        Error::InvalidRequest {
            message: "x".into(),
        },
        Error::IndexInProgress,
        Error::JobCancelled,
        Error::JobAlreadyTerminal,
        Error::ProviderUnavailable {
            message: "x".into(),
        },
        Error::ModelMissing {
            message: "x".into(),
        },
        Error::RateLimited {
            message: "x".into(),
        },
    ];
    for err in cases {
        let decoded = Error::from_code(err.code(), "x".to_string());
        assert_eq!(decoded.as_ref(), Some(err), "round trip failed for {err:?}");
    }
}

#[test]
fn from_code_accepts_the_legacy_document_not_found_alias() {
    assert_eq!(
        Error::from_code("document_not_found", "doc-1".to_string()),
        Some(Error::ResourceNotFound {
            id: "doc-1".to_string()
        })
    );
}

#[test]
fn from_code_returns_none_for_an_unrecognized_or_unmappable_code() {
    // An unknown code (e.g. a newer daemon build) and every code whose
    // variant doesn't fit a single `message` field (internal,
    // unsupported_format, extraction_failed) all return `None` so the
    // caller applies its own fallback.
    assert_eq!(Error::from_code("something_new", "x".to_string()), None);
    assert_eq!(Error::from_code("internal", "x".to_string()), None);
    assert_eq!(
        Error::from_code("unsupported_format", "x".to_string()),
        None
    );
    assert_eq!(Error::from_code("extraction_failed", "x".to_string()), None);
}

// -- raw_message: the 9 reconstructible variants, and a few that aren't -

#[test]
fn raw_message_returns_the_bare_field_for_reconstructible_variants() {
    // Every variant `from_code` can rebuild from a single `message`
    // string must hand back exactly that field, unprefixed — this is
    // what lets a producer avoid double-prefixing when a consumer later
    // runs the string back through `from_code` + `Display`.
    assert_eq!(
        Error::StoreNotFound { id: "x".into() }.raw_message(),
        Some("x")
    );
    assert_eq!(
        Error::SourceNotFound { id: "x".into() }.raw_message(),
        Some("x")
    );
    assert_eq!(
        Error::ResourceNotFound { id: "x".into() }.raw_message(),
        Some("x")
    );
    assert_eq!(
        Error::JobNotFound { id: "x".into() }.raw_message(),
        Some("x")
    );
    assert_eq!(
        Error::InvalidConfig {
            message: "unconfigured embedder provider".into(),
        }
        .raw_message(),
        Some("unconfigured embedder provider")
    );
    assert_eq!(
        Error::InvalidRequest {
            message: "x".into()
        }
        .raw_message(),
        Some("x")
    );
    assert_eq!(
        Error::ProviderUnavailable {
            message: "x".into()
        }
        .raw_message(),
        Some("x")
    );
    assert_eq!(
        Error::ModelMissing {
            message: "x".into()
        }
        .raw_message(),
        Some("x")
    );
    assert_eq!(
        Error::RateLimited {
            message: "x".into()
        }
        .raw_message(),
        Some("x")
    );
}

#[test]
fn raw_message_is_none_for_non_reconstructible_variants() {
    // Variants `from_code` never decodes (a fixed-message variant like
    // `RuntimeStateLocked`, or one whose fields don't fit a single
    // `message` string) have no bare message to hand back — callers must
    // fall back to `to_string()`.
    assert_eq!(Error::RuntimeStateLocked.raw_message(), None);
    assert_eq!(Error::DaemonRunning.raw_message(), None);
    assert_eq!(Error::DaemonUnreachable.raw_message(), None);
    assert_eq!(Error::IndexInProgress.raw_message(), None);
    assert_eq!(Error::JobCancelled.raw_message(), None);
    assert_eq!(Error::JobAlreadyTerminal.raw_message(), None);
    assert_eq!(
        Error::UnsupportedFormat {
            format: "pdf".into()
        }
        .raw_message(),
        None
    );
    assert_eq!(
        Error::ExtractionFailed {
            format: "office/docx".into(),
            reason: "zip error".into(),
        }
        .raw_message(),
        None
    );
    assert_eq!(
        Error::Internal {
            message: "bug".into(),
            correlation_id: "abc".into(),
        }
        .raw_message(),
        None
    );
}

#[test]
fn conflict_errors_exit_4() {
    assert_eq!(Error::RuntimeStateLocked.exit_code(), 4);
    assert_eq!(Error::DaemonRunning.exit_code(), 4);
    assert_eq!(Error::IndexInProgress.exit_code(), 4);
    assert_eq!(Error::JobCancelled.exit_code(), 4);
    assert_eq!(Error::JobAlreadyTerminal.exit_code(), 4);
}

/// A failed `IndexJob`'s `error_code: "job_cancelled"` must reconstruct
/// through `Error::from_code` into exactly `Error::JobCancelled` — the
/// mechanism `cli::job_attach::finish_job` relies on to give a
/// daemon-attached CLI (e.g. `localdb index` watching a job someone else
/// cancelled) the same exit code (4) a direct `job cancel` caller gets,
/// with zero special-casing in `finish_job` itself (issue #218).
#[test]
fn job_cancelled_round_trips_through_from_code() {
    assert_eq!(
        Error::from_code("job_cancelled", "job was cancelled".to_string()),
        Some(Error::JobCancelled)
    );
}

#[test]
fn job_already_terminal_round_trips_through_from_code() {
    assert_eq!(
        Error::from_code(
            "job_already_terminal",
            "job already reached a terminal state; cannot cancel".to_string()
        ),
        Some(Error::JobAlreadyTerminal)
    );
}

#[test]
fn unavailable_errors_exit_5() {
    assert_eq!(Error::DaemonUnreachable.exit_code(), 5);
    assert_eq!(
        Error::ProviderUnavailable {
            message: "m".into()
        }
        .exit_code(),
        5
    );
    assert_eq!(
        Error::ModelMissing {
            message: "m".into()
        }
        .exit_code(),
        5
    );
    assert_eq!(
        Error::RateLimited {
            message: "m".into()
        }
        .exit_code(),
        5
    );
}

#[test]
fn auth_errors_exit_6() {
    assert_eq!(
        Error::Unauthorized {
            message: "m".into()
        }
        .exit_code(),
        6
    );
    assert_eq!(
        Error::Forbidden {
            message: "m".into()
        }
        .exit_code(),
        6
    );
    assert_eq!(
        Error::Unauthorized {
            message: "m".into()
        }
        .code(),
        "unauthorized"
    );
    assert_eq!(
        Error::Forbidden {
            message: "m".into()
        }
        .code(),
        "forbidden"
    );
}
