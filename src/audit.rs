//! Colour audit of a finished PDF: which colour spaces its content, images, gradients and
//! transparency groups use, and whether any CMYK colour carries cyan, magenta or yellow.

use std::collections::BTreeMap;

use lopdf::content::Content;
use lopdf::{Document, Object, ObjectId};

use crate::util::{Family, Masks, Spaces, content_streams, dict_of, family, masks, name_is, num};

#[derive(Default, Debug)]
pub struct Census {
    /// where each colour family was found, with a count
    pub found: BTreeMap<(&'static str, &'static str), usize>,
    /// CMYK colours with some cyan, magenta or yellow (content operators)
    pub cmy_ops: usize,
    /// CMYK images with some cyan, magenta or yellow samples (checked when decodable)
    pub cmy_images: usize,
}

impl Census {
    fn add(&mut self, f: Family, place: &'static str) {
        *self.found.entry((f.label(), place)).or_default() += 1;
    }

    pub fn count(&self, f: Family) -> usize {
        self.found
            .iter()
            .filter(|((l, _), _)| *l == f.label())
            .map(|(_, n)| n)
            .sum()
    }

    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = self
            .found
            .iter()
            .map(|((f, p), n)| format!("{f} {p}: {n}"))
            .collect();
        if self.cmy_ops > 0 {
            parts.push(format!("CMYK colours with C/M/Y ink: {}", self.cmy_ops));
        }
        if self.cmy_images > 0 {
            parts.push(format!("CMYK images with C/M/Y ink: {}", self.cmy_images));
        }
        parts.join("; ")
    }
}

fn scan_ops(content: &Content, spaces: &Spaces, c: &mut Census) {
    let mut fill = Family::Gray;
    let mut stroke = Family::Gray;
    let mut stack = Vec::new();
    for op in &content.operations {
        let o = op.operator.as_str();
        let side_fill = o.chars().next().map(|ch| ch.is_lowercase()).unwrap_or(true);
        match o {
            "q" => stack.push((fill, stroke)),
            "Q" => (fill, stroke) = stack.pop().unwrap_or((Family::Gray, Family::Gray)),
            "g" | "G" => {
                c.add(Family::Gray, "colours");
                if side_fill {
                    fill = Family::Gray
                } else {
                    stroke = Family::Gray
                }
            }
            "rg" | "RG" => {
                c.add(Family::Rgb, "colours");
                if side_fill {
                    fill = Family::Rgb
                } else {
                    stroke = Family::Rgb
                }
            }
            "k" | "K" => {
                c.add(Family::Cmyk, "colours");
                if op.operands.iter().take(3).filter_map(num).any(|v| v > 0.0) {
                    c.cmy_ops += 1;
                }
                if side_fill {
                    fill = Family::Cmyk
                } else {
                    stroke = Family::Cmyk
                }
            }
            "cs" | "CS" => {
                let f = op
                    .operands
                    .first()
                    .and_then(|n| n.as_name().ok())
                    .map(|n| spaces.of(n))
                    .unwrap_or(Family::Unknown);
                if f != Family::Pattern {
                    c.add(f, "colours");
                }
                if side_fill { fill = f } else { stroke = f }
            }
            "sc" | "scn" | "SC" | "SCN" => {
                let f = if side_fill { fill } else { stroke };
                if f == Family::Cmyk && op.operands.iter().take(3).filter_map(num).any(|v| v > 0.0)
                {
                    c.cmy_ops += 1;
                }
            }
            _ => {}
        }
    }
}

/// True when every CMYK sample of an 8-bit image has C = M = Y = 0.
fn k_only_samples(s: &lopdf::Stream) -> Option<bool> {
    if s.dict.get(b"BitsPerComponent").ok().and_then(num) != Some(8.0) || !crate::util::decodable(s)
    {
        return None;
    }
    let data = s.decompressed_content().ok()?;
    Some(
        data.as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0),
    )
}

pub fn census(doc: &Document, check_samples: bool) -> Census {
    let mut c = Census::default();
    let m: Masks = masks(doc);
    let pages: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    for page in pages {
        let spaces = Spaces::of_page(doc, page);
        if let Ok(content) = Content::decode(&doc.get_page_content(page)) {
            scan_ops(&content, &spaces, &mut c);
        }
    }
    for (id, spaces) in content_streams(doc) {
        if m.forms.contains(&id) {
            continue;
        }
        let Ok(s) = doc.get_object(id).and_then(Object::as_stream) else {
            continue;
        };
        let data = s
            .decompressed_content()
            .unwrap_or_else(|_| s.content.clone());
        if let Ok(content) = Content::decode(&data) {
            scan_ops(&content, &spaces, &mut c);
        }
    }
    for (id, o) in &doc.objects {
        let Some(d) = dict_of(doc, o) else { continue };
        if name_is(d, b"Subtype", b"Image") {
            if m.images.contains(id)
                || d.get(b"ImageMask")
                    .and_then(Object::as_bool)
                    .unwrap_or(false)
            {
                continue;
            }
            if let Ok(cs) = d.get(b"ColorSpace") {
                let f = family(doc, cs);
                c.add(f, "images");
                if check_samples
                    && f == Family::Cmyk
                    && let Ok(s) = o.as_stream()
                    && k_only_samples(s) == Some(false)
                {
                    c.cmy_images += 1;
                }
            }
        }
        if d.has(b"ShadingType")
            && let Ok(cs) = d.get(b"ColorSpace")
        {
            c.add(family(doc, cs), "gradients");
        }
        if !m.forms.contains(id)
            && let Some(g) = d.get(b"Group").ok().and_then(|g| dict_of(doc, g))
            && let Ok(cs) = g.get(b"CS")
        {
            c.add(family(doc, cs), "transparency groups");
        }
    }
    c
}
