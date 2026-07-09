use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::writer::Writer;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
