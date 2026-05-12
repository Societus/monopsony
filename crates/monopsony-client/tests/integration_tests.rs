//! Integration tests for monopsony-client
//!
//! These tests verify the URL filtering logic without requiring a running server.

use monopsony_common::package_manager::{get_available_backends, matches_package_url, PackageManagerBackend};

#[test]
fn test_available_backends_filtering() {
    let backends = get_available_backends();
    for backend in &backends {
        assert!(
            backend.is_available(),
            "Backend {:?} should be available",
            backend.kind()
        );
    }
}

#[test]
fn test_matches_non_package_url() {
    let non_package_urls = [
        "https://www.google.com/",
        "https://github.com/",
        "https://pypi.org/",
        "https://npmjs.com/",
        "https://crates.io/",
        "http://127.0.0.1:8080/",
        "https://localhost/api/v1/health",
    ];
    for url in &non_package_urls {
        assert!(
            !matches_package_url(url),
            "URL should NOT be matched as package URL: {}",
            url
        );
    }
}

#[test]
fn test_url_patterns_exist_on_available_backends() {
    let backends = get_available_backends();
    for backend in &backends {
        let patterns = backend.upstream_url_patterns();
        assert!(
            !patterns.is_empty(),
            "Backend {:?} should have URL patterns",
            backend.kind()
        );
    }
}

#[test]
fn test_all_backends_have_patterns() {
    use monopsony_common::package_manager::{
        AptBackend, ChocoBackend, DnfBackend, PacmanBackend, ScoopBackend, WingetBackend,
    };

    let all_backends: Vec<Box<dyn PackageManagerBackend>> = vec![
        Box::new(AptBackend::new()),
        Box::new(DnfBackend::new()),
        Box::new(PacmanBackend::new()),
        Box::new(WingetBackend::new()),
        Box::new(ChocoBackend::new()),
        Box::new(ScoopBackend::new()),
    ];

    for backend in &all_backends {
        let kind = backend.kind();
        let patterns = backend.upstream_url_patterns();
        assert!(
            !patterns.is_empty(),
            "Backend {:?} should have URL patterns defined",
            kind
        );
        for pattern in &patterns {
            assert!(pattern.priority > 0, "Pattern priority must be > 0");
            assert!(
                !pattern.description.is_empty(),
                "Pattern must have a description"
            );
        }
    }
}

#[test]
fn test_url_pattern_priority_ordering() {
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
