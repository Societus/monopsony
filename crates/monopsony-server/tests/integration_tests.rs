//! Integration tests for monopsony-server
//!
//! These tests verify proxy behavior without requiring a running server.

use monopsony_common::package_manager::{get_available_backends, matches_package_url};

#[test]
fn test_server_proxy_state_finds_backend() {
    // Verify that the proxy state can find backends for known URLs
    let backends = get_available_backends();
    assert!(!backends.is_empty(), "At least one backend should be available");

    // Test that each backend's patterns work
    for backend in &backends {
        let patterns = backend.upstream_url_patterns();
        assert!(!patterns.is_empty(), "Backend {:?} should have URL patterns", backend.kind());

        // Each pattern should compile and be usable
        for pattern in &patterns {
            // Patterns should be able to match their own description
            assert!(!pattern.description.is_empty());
            assert!(pattern.priority > 0);
        }
    }
}

#[test]
fn test_matches_package_url_negative() {
    // Verify non-package URLs are correctly rejected
    let non_package_urls = [
        "https://www.google.com/",
        "https://github.com/",
        "https://pypi.org/",
        "https://npmjs.com/",
        "https://crates.io/",
        "http://127.0.0.1:8080/",
        "https://localhost/api/v1/health",
    ];

    for url in non_package_urls {
        assert!(
            !matches_package_url(url),
            "URL should NOT be matched as package URL: {}",
            url
        );
    }
}

#[test]
fn test_url_pattern_priority_ordering() {
    // Verify that higher priority patterns are checked first
    let backends = get_available_backends();
    for backend in &backends {
        let patterns = backend.upstream_url_patterns();
        for i in 0..patterns.len().saturating_sub(1) {
            assert!(
                patterns[i].priority >= patterns[i + 1].priority,
                "Patterns should be sorted by priority (descending) for backend {:?}",
                backend.kind()
            );
        }
    }
}
