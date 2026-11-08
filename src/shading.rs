//! Grey shadings (gradients) and transparency groups to black ink only.
//!
//! A grey shading's colour comes from a function giving the grey. As DeviceCMYK the shading takes
//! four functions: zero for C, M and Y, and for K the grey function inverted (`1 - grey`), which
//! every function type allows: type 2 inverts C0 and C1, type 3 inverts its parts, type 0 inverts
//! its /Decode, type 4 appends `1 exch sub`. New functions are new objects, so a function shared
//! with another shading is never changed twice.

use anyhow::{Result, bail};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::util::{Family, Masks, dict_of, family, num, resolve};

#[derive(Default, Debug)]
pub struct ShadingStats {
    pub converted: usize,
    /// mesh shadings that carry their grey in the vertex data (no function): left in DeviceGray
    pub left_gray: usize,
    pub groups: usize,
}

#[derive(Clone, Debug)]
enum Step {
    Key(Vec<u8>),
    Index(usize),
}

/// Where a shading dictionary sits: an indirect object, then keys and indexes inside it.
type Path = (ObjectId, Vec<Step>);

fn find(o: &Object, path: &mut Vec<Step>, out: &mut Vec<Vec<Step>>, doc: &Document, depth: u32) {
    if depth > 40 {
        return;
    }
    let d = match o {
        Object::Dictionary(d) => d,
        Object::Stream(s) => &s.dict,
        Object::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                path.push(Step::Index(i));
                find(x, path, out, doc, depth + 1);
                path.pop();
            }
            return;
        }
        _ => return,
    };
    if d.has(b"ShadingType")
        && d.get(b"ColorSpace")
            .map(|cs| family(doc, cs) == Family::Gray)
            .unwrap_or(false)
    {
        out.push(path.clone());
    }
    for (k, v) in d.iter() {
        path.push(Step::Key(k.clone()));
        find(v, path, out, doc, depth + 1);
        path.pop();
    }
}

fn locate<'a>(doc: &'a mut Document, (id, steps): &Path) -> Option<&'a mut Dictionary> {
    let mut o: &mut Object = doc.objects.get_mut(id)?;
    for s in steps {
        o = match (o, s) {
            (Object::Dictionary(d), Step::Key(k)) => d.get_mut(k).ok()?,
            (Object::Stream(st), Step::Key(k)) => st.dict.get_mut(k).ok()?,
            (Object::Array(a), Step::Index(i)) => a.get_mut(*i)?,
            _ => return None,
        };
    }
    match o {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&mut s.dict),
        _ => None,
    }
}

/// New objects to add to the document, with ids handed out from `next`.
struct Alloc {
    next: u32,
    objects: Vec<(ObjectId, Object)>,
}

impl Alloc {
    fn add(&mut self, o: Object) -> Object {
        let id = (self.next, 0);
        self.next += 1;
        self.objects.push((id, o));
        Object::Reference(id)
    }
}

fn floats(o: Option<&Object>, default: &[f32]) -> Vec<f32> {
    o.and_then(|o| o.as_array().ok())
        .and_then(|a| a.iter().map(num).collect())
        .unwrap_or_else(|| default.to_vec())
}

fn reals(v: &[f32]) -> Object {
    Object::Array(v.iter().map(|x| Object::Real(*x)).collect())
}

