//! Small PDF helpers shared by the passes: numbers, references and colour-space families.

use std::collections::HashMap;
use std::io::Write;

use anyhow::Result;
use lopdf::{Dictionary, Document, Object, ObjectId};

pub fn num(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r),
        _ => None,
    }
}

/// Rounded to 4 decimals, so content streams stay short and stable.
pub fn round4(v: f32) -> f32 {
    (v * 10000.0).round() / 10000.0
}

/// `0 0 0 (1-gray)`: a grey as a tint of black ink only.
pub fn k_of_gray(gray: f32) -> Vec<Object> {
    let k = round4((1.0 - gray).clamp(0.0, 1.0));
    vec![0.into(), 0.into(), 0.into(), Object::Real(k)]
}

pub fn resolve<'a>(doc: &'a Document, o: &'a Object) -> &'a Object {
    doc.dereference(o).map(|(_, o)| o).unwrap_or(o)
}

pub fn dict_of<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Dictionary> {
    match resolve(doc, o) {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

pub fn name_is(d: &Dictionary, key: &[u8], value: &[u8]) -> bool {
    d.get(key)
        .and_then(Object::as_name)
        .map(|n| n == value)
        .unwrap_or(false)
}

pub fn ref_id(o: &Object) -> Option<ObjectId> {
    o.as_reference().ok()
}

/// What a colour space is, as far as printing goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Gray,
    Rgb,
    Cmyk,
    Lab,
    Indexed,
    Separation,
    /// `/Separation /All`: the registration colour of printer's marks, on every plate
    Registration,
    DeviceN,
    Pattern,
    Unknown,
}

impl Family {
    pub fn label(self) -> &'static str {
        match self {
            Family::Gray => "grey",
            Family::Rgb => "RGB",
            Family::Cmyk => "CMYK",
            Family::Lab => "Lab",
            Family::Indexed => "indexed",
            Family::Separation => "spot (Separation)",
            Family::Registration => "registration (all plates)",
            Family::DeviceN => "DeviceN",
            Family::Pattern => "pattern",
            Family::Unknown => "unknown",
        }
    }
}

/// The family of a device colour-space name, or `None` for a resource name.
pub fn device_family(name: &[u8]) -> Option<Family> {
    Some(match name {
        b"DeviceGray" | b"G" | b"CalGray" => Family::Gray,
        b"DeviceRGB" | b"RGB" | b"CalRGB" => Family::Rgb,
        b"DeviceCMYK" | b"CMYK" => Family::Cmyk,
        b"Pattern" => Family::Pattern,
        _ => return None,
    })
}

pub fn family(doc: &Document, cs: &Object) -> Family {
    match resolve(doc, cs) {
        Object::Name(n) => device_family(n).unwrap_or(Family::Unknown),
        Object::Array(a) => {
            let head = a
                .first()
                .map(|o| resolve(doc, o))
                .and_then(|o| o.as_name().ok());
            match head {
                Some(b"CalGray") => Family::Gray,
                Some(b"CalRGB") => Family::Rgb,
                Some(b"Lab") => Family::Lab,
                Some(b"ICCBased") => {
                    let n = a
                        .get(1)
                        .and_then(|s| dict_of(doc, s))
                        .and_then(|d| d.get(b"N").ok())
                        .and_then(num);
                    match n {
                        Some(1.0) => Family::Gray,
                        Some(3.0) => Family::Rgb,
                        Some(4.0) => Family::Cmyk,
                        _ => Family::Unknown,
                    }
                }
                Some(b"Indexed") | Some(b"I") => Family::Indexed,
                Some(b"Separation") => match a.get(1).map(|n| resolve(doc, n)) {
                    Some(Object::Name(n)) if n == b"All" => Family::Registration,
                    _ => Family::Separation,
                },
                Some(b"DeviceN") => Family::DeviceN,
                Some(b"Pattern") => Family::Pattern,
                Some(n) if a.len() == 1 => device_family(n).unwrap_or(Family::Unknown),
                _ => Family::Unknown,
            }
        }
        _ => Family::Unknown,
    }
}

/// The colour spaces a content stream can name with `cs`/`CS`: its resources' `/ColorSpace`.
#[derive(Default, Clone)]
pub struct Spaces(pub HashMap<Vec<u8>, Family>);

