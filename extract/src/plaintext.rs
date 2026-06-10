//! Plain text extraction.
//!
//! Splits text into paragraph blocks by blank lines.
//! No structure is detected — all blocks are `BlockKind::Paragraph`.

use crate::{make_block, ExtractionOutput};
use localdb_core::{BlockKind, Error, Span};

/// Extract a plain text document.
///
/// Paragraphs are delimited by one or more blank lines.
/// The normalized text is the input text verbatim (no transformation),
/// with a trailing newline ensured.
pub fn extract_plaintext(input: &str) -> Result<ExtractionOutput, Error> {
    // Normalize line endings
    let normalized_input = input.replace("\r\n", "\n").replace('\r', "\n");

    // Ensure trailing newline in normalized text
    let mut normalized = normalized_input.clone();
    if !normalized.ends_with('\n') {
        normalized.push('\n');
    }

    let mut blocks = Vec::new();
    let mut ordinal = 0usize;
    let mut pos = 0usize;

    // Split by double newlines (blank line separates paragraphs)
    // We walk through the normalized text collecting paragraphs
    let text_bytes = normalized.as_bytes();

    while pos < text_bytes.len() {
        // Skip leading blank lines
        while pos < text_bytes.len() {
            if text_bytes[pos] == b'\n' {
                pos += 1;
            } else {
                break;
            }
        }

        if pos >= text_bytes.len() {
            break;
        }

        let start = pos;

        // Find the end of the paragraph (double newline or end of text)
        loop {
            if pos >= text_bytes.len() {
                break;
            }

            // Check for blank line: \n\n
            if text_bytes[pos] == b'\n' {
                if pos + 1 < text_bytes.len() && text_bytes[pos + 1] == b'\n' {
                    // End of paragraph — include the first newline in the span
                    pos += 1; // consume the first newline
                    break;
                } else {
                    pos += 1;
                }
            } else {
                pos += 1;
            }
        }

        let end = pos;
        let para_text = &normalized[start..end];
        let para_text_trimmed = para_text.trim();

        if !para_text_trimmed.is_empty() {
            let block = make_block(
                ordinal,
                BlockKind::Paragraph,
                para_text_trimmed.to_string(),
                Span::new(start, end),
                vec![], // no heading context for plain text
            );
            blocks.push(block);
            ordinal += 1;
        }
    }

    Ok(ExtractionOutput {
        text: normalized,
        blocks,
        title: None, // plain text has no title structure
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::BlockKind;

    #[test]
    fn extracts_single_paragraph() {
        let input = "Hello world. This is a single paragraph.";
        let out = extract_plaintext(input).unwrap();
        assert_eq!(out.blocks.len(), 1);
        assert_eq!(out.blocks[0].kind, BlockKind::Paragraph);
        assert!(out.blocks[0].text.contains("Hello world"));
    }

    #[test]
    fn splits_on_blank_lines() {
        let input = "First paragraph.\nStill first.\n\nSecond paragraph.\n";
        let out = extract_plaintext(input).unwrap();
        assert_eq!(
            out.blocks.len(),
            2,
            "Expected 2 paragraphs, got {}",
            out.blocks.len()
        );
        assert!(out.blocks[0].text.contains("First paragraph"));
        assert!(out.blocks[1].text.contains("Second paragraph"));
    }

    #[test]
    fn handles_multiple_blank_lines() {
        let input = "Para one.\n\n\n\nPara two.\n";
        let out = extract_plaintext(input).unwrap();
        assert_eq!(out.blocks.len(), 2);
    }

    #[test]
    fn spans_index_into_normalized_text_exactly() {
        let input = "First paragraph.\n\nSecond paragraph.\n";
        let out = extract_plaintext(input).unwrap();

        for block in &out.blocks {
            let span_text = &out.text[block.span.start..block.span.end];
            assert!(
                span_text.contains(block.text.trim()),
                "Span [{}, {}) {:?} should contain {:?}",
                block.span.start,
                block.span.end,
                span_text,
                block.text
            );
        }
    }

    #[test]
    fn no_title_for_plain_text() {
        let input = "Some text.";
        let out = extract_plaintext(input).unwrap();
        assert!(out.title.is_none());
    }

    #[test]
    fn empty_input_produces_no_blocks() {
        let out = extract_plaintext("").unwrap();
        assert!(out.blocks.is_empty());
    }

    #[test]
    fn heading_paths_are_empty() {
        let input = "Para one.\n\nPara two.";
        let out = extract_plaintext(input).unwrap();
        for block in &out.blocks {
            assert!(
                block.heading_path.is_empty(),
                "Plain text blocks should have empty heading_path"
            );
        }
    }

    #[test]
    fn golden_file_plain_txt() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/plain.txt"
        ))
        .expect("fixture file not found");
        let out = extract_plaintext(&text).unwrap();

        // Fixture has 3 paragraphs
        assert_eq!(
            out.blocks.len(),
            3,
            "Expected 3 paragraphs, got {}",
            out.blocks.len()
        );

        // All spans valid
        for block in &out.blocks {
            assert!(block.span.end <= out.text.len());
            let span_text = &out.text[block.span.start..block.span.end];
            assert!(span_text.contains(block.text.trim()));
        }
    }
}
