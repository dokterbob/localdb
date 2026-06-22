/// Regression test confirming anytomd hangs on large XLSX files.
///
/// Run with:
///   cargo test -p extract --test anytomd_xlsx_bug -- --ignored --nocapture
#[test]
#[ignore]
fn anytomd_xlsx_hangs_on_large_file() {
    use std::time::{Duration, Instant};

    let path = "/Users/drbob/Nextcloud/Work/Treescape/Data/EU Species Lists/official_EU_list_2023_05_16.xlsx";
    let bytes = std::fs::read(path).expect("test XLSX file must exist");
    eprintln!("Read {} bytes from XLSX", bytes.len());

    let t = Instant::now();
    let handle = std::thread::spawn(move || {
        anytomd::convert_bytes(&bytes, "xlsx", &anytomd::ConversionOptions::default())
    });

    let result = loop {
        if handle.is_finished() {
            break handle.join();
        }
        if t.elapsed() > Duration::from_secs(30) {
            eprintln!(
                "TIMED OUT after 30s — anytomd XLSX is confirmed broken for large files"
            );
            return; // test passes: we confirmed the bug
        }
        std::thread::sleep(Duration::from_millis(500));
    };

    let elapsed_ms = t.elapsed().as_millis();
    match result {
        Ok(Ok(_)) => eprintln!("Completed successfully in {elapsed_ms}ms"),
        Ok(Err(e)) => eprintln!("Completed with error in {elapsed_ms}ms: {e}"),
        Err(e) => eprintln!("Thread panicked in {elapsed_ms}ms: {e:?}"),
    }
    if elapsed_ms > 10_000 {
        panic!("anytomd took {elapsed_ms}ms on a 6.9 MB XLSX — performance bug confirmed");
    }
}
