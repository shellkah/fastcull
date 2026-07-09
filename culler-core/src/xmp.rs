use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::writer::Writer;
use std::io::{self, Write};
use std::path::Path;

/// Build an XMP sidecar document string: `xmp:Rating` from `rating` (when Some)
/// and a `dc:subject` `rdf:Bag` of keywords (one `rdf:li` per tag). Wrapped in
/// the conventional `xpacket` envelope so Lightroom / darktable / Bridge import it.
pub fn build_xmp(tags: &[String], rating: Option<i32>) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 1);

    let mut meta = BytesStart::new("x:xmpmeta");
    meta.push_attribute(("xmlns:x", "adobe:ns:meta/"));
    w.write_event(Event::Start(meta)).expect("write xmpmeta");

    let mut rdf = BytesStart::new("rdf:RDF");
    rdf.push_attribute(("xmlns:rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"));
    w.write_event(Event::Start(rdf)).expect("write rdf");

    let mut desc = BytesStart::new("rdf:Description");
    desc.push_attribute(("rdf:about", ""));
    desc.push_attribute(("xmlns:dc", "http://purl.org/dc/elements/1.1/"));
    desc.push_attribute(("xmlns:xmp", "http://ns.adobe.com/xap/1.0/"));
    w.write_event(Event::Start(desc))
        .expect("write description");

    if let Some(r) = rating {
        w.write_event(Event::Start(BytesStart::new("xmp:Rating")))
            .expect("write");
        w.write_event(Event::Text(BytesText::new(&r.to_string())))
            .expect("write");
        w.write_event(Event::End(BytesEnd::new("xmp:Rating")))
            .expect("write");
    }

    if !tags.is_empty() {
        w.write_event(Event::Start(BytesStart::new("dc:subject")))
            .expect("write");
        w.write_event(Event::Start(BytesStart::new("rdf:Bag")))
            .expect("write");
        for tag in tags {
            w.write_event(Event::Start(BytesStart::new("rdf:li")))
                .expect("write");
            w.write_event(Event::Text(BytesText::new(tag)))
                .expect("write");
            w.write_event(Event::End(BytesEnd::new("rdf:li")))
                .expect("write");
        }
        w.write_event(Event::End(BytesEnd::new("rdf:Bag")))
            .expect("write");
        w.write_event(Event::End(BytesEnd::new("dc:subject")))
            .expect("write");
    }

    w.write_event(Event::End(BytesEnd::new("rdf:Description")))
        .expect("write");
    w.write_event(Event::End(BytesEnd::new("rdf:RDF")))
        .expect("write");
    w.write_event(Event::End(BytesEnd::new("x:xmpmeta")))
        .expect("write");

    let body = String::from_utf8(w.into_inner()).expect("xmp is valid utf8");
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n{body}\n<?xpacket end=\"w\"?>\n"
    )
}

/// Write `build_xmp(tags, rating)` to `path` atomically AND no-clobber: content
/// goes to a sibling temp file, is fsynced, then published with
/// `renameat2(RENAME_NOREPLACE)`. An existing file at `path` yields
/// `ErrorKind::AlreadyExists` and is never overwritten — the same guarantee
/// every file move has (spec §8 rev 3); a plain `rename` here was the one
/// destination write that could silently clobber. Caller chooses the path
/// (`<stem>.xmp`, Adobe style).
pub fn write_sidecar(path: &Path, tags: &[String], rating: Option<i32>) -> io::Result<()> {
    let content = build_xmp(tags, rating);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "sidecar path has no file name")
    })?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));

    if let Err(e) = write_temp_and_sync(&tmp, content.as_bytes()) {
        let _ = std::fs::remove_file(&tmp); // create/write/fsync failure leaves no litter
        return Err(e);
    }
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    if let Err(e) = renameat_with(CWD, &tmp, CWD, path, RenameFlags::NOREPLACE) {
        let _ = std::fs::remove_file(&tmp); // refused publish leaves no litter
        return Err(io::Error::from(e));
    }
    Ok(())
}

/// Create `tmp`, write `content`, and fsync it. Split out of `write_sidecar`
/// so every failure in this sequence (create, write, sync) is a single `Err`
/// that the caller can clean up uniformly, alongside the existing
/// rename-failure cleanup.
fn write_temp_and_sync(tmp: &Path, content: &[u8]) -> io::Result<()> {
    let mut f = std::fs::File::create(tmp)?;
    #[cfg(test)]
    if test_hooks::should_fail_write() {
        return Err(io::Error::other("injected write failure (test)"));
    }
    f.write_all(content)?;
    f.sync_all()
}

/// Test-only failure-injection seam for `write_sidecar`. Kept deliberately
/// tiny and private: a thread-local flag lets a test force the write/sync
/// step to fail deterministically right after the temp file is created,
/// without resorting to OS-level `RLIMIT_FSIZE` + `SIGXFSZ` manipulation
/// (which is process-wide state and would be racy/destructive in a
/// multi-threaded `cargo test` binary where other tests run concurrently on
/// other OS threads).
#[cfg(test)]
mod test_hooks {
    use std::cell::Cell;

    thread_local! {
        static FAIL_WRITE: Cell<bool> = const { Cell::new(false) };
    }

