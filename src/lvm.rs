//! LVM2: physical-volume labels, volume-group metadata, and logical
//! volumes as block devices.
//!
//! A disk (or partition, or `md` array) that is an LVM2 physical volume
//! carries a label in one of its first four sectors, a metadata area
//! holding the volume group's description as text, and a data area cut
//! into fixed-size physical extents. A logical volume is a list of
//! segments, each mapping a run of its own extents onto extents of one or
//! more physical volumes. This module reads the label, finds the newest
//! metadata, parses it, and exposes a logical volume as one
//! [`BlockRead`] that reads the bytes the kernel's `/dev/<vg>/<lv>` would.
//!
//! # What is read
//!
//! * The label: `LABELONE`, its CRC, and the `LVM2 001` PV header with
//!   its data and metadata area lists.
//! * The metadata area header (` LVM2 x[5A%r0N*>`, CRC checked) and the
//!   newest metadata text it points at, including text that wraps round
//!   the end of the circular buffer.
//! * The metadata text grammar: sections, `key = value`, strings,
//!   integers, lists, and `#` comments.
//! * Segments of type `striped`, which is how LVM writes both linear
//!   volumes (`stripe_count = 1`) and striped ones.
//!
//! Every other segment type — mirror, raid*, thin, cache, snapshot,
//! vdo — is refused by name with [`LvmError::Unsupported`]. So is a
//! volume whose physical volumes are not all given.
//!
//! The layouts come from LVM2's on-disk format as documented and as
//! observed on volumes `pvcreate`, `vgcreate` and `lvcreate` made;
//! `tests/oracle_lvm.rs` compares every byte read through
//! [`LogicalVolume`] with what the kernel's device-mapper returned.
//!
//! Read-only: nothing here writes metadata or data.

use std::collections::BTreeMap;
use std::fmt;

use crate::BlockRead;

const SECTOR: u64 = 512;
/// The label is in one of the first four sectors.
const LABEL_SCAN_SECTORS: u64 = 4;
const LABEL_ID: &[u8; 8] = b"LABELONE";
const LABEL_TYPE: &[u8; 8] = b"LVM2 001";
/// Magic of a metadata area header.
const MDA_MAGIC: &[u8; 16] = b" LVM2 x[5A%r0N*>";
/// Size of the metadata area header; text starts after it.
const MDA_HEADER_SIZE: u64 = 512;
/// Seed of the CRC LVM2 puts in its label and metadata area header.
const LVM_CRC_SEED: u32 = 0xf597_a6cf;
/// Refuse metadata text larger than this: a real VG's is kilobytes.
const MAX_METADATA: u64 = 16 << 20;
/// `raw_locn` flag: this metadata area is not to be read.
const RAW_LOCN_IGNORED: u32 = 0x1;

/// Why a physical volume or logical volume could not be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum LvmError {
    /// The underlying device failed.
    Block(fs_core::Error),
    /// Device `device` has no LVM2 label.
    NoLabel { device: usize },
    /// A label or metadata area header CRC does not match.
    BadChecksum { device: usize, what: &'static str },
    /// A structure is out of range or inconsistent.
    Corrupt(String),
    /// The metadata text does not parse.
    Syntax { line: usize, reason: String },
    /// No logical volume of that name.
    NoSuchVolume(String),
    /// A physical volume the volume needs was not given.
    MissingPv(String),
    /// A segment type or state this module does not read.
    Unsupported(String),
}

impl fmt::Display for LvmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LvmError::Block(e) => write!(f, "{e}"),
            LvmError::NoLabel { device } => write!(f, "device {device}: no LVM2 label"),
            LvmError::BadChecksum { device, what } => {
                write!(f, "device {device}: {what} checksum mismatch")
            }
            LvmError::Corrupt(s) => write!(f, "corrupt: {s}"),
            LvmError::Syntax { line, reason } => write!(f, "metadata line {line}: {reason}"),
            LvmError::NoSuchVolume(s) => write!(f, "no logical volume {s:?}"),
            LvmError::MissingPv(s) => write!(f, "physical volume {s} not given"),
            LvmError::Unsupported(s) => write!(f, "unsupported: {s}"),
        }
    }
}

impl std::error::Error for LvmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LvmError::Block(e) => Some(e),
            _ => None,
        }
    }
}

impl From<fs_core::Error> for LvmError {
    fn from(e: fs_core::Error) -> Self {
        LvmError::Block(e)
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// LVM2's CRC: the reflected CRC-32 polynomial, seeded with
/// `0xf597a6cf`, and no final inversion.
pub fn lvm_crc(data: &[u8]) -> u32 {
    let mut crc = LVM_CRC_SEED;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// A physical volume's label.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PvLabel {
    /// Sector the label was found in (0–3).
    pub label_sector: u64,
    /// The PV UUID, 32 characters with no dashes, as stored.
    pub uuid: String,
    /// Device size the PV recorded, in bytes.
    pub device_size: u64,
    /// Data areas: (offset, size) in bytes. A size of 0 means "to the end".
    pub data_areas: Vec<(u64, u64)>,
    /// Metadata areas: (offset, size) in bytes.
    pub metadata_areas: Vec<(u64, u64)>,
}

/// Read the LVM2 label of `dev`. `Ok(None)` when it carries none.
pub fn read_pv_label<R: BlockRead + ?Sized>(dev: &R) -> Result<Option<PvLabel>, LvmError> {
    read_pv_label_of(dev, 0)
}

fn read_pv_label_of<R: BlockRead + ?Sized>(
    dev: &R,
    device: usize,
) -> Result<Option<PvLabel>, LvmError> {
    let mut s = [0u8; SECTOR as usize];
    for sector in 0..LABEL_SCAN_SECTORS {
        if (sector + 1) * SECTOR > dev.size_bytes() {
            break;
        }
        dev.read_at(sector * SECTOR, &mut s)?;
        if &s[0..8] != LABEL_ID {
            continue;
        }
        if le64(&s, 8) != sector {
            continue;
        }
        if lvm_crc(&s[20..]) != le32(&s, 16) {
            return Err(LvmError::BadChecksum {
                device,
                what: "label",
            });
        }
        if &s[24..32] != LABEL_TYPE {
            return Err(LvmError::Unsupported(format!(
                "label type {:?}",
                String::from_utf8_lossy(&s[24..32])
            )));
        }
        let mut at = le32(&s, 20) as usize;
        if at < 32 || at + 40 > s.len() {
            return Err(LvmError::Corrupt(format!("pv_header offset {at}")));
        }
        let uuid = String::from_utf8_lossy(&s[at..at + 32]).into_owned();
        let device_size = le64(&s, at + 32);
        at += 40;
        let mut lists = [Vec::new(), Vec::new()];
        for list in &mut lists {
            loop {
                if at + 16 > s.len() {
                    return Err(LvmError::Corrupt("unterminated area list".into()));
                }
                let (off, size) = (le64(&s, at), le64(&s, at + 8));
                at += 16;
                if off == 0 {
                    break;
                }
                list.push((off, size));
            }
        }
        let [data_areas, metadata_areas] = lists;
        return Ok(Some(PvLabel {
            label_sector: sector,
            uuid,
            device_size,
            data_areas,
            metadata_areas,
        }));
    }
    Ok(None)
}

/// The newest metadata text among the metadata areas of `label`.
///
/// A PV made with `--pvmetadatacopies 2` keeps a second area at its end.
/// Every area is read and the copy with the highest `seqno` wins; an
/// area that cannot be read is passed over when another holds a good
/// copy, which is what the second copy is for. When none can be read,
/// the first area's error is returned. `Ok(None)` when the PV has no
/// metadata area, or no area holds text that is not marked to be ignored
/// (`pvchange --metadataignore`).
pub fn read_metadata_text<R: BlockRead + ?Sized>(
    dev: &R,
    label: &PvLabel,
) -> Result<Option<String>, LvmError> {
    read_metadata_text_of(dev, label, 0)
}

fn read_metadata_text_of<R: BlockRead + ?Sized>(
    dev: &R,
    label: &PvLabel,
    device: usize,
) -> Result<Option<String>, LvmError> {
    let mut newest: Option<(u64, String)> = None;
    let mut first_error = None;
    for &(start, size) in &label.metadata_areas {
        let text = match read_area(dev, start, size, device) {
            Ok(Some(text)) => text,
            Ok(None) => continue,
            Err(e) => {
                first_error.get_or_insert(e);
                continue;
            }
        };
        // A copy that does not parse is no better than one whose
        // checksum failed.
        let seqno = match parse_metadata(&text).and_then(|top| VolumeGroup::from_metadata(&top)) {
            Ok(vg) => vg.seqno,
            Err(e) => {
                first_error.get_or_insert(e);
                continue;
            }
        };
        if newest.as_ref().is_none_or(|(n, _)| seqno > *n) {
            newest = Some((seqno, text));
        }
    }
    match (newest, first_error) {
        (Some((_, text)), _) => Ok(Some(text)),
        (None, Some(e)) => Err(e),
        (None, None) => Ok(None),
    }
}

/// The text in the metadata area at `start`, `size` bytes long.
fn read_area<R: BlockRead + ?Sized>(
    dev: &R,
    start: u64,
    size: u64,
    device: usize,
) -> Result<Option<String>, LvmError> {
    if size <= MDA_HEADER_SIZE || start.checked_add(size).is_none_or(|e| e > dev.size_bytes()) {
        return Err(LvmError::Corrupt(format!(
            "metadata area {start}+{size} on a {}-byte device",
            dev.size_bytes()
        )));
    }
    let mut h = [0u8; MDA_HEADER_SIZE as usize];
    dev.read_at(start, &mut h)?;
    if lvm_crc(&h[4..]) != le32(&h, 0) {
        return Err(LvmError::BadChecksum {
            device,
            what: "metadata area header",
        });
    }
    if &h[4..20] != MDA_MAGIC {
        return Err(LvmError::Corrupt("metadata area magic".into()));
    }
    // raw_locn[0]: offset (from the area start), size, checksum, flags.
    let off = le64(&h, 40);
    let len = le64(&h, 48);
    if off == 0 || len == 0 || le32(&h, 60) & RAW_LOCN_IGNORED != 0 {
        return Ok(None);
    }
    if len > MAX_METADATA || off < MDA_HEADER_SIZE || off >= size {
        return Err(LvmError::Corrupt(format!("metadata at {off}+{len}")));
    }
    let mut text = vec![0u8; len as usize];
    // The text area is a ring from the end of the header to the end of
    // the metadata area; a record may wrap.
    let first = (size - off).min(len);
    dev.read_at(start + off, &mut text[..first as usize])?;
    if first < len {
        let rest = len - first;
        if MDA_HEADER_SIZE + rest > size {
            return Err(LvmError::Corrupt("metadata wraps past its area".into()));
        }
        dev.read_at(start + MDA_HEADER_SIZE, &mut text[first as usize..])?;
    }
    if lvm_crc(&text) != le32(&h, 56) {
        return Err(LvmError::BadChecksum {
            device,
            what: "metadata text",
        });
    }
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    Ok(Some(String::from_utf8_lossy(&text[..end]).into_owned()))
}

/// A value in LVM2's metadata text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// An integer, e.g. `seqno = 4`.
    Int(i64),
    /// A quoted string, unescaped.
    Str(String),
    /// A `[...]` list.
    List(Vec<Value>),
    /// A nested `name { ... }` section.
    Section(Section),
}

/// A `name { ... }` section: its keys in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Section(pub Vec<(String, Value)>);

impl Section {
    /// The value of `key`, if present.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    fn int(&self, key: &str) -> Result<i64, LvmError> {
        match self.get(key) {
            Some(Value::Int(i)) => Ok(*i),
            _ => Err(LvmError::Corrupt(format!("{key}: expected an integer"))),
        }
    }
    fn uint(&self, key: &str) -> Result<u64, LvmError> {
        u64::try_from(self.int(key)?).map_err(|_| LvmError::Corrupt(format!("{key}: negative")))
    }
    fn str(&self, key: &str) -> Result<&str, LvmError> {
        match self.get(key) {
            Some(Value::Str(s)) => Ok(s),
            _ => Err(LvmError::Corrupt(format!("{key}: expected a string"))),
        }
    }
    fn section(&self, key: &str) -> Result<&Section, LvmError> {
        match self.get(key) {
            Some(Value::Section(s)) => Ok(s),
            _ => Err(LvmError::Corrupt(format!("{key}: expected a section"))),
        }
    }
    fn sections(&self) -> impl Iterator<Item = (&str, &Section)> {
        self.0.iter().filter_map(|(k, v)| match v {
            Value::Section(s) => Some((k.as_str(), s)),
            _ => None,
        })
    }
}

