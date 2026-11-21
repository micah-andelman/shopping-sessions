//! Minimal Parquet writer: one row group, one PLAIN data page (v1) per column, uncompressed,
//! no dictionary, no statistics. Values are appended straight into page buffers, so writing a
//! table needs no intermediate Arrow arrays and no per-row String allocations; strings are
//! written as their raw bytes. Metadata is Thrift compact protocol, written by hand.

use std::io::Write;
use std::path::Path;

#[derive(Clone, Copy)]
pub enum Kind {
    I32,
    I64,
    Date,
    Str,
    OptStr,
}

pub struct Col {
    name: &'static str,
    kind: Kind,
    buf: Vec<u8>,
    defs: Vec<bool>,
}

impl Col {
    pub fn new(name: &'static str, kind: Kind, rows: usize) -> Col {
        let per = match kind {
            Kind::I32 | Kind::Date => 4,
            Kind::I64 => 8,
            Kind::Str | Kind::OptStr => 16,
        };
        Col { name, kind, buf: Vec::with_capacity(rows * per), defs: Vec::new() }
    }
    #[inline]
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(b);
        if let Kind::OptStr = self.kind {
            self.defs.push(true);
        }
    }
    /// Append a string value written by `f` into the buffer (avoids a temporary Vec).
    #[inline]
    pub fn bytes_with(&mut self, f: impl FnOnce(&mut Vec<u8>)) {
        let at = self.buf.len();
        self.buf.extend_from_slice(&[0; 4]);
        f(&mut self.buf);
        let n = (self.buf.len() - at - 4) as u32;
        self.buf[at..at + 4].copy_from_slice(&n.to_le_bytes());
        if let Kind::OptStr = self.kind {
            self.defs.push(true);
        }
    }
    #[inline]
    pub fn null(&mut self) {
        self.defs.push(false);
    }
}

// ---------------------------------------------------------------- Thrift compact protocol
struct T {
    b: Vec<u8>,
    last: Vec<i16>,
}
impl T {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.b.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.b.push(v as u8);
    }
    fn zz(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn field(&mut self, id: i16, ty: u8) {
        let d = id - *self.last.last().unwrap();
        if d > 0 && d <= 15 {
            self.b.push(((d as u8) << 4) | ty);
        } else {
            self.b.push(ty);
            self.zz(id as i64);
        }
        *self.last.last_mut().unwrap() = id;
    }
    fn i32(&mut self, id: i16, v: i32) {
        self.field(id, 5);
        self.zz(v as i64);
    }
    fn i64(&mut self, id: i16, v: i64) {
        self.field(id, 6);
        self.zz(v);
    }
    fn bin(&mut self, id: i16, v: &[u8]) {
        self.field(id, 8);
        self.varint(v.len() as u64);
        self.b.extend_from_slice(v);
    }
    fn list(&mut self, id: i16, elem: u8, n: usize) {
        self.field(id, 9);
        if n < 15 {
            self.b.push(((n as u8) << 4) | elem);
        } else {
            self.b.push(0xF0 | elem);
            self.varint(n as u64);
        }
    }
    fn begin(&mut self, id: i16) {
        self.field(id, 12);
        self.last.push(0);
    }
    /// a struct that is a list element (no field header)
    fn begin_elem(&mut self) {
        self.last.push(0);
    }
    fn end(&mut self) {
        self.b.push(0);
        self.last.pop();
    }
}

/// Definition levels (bit width 1) as one bit-packed run, prefixed by its byte length (page v1).
fn def_levels(defs: &[bool], out: &mut Vec<u8>) {
    let groups = defs.len().div_ceil(8);
    let mut run = Vec::with_capacity(groups + 5);
    let mut h = ((groups as u64) << 1) | 1;
    while h >= 0x80 {
        run.push((h as u8) | 0x80);
        h >>= 7;
    }
    run.push(h as u8);
    for g in defs.chunks(8) {
        let mut byte = 0u8;
        for (i, &d) in g.iter().enumerate() {
            byte |= (d as u8) << i;
        }
        run.push(byte);
    }
    out.extend_from_slice(&(run.len() as u32).to_le_bytes());
    out.extend_from_slice(&run);
}