/// The same one-output function giving `1 - output`.
fn invert(doc: &Document, f: &Object, alloc: &mut Alloc, depth: u32) -> Result<Object> {
    if depth > 16 {
        bail!("function nesting too deep");
    }
    let f = resolve(doc, f);
    let dict =
        dict_of(doc, f).ok_or_else(|| anyhow::anyhow!("a shading function is not a dictionary"))?;
    let ty = dict.get(b"FunctionType").ok().and_then(num).unwrap_or(-1.0) as i32;
    match (ty, f) {
        (2, _) => {
            let mut d = dict.clone();
            let c0: Vec<f32> = floats(d.get(b"C0").ok(), &[0.0])
                .iter()
                .map(|c| 1.0 - c)
                .collect();
            let c1: Vec<f32> = floats(d.get(b"C1").ok(), &[1.0])
                .iter()
                .map(|c| 1.0 - c)
                .collect();
            d.set("C0", reals(&c0));
            d.set("C1", reals(&c1));
            Ok(Object::Dictionary(d))
        }
        (3, _) => {
            let mut d = dict.clone();
            let parts = d
                .get(b"Functions")
                .ok()
                .map(|o| resolve(doc, o))
                .and_then(|o| o.as_array().ok())
                .cloned()
                .unwrap_or_default();
            let inv: Result<Vec<Object>> = parts
                .iter()
                .map(|p| invert(doc, p, alloc, depth + 1))
                .collect();
            d.set("Functions", Object::Array(inv?));
            Ok(Object::Dictionary(d))
        }
        (0, Object::Stream(s)) => {
            let mut s: Stream = s.clone();
            let range = floats(s.dict.get(b"Range").ok(), &[0.0, 1.0]);
            let dec = floats(s.dict.get(b"Decode").ok(), &range);
            if dec.len() != 2 {
                bail!("a sampled shading function with more than one output");
            }
            s.dict.set("Decode", reals(&[1.0 - dec[0], 1.0 - dec[1]]));
            Ok(alloc.add(Object::Stream(s)))
        }
        (4, Object::Stream(s)) => {
            let code = s
                .decompressed_content()
                .unwrap_or_else(|_| s.content.clone());
            let Some(end) = code.iter().rposition(|b| *b == b'}') else {
                bail!("a PostScript function without braces")
            };
            let mut new = code[..end].to_vec();
            new.extend_from_slice(b" 1 exch sub }");
            let mut s = Stream::new(s.dict.clone(), Vec::new());
            s.set_plain_content(new);
            Ok(alloc.add(Object::Stream(s)))
        }
        _ => bail!("shading function type {ty} not handled"),
    }
}

/// A function of the same domain that is always 0.
fn zero(doc: &Document, f: &Object, alloc: &mut Alloc) -> Object {
    let domain = dict_of(doc, resolve(doc, f))
        .map(|d| floats(d.get(b"Domain").ok(), &[0.0, 1.0]))
        .unwrap_or(vec![0.0, 1.0]);
    if domain.len() == 2 {
        let mut d = Dictionary::new();
        d.set("FunctionType", 2);
        d.set("Domain", reals(&domain));
        d.set("C0", reals(&[0.0]));
        d.set("C1", reals(&[0.0]));
        d.set("N", 1);
        return Object::Dictionary(d);
    }
    let mut d = Dictionary::new();
    d.set("FunctionType", 4);
    d.set("Domain", reals(&domain));
    d.set("Range", reals(&[0.0, 1.0]));
    let pops = "pop ".repeat(domain.len() / 2);
    alloc.add(Object::Stream(Stream::new(
        d,
        format!("{{{pops}0}}").into_bytes(),
    )))
}

/// Grey shadings to DeviceCMYK with black ink only, and grey or RGB transparency groups (pages and
/// forms, not soft masks) to DeviceCMYK.
pub fn gray_shadings_to_k(doc: &mut Document, masks: &Masks) -> Result<ShadingStats> {
    let mut st = ShadingStats::default();
    let mut paths: Vec<Path> = Vec::new();
    for (id, o) in &doc.objects {
        let mut found = Vec::new();
        find(o, &mut Vec::new(), &mut found, doc, 0);
        paths.extend(found.into_iter().map(|p| (*id, p)));
    }
    let mut alloc = Alloc {
        next: doc.max_id + 1,
        objects: Vec::new(),
    };
    let mut patches: Vec<(Path, Object, Option<Object>)> = Vec::new();
    for path in paths {
        let Some(d) = locate_ro(doc, &path) else {
            continue;
        };
        let Ok(f) = d.get(b"Function") else {
            st.left_gray += 1;
            continue;
        };
        let f = match resolve(doc, f) {
            Object::Array(a) if a.len() == 1 => a[0].clone(),
            Object::Array(_) => {
                st.left_gray += 1;
                continue;
            }
            _ => f.clone(),
        };
        let inv = match invert(doc, &f, &mut alloc, 0) {
            Ok(inv) => inv,
            Err(_) => {
                st.left_gray += 1;
                continue;
            }
        };
        let z = zero(doc, &f, &mut alloc);
        let funcs = Object::Array(vec![z.clone(), z.clone(), z, inv]);
        let background = d
            .get(b"Background")
            .ok()
            .and_then(|b| b.as_array().ok())
            .and_then(|a| a.first())
            .and_then(num)
            .map(|g| reals(&[0.0, 0.0, 0.0, 1.0 - g]));
        patches.push((path, funcs, background));
    }
    for (id, o) in alloc.objects.drain(..) {
        doc.objects.insert(id, o);
    }
    doc.max_id = doc.max_id.max(alloc.next - 1);
    for (path, funcs, background) in patches {
        if let Some(d) = locate(doc, &path) {
            d.set("ColorSpace", Object::Name(b"DeviceCMYK".to_vec()));
            d.set("Function", funcs);
            if let Some(b) = background {
                d.set("Background", b);
            }
            st.converted += 1;
        }
    }
    // Transparency groups of pages and forms: they blend in CMYK, the space of the content now
    // (Ghostscript 10.06 converts the colours but leaves a grey or RGB blending space).
    let holders: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter(|(id, o)| {
            !masks.forms.contains(id) && dict_of(doc, o).map(|d| d.has(b"Group")).unwrap_or(false)
        })
        .map(|(id, _)| *id)
        .collect();
    for id in holders {
        let group = doc
            .get_object(id)
            .ok()
            .and_then(|o| dict_of(doc, o))
            .and_then(|d| d.get(b"Group").ok())
            .and_then(|g| dict_of(doc, g))
            .cloned();
        let Some(mut g) = group else { continue };
        if g.get(b"CS")
            .map(|cs| matches!(family(doc, cs), Family::Gray | Family::Rgb | Family::Lab))
            .unwrap_or(false)
        {
            g.set("CS", Object::Name(b"DeviceCMYK".to_vec()));
            let holder = match doc.get_object_mut(id)? {
                Object::Dictionary(d) => d,
                Object::Stream(s) => &mut s.dict,
                _ => continue,
            };
            holder.set("Group", Object::Dictionary(g));
            st.groups += 1;
        }
    }
    Ok(st)
}