/// Parse LVM2 metadata text into its top-level section.
pub fn parse_metadata(text: &str) -> Result<Section, LvmError> {
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
        line: 1,
        depth: 0,
    };
    let top = p.section_body(true)?;
    Ok(top)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    line: usize,
    depth: usize,
}

impl Parser<'_> {
    fn err(&self, reason: impl Into<String>) -> LvmError {
        LvmError::Syntax {
            line: self.line,
            reason: reason.into(),
        }
    }
    fn skip(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            match c {
                b'\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                b' ' | b'\t' | b'\r' => self.i += 1,
                b'#' => {
                    while self.b.get(self.i).is_some_and(|&c| c != b'\n') {
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
    }
    fn peek(&mut self) -> Option<u8> {
        self.skip();
        self.b.get(self.i).copied()
    }
    fn ident(&mut self) -> Result<String, LvmError> {
        self.skip();
        let s = self.i;
        while self
            .b
            .get(self.i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || b"_.-+".contains(c))
        {
            self.i += 1;
        }
        if s == self.i {
            return Err(self.err("expected a name"));
        }
        Ok(String::from_utf8_lossy(&self.b[s..self.i]).into_owned())
    }
    fn section_body(&mut self, top: bool) -> Result<Section, LvmError> {
        self.depth += 1;
        if self.depth > 32 {
            return Err(self.err("sections nested too deep"));
        }
        let mut out = Section::default();
        loop {
            match self.peek() {
                None if top => break,
                None => return Err(self.err("unterminated section")),
                Some(b'}') if !top => {
                    self.i += 1;
                    break;
                }
                Some(_) => {}
            }
            let key = self.ident()?;
            match self.peek() {
                Some(b'{') => {
                    self.i += 1;
                    let s = self.section_body(false)?;
                    out.0.push((key, Value::Section(s)));
                }
                Some(b'=') => {
                    self.i += 1;
                    let v = self.value()?;
                    out.0.push((key, v));
                }
                _ => return Err(self.err(format!("expected '=' or '{{' after {key}"))),
            }
        }
        self.depth -= 1;
        Ok(out)
    }
    fn value(&mut self) -> Result<Value, LvmError> {
        match self.peek() {
            Some(b'"') => self.string().map(Value::Str),
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    match self.peek() {
                        Some(b']') => {
                            self.i += 1;
                            break;
                        }
                        Some(b',') => self.i += 1,
                        None => return Err(self.err("unterminated list")),
                        Some(_) => items.push(self.value()?),
                    }
                }
                Ok(Value::List(items))
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => {
                let s = self.i;
                self.i += 1;
                while self.b.get(self.i).is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.b[s..self.i]).expect("ascii");
                t.parse()
                    .map(Value::Int)
                    .map_err(|_| self.err(format!("bad integer {t}")))
            }
            _ => Err(self.err("expected a value")),
        }
    }
    fn string(&mut self) -> Result<String, LvmError> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            match self.b.get(self.i) {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.i += 1;
                    break;
                }
                Some(b'\\') => {
                    if let Some(&c) = self.b.get(self.i + 1) {
                        out.push(c);
                    }
                    self.i += 2;
                }
                Some(&c) => {
                    if c == b'\n' {
                        self.line += 1;
                    }
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

/// A physical volume as the volume group's metadata describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PhysicalVolume {
    /// Its key in the metadata (`pv0`, `pv1`, ...).
    pub name: String,
    /// Its UUID, dashes removed, as the label stores it.
    pub uuid: String,
    /// Byte offset of extent 0 on the device.
    pub pe_start: u64,
    /// Number of extents.
    pub pe_count: u64,
}

/// One stripe of a segment: a PV and the extent on it where the stripe
/// starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stripe {
    /// Key of the PV in [`VolumeGroup::physical_volumes`].
    pub pv: String,
    /// First extent on that PV.
    pub start_extent: u64,
}

/// A run of a logical volume's extents.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Segment {
    /// First logical extent this segment covers.
    pub start_extent: u64,
    /// Number of logical extents.
    pub extent_count: u64,
    /// The segment type as written (`striped` is the only one read).
    pub kind: String,
    /// Stripe size in bytes (0 for a single stripe).
    pub stripe_size: u64,
    /// The stripes, in order.
    pub stripes: Vec<Stripe>,
    /// A `raid*` or `mirror` segment's images, in order: the hidden
    /// sub-volumes (`<lv>_rimage_N`, `<lv>_mimage_N`) holding the data.
    pub images: Vec<String>,
    /// A `raid*` segment's metadata sub-volumes (`<lv>_rmeta_N`), one per
    /// image, each starting with the dm-raid superblock.
    pub metadata: Vec<String>,
}

/// A logical volume.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogicalVolumeInfo {
    /// The volume's name, its key in the metadata.
    pub name: String,
    /// Its UUID as the metadata writes it, with dashes.
    pub id: String,
    /// Its segments, ordered by first logical extent.
    pub segments: Vec<Segment>,
}