/// Write `cols` (all with `rows` values) as one Parquet file.
pub fn write(path: &Path, cols: &[Col], rows: usize) -> std::io::Result<()> {
    // The file is written piecewise: magic, then per column a small head (page header and
    // definition levels) followed by the column's value buffer as it is, then the footer.
    // The value buffers are never copied into one big buffer first.
    let mut heads: Vec<Vec<u8>> = Vec::with_capacity(cols.len());
    let mut pos = 4i64; // after "PAR1"
    // (offset of page header, total chunk bytes)
    let mut chunks: Vec<(i64, i64)> = Vec::with_capacity(cols.len());
    if rows > 0 {
        for c in cols {
            let mut page = Vec::new();
            if let Kind::OptStr = c.kind {
                def_levels(&c.defs, &mut page);
            }
            let size = page.len() + c.buf.len();
            let mut h = T { b: Vec::new(), last: vec![0] };
            h.i32(1, 0); // DATA_PAGE
            h.i32(2, size as i32);
            h.i32(3, size as i32);
            h.begin(5);
            h.i32(1, rows as i32);
            h.i32(2, 0); // PLAIN
            h.i32(3, 3); // RLE definition levels
            h.i32(4, 3); // RLE repetition levels
            h.end();
            h.b.push(0);
            let mut head = h.b;
            head.extend_from_slice(&page);
            let len = (head.len() + c.buf.len()) as i64;
            chunks.push((pos, len));
            pos += len;
            heads.push(head);
        }
    }
    // FileMetaData
    let ptype = |k: Kind| match k {
        Kind::I32 | Kind::Date => 1,
        Kind::I64 => 2,
        Kind::Str | Kind::OptStr => 6,
    };
    let mut m = T { b: Vec::new(), last: vec![0] };
    m.i32(1, 1);
    m.list(2, 12, cols.len() + 1);
    m.begin_elem();
    m.bin(4, b"schema");
    m.i32(5, cols.len() as i32);
    m.end();
    for c in cols {
        m.begin_elem();
        m.i32(1, ptype(c.kind));
        m.i32(3, if let Kind::OptStr = c.kind { 1 } else { 0 });
        m.bin(4, c.name.as_bytes());
        match c.kind {
            Kind::Str | Kind::OptStr => {
                m.i32(6, 0); // converted type UTF8
                m.begin(10); // LogicalType { STRING: StringType {} }
                m.begin(1);
                m.end();
                m.end();
            }
            Kind::Date => {
                m.i32(6, 6); // converted type DATE
                m.begin(10); // LogicalType { DATE: DateType {} }
                m.begin(6);
                m.end();
                m.end();
            }
            _ => {}
        }
        m.end();
    }
    m.i64(3, rows as i64);
    if rows > 0 {
        m.list(4, 12, 1);
        m.begin_elem();
        m.list(1, 12, cols.len());
        let mut total = 0i64;
        for (c, &(off, len)) in cols.iter().zip(&chunks) {
            total += len;
            m.begin_elem();
            m.i64(2, off);
            m.begin(3);
            m.i32(1, ptype(c.kind));
            m.list(2, 5, 2);
            m.zz(0); // PLAIN
            m.zz(3); // RLE
            m.list(3, 8, 1);
            m.varint(c.name.len() as u64);
            m.b.extend_from_slice(c.name.as_bytes());
            m.i32(4, 0); // UNCOMPRESSED
            m.i64(5, rows as i64);
            m.i64(6, len);
            m.i64(7, len);
            m.i64(9, off);
            m.end();
            m.end();
        }
        m.i64(2, total);
        m.i64(3, rows as i64);
        m.end();
    } else {
        m.list(4, 12, 0);
    }
    m.bin(6, b"shopping-sessions");
    m.b.push(0);
    let mut foot = m.b;
    let mlen = foot.len() as u32;
    foot.extend_from_slice(&mlen.to_le_bytes());
    foot.extend_from_slice(b"PAR1");
    let mut f = std::fs::File::create(path)?;
    let mut first: Vec<u8> = b"PAR1".to_vec();
    for (c, head) in cols.iter().zip(&heads) {
        first.extend_from_slice(head);
        f.write_all(&first)?;
        first.clear();
        f.write_all(&c.buf)?;
    }
    first.extend_from_slice(&foot);
    f.write_all(&first)
}