    pub fn should_fail_write() -> bool {
        FAIL_WRITE.with(Cell::get)
    }

    /// RAII guard: arms failure injection for the current thread while
    /// alive; disarms on drop, including on test panic (`Drop` still runs
    /// under the default per-test `catch_unwind`), so a failure in one test
    /// can never leak into another test that reuses the same OS thread.
    pub struct FailWriteGuard(());

    impl FailWriteGuard {
        pub fn new() -> Self {
            FAIL_WRITE.with(|f| f.set(true));
            FailWriteGuard(())
        }
    }

    impl Drop for FailWriteGuard {
        fn drop(&mut self) {
            FAIL_WRITE.with(|f| f.set(false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fastcull-xmp-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn build_xmp_emits_dc_subject_bag() {
        let xml = build_xmp(&["red".to_string(), "sky".to_string()], None);
        assert!(xml.contains("<dc:subject>"), "xml was: {xml}");
        assert!(xml.contains("<rdf:Bag>"), "xml was: {xml}");
        assert!(xml.contains("<rdf:li>red</rdf:li>"), "xml was: {xml}");
        assert!(xml.contains("<rdf:li>sky</rdf:li>"), "xml was: {xml}");

        // empty tags => no dc:subject block at all
        let empty = build_xmp(&[], None);
        assert!(!empty.contains("dc:subject"), "xml was: {empty}");
    }

    #[test]
    fn build_xmp_round_trips_rating_and_tags() {
        fn parse_xmp(xml: &str) -> (Vec<String>, Option<i32>) {
            use quick_xml::Reader;
            use quick_xml::events::Event;
            let mut reader = Reader::from_str(xml);
            let mut tags = Vec::new();
            let mut rating = None;
            let mut in_li = false;
            let mut in_rating = false;
            loop {
                match reader.read_event() {
                    Ok(Event::Start(e)) => match e.name().as_ref() {
                        b"rdf:li" => in_li = true,
                        b"xmp:Rating" => in_rating = true,
                        _ => {}
                    },
                    Ok(Event::End(e)) => match e.name().as_ref() {
                        b"rdf:li" => in_li = false,
                        b"xmp:Rating" => in_rating = false,
                        _ => {}
                    },
                    Ok(Event::Text(t)) => {
                        let txt = String::from_utf8_lossy(t.as_ref()).into_owned();
                        if in_li {
                            tags.push(txt);
                        } else if in_rating {
                            rating = txt.trim().parse::<i32>().ok();
                        }
                    }
                    Ok(Event::Eof) => break,
                    Err(e) => panic!("parse error: {e}"),
                    _ => {}
                }
            }
            (tags, rating)
        }

        let xml = build_xmp(&["red".to_string(), "sky".to_string()], Some(4));
        assert!(xml.contains("<xmp:Rating>4</xmp:Rating>"), "xml was: {xml}");
        let (tags, rating) = parse_xmp(&xml);
        assert_eq!(tags, vec!["red".to_string(), "sky".to_string()]);
        assert_eq!(rating, Some(4));

        // reject rating (-1, the Bridge/darktable convention) survives
        let (_, r) = parse_xmp(&build_xmp(&[], Some(-1)));
        assert_eq!(r, Some(-1));

        // rating None => no xmp:Rating element at all
        let none = build_xmp(&["x".to_string()], None);
        assert!(!none.contains("xmp:Rating"), "xml was: {none}");
        let (t2, r2) = parse_xmp(&none);
        assert_eq!(t2, vec!["x".to_string()]);
        assert_eq!(r2, None);
    }

    #[test]
    fn write_sidecar_writes_atomically_and_parses_back() {
        let dir = unique_tmp_dir("sidecar");
        let path = dir.join("IMG_1234.xmp");
        write_sidecar(&path, &["red".to_string()], Some(5)).expect("write_sidecar");

        let content = std::fs::read_to_string(&path).expect("read back");
        assert!(
            content.contains("<rdf:li>red</rdf:li>"),
            "content: {content}"
        );
        assert!(
            content.contains("<xmp:Rating>5</xmp:Rating>"),
            "content: {content}"
        );

        // atomic write leaves no temp file behind: only the final sidecar remains
        let mut entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            vec!["IMG_1234.xmp".to_string()],
            "leftover files: {entries:?}"
        );

        // NO-CLOBBER (spec §8 rev 3): a second write onto the same path must fail
        // AlreadyExists, leave the original byte-for-byte intact, and clean its temp.
        let before = std::fs::read(&path).unwrap();
        let err = write_sidecar(&path, &["other".to_string()], Some(1)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "existing sidecar untouched"
        );
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 1, "refused publish leaves no temp litter");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_sidecar_cleans_up_temp_on_write_failure() {
        let dir = unique_tmp_dir("write-fail");
        let path = dir.join("IMG_5678.xmp");

        let guard = test_hooks::FailWriteGuard::new();
        let err = write_sidecar(&path, &["red".to_string()], Some(3)).unwrap_err();
        drop(guard);
        assert_eq!(err.kind(), std::io::ErrorKind::Other, "error was: {err:?}");

        let entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            entries.is_empty(),
            "temp file left behind after write failure: {entries:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
