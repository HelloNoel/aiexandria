use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct Chunk {
    pub key: String,
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub input: String,
}

pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

/// Line-preserving passages. The full embedding input participates in identity.
pub fn chunks(title: &str, body: &str, budget: usize) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut heading = String::new();
    let mut text = String::new();
    let mut start = 1;
    let mut end = 0;
    let mut fence = false;
    let title: String = title.chars().take(120).collect();
    let emit = |chunks: &mut Vec<Chunk>, text: &mut String, start, end, heading: &str| {
        if text.trim().is_empty() {
            text.clear();
            return;
        }
        let input = format!("{title}\n{heading}\n{text}");
        chunks.push(Chunk {
            key: digest(format!("chunk-v1\n{input}")),
            start,
            end,
            text: std::mem::take(text),
            input,
        });
    };
    for (i, line) in body.lines().enumerate() {
        let is_heading =
            !fence && line.starts_with('#') && line.chars().take_while(|c| *c == '#').count() <= 6;
        if !text.is_empty()
            && (is_heading || text.len() + line.len() + 1 > budget || i + 1 - start >= 48)
        {
            emit(&mut chunks, &mut text, start, end, &heading);
        }
        if is_heading {
            heading = line.chars().take(120).collect();
        }
        if line.trim_start().starts_with("~~~")
            || line.trim_start().starts_with("\u{0060}\u{0060}\u{0060}")
        {
            fence = !fence;
        }
        // Very long source lines remain traceable to the same original line.
        let mut rest = line;
        while rest.len() > budget {
            let mut split = budget;
            while !rest.is_char_boundary(split) {
                split -= 1;
            }
            text.push_str(&rest[..split]);
            emit(&mut chunks, &mut text, i + 1, i + 1, &heading);
            rest = &rest[split..];
        }
        if text.is_empty() {
            start = i + 1;
        }
        text.push_str(rest);
        text.push('\n');
        end = i + 1;
    }
    emit(&mut chunks, &mut text, start, end, &heading);
    chunks
}
