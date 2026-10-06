//! Writes real Databento files through the capture, locally, and checks what comes back.
//!
//! `capture_real OUTDIR FILE.dbn.zst...`
//!
//! Every record of each file goes through [`RawWriter`] as a provider-native record, in the order the
//! files are given; the capture is then verified and replayed, and the events of the replay are
//! compared with the events of decoding the same files directly. Reports the write speed.

use std::time::Instant;

use dbn::decode::{DbnDecoder, DecodeRecordRef};
use tf_capture::{CaptureReplay, Config, RawWriter, verify};
use tf_core::Event;
use tf_databento::{Decoder, Item};
use tf_provider::{Poll, Provider};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let out = std::path::PathBuf::from(&a[0]);
    let _ = std::fs::remove_dir_all(&out);
    let (mut w, _) = RawWriter::open(Config::new(&out, "XNAS.BASIC")).expect("open");
    let started = Instant::now();
    let mut records = 0u64;
    for f in &a[1..] {
        let mut dec = DbnDecoder::from_zstd_file(f).expect("open dbn");
        while let Some(rec) = dec.decode_record_ref().expect("decode") {
            w.write(&rec).expect("write");
            records += 1;
            if records % 100_000 == 0 {
                w.sync().expect("sync");
            }
        }
    }
    let (n, segments) = w.finish().expect("finish");
    let secs = started.elapsed().as_secs_f64();
    println!(
        "wrote {n} records into {segments} segments in {secs:.2}s = {:.2}M records/s (decode + write + zstd + fsync every 100k)",
        n as f64 / secs / 1e6
    );
    let on_disk: u64 = tf_capture::list(&out)
        .expect("list")
        .iter()
        .map(|e| e.bytes)
        .sum();
    let input: u64 = a[1..]
        .iter()
        .map(|f| std::fs::metadata(f).expect("size").len())
        .sum();
    println!(
        "on disk {:.1} MB; the input files are {:.1} MB",
        on_disk as f64 / 1e6,
        input as f64 / 1e6
    );

    let t = Instant::now();
    let rep = verify(&out).expect("verify");
    println!(
        "verify: {} segments, {} records, {} problems ({:.2}s)",
        rep.segments,
        rep.records,
        rep.problems.len(),
        t.elapsed().as_secs_f64()
    );

    // The same events, whether decoded straight from the files or from the capture.
    let mut direct: Vec<Event> = Vec::new();
    let mut ids = tf_databento::InstrumentMap::default();
    for f in &a[1..] {
        let mut d = Decoder::from_zstd_file(f)
            .expect("open")
            .with_instruments(ids);
        while let Some(i) = d.next_item().expect("decode") {
            if let Item::Event(e) = i {
                direct.push(e);
            }
        }
        ids = d.into_instruments();
    }
    let t = Instant::now();
    let mut replay = CaptureReplay::open(&out).expect("replay");
    let mut got: Vec<Event> = Vec::new();
    while let Poll::Events(_) = replay.poll(&mut got, 65_536) {}
    println!(
        "replayed {} events ({:.2}s); decoding the files directly gives {}",
        got.len(),
        t.elapsed().as_secs_f64(),
        direct.len()
    );
    let same = got == direct;
    println!(
        "{}",
        if same {
            "replay and direct decoding agree event for event"
        } else {
            "THEY DIFFER"
        }
    );
    if !same || !rep.is_clean() || rep.records != n {
        std::process::exit(1);
    }
}