impl Spaces {
    pub fn of(&self, name: &[u8]) -> Family {
        device_family(name)
            .or_else(|| self.0.get(name).copied())
            .unwrap_or(Family::Unknown)
    }

    fn add(&mut self, doc: &Document, resources: &Dictionary) {
        if let Some(cs) = resources
            .get(b"ColorSpace")
            .ok()
            .and_then(|o| dict_of(doc, o))
        {
            for (k, v) in cs.iter() {
                self.0.entry(k.clone()).or_insert_with(|| family(doc, v));
            }
        }
    }

    pub fn of_page(doc: &Document, page: ObjectId) -> Spaces {
        let mut s = Spaces::default();
        if let Ok((direct, ids)) = doc.get_page_resources(page) {
            if let Some(d) = direct {
                s.add(doc, d);
            }
            for id in ids {
                if let Ok(d) = doc.get_dictionary(id) {
                    s.add(doc, d);
                }
            }
        }
        s
    }

    pub fn of_dict(doc: &Document, holder: &Dictionary) -> Spaces {
        let mut s = Spaces::default();
        if let Some(r) = holder.get(b"Resources").ok().and_then(|o| dict_of(doc, o)) {
            s.add(doc, r);
        }
        s
    }
}

pub fn zlib(data: &[u8]) -> Result<Vec<u8>> {
    let mut e = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(data.len() / 4 + 64),
        flate2::Compression::new(6),
    );
    e.write_all(data)?;
    Ok(e.finish()?)
}

/// Filters lopdf can undo; image data under any other filter (DCT, JPX, CCITT, JBIG2) stays as it is.
pub fn decodable(s: &lopdf::Stream) -> bool {
    match s.filters() {
        Ok(f) => f.iter().all(|n| {
            matches!(
                *n,
                b"FlateDecode"
                    | b"Fl"
                    | b"LZWDecode"
                    | b"LZW"
                    | b"ASCII85Decode"
                    | b"A85"
                    | b"ASCIIHexDecode"
                    | b"AHx"
            )
        }),
        Err(_) => true, // no /Filter
    }
}

