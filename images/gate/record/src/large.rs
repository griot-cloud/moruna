//! The three large jobs over a record: FineWeb MinHash and the two Overture functions, their
//! nested input columns read in place.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use moruna_gate_functions::cols::{BitsBuf, Col, Offsets, Strs};
use moruna_gate_functions::overture::{AttrsIn, BBox, Floats, Sources};
use moruna_gate_functions::{fineweb, overture};

use crate::decode::{Array, Batch};
use crate::encode::{OutArray, TextBuf};
use crate::types::{LField, LType};
use crate::{Error, Output};

/// A string or binary array as the core reads it.
fn strs<'a>(a: &Array<'a>, ty: &LType, what: &str) -> Result<Strs<'a>, Error> {
    let offsets = match ty {
        LType::Utf8 | LType::Binary => Offsets::Small(a.values::<i32>(1, a.len + 1)?),
        LType::LargeUtf8 | LType::LargeBinary => Offsets::Large(a.values::<i64>(1, a.len + 1)?),
        other => {
            return Err(Error::schema(&format!(
                "{what} is {other:?}, not a string or binary column"
            )));
        }
    };
    Ok(Strs {
        offsets,
        data: a.buffers.get(2).copied().unwrap_or(&[]),
        valid: a.validity()?,
    })
}

fn floats<'a>(a: &Array<'a>, ty: &LType, what: &str) -> Result<Floats<'a>, Error> {
    match ty {
        LType::Float { bits: 32 } => Ok(Floats::F32(a.col::<f32>()?)),
        LType::Float { bits: 64 } => Ok(Floats::F64(a.col::<f64>()?)),
        other => Err(Error::schema(&format!(
            "{what} is {other:?}, not floating point"
        ))),
    }
}

/// A struct's child by name, with its field.
fn child<'b, 'a>(
    fields: &'b [LField],
    a: &'b Array<'a>,
    name: &str,
    what: &str,
) -> Result<(&'b LField, &'b Array<'a>), Error> {
    fields
        .iter()
        .zip(&a.children)
        .find(|(f, _)| f.name == name)
        .ok_or_else(|| Error::schema(&format!("{what} has no field {name:?}")))
}

fn struct_fields(f: &LField) -> Result<&[LField], Error> {
    match &f.ty {
        LType::Struct(fs) => Ok(fs),
        other => Err(Error::schema(&format!(
            "column {:?} is {other:?}, not a struct",
            f.name
        ))),
    }
}

fn pass<'a>(out: &mut Output<'a>, batch: &Batch<'a>, name: &str) -> Result<(), Error> {
    let (f, a) = batch.column(name)?;
    out.push(
        LField {
            name: String::from(name),
            nullable: true,
            ty: f.ty.clone(),
        },
        OutArray::borrowed(a),
    );
    Ok(())
}

fn text_column<'a, S: AsRef<str>>(values: &[Option<S>]) -> Result<OutArray<'a>, Error> {
    let mut t = TextBuf::new();
    for v in values {
        t.push(v.as_ref().map(|s| s.as_ref().as_bytes()))?;
    }
    Ok(t.finish())
}

fn nullable_f64<'a>(values: &[Option<f64>]) -> OutArray<'a> {
    let mut valid = BitsBuf::with_capacity(values.len());
    let v: Vec<f64> = values
        .iter()
        .map(|x| {
            valid.push(x.is_some());
            x.unwrap_or(0.0)
        })
        .collect();
    OutArray::fixed(v, Some(valid))
}

/// A fixed-size list column of `size` values each, no nulls.
fn fixed_list<'a, T: bytemuck::Pod>(values: Vec<T>, size: usize) -> OutArray<'a> {
    let len = values.len() / size.max(1);
    OutArray {
        len,
        null_count: 0,
        buffers: alloc::vec![alloc::borrow::Cow::Borrowed(&[][..])],
        children: alloc::vec![OutArray::fixed(values, None)],
    }
}

pub fn fw_minhash<'a>(batch: &Batch<'a>) -> Result<Output<'a>, Error> {
    let (f, a) = batch.column("text")?;
    let (sigs, keys) = fineweb::minhash(&strs(a, &f.ty, "column \"text\"")?)?;
    let mut out = Output::new(batch.rows);
    pass(&mut out, batch, "id")?;
    pass(&mut out, batch, "url")?;
    let list = |size: usize, item: LType| LType::FixedSizeList {
        size: size as i32,
        child: alloc::boxed::Box::new(LField::new("item", item)),
    };
    out.push(
        LField::new("minhash", list(fineweb::PERMS, LType::U32)),
        fixed_list(sigs, fineweb::PERMS),
    );
    out.push(
        LField::new("bands", list(fineweb::BANDS, LType::U64)),
        fixed_list(keys, fineweb::BANDS),
    );
    Ok(out)
}