/// A volume group, as its metadata describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VolumeGroup {
    /// The group's name, e.g. `vg1000`.
    pub name: String,
    /// Its UUID as the metadata writes it, with dashes.
    pub id: String,
    /// Metadata sequence number; the highest among the PVs is current.
    pub seqno: u64,
    /// Extent size in bytes.
    pub extent_size: u64,
    /// The PVs the group spans.
    pub physical_volumes: Vec<PhysicalVolume>,
    /// Every logical volume, including hidden ones.
    pub logical_volumes: Vec<LogicalVolumeInfo>,
}

impl VolumeGroup {
    /// Interpret parsed metadata text.
    pub fn from_metadata(top: &Section) -> Result<Self, LvmError> {
        let (name, vg) = top
            .sections()
            .next()
            .ok_or_else(|| LvmError::Corrupt("no volume group section".into()))?;
        let extent_size = vg
            .uint("extent_size")?
            .checked_mul(SECTOR)
            .filter(|&e| e > 0)
            .ok_or_else(|| LvmError::Corrupt("extent_size".into()))?;
        let mut physical_volumes = Vec::new();
        for (pv_name, pv) in vg.section("physical_volumes")?.sections() {
            physical_volumes.push(PhysicalVolume {
                name: pv_name.to_string(),
                uuid: pv.str("id")?.replace('-', ""),
                pe_start: pv
                    .uint("pe_start")?
                    .checked_mul(SECTOR)
                    .ok_or_else(|| LvmError::Corrupt("pe_start".into()))?,
                pe_count: pv.uint("pe_count")?,
            });
        }
        let mut logical_volumes = Vec::new();
        if let Some(Value::Section(lvs)) = vg.get("logical_volumes") {
            for (lv_name, lv) in lvs.sections() {
                let mut segments = Vec::new();
                for (key, seg) in lv.sections() {
                    if !key.starts_with("segment") {
                        continue;
                    }
                    let kind = seg.str("type")?.to_string();
                    let mut stripes = Vec::new();
                    let mut stripe_size = 0;
                    if kind == "striped" {
                        let Some(Value::List(items)) = seg.get("stripes") else {
                            return Err(LvmError::Corrupt(format!("{lv_name}/{key}: no stripes")));
                        };
                        for pair in items.chunks(2) {
                            match pair {
                                [Value::Str(pv), Value::Int(e)] if *e >= 0 => {
                                    stripes.push(Stripe {
                                        pv: pv.clone(),
                                        start_extent: *e as u64,
                                    })
                                }
                                _ => {
                                    return Err(LvmError::Corrupt(format!(
                                        "{lv_name}/{key}: stripes list"
                                    )))
                                }
                            }
                        }
                        let count = seg.uint("stripe_count")?;
                        if count != stripes.len() as u64 || count == 0 {
                            return Err(LvmError::Corrupt(format!(
                                "{lv_name}/{key}: stripe_count {count}, {} stripes",
                                stripes.len()
                            )));
                        }
                        if count > 1 {
                            stripe_size = seg
                                .uint("stripe_size")?
                                .checked_mul(SECTOR)
                                .filter(|&s| s > 0)
                                .ok_or_else(|| LvmError::Corrupt("stripe_size".into()))?;
                        }
                    }
                    let mut images = Vec::new();
                    let mut metadata = Vec::new();
                    if kind.starts_with("raid") {
                        // [rmeta_0, rimage_0, rmeta_1, rimage_1, ...]
                        let Some(Value::List(items)) = seg.get("raids") else {
                            return Err(LvmError::Corrupt(format!("{lv_name}/{key}: no raids")));
                        };
                        for pair in items.chunks(2) {
                            match pair {
                                [Value::Str(meta), Value::Str(image)] => {
                                    metadata.push(meta.clone());
                                    images.push(image.clone());
                                }
                                _ => {
                                    return Err(LvmError::Unsupported(format!(
                                        "{lv_name}/{key}: a raids list without a metadata \
                                         sub-volume per image"
                                    )))
                                }
                            }
                        }
                    } else if kind == "mirror" {
                        // [mimage_0, 0, mimage_1, 0, ...]
                        let Some(Value::List(items)) = seg.get("mirrors") else {
                            return Err(LvmError::Corrupt(format!("{lv_name}/{key}: no mirrors")));
                        };
                        for pair in items.chunks(2) {
                            match pair {
                                [Value::Str(image), Value::Int(0)] => images.push(image.clone()),
                                _ => {
                                    return Err(LvmError::Unsupported(format!(
                                        "{lv_name}/{key}: a mirror image that does not start \
                                         at its sub-volume's first extent"
                                    )))
                                }
                            }
                        }
                    }
                    segments.push(Segment {
                        start_extent: seg.uint("start_extent")?,
                        extent_count: seg.uint("extent_count")?,
                        kind,
                        stripe_size,
                        stripes,
                        images,
                        metadata,
                    });
                }
                segments.sort_by_key(|s| s.start_extent);
                logical_volumes.push(LogicalVolumeInfo {
                    name: lv_name.to_string(),
                    id: lv.str("id").unwrap_or_default().to_string(),
                    segments,
                });
            }
        }
        Ok(VolumeGroup {
            name: name.to_string(),
            id: vg.str("id")?.to_string(),
            seqno: vg.uint("seqno")?,
            extent_size,
            physical_volumes,
            logical_volumes,
        })
    }
}

/// Read the volume group the given physical volumes belong to: the
/// metadata with the highest `seqno` among them wins.
pub fn read_volume_group<R: BlockRead>(devices: &[R]) -> Result<VolumeGroup, LvmError> {
    let mut best: Option<VolumeGroup> = None;
    for (i, dev) in devices.iter().enumerate() {
        let label = read_pv_label_of(dev, i)?.ok_or(LvmError::NoLabel { device: i })?;
        if let Some(text) = read_metadata_text_of(dev, &label, i)? {
            let vg = VolumeGroup::from_metadata(&parse_metadata(&text)?)?;
            if best.as_ref().is_none_or(|b| vg.seqno > b.seqno) {
                best = Some(vg);
            }
        }
    }
    best.ok_or_else(|| LvmError::Corrupt("no physical volume holds metadata".into()))
}

/// Physical volumes sorted by the volume group each belongs to: what
/// [`scan`] returns.
#[derive(Debug)]
#[non_exhaustive]
pub struct LvmScan<R> {
    /// One entry per volume group found, in the order its first PV was
    /// given.
    pub volume_groups: Vec<VolumeGroupDevices<R>>,
    /// Devices with an LVM2 label that no metadata among the devices
    /// names: a PV in no group, or one whose group's metadata is only on
    /// devices not given.
    pub unclaimed: Vec<R>,
    /// Devices with no LVM2 label.
    pub others: Vec<R>,
    /// Devices whose label or metadata is there but could not be read,
    /// with why.
    pub refused: Vec<(R, LvmError)>,
}

/// The physical volumes of one volume group that [`scan`] found.
#[derive(Debug)]
#[non_exhaustive]
pub struct VolumeGroupDevices<R> {
    /// The group, from the newest metadata among its PVs.
    pub volume_group: VolumeGroup,
    /// The PVs found, in the order given: what [`LogicalVolume::open`]
    /// takes.
    pub devices: Vec<R>,
    /// Keys (`pv0`, ...) of the PVs the metadata names and no device given
    /// is.
    pub missing: Vec<String>,
}