/// Every stream that holds page content of its own: Form XObjects, tiling patterns and the glyph
/// procedures of Type 3 fonts, each with the colour spaces its content can name.
pub fn content_streams(doc: &Document) -> Vec<(ObjectId, Spaces)> {
    let mut out = Vec::new();
    for (id, obj) in &doc.objects {
        match obj {
            Object::Stream(s) => {
                let form = name_is(&s.dict, b"Subtype", b"Form");
                let tiling = s.dict.get(b"PatternType").ok().and_then(num) == Some(1.0);
                if form || tiling {
                    out.push((*id, Spaces::of_dict(doc, &s.dict)));
                }
            }
            Object::Dictionary(d) if name_is(d, b"Subtype", b"Type3") => {
                let spaces = Spaces::of_dict(doc, d);
                if let Some(procs) = d.get(b"CharProcs").ok().and_then(|o| dict_of(doc, o)) {
                    for (_, p) in procs.iter() {
                        if let Some(pid) = ref_id(p) {
                            out.push((pid, spaces.clone()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Masks: the images used as `/SMask` or `/Mask` of other images, and the forms that make soft
/// masks (`/SMask << /G form >>` in a graphics state). Masks hold no ink; they stay as they are.
pub struct Masks {
    pub images: std::collections::HashSet<ObjectId>,
    pub forms: std::collections::HashSet<ObjectId>,
}

pub fn masks(doc: &Document) -> Masks {
    let mut m = Masks {
        images: Default::default(),
        forms: Default::default(),
    };
    fn walk(doc: &Document, o: &Object, m: &mut Masks, depth: u32) {
        if depth > 40 {
            return;
        }
        let d = match o {
            Object::Dictionary(d) => d,
            Object::Stream(s) => &s.dict,
            Object::Array(a) => {
                a.iter().for_each(|x| walk(doc, x, m, depth + 1));
                return;
            }
            _ => return,
        };
        let image = name_is(d, b"Subtype", b"Image");
        for (k, v) in d.iter() {
            match (k.as_slice(), image) {
                (b"SMask" | b"Mask", true) => {
                    if let Some(id) = ref_id(v) {
                        m.images.insert(id);
                    }
                }
                (b"SMask", false) => {
                    if let Some(g) = dict_of(doc, v)
                        .and_then(|sm| sm.get(b"G").ok())
                        .and_then(ref_id)
                    {
                        m.forms.insert(g);
                    }
                }
                _ => {}
            }
            walk(doc, v, m, depth + 1);
        }
    }
    for o in doc.objects.values() {
        walk(doc, o, &mut m, 0);
    }
    m
}

/// Whether the PDF names a spot colour space (Separation or DeviceN) anywhere, other than the
/// registration colour `/All` and the invisible `/None`.
pub fn has_spots(doc: &Document) -> bool {
    fn walk(o: &Object, depth: u32) -> bool {
        if depth > 40 {
            return false;
        }
        match o {
            Object::Array(a) => {
                (matches!(a.first(), Some(Object::Name(n)) if n == b"Separation" || n == b"DeviceN")
                    && !matches!(a.get(1), Some(Object::Name(n)) if n == b"All" || n == b"None"))
                    || a.iter().any(|x| walk(x, depth + 1))
            }
            Object::Dictionary(d) => d.iter().any(|(_, v)| walk(v, depth + 1)),
            Object::Stream(s) => s.dict.iter().any(|(_, v)| walk(v, depth + 1)),
            _ => false,
        }
    }
    doc.objects.values().any(|o| walk(o, 0))
}

/// Gives every registration colour (`/Separation /All`) the standard alternate: tint t is
/// `t t t t` in DeviceCMYK. Ghostscript 10.06 rewrites its tint transform with stray values
/// (a white with magenta and yellow), which a RIP ignores but previews and checks show.
/// Returns how many were set.
pub fn normalize_registration(doc: &mut Document) -> usize {
    fn standard() -> Object {
        let mut f = Dictionary::new();
        f.set("FunctionType", 2);
        f.set("Domain", vec![0.into(), 1.into()]);
        f.set("C0", vec![0.into(), 0.into(), 0.into(), 0.into()]);
        f.set("C1", vec![1.into(), 1.into(), 1.into(), 1.into()]);
        f.set("N", 1);
        Object::Dictionary(f)
    }
    fn walk(o: &mut Object, n: &mut usize, depth: u32) {
        if depth > 40 {
            return;
        }
        match o {
            Object::Array(a) => {
                let all = a.len() == 4
                    && matches!(&a[0], Object::Name(s) if s == b"Separation")
                    && matches!(&a[1], Object::Name(s) if s == b"All");
                if all {
                    a[2] = Object::Name(b"DeviceCMYK".to_vec());
                    a[3] = standard();
                    *n += 1;
                } else {
                    a.iter_mut().for_each(|x| walk(x, n, depth + 1));
                }
            }
            Object::Dictionary(d) => d.iter_mut().for_each(|(_, v)| walk(v, n, depth + 1)),
            Object::Stream(s) => s.dict.iter_mut().for_each(|(_, v)| walk(v, n, depth + 1)),
            _ => {}
        }
    }
    let mut n = 0;
    for o in doc.objects.values_mut() {
        walk(o, &mut n, 0);
    }
    n
}

/// How many registration colours (`/Separation /All`) the PDF defines.
pub fn count_registration(doc: &Document) -> usize {
    fn walk(o: &Object, n: &mut usize, depth: u32) {
        if depth > 40 {
            return;
        }
        match o {
            Object::Array(a) => {
                if a.len() == 4
                    && matches!(&a[0], Object::Name(s) if s == b"Separation")
                    && matches!(&a[1], Object::Name(s) if s == b"All")
                {
                    *n += 1;
                } else {
                    a.iter().for_each(|x| walk(x, n, depth + 1));
                }
            }
            Object::Dictionary(d) => d.iter().for_each(|(_, v)| walk(v, n, depth + 1)),
            Object::Stream(s) => s.dict.iter().for_each(|(_, v)| walk(v, n, depth + 1)),
            _ => {}
        }
    }
    let mut n = 0;
    doc.objects.values().for_each(|o| walk(o, &mut n, 0));
    n
}
