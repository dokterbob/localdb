#[cfg(test)]
mod bench_extract_chunk {
    use std::time::Instant;

    #[test]
    #[ignore = "requires LOCALDB_BENCH_FILE env var and file on disk"]
    fn bench_file_extract_chunk() {
        let path = match std::env::var("LOCALDB_BENCH_FILE") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("LOCALDB_BENCH_FILE not set — skipping");
                return;
            }
        };

        let bytes = std::fs::read(&path).expect("read file");
        let filename = std::path::Path::new(&path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        println!(
            "\n=== bench_file_extract_chunk: {filename} ({} bytes) ===",
            bytes.len()
        );

        // Extract
        let t_extract = Instant::now();
        let extractor = extract::ChainExtractor::with_defaults().expect("build extractor");
        use localdb_core::ingestion::DocumentExtractor as _;
        let extraction = extractor
            .extract(&bytes, Some(filename))
            .expect("extraction failed");
        let extract_ms = t_extract.elapsed().as_millis();
        let markdown_bytes = extraction.markdown.len();
        let inflation = markdown_bytes as f64 / bytes.len() as f64;
        println!(
            "Extract: {extract_ms}ms | {markdown_bytes} markdown bytes | inflation {inflation:.2}x"
        );

        // Helper: compute distribution
        fn distribution(sizes: &[usize]) -> (usize, usize, usize, usize) {
            let mut s = sizes.to_vec();
            s.sort_unstable();
            let n = s.len();
            if n == 0 {
                return (0, 0, 0, 0);
            }
            let median = s[n / 2];
            let p90 = s[(n * 9) / 10];
            (*s.first().unwrap(), median, p90, *s.last().unwrap())
        }

        fn histogram(sizes: &[usize]) -> [usize; 6] {
            let mut h = [0usize; 6];
            for &s in sizes {
                let b = if s <= 128 {
                    0
                } else if s <= 512 {
                    1
                } else if s <= 1024 {
                    2
                } else if s <= 2048 {
                    3
                } else if s <= 4096 {
                    4
                } else {
                    5
                };
                h[b] += 1;
            }
            h
        }

        // A stable document ID string for chunk_blocks
        let doc_id = "bench";

        for (label, cfg) in [
            ("code ", localdb_core::ChunkerConfig::code()),
            ("prose", localdb_core::ChunkerConfig::prose()),
        ] {
            let t_chunk = Instant::now();
            let sizer = localdb_core::CharSizer;
            let blocks = localdb_core::markdown_to_blocks(&extraction.markdown);
            let chunks =
                localdb_core::chunk_blocks(doc_id, &blocks, &cfg, &sizer).expect("chunk failed");
            let chunk_ms = t_chunk.elapsed().as_millis();

            let char_sizes: Vec<usize> = chunks.iter().map(|c| c.text.chars().count()).collect();
            let (mn, med, p90, mx) = distribution(&char_sizes);
            let hist = histogram(&char_sizes);
            println!(
                "Chunk [{label}]: {chunk_ms}ms | n={} | chars min={mn} med={med} p90={p90} max={mx}",
                chunks.len()
            );
            println!(
                "  histogram (chars): ≤128={} ≤512={} ≤1024={} ≤2048={} ≤4096={} >4096={}",
                hist[0], hist[1], hist[2], hist[3], hist[4], hist[5]
            );
        }
    }
}
