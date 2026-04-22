/// Parse `<tag>...</tag>` from simple multipart XML.
pub fn parse_xml_tag(xml: &str, tag: &str) -> String {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let start = xml.find(&start_tag).expect("opening XML tag") + start_tag.len();
    let end = xml.find(&end_tag).expect("closing XML tag");
    xml[start..end].to_owned()
}