/// Sort `devices` into the volume groups they are physical volumes of.
///
/// A PV belongs to the group whose newest metadata, among the devices
/// given, names its label's UUID, so a PV that keeps no metadata of its
/// own (`--pvmetadatacopies 0`) is still placed. Give `md` arrays here
/// assembled, not their members: a member whose data starts at byte 0
/// shows the array's PV label too. Errors in `refused` name a device by
/// its position in `devices`.
pub fn scan<R: BlockRead>(devices: Vec<R>) -> LvmScan<R> {
    let mut found = LvmScan {
        volume_groups: Vec::new(),
        unclaimed: Vec::new(),
        others: Vec::new(),
        refused: Vec::new(),
    };
    // Each labelled PV with its UUID, and the newest metadata of each
    // group, in the order the groups were first met.
    let mut pvs: Vec<(R, String)> = Vec::new();
    let mut groups: Vec<VolumeGroup> = Vec::new();
    for (i, dev) in devices.into_iter().enumerate() {
        let label = match read_pv_label_of(&dev, i) {
            Ok(Some(label)) => label,
            Ok(None) => {
                found.others.push(dev);
                continue;
            }
            Err(e) => {
                found.refused.push((dev, e));
                continue;
            }
        };
        let vg = read_metadata_text_of(&dev, &label, i).and_then(|text| match text {
            Some(text) => VolumeGroup::from_metadata(&parse_metadata(&text)?).map(Some),
            None => Ok(None),
        });
        match vg {
            Err(e) => found.refused.push((dev, e)),
            Ok(vg) => {
                if let Some(vg) = vg {
                    match groups.iter_mut().find(|g| g.id == vg.id) {
                        Some(g) if vg.seqno > g.seqno => *g = vg,
                        Some(_) => {}
                        None => groups.push(vg),
                    }
                }
                pvs.push((dev, label.uuid));
            }
        }
    }
    // Each group's PVs with their UUIDs, so the PVs its metadata names
    // and nobody gave can be listed.
    let mut members: Vec<Vec<(R, String)>> = groups.iter().map(|_| Vec::new()).collect();
    for (dev, uuid) in pvs {
        let named = |g: &VolumeGroup| g.physical_volumes.iter().any(|pv| pv.uuid == uuid);
        match groups.iter().position(named) {
            Some(i) => members[i].push((dev, uuid)),
            None => found.unclaimed.push(dev),
        }
    }
    for (volume_group, given) in groups.into_iter().zip(members) {
        let missing = volume_group
            .physical_volumes
            .iter()
            .filter(|pv| !given.iter().any(|(_, uuid)| *uuid == pv.uuid))
            .map(|pv| pv.name.clone())
            .collect();
        found.volume_groups.push(VolumeGroupDevices {
            volume_group,
            devices: given.into_iter().map(|(dev, _)| dev).collect(),
            missing,
        });
    }
    found
}

/// One mapped run of a logical volume, resolved to device indexes.
struct Run {
    start: u64,
    len: u64,
    map: Map,
}

/// How a run's bytes are laid out.
enum Map {
    /// `striped`: `stripe_size` chunks round the stripes in turn, or one
    /// stripe holding the run whole. (device index, byte offset of the
    /// stripe's first extent).
    Striped {
        stripe_size: u64,
        stripes: Vec<(usize, u64)>,
    },
    /// `raid*` and `mirror`: the data is laid out over images, each a
    /// hidden sub-volume mapped as runs of its own.
    Raid(Box<RaidMap>),
}

/// A dm-raid or dm-mirror segment, as the kernel lays it out.
struct RaidMap {
    /// md level: 1 (also `mirror`), 5 or 6. raid4 is read as level 5
    /// with its parity-first or parity-last layout, which is how md
    /// places those.
    level: i32,
    /// md layout, as the dm-raid superblock records it.
    layout: u32,
    /// Chunk size in bytes (unused at level 1).
    chunk: u64,
    /// Where the data starts on each image, in bytes.
    data_offset: u64,
    /// Each image's runs, in image order.
    images: Vec<Vec<Run>>,
}

/// dm-raid's on-disk superblock, at the start of each `rmeta` sub-volume:
/// the kernel's `struct dm_raid_superblock` (drivers/md/dm-raid.c).
mod dm_raid {
    pub const MAGIC: u32 = 0x6452_6d44; // "DmRd"
    pub const COMPAT_V190: u32 = 0x1;
    pub const COMPAT_FEATURES: usize = 4;
    pub const LEVEL: usize = 48;
    pub const LAYOUT: usize = 52;
    pub const STRIPE_SECTORS: usize = 56;
    // After stripe_sectors, the 1.9.0 extension: flags (60),
    // reshape_position (64), new_level, new_layout, new_stripe_sectors,
    // delta_disks (72..88), array_sectors (88), then data_offset (96),
    // new_data_offset (104) and sectors (112). Offset 88 is the array's
    // size, which a first reading took for the data offset: every image
    // then began past its own end (CI run 37975574393).
    pub const DATA_OFFSET: usize = 96;
    pub const SIZE: usize = 120;
}

/// The parts of a dm-raid superblock reading needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DmRaidSuper {
    level: i32,
    layout: u32,
    chunk: u64,
    data_offset: u64,
}

fn parse_dm_raid_super(b: &[u8], what: &str) -> Result<DmRaidSuper, LvmError> {
    if b.len() < dm_raid::SIZE || le32(b, 0) != dm_raid::MAGIC {
        return Err(LvmError::Corrupt(format!("{what}: no dm-raid superblock")));
    }
    // The data offset is a 1.9.0 extension; before it, data starts at 0.
    let data_offset = if le32(b, dm_raid::COMPAT_FEATURES) & dm_raid::COMPAT_V190 != 0 {
        le64(b, dm_raid::DATA_OFFSET)
    } else {
        0
    };
    Ok(DmRaidSuper {
        level: le32(b, dm_raid::LEVEL) as i32,
        layout: le32(b, dm_raid::LAYOUT),
        chunk: u64::from(le32(b, dm_raid::STRIPE_SECTORS)) * SECTOR,
        data_offset: data_offset
            .checked_mul(SECTOR)
            .ok_or_else(|| LvmError::Corrupt(format!("{what}: data offset overflows")))?,
    })
}

/// What resolving a volume's segments needs from the group and devices.
struct Mapper<'a, R: BlockRead> {
    vg: &'a VolumeGroup,
    uuid_to_dev: &'a BTreeMap<String, usize>,
    devices: &'a [R],
}

impl<R: BlockRead> Mapper<'_, R> {
    fn lv(&self, name: &str) -> Result<&LogicalVolumeInfo, LvmError> {
        self.vg
            .logical_volumes
            .iter()
            .find(|lv| lv.name == name)
            .ok_or_else(|| LvmError::NoSuchVolume(name.to_string()))
    }

    /// `name`'s runs and its size in bytes. `depth` bounds sub-volume
    /// nesting, so a cycle in the metadata is refused, not followed.
    fn map(&self, name: &str, depth: u32) -> Result<(Vec<Run>, u64), LvmError> {
        if depth > 2 {
            return Err(LvmError::Unsupported(format!(
                "{name}: sub-volumes nested deeper than an image of an image"
            )));
        }
        let info = self.lv(name)?;
        let ext = self.vg.extent_size;
        let mut runs = Vec::new();
        let mut next = 0u64;
        for seg in &info.segments {
            if seg.start_extent != next {
                return Err(LvmError::Corrupt(format!(
                    "{name}: segments leave a gap at extent {next}"
                )));
            }
            let len = seg
                .extent_count
                .checked_mul(ext)
                .ok_or_else(|| LvmError::Corrupt("segment size overflows".into()))?;
            let map = match seg.kind.as_str() {
                "striped" => self.striped(name, seg)?,
                "mirror" => Map::Raid(Box::new(RaidMap {
                    level: 1,
                    layout: 0,
                    chunk: 0,
                    data_offset: 0,
                    images: self.images(seg, depth)?,
                })),
                kind if kind == "raid1"
                    || kind == "raid4"
                    || kind.starts_with("raid5")
                    || kind.starts_with("raid6") =>
                {
                    self.raid(name, seg, depth)?
                }
                kind => {
                    return Err(LvmError::Unsupported(format!(
                        "segment type {kind:?} in {name}"
                    )))
                }
            };
            runs.push(Run {
                start: next * ext,
                len,
                map,
            });
            next += seg.extent_count;
        }
        Ok((runs, next * ext))
    }

    fn images(&self, seg: &Segment, depth: u32) -> Result<Vec<Vec<Run>>, LvmError> {
        if seg.images.is_empty() {
            return Err(LvmError::Corrupt(
                "a raid or mirror segment with no images".into(),
            ));
        }
        seg.images
            .iter()
            .map(|image| self.map(image, depth + 1).map(|(runs, _)| runs))
            .collect()
    }

    fn raid(&self, name: &str, seg: &Segment, depth: u32) -> Result<Map, LvmError> {
        if seg.metadata.len() != seg.images.len() {
            return Err(LvmError::Corrupt(format!(
                "{name}: {} images and {} metadata sub-volumes",
                seg.images.len(),
                seg.metadata.len()
            )));
        }
        // Every image's superblock describes the array; the first one
        // present is read, and the rest are not needed to place data.
        let (meta_runs, _) = self.map(&seg.metadata[0], depth + 1)?;
        let mut b = [0u8; dm_raid::SIZE];
        read_runs(self.devices, &meta_runs, 0, &mut b).map_err(LvmError::Block)?;
        let sb = parse_dm_raid_super(&b, &seg.metadata[0])?;
        let images = self.images(seg, depth)?;
        let n = images.len() as u64;
        let (level, parity) = match sb.level {
            1 => (1, 0),
            4 | 5 => (5, 1),
            6 => (6, 2),
            l => return Err(LvmError::Unsupported(format!("{name}: dm-raid level {l}"))),
        };
        if level != 1 && (sb.chunk == 0 || n <= parity) {
            return Err(LvmError::Corrupt(format!(
                "{name}: raid{} with {n} images and a {}-byte chunk",
                sb.level, sb.chunk
            )));
        }
        Ok(Map::Raid(Box::new(RaidMap {
            level,
            layout: sb.layout,
            chunk: sb.chunk,
            data_offset: sb.data_offset,
            images,
        })))
    }

    fn striped(&self, name: &str, seg: &Segment) -> Result<Map, LvmError> {
        let ext = self.vg.extent_size;
        let n = seg.stripes.len() as u64;
        if n == 0 || !seg.extent_count.is_multiple_of(n) {
            return Err(LvmError::Corrupt(format!(
                "{name}: {} extents over {n} stripes",
                seg.extent_count
            )));
        }
        let per_stripe = seg.extent_count / n;
        let mut stripes = Vec::new();
        for st in &seg.stripes {
            let pv = self
                .vg
                .physical_volumes
                .iter()
                .find(|p| p.name == st.pv)
                .ok_or_else(|| LvmError::Corrupt(format!("unknown PV {}", st.pv)))?;
            let &dev = self
                .uuid_to_dev
                .get(&pv.uuid)
                .ok_or_else(|| LvmError::MissingPv(pv.uuid.clone()))?;
            if st
                .start_extent
                .checked_add(per_stripe)
                .is_none_or(|e| e > pv.pe_count)
            {
                return Err(LvmError::Corrupt(format!("{name}: stripe past {}", st.pv)));
            }
            let off = st
                .start_extent
                .checked_mul(ext)
                .and_then(|o| o.checked_add(pv.pe_start))
                .ok_or_else(|| LvmError::Corrupt("extent offset overflows".into()))?;
            let end = off.checked_add(per_stripe * ext);
            if end.is_none_or(|e| e > self.devices[dev].size_bytes()) {
                return Err(LvmError::Corrupt(format!(
                    "{name}: stripe runs past the end of {}",
                    st.pv
                )));
            }
            stripes.push((dev, off));
        }
        if n > 1 && !(per_stripe * ext).is_multiple_of(seg.stripe_size) {
            return Err(LvmError::Unsupported(format!(
                "{name}: stripe size {} does not divide the extent run",
                seg.stripe_size
            )));
        }
        Ok(Map::Striped {
            stripe_size: seg.stripe_size,
            stripes,
        })
    }
}