fn locate_ro<'a>(doc: &'a Document, (id, steps): &Path) -> Option<&'a Dictionary> {
    let mut o: &Object = doc.objects.get(id)?;
    for s in steps {
        o = match (o, s) {
            (Object::Dictionary(d), Step::Key(k)) => d.get(k).ok()?,
            (Object::Stream(st), Step::Key(k)) => st.dict.get(k).ok()?,
            (Object::Array(a), Step::Index(i)) => a.get(*i)?,
            _ => return None,
        };
    }
    match o {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with(f: Object) -> (Document, Object) {
        let mut doc = Document::with_version("1.7");
        let id = doc.add_object(f);
        (doc, Object::Reference(id))
    }

    #[test]
    fn type2_and_type3_invert_their_colours() {
        let mut d = Dictionary::new();
        d.set("FunctionType", 2);
        d.set("Domain", reals(&[0.0, 1.0]));
        d.set("C0", reals(&[0.2]));
        d.set("C1", reals(&[1.0]));
        d.set("N", 1);
        let mut s = Dictionary::new();
        s.set("FunctionType", 3);
        s.set("Domain", reals(&[0.0, 1.0]));
        s.set("Functions", Object::Array(vec![Object::Dictionary(d)]));
        let (doc, r) = doc_with(Object::Dictionary(s));
        let mut alloc = Alloc {
            next: 100,
            objects: Vec::new(),
        };
        let inv = invert(&doc, &r, &mut alloc, 0).unwrap();
        let inner = inv
            .as_dict()
            .unwrap()
            .get(b"Functions")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .as_dict()
            .unwrap()
            .clone();
        assert_eq!(floats(inner.get(b"C0").ok(), &[]), vec![0.8]);
        assert_eq!(floats(inner.get(b"C1").ok(), &[]), vec![0.0]);
    }

    #[test]
    fn type4_appends_the_inversion() {
        let mut d = Dictionary::new();
        d.set("FunctionType", 4);
        d.set("Domain", reals(&[0.0, 1.0]));
        d.set("Range", reals(&[0.0, 1.0]));
        let (doc, r) = doc_with(Object::Stream(Stream::new(d, b"{ dup mul }".to_vec())));
        let mut alloc = Alloc {
            next: 100,
            objects: Vec::new(),
        };
        invert(&doc, &r, &mut alloc, 0).unwrap();
        let s = alloc.objects[0].1.as_stream().unwrap();
        assert_eq!(s.content, b"{ dup mul  1 exch sub }".to_vec());
    }

    #[test]
    fn type0_inverts_its_decode() {
        let mut d = Dictionary::new();
        d.set("FunctionType", 0);
        d.set("Domain", reals(&[0.0, 1.0]));
        d.set("Range", reals(&[0.0, 1.0]));
        let (doc, r) = doc_with(Object::Stream(Stream::new(d, vec![0, 255])));
        let mut alloc = Alloc {
            next: 100,
            objects: Vec::new(),
        };
        invert(&doc, &r, &mut alloc, 0).unwrap();
        let s = alloc.objects[0].1.as_stream().unwrap();
        assert_eq!(floats(s.dict.get(b"Decode").ok(), &[]), vec![1.0, 0.0]);
    }
}