pub fn ov_attrs<'a>(batch: &Batch<'a>) -> Result<Output<'a>, Error> {
    let (nf, names) = batch.column("names")?;
    let (pf, primary) = child(struct_fields(nf)?, names, "primary", "names")?;
    let (sf, src) = batch.column("sources")?;
    let (item_field, offsets) = match &sf.ty {
        LType::List { child } => (
            child.as_ref(),
            Offsets::Small(src.values::<i32>(1, src.len + 1)?),
        ),
        LType::LargeList { child } => (
            child.as_ref(),
            Offsets::Large(src.values::<i64>(1, src.len + 1)?),
        ),
        other => {
            return Err(Error::schema(&format!(
                "column \"sources\" is {other:?}, not a list"
            )));
        }
    };
    let items = src
        .children
        .first()
        .ok_or_else(|| Error::layout("a list without its items"))?;
    let item_fields = struct_fields(item_field)?;
    let (df, dataset) = child(item_fields, items, "dataset", "sources")?;
    let (cf, confidence) = child(item_fields, items, "confidence", "sources")?;
    let (bf, bbox) = batch.column("bbox")?;
    let bfs = struct_fields(bf)?;
    let bound = |n: &str| -> Result<Floats<'a>, Error> {
        let (f, a) = child(bfs, bbox, n, "bbox")?;
        floats(a, &f.ty, n)
    };
    let (ff, floors) = batch.column("num_floors")?;
    if ff.ty != LType::I32 {
        return Err(Error::schema("column \"num_floors\" is not int32"));
    }
    let (clf, class) = batch.column("class")?;
    let (stf, subtype) = batch.column("subtype")?;
    let input = AttrsIn {
        rows: batch.rows,
        names_valid: names.validity()?,
        primary: strs(primary, &pf.ty, "names.primary")?,
        sources: Sources {
            offsets,
            valid: src.validity()?,
            item_valid: items.validity()?,
            dataset: strs(dataset, &df.ty, "sources.dataset")?,
            confidence: floats(confidence, &cf.ty, "sources.confidence")?,
        },
        height: batch.floats("height")?,
        num_floors: Col::new(floors.values::<i32>(1, floors.len)?, floors.validity()?),
        class: strs(class, &clf.ty, "column \"class\"")?,
        subtype: strs(subtype, &stf.ty, "column \"subtype\"")?,
        bbox: BBox {
            valid: bbox.validity()?,
            xmin: bound("xmin")?,
            xmax: bound("xmax")?,
            ymin: bound("ymin")?,
            ymax: bound("ymax")?,
        },
    };
    let o = overture::attrs(&input)?;
    let mut out = Output::new(batch.rows);
    pass(&mut out, batch, "id")?;
    out.push(LField::new("name", LType::Utf8), text_column(&o.name)?);
    out.push(
        LField::new("sources", LType::I32),
        OutArray::fixed(o.sources, None),
    );
    out.push(
        LField::new("best_source", LType::Utf8),
        text_column(&o.best_source)?,
    );
    out.push(
        LField::new("height_m", LType::F64),
        nullable_f64(&o.height_m),
    );
    let usage: Vec<Option<&str>> = o.usage.iter().map(|u| Some(*u)).collect();
    out.push(LField::new("usage", LType::Utf8), text_column(&usage)?);
    out.push(
        LField::new("volume_m3", LType::F64),
        nullable_f64(&o.volume_m3),
    );
    Ok(out)
}

pub fn ov_shape<'a>(batch: &Batch<'a>) -> Result<Output<'a>, Error> {
    let (gf, g) = batch.column("geometry")?;
    let s = overture::shapes(&strs(g, &gf.ty, "column \"geometry\"")?)?;
    let mut out = Output::new(batch.rows);
    for n in ["id", "class", "subtype", "height"] {
        pass(&mut out, batch, n)?;
    }
    let rows = s.area_m2.len();
    out.push(
        LField::new("area_m2", LType::F64),
        OutArray::fixed(s.area_m2, None),
    );
    if s.pairs == 0 {
        out.push(
            LField::new("perimeter_m", LType::I64),
            OutArray::fixed(alloc::vec![0i64; rows], None),
        );
    } else {
        out.push(
            LField::new("perimeter_m", LType::F64),
            OutArray::fixed(s.perimeter_m, None),
        );
    }
    out.push(LField::new("lon", LType::F64), OutArray::fixed(s.lon, None));
    out.push(LField::new("lat", LType::F64), OutArray::fixed(s.lat, None));
    out.push(
        LField::new("vertices", LType::I32),
        OutArray::fixed(s.vertices, None),
    );
    out.push(
        LField::new("compactness", LType::F64),
        OutArray::fixed(s.compactness, None),
    );
    out.push(
        LField::new("tile_x", LType::I32),
        OutArray::fixed(s.tile_x, None),
    );
    out.push(
        LField::new("tile_y", LType::I32),
        OutArray::fixed(s.tile_y, None),
    );
    Ok(out)
}
