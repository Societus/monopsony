//! Shannon entropy heuristic for cache content scanning.
//!
//! Computes Shannon entropy for files inside tar archives (or raw blobs)
//! and flags high-entropy content that may indicate encrypted, compressed,
//! or packed payloads. Generates `flagged` status only — never `blocked`.

use std::io::Read;

/// Maximum bytes to read per tar member when computing entropy.
/// Prevents OOM on huge files; entropy converges well before 10MB.
const MAX_MEMBER_BYTES: usize = 10 * 1024 * 1024;

/// Result of an entropy scan.
#[derive(Debug, Clone)]
pub struct EntropyResult {
    /// Whether any member exceeded the threshold.
    pub flagged: bool,
    /// Highest entropy value found across all members.
    pub max_entropy: f64,
    /// Members that exceeded the threshold.
    pub flagged_members: Vec<FlaggedMember>,
}

/// A single tar member that exceeded the entropy threshold.
#[derive(Debug, Clone)]
pub struct FlaggedMember {
    /// File path within the archive.
    pub name: String,
    /// Shannon entropy of the file content.
    pub entropy: f64,
}

/// Compute Shannon entropy of a byte slice.
///
/// Returns a value between 0.0 (all identical bytes) and 8.0 (perfectly random).
/// Uses base-2 logarithm (bits per byte).
pub fn shannon_entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut freq = [0usize; 256];
    for &b in data {
        freq[b as usize] += 1;
    }
    let len = data.len() as f64;
    let mut entropy = 0.0;
    for &count in &freq {
        if count == 0 {
            continue;
        }
        let p = count as f64 / len;
        entropy -= p * p.log2();
    }
    entropy
}

/// Scan data for high-entropy content.
///
/// If the data is a tar archive, each member is scanned individually.
/// Otherwise, entropy is computed on the entire blob.
pub fn scan_entropy(data: &[u8], threshold: f64) -> EntropyResult {
    let mut max_entropy = 0.0f64;
    let mut flagged_members = Vec::new();

    // Try parsing as tar archive
    let cursor = std::io::Cursor::new(data);
    let mut archive = tar::Archive::new(cursor);

    // Iterate tar entries
    let entries_result = archive.entries();
    match entries_result {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                let name = entry
                    .path()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();

                // Skip non-regular files
                if !entry.header().entry_type().is_file() {
                    continue;
                }

                // Read up to MAX_MEMBER_BYTES
                let mut content = Vec::new();
                let mut limited = entry.take(MAX_MEMBER_BYTES as u64);
                let _ = std::io::Read::read_to_end(&mut limited, &mut content);

                if content.is_empty() {
                    continue;
                }

                let e = shannon_entropy(&content);
                if e > max_entropy {
                    max_entropy = e;
                }
                if e > threshold {
                    flagged_members.push(FlaggedMember { name, entropy: e });
                }
            }
        }
        Err(_) => {
            // Not a valid tar — compute entropy on whole blob
            let e = shannon_entropy(data);
            return EntropyResult {
                flagged: e > threshold,
                max_entropy: e,
                flagged_members: if e > threshold {
                    vec![FlaggedMember {
                        name: "<raw-data>".to_string(),
                        entropy: e,
                    }]
                } else {
                    vec![]
                },
            };
        }
    }

    // Handle empty archives — no entries found
    if max_entropy == 0.0 && flagged_members.is_empty() && !data.is_empty() {
        // Archive had no scannable file entries; check the raw data
        let e = shannon_entropy(data);
        max_entropy = e;
        if e > threshold {
            flagged_members.push(FlaggedMember {
                name: "<archive>".to_string(),
                entropy: e,
            });
        }
    }

    EntropyResult {
        flagged: !flagged_members.is_empty(),
        max_entropy,
        flagged_members,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entropy_uniform_bytes() {
        let data = vec![0u8; 1000];
        let entropy = shannon_entropy(&data);
        assert!(
            entropy < 0.01,
            "uniform bytes should have ~0 entropy, got {}",
            entropy
        );
    }

    #[test]
    fn test_entropy_all_byte_values() {
        // Each byte value 0-255 appears 10 times — max entropy
        let data: Vec<u8> = (0..=255u8).cycle().take(2560).collect();
        let entropy = shannon_entropy(&data);
        assert!(
            entropy > 7.9,
            "all byte values should give ~8.0 entropy, got {}",
            entropy
        );
    }

    #[test]
    fn test_entropy_empty() {
        assert_eq!(shannon_entropy(&[]), 0.0);
    }

    #[test]
    fn test_entropy_single_byte() {
        // Single byte — entropy is 0 (only one possible value)
        assert_eq!(shannon_entropy(&[42]), 0.0);
    }

    #[test]
    fn test_entropy_ascii_text() {
        // Typical English text — moderate entropy (~4-5 bits)
        let text = b"the quick brown fox jumps over the lazy dog";
        let entropy = shannon_entropy(text);
        assert!(
            entropy > 3.0 && entropy < 6.0,
            "ASCII text should be ~4-5 entropy, got {}",
            entropy
        );
    }

    #[test]
    fn test_scan_entropy_non_tar_below_threshold() {
        let data = vec![0u8; 100];
        let result = scan_entropy(&data, 7.5);
        assert!(!result.flagged);
        assert!(result.max_entropy < 0.01);
    }

    #[test]
    fn test_scan_entropy_tar_high_entropy_member() {
        let tar_data = make_test_tar(&[
            ("low.txt", &[0u8; 100]),
            ("high.bin", &(0..=255u8).cycle().take(512).collect::<Vec<_>>()),
        ]);
        let result = scan_entropy(&tar_data, 7.5);
        assert!(result.flagged, "should flag high-entropy member");
        assert!(
            result.max_entropy > 7.5,
            "max entropy should exceed threshold, got {}",
            result.max_entropy
        );
        assert_eq!(result.flagged_members.len(), 1);
        assert_eq!(result.flagged_members[0].name, "high.bin");
    }

    #[test]
    fn test_scan_entropy_tar_all_low() {
        let tar_data = make_test_tar(&[("normal.txt", b"hello world this is normal text")]);
        let result = scan_entropy(&tar_data, 7.5);
        assert!(!result.flagged);
        assert!(result.max_entropy < 7.5);
    }

    #[test]
    fn test_scan_entropy_empty_data() {
        let result = scan_entropy(&[], 7.5);
        assert!(!result.flagged);
        assert_eq!(result.max_entropy, 0.0);
    }

    fn make_test_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_path(name).unwrap();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }
}