/// Read `buf` at `offset` of the volume `runs` describe.
fn read_runs<R: BlockRead>(
    devices: &[R],
    runs: &[Run],
    offset: u64,
    buf: &mut [u8],
) -> fs_core::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let pos = offset + done as u64;
        let run = runs
            .iter()
            .find(|r| pos >= r.start && pos < r.start + r.len)
            .ok_or(fs_core::Error::OutOfBounds {
                offset: pos,
                len: (buf.len() - done) as u64,
                size: runs.last().map_or(0, |r| r.start + r.len),
            })?;
        let within = pos - run.start;
        let room = (run.start + run.len - pos) as usize;
        let want = room.min(buf.len() - done);
        let take = match &run.map {
            Map::Striped {
                stripe_size,
                stripes,
            } => {
                let (dev, at, take) = if stripes.len() == 1 {
                    let (d, o) = stripes[0];
                    (d, o + within, want)
                } else {
                    let n = stripes.len() as u64;
                    let k = within / stripe_size;
                    let inner = within % stripe_size;
                    let (d, o) = stripes[(k % n) as usize];
                    (
                        d,
                        o + (k / n) * stripe_size + inner,
                        ((stripe_size - inner) as usize).min(want),
                    )
                };
                devices[dev].read_at(at, &mut buf[done..done + take])?;
                take
            }
            Map::Raid(raid) => {
                let (image, at, take) = if raid.level == 1 {
                    (0, raid.data_offset + within, want)
                } else {
                    let n = raid.images.len() as u64;
                    let parity = if raid.level == 6 { 2 } else { 1 };
                    let data = n - parity;
                    let c = within / raid.chunk;
                    let inner = within % raid.chunk;
                    let stripe = c / data;
                    let (dd, _, _) =
                        crate::md::parity_map(raid.level, raid.layout, n, stripe, c % data);
                    (
                        dd,
                        raid.data_offset + stripe * raid.chunk + inner,
                        ((raid.chunk - inner) as usize).min(want),
                    )
                };
                read_runs(
                    devices,
                    &raid.images[image],
                    at,
                    &mut buf[done..done + take],
                )?;
                take
            }
        };
        done += take;
    }
    Ok(())
}

/// A logical volume, read through its physical volumes.
pub struct LogicalVolume<R: BlockRead> {
    devices: Vec<R>,
    runs: Vec<Run>,
    size: u64,
    info: LogicalVolumeInfo,
}

impl<R: BlockRead> fmt::Debug for LogicalVolume<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogicalVolume")
            .field("name", &self.info.name)
            .field("size", &self.size)
            .finish()
    }
}

impl<R: BlockRead> LogicalVolume<R> {
    /// Open logical volume `name` of the volume group on `devices`, which
    /// must include every physical volume the volume touches, in any
    /// order.
    ///
    /// `striped` segments are read, and so are `raid1`, `raid4`,
    /// `raid5*`, `raid6*` and `mirror` ones, through their hidden image
    /// sub-volumes. A dm-raid segment's level, layout, chunk and data
    /// offset come from the dm-raid superblock in its first metadata
    /// sub-volume, which is what the kernel was given. Every image must
    /// be present; other segment types are refused by name.
    pub fn open(devices: Vec<R>, name: &str) -> Result<Self, LvmError> {
        let vg = read_volume_group(&devices)?;
        // PV key -> device index, by matching the label UUID.
        let mut uuid_to_dev = BTreeMap::new();
        for (i, dev) in devices.iter().enumerate() {
            let label = read_pv_label_of(dev, i)?.ok_or(LvmError::NoLabel { device: i })?;
            uuid_to_dev.insert(label.uuid, i);
        }
        let mapper = Mapper {
            vg: &vg,
            uuid_to_dev: &uuid_to_dev,
            devices: &devices,
        };
        let info = mapper.lv(name)?.clone();
        let (runs, size) = mapper.map(name, 0)?;
        Ok(LogicalVolume {
            devices,
            runs,
            size,
            info,
        })
    }

    /// The volume as the metadata describes it.
    pub fn info(&self) -> &LogicalVolumeInfo {
        &self.info
    }
}

impl<R: BlockRead> BlockRead for LogicalVolume<R> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let len = buf.len() as u64;
        match offset.checked_add(len) {
            Some(end) if end <= self.size => {}
            _ => {
                return Err(fs_core::Error::OutOfBounds {
                    offset,
                    len,
                    size: self.size,
                })
            }
        }
        read_runs(&self.devices, &self.runs, offset, buf)
    }

    fn size_bytes(&self) -> u64 {
        self.size
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const SAMPLE: &str = r#"# Generated by LVM2
vg0 {
id = "abcdef-0123-4567-89ab-cdef-0123-456789"
seqno = 4
format = "lvm2" # informational
status = ["RESIZEABLE", "READ", "WRITE"]
flags = []
extent_size = 8192
max_lv = 0

physical_volumes {

pv0 {
id = "AAAAAA-BBBB-CCCC-DDDD-EEEE-FFFF-GGGGGG"
device = "/dev/loop0"
status = ["ALLOCATABLE"]
dev_size = 204800
pe_start = 2048
pe_count = 24
}
}

logical_volumes {

data {
id = "x"
status = ["READ", "WRITE", "VISIBLE"]
segment_count = 2

segment1 {
start_extent = 0
extent_count = 2
type = "striped"
stripe_count = 1 # linear
stripes = [
"pv0", 4
]
}
segment2 {
start_extent = 2
extent_count = 4
type = "striped"
stripe_count = 2
stripe_size = 128
stripes = [
"pv0", 10,
"pv0", 16
]
}
}
}
}
contents = "Text Format Volume Group"
version = 1
"#;

    #[test]
    fn the_sample_metadata_parses() {
        let vg = VolumeGroup::from_metadata(&parse_metadata(SAMPLE).unwrap()).unwrap();
        assert_eq!(vg.name, "vg0");
        assert_eq!(vg.seqno, 4);
        assert_eq!(vg.extent_size, 8192 * 512);
        assert_eq!(vg.physical_volumes.len(), 1);
        assert_eq!(
            vg.physical_volumes[0].uuid,
            "AAAAAABBBBCCCCDDDDEEEEFFFFGGGGGG"
        );
        assert_eq!(vg.physical_volumes[0].pe_start, 2048 * 512);
        let lv = &vg.logical_volumes[0];
        assert_eq!(lv.name, "data");
        assert_eq!(lv.segments.len(), 2);
        assert_eq!(lv.segments[1].stripe_size, 128 * 512);
        assert_eq!(lv.segments[1].stripes[1].start_extent, 16);
    }

    #[test]
    fn malformed_metadata_is_an_error_not_a_panic() {
        for bad in [
            "vg {",
            "vg { a = }",
            "vg { a = \"x }",
            "vg { a = [1, 2 }",
            "= 1",
            "vg { a = 99999999999999999999 }",
            &"a { ".repeat(100),
        ] {
            assert!(parse_metadata(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_crc_matches_a_known_value() {
        // The empty input leaves the seed untouched.
        assert_eq!(lvm_crc(&[]), LVM_CRC_SEED);
    }

    // Physical volumes built here and read back here. Self-consistency
    // only: whether this is the layout lvm2 writes is
    // `tests/oracle_lvm.rs`'s question.

    struct Mem(Vec<u8>);
    impl BlockRead for Mem {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let o = offset as usize;
            if o + buf.len() > self.0.len() {
                return Err(fs_core::Error::OutOfBounds {
                    offset,
                    len: buf.len() as u64,
                    size: self.0.len() as u64,
                });
            }
            buf.copy_from_slice(&self.0[o..o + buf.len()]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
    }

    /// Sectors per extent in the volume groups built here (64 KiB).
    const EXT_SECTORS: u64 = 128;
    const EXT: u64 = EXT_SECTORS * SECTOR;
    pub(crate) const DEV: usize = 4 << 20;
    const MDA_START: u64 = 4096;
    const MDA_SIZE: u64 = 64 * 1024;
    const PE_START: u64 = 1 << 20;
    const PE_COUNT: u64 = (DEV as u64 - PE_START) / EXT;

    fn put32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], at: usize, v: u64) {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// PV `n`'s UUID as its label stores it: 32 characters, no dashes.
    pub(crate) fn uuid(n: u8) -> String {
        std::iter::repeat_n(char::from(b'A' + n), 32).collect()
    }

    /// The same UUID as the metadata text writes it, 6-4-4-4-4-4-6.
    fn dashed(u: &str) -> String {
        let mut out = String::new();
        let mut at = 0;
        for (i, w) in [6, 4, 4, 4, 4, 4, 6].into_iter().enumerate() {
            if i > 0 {
                out.push('-');
            }
            out.push_str(&u[at..at + w]);
            at += w;
        }
        out
    }

    /// A PV with its label in sector 1, one data area from `PE_START` to
    /// the end, and one metadata area holding `text` at `ring_at` bytes
    /// from the area's start, wrapping round the ring if it runs off the
    /// end.
    pub(crate) fn pv_image(uuid: &str, text: &str, ring_at: u64) -> Vec<u8> {
        let mut d = vec![0u8; DEV];
        let l = SECTOR as usize;
        d[l..l + 8].copy_from_slice(LABEL_ID);
        put64(&mut d, l + 8, 1);
        put32(&mut d, l + 20, 32);
        d[l + 24..l + 32].copy_from_slice(LABEL_TYPE);
        let h = l + 32;
        d[h..h + 32].copy_from_slice(uuid.as_bytes());
        put64(&mut d, h + 32, DEV as u64);
        // Data areas: one, size 0 meaning "to the end", then the
        // terminating zero pair. Metadata areas: one, then the pair.
        put64(&mut d, h + 40, PE_START);
        put64(&mut d, h + 72, MDA_START);
        put64(&mut d, h + 80, MDA_SIZE);
        let crc = lvm_crc(&d[l + 20..l + SECTOR as usize]);
        put32(&mut d, l + 16, crc);

        write_mda(&mut d, MDA_START, text, ring_at);
        d
    }

    /// A metadata area at `start` holding `text` at `ring_at` bytes from
    /// the area's start, wrapping round the ring if it runs off the end.
    fn write_mda(d: &mut [u8], start: u64, text: &str, ring_at: u64) {
        let m = start as usize;
        let text = text.as_bytes();
        d[m + 4..m + 20].copy_from_slice(MDA_MAGIC);
        put32(d, m + 20, 1);
        put64(d, m + 24, start);
        put64(d, m + 32, MDA_SIZE);
        put64(d, m + 40, ring_at);
        put64(d, m + 48, text.len() as u64);
        put32(d, m + 56, lvm_crc(text));
        let ring = (MDA_SIZE - MDA_HEADER_SIZE) as usize;
        let first = (ring_at - MDA_HEADER_SIZE) as usize;
        for (i, &b) in text.iter().enumerate() {
            d[m + MDA_HEADER_SIZE as usize + (first + i) % ring] = b;
        }
        let crc = lvm_crc(&d[m + 4..m + MDA_HEADER_SIZE as usize]);
        put32(d, m, crc);
    }

    /// Where a PV's second metadata area goes: the end of the device, as
    /// `pvcreate --pvmetadatacopies 2` puts it.
    const MDA2_START: u64 = DEV as u64 - MDA_SIZE;

    /// [`pv_image`] with a second metadata area at the end of the device
    /// holding `text2`.
    fn pv_image_two_mdas(uuid: &str, text1: &str, text2: &str) -> Vec<u8> {
        let mut d = pv_image(uuid, text1, MDA_HEADER_SIZE);
        let h = SECTOR as usize + 32;
        put64(&mut d, h + 88, MDA2_START);
        put64(&mut d, h + 96, MDA_SIZE);
        let l = SECTOR as usize;
        let crc = lvm_crc(&d[l + 20..l + SECTOR as usize]);
        put32(&mut d, l + 16, crc);
        write_mda(&mut d, MDA2_START, text2, MDA_HEADER_SIZE);
        d
    }

    const LVS: &str = r#"
lin {
id = "l"
segment1 {
start_extent = 0
extent_count = 2
type = "striped"
stripe_count = 1
stripes = ["pv1", 7]
}
segment2 {
start_extent = 2
extent_count = 3
type = "striped"
stripe_count = 1
stripes = ["pv0", 1]
}
}
str {
id = "s"
segment1 {
start_extent = 0
extent_count = 4
type = "striped"
stripe_count = 2
stripe_size = 16
stripes = ["pv0", 20, "pv1", 30]
}
}
pool {
id = "t"
segment1 {
start_extent = 0
extent_count = 1
type = "thin-pool"
metadata = "pool_tmeta"
pool = "pool_tdata"
}
}
"#;

    pub(crate) fn vg_text(seqno: u64, lvs: &str) -> String {
        format!(
            "vg0 {{\nid = \"vgvgvg-vgvg-vgvg-vgvg-vgvg-vgvg-vgvgvg\"\nseqno = {seqno}\n\
             extent_size = {EXT_SECTORS}\nphysical_volumes {{\n\
             pv0 {{\nid = \"{u0}\"\npe_start = {pe}\npe_count = {PE_COUNT}\n}}\n\
             pv1 {{\nid = \"{u1}\"\npe_start = {pe}\npe_count = {PE_COUNT}\n}}\n}}\n\
             logical_volumes {{{lvs}}}\n}}\ncontents = \"Text Format Volume Group\"\n",
            u0 = dashed(&uuid(0)),
            u1 = dashed(&uuid(1)),
            pe = PE_START / SECTOR,
        )
    }

    /// A pseudo-random volume of `len` bytes.
    fn pattern(len: u64, seed: u32) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    /// Two PVs carrying `LVS`, with `lin` and `str` written into them
    /// by hand, extent by extent and stripe by stripe. pv0 holds an
    /// older copy of the metadata with no volumes in it; pv1's newer
    /// copy wraps round the end of its ring.
    fn two_pvs() -> (Vec<Vec<u8>>, Vec<u8>, Vec<u8>) {
        let mut pv0 = pv_image(&uuid(0), &vg_text(3, ""), MDA_HEADER_SIZE);
        let text = vg_text(4, LVS);
        let mut pv1 = pv_image(&uuid(1), &text, MDA_SIZE - 100);
        let lin = pattern(5 * EXT, 7);
        let at = |extent: u64| (PE_START + extent * EXT) as usize;
        pv1[at(7)..at(9)].copy_from_slice(&lin[..2 * EXT as usize]);
        pv0[at(1)..at(4)].copy_from_slice(&lin[2 * EXT as usize..]);
        let str_ = pattern(4 * EXT, 11);
        let chunk = 16 * SECTOR as usize;
        for (k, c) in str_.chunks(chunk).enumerate() {
            let (pv, first) = if k % 2 == 0 {
                (&mut pv0, at(20))
            } else {
                (&mut pv1, at(30))
            };
            let o = first + (k / 2) * chunk;
            pv[o..o + chunk].copy_from_slice(c);
        }
        (vec![pv0, pv1], lin, str_)
    }

    fn read_all(lv: &LogicalVolume<Mem>) -> Vec<u8> {
        let mut got = vec![0u8; lv.size_bytes() as usize];
        lv.read_at(0, &mut got).unwrap();
        got
    }

    #[test]
    fn linear_segments_and_stripes_read_back_whatever_the_pv_order() {
        let (pvs, lin, str_) = two_pvs();
        for reverse in [false, true] {
            let mut devs: Vec<Mem> = pvs.iter().cloned().map(Mem).collect();
            if reverse {
                devs.reverse();
            }
            let devs2: Vec<Mem> = devs.iter().map(|m| Mem(m.0.clone())).collect();
            let a = LogicalVolume::open(devs, "lin").unwrap();
            assert_eq!(a.info().segments.len(), 2);
            assert_eq!(read_all(&a), lin, "lin, reversed {reverse}");
            let s = LogicalVolume::open(devs2, "str").unwrap();
            assert_eq!(read_all(&s), str_, "str, reversed {reverse}");
            // Unaligned, across a stripe boundary.
            let mut part = vec![0u8; 3 * 8192 + 9];
            s.read_at(8192 - 4, &mut part).unwrap();
            assert_eq!(part[..], str_[8192 - 4..][..part.len()]);
        }
    }

    #[test]
    fn the_newest_metadata_wins_even_when_it_wraps() {
        let (pvs, _, _) = two_pvs();
        let devs: Vec<Mem> = pvs.into_iter().map(Mem).collect();
        let vg = read_volume_group(&devs).unwrap();
        assert_eq!(vg.seqno, 4);
        assert_eq!(vg.name, "vg0");
        let names: Vec<&str> = vg.logical_volumes.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["lin", "str", "pool"]);
    }

    #[test]
    fn a_missing_pv_and_an_unread_segment_type_are_named() {
        let (pvs, _, _) = two_pvs();
        let only1 = vec![Mem(pvs[1].clone())];
        assert!(matches!(
            LogicalVolume::open(only1, "str"),
            Err(LvmError::MissingPv(_))
        ));
        let devs: Vec<Mem> = pvs.iter().cloned().map(Mem).collect();
        assert!(matches!(
            LogicalVolume::open(devs, "pool"),
            Err(LvmError::Unsupported(_))
        ));
        let devs: Vec<Mem> = pvs.into_iter().map(Mem).collect();
        assert!(matches!(
            LogicalVolume::open(devs, "nope"),
            Err(LvmError::NoSuchVolume(_))
        ));
    }

    #[test]
    fn bad_crcs_are_refused() {
        let (pvs, _, _) = two_pvs();
        // A byte of the PV header.
        let mut d = pvs[1].clone();
        d[SECTOR as usize + 40] ^= 1;
        assert!(matches!(
            read_pv_label(&Mem(d)),
            Err(LvmError::BadChecksum { what: "label", .. })
        ));
        // A byte of the metadata text, at the start of the ring where the
        // wrapped half lies.
        let mut d = pvs[1].clone();
        d[(MDA_START + MDA_HEADER_SIZE) as usize] ^= 1;
        let m = Mem(d);
        let label = read_pv_label(&m).unwrap().unwrap();
        assert!(matches!(
            read_metadata_text(&m, &label),
            Err(LvmError::BadChecksum {
                what: "metadata text",
                ..
            })
        ));
    }

    #[test]
    fn an_ignored_metadata_area_is_not_read() {
        let (mut pvs, _, _) = two_pvs();
        let m = MDA_START as usize;
        put32(&mut pvs[1], m + 60, RAW_LOCN_IGNORED);
        let crc = lvm_crc(&pvs[1][m + 4..m + MDA_HEADER_SIZE as usize]);
        put32(&mut pvs[1], m, crc);
        let devs: Vec<Mem> = pvs.into_iter().map(Mem).collect();
        assert_eq!(read_volume_group(&devs).unwrap().seqno, 3);
    }

    #[test]
    fn a_device_without_a_label_is_not_a_pv_and_reads_stay_in_bounds() {
        assert!(read_pv_label(&Mem(vec![0u8; DEV])).unwrap().is_none());
        assert!(matches!(
            LogicalVolume::open(vec![Mem(vec![0u8; DEV])], "lin"),
            Err(LvmError::NoLabel { device: 0 })
        ));
        let (pvs, _, _) = two_pvs();
        let a = LogicalVolume::open(pvs.into_iter().map(Mem).collect(), "lin").unwrap();
        let mut b = [0u8; 2];
        assert!(a.read_at(a.size_bytes() - 1, &mut b).is_err());
        assert!(a.read_at(u64::MAX, &mut b).is_err());
    }

    fn ignore_metadata(pv: &mut [u8]) {
        let m = MDA_START as usize;
        put32(pv, m + 60, RAW_LOCN_IGNORED);
        let crc = lvm_crc(&pv[m + 4..m + MDA_HEADER_SIZE as usize]);
        put32(pv, m, crc);
    }

    #[test]
    fn a_scan_groups_pvs_by_volume_group() {
        let (pvs, lin, _) = two_pvs();
        // A second group, vg1, on a PV of its own.
        let other = vg_text(1, "")
            .replace("vg0 {", "vg1 {")
            .replace(
                "vgvgvg-vgvg-vgvg-vgvg-vgvg-vgvg-vgvgvg",
                "wwwwww-wwww-wwww-wwww-wwww-wwww-wwwwww",
            )
            .replace(&dashed(&uuid(0)), &dashed(&uuid(2)))
            .replace(&dashed(&uuid(1)), &dashed(&uuid(3)));
        let devices = vec![
            Mem(vec![0u8; DEV]),
            Mem(pvs[1].clone()),
            Mem(pv_image(&uuid(2), &other, MDA_HEADER_SIZE)),
            Mem(pvs[0].clone()),
        ];
        let found = scan(devices);
        assert_eq!(found.others.len(), 1, "the blank device has no label");
        assert!(found.refused.is_empty());
        assert!(found.unclaimed.is_empty());
        let names: Vec<&str> = found
            .volume_groups
            .iter()
            .map(|g| g.volume_group.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["vg0", "vg1"],
            "in the order each group was first met"
        );
        let vg0 = &found.volume_groups[0];
        assert_eq!(
            vg0.volume_group.seqno, 4,
            "the newest metadata among the group's PVs"
        );
        assert_eq!(vg0.devices.len(), 2);
        assert!(vg0.missing.is_empty());
        let vg1 = &found.volume_groups[1];
        assert_eq!(vg1.devices.len(), 1);
        assert_eq!(vg1.missing, ["pv1"], "vg1's second PV was not given");

        let vg0 = found.volume_groups.into_iter().next().unwrap();
        let lv = LogicalVolume::open(vg0.devices, "lin").unwrap();
        assert_eq!(read_all(&lv), lin);
    }

    #[test]
    fn a_pv_with_no_metadata_joins_the_group_that_names_it() {
        let (mut pvs, lin, _) = two_pvs();
        // pv0 keeps no metadata of its own; only pv1's names it.
        ignore_metadata(&mut pvs[0]);
        let found = scan(vec![Mem(pvs[0].clone()), Mem(pvs[1].clone())]);
        assert_eq!(found.volume_groups.len(), 1);
        assert_eq!(found.volume_groups[0].devices.len(), 2);
        let g = found.volume_groups.into_iter().next().unwrap();
        assert_eq!(
            read_all(&LogicalVolume::open(g.devices, "lin").unwrap()),
            lin
        );

        // Alone, nothing given says which group it is in.
        let found = scan(vec![Mem(pvs[0].clone())]);
        assert!(found.volume_groups.is_empty());
        assert_eq!(found.unclaimed.len(), 1, "a labelled PV no metadata names");
    }

    #[test]
    fn a_damaged_label_is_refused_by_name_not_dropped() {
        let (pvs, _, _) = two_pvs();
        let mut bad = pvs[1].clone();
        bad[SECTOR as usize + 40] ^= 1;
        let found = scan(vec![Mem(bad), Mem(pvs[0].clone())]);
        assert_eq!(found.refused.len(), 1);
        assert!(matches!(
            found.refused[0].1,
            LvmError::BadChecksum { what: "label", .. }
        ));
        // pv0's own, older metadata still makes a group of it.
        assert_eq!(found.volume_groups.len(), 1);
        assert_eq!(found.volume_groups[0].volume_group.seqno, 3);
        assert_eq!(found.volume_groups[0].missing, ["pv1"]);
    }

    #[test]
    fn of_two_metadata_areas_the_newer_copy_is_read() {
        for newer_second in [true, false] {
            let (old, new) = (vg_text(3, ""), vg_text(4, LVS));
            let (t1, t2) = if newer_second {
                (&old, &new)
            } else {
                (&new, &old)
            };
            let pv = pv_image_two_mdas(&uuid(1), t1, t2);
            let label = read_pv_label(&Mem(pv.clone())).unwrap().unwrap();
            assert_eq!(label.metadata_areas.len(), 2);
            let vg = read_volume_group(&[Mem(pv)]).unwrap();
            assert_eq!(vg.seqno, 4, "newer copy second: {newer_second}");
        }
    }

    #[test]
    fn a_damaged_first_metadata_area_is_read_from_the_second() {
        let text = vg_text(4, LVS);
        let mut pv = pv_image_two_mdas(&uuid(1), &text, &text);
        // The first area's header checksum no longer matches.
        pv[MDA_START as usize + 30] ^= 1;
        let vg = read_volume_group(&[Mem(pv.clone())]).unwrap();
        assert_eq!(vg.seqno, 4, "the second copy is read");

        // With both damaged, the first area's error is the one reported.
        pv[MDA2_START as usize + 30] ^= 1;
        let label = read_pv_label(&Mem(pv.clone())).unwrap().unwrap();
        assert!(matches!(
            read_metadata_text(&Mem(pv), &label),
            Err(LvmError::BadChecksum {
                what: "metadata area header",
                ..
            })
        ));
    }

    /// A dm-raid superblock: `level`, md `layout`, a chunk of
    /// `chunk_sectors` and data starting `data_offset_sectors` into each
    /// image.
    fn dm_raid_sb(
        level: u32,
        layout: u32,
        chunk_sectors: u32,
        data_offset_sectors: u64,
    ) -> Vec<u8> {
        let mut b = vec![0u8; SECTOR as usize];
        put32(&mut b, 0, dm_raid::MAGIC);
        put32(&mut b, dm_raid::COMPAT_FEATURES, dm_raid::COMPAT_V190);
        put32(&mut b, dm_raid::LEVEL, level);
        put32(&mut b, dm_raid::LAYOUT, layout);
        put32(&mut b, dm_raid::STRIPE_SECTORS, chunk_sectors);
        put64(&mut b, dm_raid::DATA_OFFSET, data_offset_sectors);
        b
    }

    /// One `striped` sub-volume of `count` extents at `extent` of `pv`.
    fn sub_lv(name: &str, pv: &str, extent: u64, count: u64) -> String {
        format!(
            "{name} {{\nid = \"{name}\"\nsegment1 {{\nstart_extent = 0\nextent_count = {count}\n\
             type = \"striped\"\nstripe_count = 1\nstripes = [\"{pv}\", {extent}]\n}}\n}}\n"
        )
    }

    /// Where extent `e` of PV image `pv` starts, in bytes.
    fn at(e: u64) -> usize {
        (PE_START + e * EXT) as usize
    }

    /// raid1 over two images, data starting one chunk into each image as
    /// the dm-raid superblock says, and a mirror over two images.
    #[test]
    fn raid1_and_mirror_segments_read_their_first_image() {
        let data = pattern(4 * EXT, 23);
        let lvs = format!(
            "r {{\nid = \"r\"\nsegment1 {{\nstart_extent = 0\nextent_count = 4\ntype = \"raid1\"\n\
             device_count = 2\nraids = [\"r_rmeta_0\", \"r_rimage_0\", \"r_rmeta_1\", \"r_rimage_1\"]\n}}\n}}\n\
             m {{\nid = \"m\"\nsegment1 {{\nstart_extent = 0\nextent_count = 4\ntype = \"mirror\"\n\
             mirror_count = 2\nmirrors = [\"m_mimage_0\", 0, \"m_mimage_1\", 0]\n}}\n}}\n{}{}{}{}{}{}",
            sub_lv("r_rmeta_0", "pv0", 10, 1),
            sub_lv("r_rimage_0", "pv0", 11, 5),
            sub_lv("r_rmeta_1", "pv1", 10, 1),
            sub_lv("r_rimage_1", "pv1", 11, 5),
            sub_lv("m_mimage_0", "pv0", 20, 4),
            sub_lv("m_mimage_1", "pv1", 20, 4),
        );
        let text = vg_text(5, &lvs);
        let mut pv0 = pv_image(&uuid(0), &text, MDA_HEADER_SIZE);
        let mut pv1 = pv_image(&uuid(1), &text, MDA_HEADER_SIZE);
        // Data one chunk (16 sectors) into each raid1 image.
        let off = 16 * SECTOR as usize;
        for pv in [&mut pv0, &mut pv1] {
            pv[at(10)..at(10) + SECTOR as usize].copy_from_slice(&dm_raid_sb(1, 0, 16, 16));
            pv[at(11) + off..at(11) + off + data.len()].copy_from_slice(&data);
            pv[at(20)..at(20) + data.len()].copy_from_slice(&data);
        }
        for name in ["r", "m"] {
            let lv = LogicalVolume::open(vec![Mem(pv0.clone()), Mem(pv1.clone())], name).unwrap();
            assert_eq!(lv.size_bytes(), 4 * EXT, "{name}");
            assert_eq!(read_all(&lv), data, "{name}");
        }
    }

    /// raid5 (left-symmetric) over three images, data placed by md's
    /// parity map: what the superblock's level, layout and chunk say.
    #[test]
    fn a_raid5_segment_reads_its_data_chunks_round_the_images() {
        let chunk = 16 * SECTOR;
        let data = pattern(4 * EXT, 29);
        let lvs = format!(
            "r {{\nid = \"r\"\nsegment1 {{\nstart_extent = 0\nextent_count = 4\ntype = \"raid5_ls\"\n\
             device_count = 3\nstripe_size = 16\nraids = [\"r_rmeta_0\", \"r_rimage_0\", \"r_rmeta_1\", \
             \"r_rimage_1\", \"r_rmeta_2\", \"r_rimage_2\"]\n}}\n}}\n{}{}{}{}{}{}",
            sub_lv("r_rmeta_0", "pv0", 30, 1),
            sub_lv("r_rimage_0", "pv0", 31, 2),
            sub_lv("r_rmeta_1", "pv1", 30, 1),
            sub_lv("r_rimage_1", "pv1", 31, 2),
            sub_lv("r_rmeta_2", "pv0", 34, 1),
            sub_lv("r_rimage_2", "pv0", 35, 2),
        );
        let text = vg_text(5, &lvs);
        let mut pvs = [
            pv_image(&uuid(0), &text, MDA_HEADER_SIZE),
            pv_image(&uuid(1), &text, MDA_HEADER_SIZE),
        ];
        // (PV index, first extent) of each image and its metadata.
        let images = [(0usize, 31u64), (1, 31), (0, 35)];
        for (pv, meta) in [(0usize, 30u64), (1, 30), (0, 34)] {
            pvs[pv][at(meta)..at(meta) + SECTOR as usize].copy_from_slice(&dm_raid_sb(5, 2, 16, 0));
        }
        let c = chunk as usize;
        for (k, src) in data.chunks(c).enumerate() {
            let (stripe, i) = (k as u64 / 2, k as u64 % 2);
            let (dd, _, _) = crate::md::parity_map(5, 2, 3, stripe, i);
            let (pv, first) = images[dd];
            let o = at(first) + (stripe * chunk) as usize;
            pvs[pv][o..o + c].copy_from_slice(src);
        }
        let devs: Vec<Mem> = pvs.into_iter().map(Mem).collect();
        let lv = LogicalVolume::open(devs, "r").unwrap();
        assert_eq!(read_all(&lv), data);
        // Unaligned, across a chunk boundary and an image boundary.
        let mut part = vec![0u8; 3 * c + 7];
        lv.read_at(chunk - 3, &mut part).unwrap();
        assert_eq!(part[..], data[c - 3..][..part.len()]);
    }

    #[test]
    fn a_raid_segment_without_a_dm_raid_superblock_or_of_another_level_is_refused() {
        let lvs = format!(
            "r {{\nid = \"r\"\nsegment1 {{\nstart_extent = 0\nextent_count = 2\ntype = \"raid1\"\n\
             device_count = 1\nraids = [\"r_rmeta_0\", \"r_rimage_0\"]\n}}\n}}\n\
             t {{\nid = \"t\"\nsegment1 {{\nstart_extent = 0\nextent_count = 2\ntype = \"raid10\"\n\
             device_count = 1\nraids = [\"r_rmeta_0\", \"r_rimage_0\"]\n}}\n}}\n{}{}",
            sub_lv("r_rmeta_0", "pv0", 10, 1),
            sub_lv("r_rimage_0", "pv0", 41, 2),
        );
        let text = vg_text(5, &lvs);
        let pvs = || {
            vec![
                Mem(pv_image(&uuid(0), &text, MDA_HEADER_SIZE)),
                Mem(pv_image(&uuid(1), &text, MDA_HEADER_SIZE)),
            ]
        };
        assert!(matches!(
            LogicalVolume::open(pvs(), "r"),
            Err(LvmError::Corrupt(_))
        ));
        assert!(matches!(
            LogicalVolume::open(pvs(), "t"),
            Err(LvmError::Unsupported(_))
        ));
    }
}
