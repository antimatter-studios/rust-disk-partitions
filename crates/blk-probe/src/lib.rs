//! blk.probe — open a disk image (raw or container), walk its partition
//! table, emit JSON describing what's inside.
//!
//! This is the library. [`probe_path`] is what the `blk.probe` binary runs,
//! and [`probe_bytes`] is the same probe over an image already in memory —
//! container detection, the container reader, the partition walk and the
//! per-partition sniff, in the order a hostile image meets them. The binary
//! adds argument parsing and the exit codes below, and nothing else.
//!
//! Usage:
//!   blk.probe <path>
//!   blk.probe <path> --container=qcow2|vhd|vhdx|vmdk
//!
//! When `--container` is omitted, container kind is auto-detected from
//! the magic at offset 0 (or the trailing 512-byte footer for fixed
//! VHDs). If no container is recognised the file is treated as a raw
//! disk image (which is correct for whole-disk `.img` / `.dd` dumps).
//!
//! Exit codes:
//!   0  — JSON written to stdout
//!   1  — argument / option error
//!   2  — file open / container layer error
//!   3  — partition probe error: the table could not be read
//!
//! A disk with *no* partition table is not exit 3. That is a normal answer
//! — a whole-device filesystem — and it exits 0 with `"table":"none"` plus a
//! `device_fs_kind` naming whatever was sniffed at offset 0. Exit 3 is the
//! opposite case: a table is there and this program could not read it (CRC
//! mismatch, truncation, I/O failure), so nothing is written to stdout.
//!
//! A failed *filesystem sniff* is reported in-band instead, because the rest
//! of the document is still true. The partition gets `"fs_kind":"error"` and
//! an `"fs_kind_error"` holding the reason; the whole-device equivalents are
//! `"device_fs_kind"` and `"device_fs_error"`. `"unknown"` therefore means
//! only what it says — the sniff ran and recognised nothing.
//!
//! JSON shape:
//!   {
//!     "path": "/path/to/file",
//!     "container": "qcow2"|"vhd"|"vhdx"|"vmdk"|"raw",
//!     "container_size_bytes": 12345,        // virtual size after container unwrap
//!     "table": "gpt"|"mbr"|"none",
//!     "device_fs_kind": "ext4",             // only when "table" is "none"
//!     "device_fs_error": "read failed: ..", // only when that sniff failed
//!     "partitions": [
//!       {
//!         "index": 0,
//!         "slot": 0,                        // zero-based table slot; absent when it has none
//!         "start": 1048576,
//!         "length": 268435456,              // what the table claims
//!         "available_length": 268435456,    // how much of it the device holds
//!         "issues": 0,                      // rust-disk-partitions PARTITIONS_ENTRY_* bits; 0 = none
//!         "fs_kind": "ext2"|"ext3"|"ext4"|"ntfs"|"fat32"|"fat16"|"exfat"|"hfs_plus"|"apfs"|"linux_swap"|"iso9660"|"squashfs"|"bcachefs"|"xfs"|"btrfs"|"erofs"|"unknown"|"error",
//!         "fs_kind_error": "read failed: ..", // only when fs_kind is "error"
//!         "type_byte": 131,                 // MBR partition type byte (0 for GPT)
//!         "type_guid": "0fc63daf-8483-...", // GPT type GUID (zeros for MBR)
//!         "label": "boot"                   // optional, may be absent
//!       },
//!       ...
//!     ]
//!   }

use std::ffi::{CStr, CString};
use std::ptr;

use disk_partitions::capi::{
    partitions_count, partitions_get, partitions_list_free, partitions_probe, partitions_sniff,
    partitions_sniff_device, partitions_table_kind, FsKindCode, PartitionInfo, PartitionList,
    TableKindCode,
};
use fs_core::ffi::{
    fs_core_device_close, fs_core_device_from_callbacks, fs_core_device_size_bytes,
    fs_core_file_open, fs_core_last_error_message, FsCoreCallbackCfg, FsCoreDevice,
    FsCoreErrorCode,
};

// Force the container-reader rlibs to be linked. We only call into them
// via extern "C" declarations below (since we don't want to depend on
// each crate's Rust API surface), but cargo would otherwise drop the
// rlibs entirely because nothing in this crate refers to them by name
// in Rust source. The `use ... as _;` keeps the rlib in the link line
// so the `#[no_mangle]` symbols resolve.
#[allow(unused_imports)]
use img_qcow2 as _;
#[allow(unused_imports)]
use img_vhd as _;
#[allow(unused_imports)]
use img_vhdx as _;
#[allow(unused_imports)]
use img_vmdk as _;

/// The exact text `disk_partitions::Error::NoPartitionTable` renders. See
/// [`classify_probe`] for why matching on a message, rather than on a
/// return code, is the only option available here.
pub const NO_PARTITION_TABLE_MESSAGE: &str = "no GPT or MBR signature found";

/// `fs_kind` / `device_fs_kind` value meaning the sniff itself failed — as
/// distinct from `"unknown"`, which means it ran and recognised nothing.
pub const SNIFF_FAILED_LABEL: &str = "error";

/// Signature bytes at offset 0, one per container format.
///
/// Three were written as readable ASCII at the call site and the fourth
/// as four loose hex bytes, which made the qcow2 arm the only one a
/// reader could not check against the format's documentation by eye.
///
/// `QCOW2` is `QFI\xfb` — three printable characters and one that is
/// not, which is why it resisted the ASCII spelling the others use.
pub mod magic {
    /// qcow2: `Q`, `F`, `I`, 0xfb.
    pub const QCOW2: &[u8] = b"QFI\xfb";
    /// VHDX, at offset 0.
    pub const VHDX: &[u8] = b"vhdxfile";
    /// VMDK sparse extent header.
    pub const VMDK: &[u8] = b"KDMV";
    /// VHD, in the header of a dynamic disk and the footer of any.
    pub const VHD: &[u8] = b"conectix";
}

/// Bytes in a VHD footer, and the distance back from the end of a fixed
/// image at which it starts.
///
/// The literal `512` appeared for this and for nothing else in the
/// probe — but it reads like a sector size, which it is not: a footer is
/// 512 bytes because the VHD format says so, and it would stay 512 on a
/// 4Kn device.
pub const VHD_FOOTER_SIZE: u64 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Raw,
    Qcow2,
    Vhd,
    Vhdx,
    Vmdk,
}

impl Container {
    pub fn label(self) -> &'static str {
        match self {
            Container::Raw => "raw",
            Container::Qcow2 => "qcow2",
            Container::Vhd => "vhd",
            Container::Vhdx => "vhdx",
            Container::Vmdk => "vmdk",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "raw" => Some(Container::Raw),
            "qcow2" => Some(Container::Qcow2),
            "vhd" => Some(Container::Vhd),
            "vhdx" => Some(Container::Vhdx),
            "vmdk" => Some(Container::Vmdk),
            _ => None,
        }
    }
}

/// Read until `buf` is full or the source genuinely ends.
///
/// # A short read is not a short file
///
/// `Read::read` is allowed to return fewer bytes than asked for without
/// an error and without being at end of file. One call filling a
/// 16-byte buffer is what happens with a local regular file on a quiet
/// machine; it is not what the trait promises, and a pipe, a network
/// filesystem or a signal makes it false.
///
/// [`auto_detect_container`] then reads the count as a statement about
/// the file. A read that returns 4 makes the `n >= 8` guards false, so
/// the vhdx and vhd magics become untestable and the image falls
/// through to `Raw` — the wrong answer, with no diagnostic, for a file
/// whose first eight bytes say exactly what it is.
///
/// # Errors are errors
///
/// The previous form was `f.read(&mut head).unwrap_or(0)`, inside a
/// function that already returns `io::Result`. It threw the error away
/// and reported the same 0 an empty file gives, so an unreadable device
/// and an empty one were indistinguishable.
///
/// `Interrupted` is the one kind that is retried rather than returned:
/// it means a signal arrived mid-call, not that anything is wrong.
///
/// A file shorter than `buf` is NOT an error — a tiny raw image is
/// legitimate — which is why this returns a count rather than using
/// `read_exact` and mapping its `UnexpectedEof` back.
pub fn read_up_to<R: std::io::Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break, // genuine end of input
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Identify the container by signature, trying the strongest evidence
/// first.
///
/// # The order is load-bearing
///
/// Steps 1-4 test magics at offset 0 and are mutually exclusive, so
/// their relative order does not matter. The last two do:
///
/// **The trailing-footer VHD check must come after them, and before the
/// raw fallback.** A fixed VHD is byte-for-byte a raw disk image with a
/// 512-byte footer glued on the end — offset 0 is ordinary
/// partition-table data. The footer is therefore the *only* thing
/// separating a fixed VHD from a raw image, and it is a far weaker
/// signal than the offset-0 magics: eight bytes at a position that is
/// payload in every other format. Testing it last among the signatures
/// lets any strong offset-0 magic win; testing it before `Raw` is what
/// stops every fixed VHD being reported as raw.
pub fn auto_detect_container(path: &str) -> std::io::Result<Container> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let mut head = [0u8; 16];
    let n = read_up_to(&mut f, &mut head)?;
    for (sig, kind) in [
        (magic::QCOW2, Container::Qcow2),
        (magic::VHDX, Container::Vhdx),
        (magic::VMDK, Container::Vmdk),
        (magic::VHD, Container::Vhd),
    ] {
        if n >= sig.len() && &head[..sig.len()] == sig {
            return Ok(kind);
        }
    }
    // Fixed VHD: footer at file_size - 512.
    let len = f.metadata()?.len();
    if len >= VHD_FOOTER_SIZE {
        f.seek(SeekFrom::Start(len - VHD_FOOTER_SIZE))?;
        let mut footer = [0u8; 8];
        if read_up_to(&mut f, &mut footer)? == footer.len() && footer == magic::VHD {
            return Ok(Container::Vhd);
        }
    }
    Ok(Container::Raw)
}

// `FsCoreDevice` is an opaque handle: allocated by a sister crate's
// constructor, passed straight back to it, and never dereferenced on
// this side. Rust cannot see that from the signature — it sees a type
// without `#[repr(C)]` crossing an FFI boundary and warns, sixteen
// times, once per declaration.
//
// The allow is scoped to this block rather than the crate, so a
// genuinely unsafe type appearing in some OTHER extern block is still
// reported.
//
// This is a local workaround for something better fixed upstream:
// `#[repr(C)]` on `FsCoreDevice` in rust-fs-core would silence it for
// every consumer at once and is the honest description of a handle that
// only ever crosses as a pointer. It is not done here because that
// crate is a dependency of seven others and the change belongs with its
// owner.
//
// Until then this crate could not adopt its family's lint gate at all:
// `cargo clippy --locked --all-targets -- -D warnings` promoted these
// sixteen warnings into sixteen hard errors, so "no CI, no lint task"
// was a consequence rather than an omission.
#[allow(improper_ctypes)]
extern "C" {
    fn qcow2_open_rw_on_device(inner: *mut FsCoreDevice) -> *mut FsCoreDevice;
    fn qcow2_open_on_device(inner: *mut FsCoreDevice) -> *mut FsCoreDevice;
    fn vhd_open_rw_on_device(inner: *mut FsCoreDevice) -> *mut FsCoreDevice;
    fn vhd_open_on_device(inner: *mut FsCoreDevice) -> *mut FsCoreDevice;
    fn vhdx_open_rw_on_device(inner: *mut FsCoreDevice) -> *mut FsCoreDevice;
    fn vhdx_open_on_device(inner: *mut FsCoreDevice) -> *mut FsCoreDevice;
    fn vmdk_open_rw_on_device(inner: *mut FsCoreDevice) -> *mut FsCoreDevice;
    fn vmdk_open_on_device(inner: *mut FsCoreDevice) -> *mut FsCoreDevice;
}

/// Stack a container reader over an already-open device.
///
/// `Container::Raw` is the identity: it hands the device straight back,
/// which is what makes a raw image one code path rather than two.
///
/// # `inner` IS CONSUMED, INCLUDING WHEN THIS FAILS
///
/// Every one of the four readers reclaims the handle box on entry -- "drop
/// the original wrapper so the caller's pointer is consumed", in
/// `rust-img-vhdx`'s words, and the other three do the same -- and it does
/// that BEFORE it tries to parse anything. So a NULL return does not mean
/// nothing happened: `inner` has already been freed, and closing it again is
/// a use-after-free.
///
/// It is written here because the compiler cannot say it and because it has
/// already been got wrong once: closing the inner handle on the failure path
/// turned a truncated VHDX into a SIGBUS in `Arc::drop`, found by
/// `fuzz/corpus/probe/vhdx-head.img` on the first run of the gate that
/// replays it.
///
/// `Container::Raw` is the identity: it hands the device straight back, which
/// is what makes a raw image one code path rather than two, and it never
/// returns NULL.
///
/// # Safety
///
/// `inner` must be a live `FsCoreDevice` handle, and the caller must not use
/// it again afterwards on either path. On success the returned handle owns
/// it; closing the returned handle is what releases both, and for `Raw` they
/// are the same pointer.
pub unsafe fn open_container_on(
    inner: *mut FsCoreDevice,
    kind: Container,
    writable: bool,
) -> *mut FsCoreDevice {
    match (kind, writable) {
        (Container::Raw, _) => inner,
        (Container::Qcow2, true) => qcow2_open_rw_on_device(inner),
        (Container::Qcow2, false) => qcow2_open_on_device(inner),
        (Container::Vhd, true) => vhd_open_rw_on_device(inner),
        (Container::Vhd, false) => vhd_open_on_device(inner),
        (Container::Vhdx, true) => vhdx_open_rw_on_device(inner),
        (Container::Vhdx, false) => vhdx_open_on_device(inner),
        (Container::Vmdk, true) => vmdk_open_rw_on_device(inner),
        (Container::Vmdk, false) => vmdk_open_on_device(inner),
    }
}

pub fn last_error() -> String {
    unsafe {
        let p = fs_core_last_error_message();
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

fn fs_kind_label(code: i32) -> &'static str {
    // Mirror the disk_partitions::FsKindCode enum.
    match code {
        x if x == FsKindCode::Ext2 as i32 => "ext2",
        x if x == FsKindCode::Ext3 as i32 => "ext3",
        x if x == FsKindCode::Ext4 as i32 => "ext4",
        x if x == FsKindCode::Ntfs as i32 => "ntfs",
        x if x == FsKindCode::ExFat as i32 => "exfat",
        x if x == FsKindCode::Fat32 as i32 => "fat32",
        x if x == FsKindCode::Fat16 as i32 => "fat16",
        x if x == FsKindCode::HfsPlus as i32 => "hfs_plus",
        x if x == FsKindCode::Apfs as i32 => "apfs",
        x if x == FsKindCode::LinuxSwap as i32 => "linux_swap",
        x if x == FsKindCode::Iso9660 as i32 => "iso9660",
        x if x == FsKindCode::Squashfs as i32 => "squashfs",
        x if x == FsKindCode::Bcachefs as i32 => "bcachefs",
        x if x == FsKindCode::Xfs as i32 => "xfs",
        x if x == FsKindCode::Btrfs as i32 => "btrfs",
        x if x == FsKindCode::Erofs as i32 => "erofs",
        _ => "unknown",
    }
}

fn table_kind_label(code: i32) -> &'static str {
    match code {
        x if x == TableKindCode::Gpt as i32 => "gpt",
        x if x == TableKindCode::Mbr as i32 => "mbr",
        _ => "none",
    }
}

/// What `partitions_probe` actually said.
#[derive(Debug, PartialEq, Eq)]
enum ProbeOutcome {
    /// A GPT or MBR table was parsed and the list handle is usable.
    Table,
    /// The device carries no partition table. Not an error — a
    /// whole-device filesystem is an ordinary disk.
    NoTable,
    /// The probe failed, and this is why.
    Failed(String),
}

/// Tell "this disk has no partition table" apart from "I could not read
/// the partition table".
///
/// The return code alone cannot do it. `partitions_probe` lifts every
/// `disk_partitions::Error` — a missing signature, a GPT CRC mismatch, a short
/// read — through `fs_core::Error::Custom`, so a corrupt table and an
/// unpartitioned ext4 image arrive here as the same `FsCoreErrorCode`. The
/// only thing that crosses the boundary still carrying the distinction is
/// the last-error text, so that is what this matches on.
///
/// That makes this function a coupling to a sibling crate's error *string*,
/// which is worth saying out loud: if `disk_partitions::Error::NoPartitionTable`
/// is ever reworded, every unpartitioned image starts exiting 3. The
/// alternative — folding both cases into exit 0, which is what this code
/// used to do — is worse, because it is wrong silently rather than loudly.
/// A dedicated `FsCoreErrorCode` for "no table" upstream would retire this.
///
/// `last_error` is consulted only when `rc` is non-Ok: the thread-local it
/// is read from can hold a stale message from an earlier call otherwise.
fn classify_probe(rc: FsCoreErrorCode, list_is_null: bool, last_error: &str) -> ProbeOutcome {
    if rc == FsCoreErrorCode::Ok {
        // The ABI promises a non-NULL list whenever it reports success, so
        // a NULL one is a broken promise — something to report, not a disk
        // that happens to have no partitions.
        return if list_is_null {
            ProbeOutcome::Failed("reported success but returned a NULL partition list".to_string())
        } else {
            ProbeOutcome::Table
        };
    }
    if last_error.trim() == NO_PARTITION_TABLE_MESSAGE {
        return ProbeOutcome::NoTable;
    }
    ProbeOutcome::Failed(if last_error.is_empty() {
        format!("failed with {rc:?} (no detail recorded)")
    } else {
        last_error.to_string()
    })
}

/// Interpret a `partitions_sniff` / `partitions_sniff_device` return value.
///
/// `-1` is the documented error sentinel, with the reason waiting in
/// `fs_core_last_error_message()`. Every other value is an `FsKindCode`
/// discriminant, of which `Unknown` (0) means the sniff ran to completion
/// and matched nothing.
///
/// Those are two different facts and this is where they stop being the same
/// string. Passing the sentinel straight to [`fs_kind_label`] does not work:
/// -1 lands on that function's catch-all arm and comes back as `"unknown"`,
/// which is exactly what a successful no-match returns. The `detail` closure
/// is only called on the error path, so the last-error thread-local is read
/// only when it holds something relevant.
fn sniff_outcome(code: i32, detail: impl FnOnce() -> String) -> Result<&'static str, String> {
    if code < 0 {
        let detail = detail();
        Err(if detail.is_empty() {
            format!("sniff returned {code} with no detail recorded")
        } else {
            detail
        })
    } else {
        Ok(fs_kind_label(code))
    }
}

/// Render the `,"<key>":"<reason>"` fragment that accompanies a
/// `SNIFF_FAILED_LABEL`, or nothing at all when the sniff succeeded.
/// The four keys every blk.probe document opens with, in the order the
/// JSON contract in this module's documentation publishes them.
///
/// There were two writers, each with its own format string, sharing
/// `path`, `container`, `container_size_bytes` and `table`. Nothing made
/// them agree — so renaming a key, or reordering one, was a change a
/// reader had to remember to make twice, in a document a consumer
/// parses.
///
/// `body` is whatever the caller appends, and must begin with a comma.
fn json_envelope(
    path: &str,
    container: Container,
    size_bytes: u64,
    table: &str,
    body: &str,
) -> String {
    format!(
        "{{\"path\":\"{}\",\"container\":\"{}\",\"container_size_bytes\":{},\"table\":\"{}\"{}}}",
        json_escape(path),
        container.label(),
        size_bytes,
        table,
        body,
    )
}

fn sniff_error_field(key: &str, detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!(",\"{}\":\"{}\"", key, json_escape(detail)),
        None => String::new(),
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn fmt_guid(b: &[u8; 16]) -> String {
    // Standard GPT GUID byte layout: first 4 bytes little-endian, next 2 LE,
    // next 2 LE, last 8 bytes big-endian.
    let d1 = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let d2 = u16::from_le_bytes([b[4], b[5]]);
    let d3 = u16::from_le_bytes([b[6], b[7]]);
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        d1, d2, d3, b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

// ---------------------------------------------------------------------------
// The probe, as a library
//
// EVERYTHING ABOVE THIS LINE USED TO BE IN `main.rs`, unreachable from
// anything but the binary's own `main`. That is what made this crate the one
// component of the family with no fuzz target: a `#[cfg(test)]` module inside
// a `[[bin]]` can call these helpers, and nothing else can -- not a fuzz
// target, not an integration test, not an oracle comparison against `blkid`.
//
// This crate is also the FIRST thing to touch an untrusted device. What it
// decides a disk is determines which parser gets handed the bytes next, and
// it is the component most likely to be pointed at something that is not a
// disk image at all. A component in that position with no way to be called
// over arbitrary bytes is the wrong way round (#16).
// ---------------------------------------------------------------------------

/// Why a probe could not produce a document.
///
/// The two variants are the two non-zero exit codes the CLI publishes, and
/// they carry the message the CLI prints. Keeping the text here rather than
/// at the call site is what lets `probe_path` and `probe_bytes` fail
/// identically while only one of them has a process to exit from.
#[derive(Debug, PartialEq, Eq)]
pub enum ProbeError {
    /// The file, or the container layer over it, would not open. Exit 2.
    Open(String),
    /// A partition table is there and could not be read. Exit 3, and
    /// nothing is written to stdout.
    Probe(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Open(m) | ProbeError::Probe(m) => f.write_str(m),
        }
    }
}

/// A finished probe: the JSON document, and anything that went wrong
/// without invalidating it.
///
/// A failed *filesystem sniff* is reported in-band -- the partition gets
/// `"fs_kind":"error"` and the reason -- and is ALSO warned about on stderr,
/// because a document that says `"error"` in one field is easy to miss. The
/// warnings are returned rather than printed so that a caller which is not a
/// command-line program has somewhere to put them.
#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    /// The JSON document, exactly as the CLI writes it to stdout.
    pub json: String,
    /// Lines the CLI writes to stderr, in order, before the document.
    pub warnings: Vec<String>,
}

/// Identify the container of an image held in memory.
///
/// The byte-slice twin of [`auto_detect_container`], reading the same two
/// positions -- the first sixteen bytes, and the eight bytes at
/// `len - 512` -- in the same order, for the same reason: a fixed VHD is a
/// raw image with a footer glued on, so the footer is the only thing telling
/// them apart and it must be tried after every offset-0 magic and before the
/// raw fallback.
///
/// The two are kept in step by `the_two_detectors_agree_on_every_shape`,
/// which builds an image, writes it to a file, and requires the same answer
/// from both. A second reader of the same format that nothing compares to the
/// first is how a probe comes to report one thing from a file and another
/// from a buffer.
pub fn detect_container_in(image: &[u8]) -> Container {
    for (sig, kind) in [
        (magic::QCOW2, Container::Qcow2),
        (magic::VHDX, Container::Vhdx),
        (magic::VMDK, Container::Vmdk),
        (magic::VHD, Container::Vhd),
    ] {
        if image.len() >= sig.len() && &image[..sig.len()] == sig {
            return kind;
        }
    }
    let len = image.len() as u64;
    if len >= VHD_FOOTER_SIZE {
        let at = image.len() - VHD_FOOTER_SIZE as usize;
        if &image[at..at + magic::VHD.len()] == magic::VHD {
            return Container::Vhd;
        }
    }
    Container::Raw
}

/// The bytes a [`memory_device`] hands out, and the only thing its callback
/// dereferences.
struct MemoryImage<'a> {
    bytes: &'a [u8],
}

/// Serve `len` bytes at `offset` out of the image behind `ctx`.
///
/// A read that runs off the end is an ERROR, not a zero-fill. That is what a
/// real device does -- `fs_core`'s own byte-buffer devices answer `ShortRead`
/// before copying anything -- and a crafted table that asks for sector
/// 2^40 must be refused rather than handed a block of zeroes it can then be
/// told means something.
///
/// # Safety
///
/// `ctx` must be a `*mut MemoryImage` that outlives the device, and `buf`
/// must be valid for `len` bytes. Both hold for the only caller, which
/// builds the context on its own stack and closes the device before
/// returning.
unsafe extern "C" fn memory_read(
    ctx: *mut std::ffi::c_void,
    offset: u64,
    buf: *mut u8,
    len: usize,
) -> std::ffi::c_int {
    if ctx.is_null() || buf.is_null() {
        return 1;
    }
    let image = unsafe { &*(ctx as *const MemoryImage) };
    let Ok(start) = usize::try_from(offset) else {
        return 1;
    };
    let Some(end) = start.checked_add(len) else {
        return 1;
    };
    if end > image.bytes.len() {
        return 1;
    }
    unsafe { std::ptr::copy_nonoverlapping(image.bytes.as_ptr().add(start), buf, len) };
    0
}

/// An `FsCoreDevice` over bytes already in memory, with no file behind it.
///
/// # Safety
///
/// The returned handle borrows `image` through a raw pointer that fs-core
/// hands back to [`memory_read`]. The caller must close the handle -- or the
/// container device stacked on it -- before `image` is dropped or moved.
unsafe fn memory_device(image: &MemoryImage<'_>) -> *mut FsCoreDevice {
    let cfg = FsCoreCallbackCfg {
        read: Some(memory_read),
        // NULL, so the device is READ-ONLY. The probe never writes, and a
        // writable device would let a container reader decide it may repair
        // the image it was handed.
        write: None,
        flush: None,
        ctx: image as *const MemoryImage as *mut std::ffi::c_void,
        size: image.bytes.len() as u64,
    };
    unsafe { fs_core_device_from_callbacks(&cfg) }
}

/// Probe an image held in memory and render the same JSON the CLI writes.
///
/// This is the entry point both fuzzing tiers call. It is the WHOLE probe --
/// container detection, the container reader stacked over the bytes, the
/// partition walk and the per-partition sniff -- because that is the sequence
/// a hostile image meets, and each step decides what the next one is handed.
///
/// `path` appears in the document's `"path"` field and is not opened.
///
/// Every failure is a value. A crafted image is *supposed* to be refused;
/// what it may not do is panic, hang, or read memory that is not the image.
pub fn probe_bytes(
    image: &[u8],
    explicit: Option<Container>,
    path: &str,
) -> Result<Report, ProbeError> {
    let container = explicit.unwrap_or_else(|| detect_container_in(image));
    let backing = MemoryImage { bytes: image };
    // SAFETY: `backing` outlives every use of the handle below -- the device
    // is closed on every path out of this function before `backing` is
    // dropped at the end of the scope.
    let inner = unsafe { memory_device(&backing) };
    if inner.is_null() {
        return Err(ProbeError::Open(format!(
            "fs_core_device_from_callbacks: {}",
            last_error()
        )));
    }
    let dev = unsafe { open_container_on(inner, container, false) };
    if dev.is_null() {
        // `inner` IS NOT CLOSED HERE, and that is not an oversight: the
        // reader consumed the handle before it failed. See
        // `open_container_on`.
        return Err(ProbeError::Open(format!(
            "{}_open_on_device: {}",
            container.label(),
            last_error()
        )));
    }
    let report = unsafe { probe_device(dev, path, container) };
    // The container device holds the inner one alive, and for `Raw` they are
    // the same handle -- so this is one close either way, which is what the
    // CLI does with a file underneath.
    unsafe { fs_core_device_close(dev) };
    report
}

/// Open `path`, stack the container reader over it and probe it.
///
/// The CLI's whole body, minus argument parsing and the exit codes. The
/// container is auto-detected when `explicit` is `None`.
pub fn probe_path(path: &str, explicit: Option<Container>) -> Result<Report, ProbeError> {
    let container = match explicit {
        Some(c) => c,
        None => auto_detect_container(path)
            .map_err(|e| ProbeError::Open(format!("auto-detect: {e}")))?,
    };

    let cpath =
        CString::new(path).map_err(|e| ProbeError::Open(format!("path is not a C string: {e}")))?;
    // Always opened READ-ONLY. A partition probe reads, and taking a write
    // lock to inspect a device is a side effect nobody asked for.
    let file = unsafe { fs_core_file_open(cpath.as_ptr(), false) };
    if file.is_null() {
        return Err(ProbeError::Open(format!(
            "fs_core_file_open: {}",
            last_error()
        )));
    }
    let dev = unsafe { open_container_on(file, container, false) };
    if dev.is_null() {
        // `file` IS NOT CLOSED HERE. The reader consumed the handle before it
        // failed -- see `open_container_on`.
        return Err(ProbeError::Open(format!(
            "{}_open_on_device: {}",
            container.label(),
            last_error()
        )));
    }
    let report = unsafe { probe_device(dev, path, container) };
    unsafe { fs_core_device_close(dev) };
    report
}

/// Walk an already-open device and render the document.
///
/// # Safety
///
/// `dev` must be a live `FsCoreDevice` handle. It is NOT closed here: the
/// caller opened it and the caller closes it, on the failure path as well as
/// the success one.
pub unsafe fn probe_device(
    dev: *mut FsCoreDevice,
    path: &str,
    container: Container,
) -> Result<Report, ProbeError> {
    let dev_size = unsafe { fs_core_device_size_bytes(dev) };
    let mut warnings: Vec<String> = Vec::new();

    let mut list: *mut PartitionList = ptr::null_mut();
    let rc = unsafe { partitions_probe(dev, &mut list) };
    match classify_probe(rc, list.is_null(), &last_error()) {
        ProbeOutcome::Failed(detail) => {
            return Err(ProbeError::Probe(format!("partitions_probe: {detail}")));
        }
        ProbeOutcome::NoTable => {
            // No partition table -- a whole-device filesystem. That is an
            // ordinary disk and an ordinary answer, not a failure.
            let sniffed = unsafe { partitions_sniff_device(dev, dev_size) };
            let sniffed = sniff_outcome(sniffed, last_error);
            let (dev_fs_label, dev_fs_error) = match sniffed {
                Ok(label) => (label, None),
                Err(detail) => {
                    warnings.push(format!("whole-device filesystem sniff failed: {detail}"));
                    (SNIFF_FAILED_LABEL, Some(detail))
                }
            };
            let body = format!(
                ",\"device_fs_kind\":\"{}\"{},\"partitions\":[]",
                dev_fs_label,
                sniff_error_field("device_fs_error", dev_fs_error.as_deref()),
            );
            return Ok(Report {
                json: json_envelope(path, container, dev_size, "none", &body),
                warnings,
            });
        }
        ProbeOutcome::Table => {}
    }

    let table_rc = unsafe { partitions_table_kind(list) };
    let count = unsafe { partitions_count(list) };

    let mut entries: Vec<String> = Vec::with_capacity(count);
    for i in 0..count {
        // ZEROED, NOT A STRUCT LITERAL. `PartitionInfo` is a `repr(C)`
        // out-parameter that `partitions_get` overwrites in full, so its
        // starting value carries no meaning -- but a literal has to name
        // every field, and `rust-disk-partitions` adds fields between releases
        // (0.5.0 added `slot`, `issues` and `available_length`). A literal
        // failed to compile against whichever side it was not written for
        // (#14), and would again at the next field. All-zero is a valid
        // value for every field: integers, byte arrays, and a null `label`.
        let mut info: PartitionInfo = unsafe { std::mem::zeroed() };
        let grc = unsafe { partitions_get(list, i, &mut info) };
        if grc != FsCoreErrorCode::Ok {
            continue;
        }
        // `partitions_sniff` fills the kind in place, but `info` is already a
        // copy, so the returned code is what is read.
        let sniffed = sniff_outcome(unsafe { partitions_sniff(list, i) }, last_error);
        let (fs_label, fs_error) = match sniffed {
            Ok(label) => (label, None),
            Err(detail) => {
                warnings.push(format!("partition {i}: filesystem sniff failed: {detail}"));
                (SNIFF_FAILED_LABEL, Some(detail))
            }
        };
        let label_str = if !info.label.is_null() && info.label_len > 0 {
            // `.cast()`, NOT `as *const u8`. `c_char` is signed on
            // x86_64-linux and UNSIGNED on aarch64-linux, so the `as` was a
            // real cast on one target and a no-op on the other -- and clippy
            // refuses a no-op cast, which made `clippy -D warnings` fail on
            // aarch64 while both CI targets passed (#15). `.cast()` is the
            // same conversion and is correct on either.
            let bytes =
                unsafe { std::slice::from_raw_parts(info.label.cast::<u8>(), info.label_len) };
            std::str::from_utf8(bytes).ok().map(|s| s.to_string())
        } else {
            None
        };

        let mut entry = format!("{{\"index\":{i}");
        // `slot` is -1 for an entry with no slot in an on-disk table, and is
        // then left out, as `label` is when there is none. It is not `i`: a
        // table with a hole in it is routine (#28).
        if info.slot >= 0 {
            entry.push_str(&format!(",\"slot\":{}", info.slot));
        }
        // `length` is what the table claims; `available_length` is how much
        // of it the device holds, and is what a buffer is sized with. The two
        // differ on an image that stops before its table does (#28).
        entry.push_str(&format!(
            ",\"start\":{},\"length\":{},\"available_length\":{},\"issues\":{},\"fs_kind\":\"{}\",\"type_byte\":{},\"type_guid\":\"{}\"",
            info.start,
            info.length,
            info.available_length,
            info.issues,
            fs_label,
            info.type_byte,
            fmt_guid(&info.type_guid),
        ));
        entry.push_str(&sniff_error_field("fs_kind_error", fs_error.as_deref()));
        if let Some(l) = label_str {
            entry.push_str(&format!(",\"label\":\"{}\"", json_escape(&l)));
        }
        entry.push('}');
        entries.push(entry);
    }

    unsafe { partitions_list_free(list) };

    let body = format!(",\"partitions\":[{}]", entries.join(","));
    Ok(Report {
        json: json_envelope(path, container, dev_size, table_kind_label(table_rc), &body),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // Test support
    // ---------------------------------------------------------------

    /// A disk image on disk, removed when the test that made it ends.
    ///
    /// `auto_detect_container` takes a path rather than a reader, so the
    /// only way to exercise it is through a real file. Each one is named
    /// after the test that created it so a crashed run leaves an
    /// identifiable corpse rather than a collision.
    struct TempImage(std::path::PathBuf);

    impl TempImage {
        fn new(name: &str, bytes: &[u8]) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!("blk.probe-test-{}-{name}", std::process::id()));
            std::fs::write(&path, bytes).expect("write temp image");
            TempImage(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().expect("temp path is utf-8")
        }
    }

    impl Drop for TempImage {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A 1 KiB image with `head` at offset 0 and, optionally, `footer` at
    /// `len - 512` — the two positions `auto_detect_container` reads.
    fn image_with(head: &[u8], footer: Option<&[u8]>) -> Vec<u8> {
        let mut bytes = vec![0u8; 1024];
        bytes[..head.len()].copy_from_slice(head);
        if let Some(footer) = footer {
            let at = bytes.len() - 512;
            bytes[at..at + footer.len()].copy_from_slice(footer);
        }
        bytes
    }

    /// A reader that hands back at most `chunk` bytes per call, the way
    /// a pipe, a network filesystem or a signal-interrupted read does.
    ///
    /// A local regular file usually fills a 16-byte buffer in one call,
    /// which is exactly why the old single-`read` form survived: it was
    /// right on the machine anybody tested it on.
    struct Dribbling<'a> {
        data: &'a [u8],
        chunk: usize,
        /// Number of `read` calls that return `Interrupted` before any
        /// data is handed over.
        interruptions: usize,
    }

    impl std::io::Read for Dribbling<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.interruptions > 0 {
                self.interruptions -= 1;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "signal",
                ));
            }
            let n = self.chunk.min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    /// A reader that fails partway through, to prove the error is not
    /// swallowed.
    struct FailsAfter(usize);

    impl std::io::Read for FailsAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::other("device went away"));
            }
            let n = self.0.min(buf.len());
            buf[..n].fill(0xAB);
            self.0 = 0;
            Ok(n)
        }
    }

    // ---------------------------------------------------------------
    // read_up_to
    // ---------------------------------------------------------------

    /// The whole point: a source that dribbles still fills the buffer.
    ///
    /// With a single `read`, `n` would be 1 here — so the `n >= 8`
    /// guards go false and a vhdx image is reported as raw.
    #[test]
    fn a_dribbling_source_still_fills_the_buffer() {
        let data = b"vhdxfile________";
        let mut r = Dribbling {
            data,
            chunk: 1,
            interruptions: 0,
        };
        let mut buf = [0u8; 16];
        assert_eq!(read_up_to(&mut r, &mut buf).unwrap(), 16);
        assert_eq!(&buf, data);
    }

    /// A file shorter than the buffer is a legitimate raw image, so it
    /// reports its length rather than failing.
    #[test]
    fn a_short_source_reports_its_length_rather_than_failing() {
        let data = b"KDMV";
        let mut r = Dribbling {
            data,
            chunk: 16,
            interruptions: 0,
        };
        let mut buf = [0u8; 16];
        assert_eq!(read_up_to(&mut r, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], data);
        assert_eq!(&buf[4..], &[0u8; 12], "the tail is left untouched");
    }

    /// `Interrupted` means a signal arrived, not that anything is wrong.
    #[test]
    fn an_interrupted_read_is_retried_rather_than_reported() {
        let data = b"conectix________";
        let mut r = Dribbling {
            data,
            chunk: 4,
            interruptions: 3,
        };
        let mut buf = [0u8; 16];
        assert_eq!(read_up_to(&mut r, &mut buf).unwrap(), 16);
        assert_eq!(&buf, data);
    }

    /// Every other error propagates. The old `unwrap_or(0)` reported the
    /// same 0 an empty file gives, so an unreadable device and an empty
    /// one could not be told apart.
    #[test]
    fn a_real_error_propagates_instead_of_reading_as_end_of_input() {
        let mut r = FailsAfter(4);
        let mut buf = [0u8; 16];
        let err = read_up_to(&mut r, &mut buf).expect_err("the error must not be swallowed");
        assert_eq!(err.to_string(), "device went away");
    }

    // ---------------------------------------------------------------
    // magics, sizes and the JSON envelope
    // ---------------------------------------------------------------

    /// The four signatures are the bytes the formats define.
    ///
    /// Asserted against literal bytes, because the point of naming them
    /// was that three were readable ASCII and the fourth was four loose
    /// hex values a reader could not check by eye.
    #[test]
    fn the_container_magics_are_the_formats_own_bytes() {
        assert_eq!(magic::QCOW2, &[0x51, 0x46, 0x49, 0xfb]);
        assert_eq!(magic::VHDX, b"vhdxfile");
        assert_eq!(magic::VMDK, b"KDMV");
        assert_eq!(magic::VHD, b"conectix");
    }

    /// No signature is a prefix of another.
    ///
    /// The probe returns the first match, so a magic that prefixed
    /// another would shadow it — and the arms are ordered by nothing in
    /// particular now that they are a list.
    #[test]
    fn no_container_magic_shadows_another() {
        let all = [magic::QCOW2, magic::VHDX, magic::VMDK, magic::VHD];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                let n = a.len().min(b.len());
                assert_ne!(&a[..n], &b[..n], "{a:?} and {b:?} share a prefix");
            }
        }
    }

    /// A VHD footer is 512 bytes because the format says so — not
    /// because a sector is.
    #[test]
    fn the_vhd_footer_size_is_the_formats_and_not_a_sector() {
        assert_eq!(VHD_FOOTER_SIZE, 512);
    }

    /// The envelope carries the four keys the contract publishes, in
    /// order, and escapes the path.
    #[test]
    fn the_json_envelope_opens_with_the_contracted_keys() {
        let out = json_envelope("/a\"b", Container::Qcow2, 4096, "gpt", ",\"partitions\":[]");
        assert_eq!(
            out,
            "{\"path\":\"/a\\\"b\",\"container\":\"qcow2\",\"container_size_bytes\":4096,\"table\":\"gpt\",\"partitions\":[]}"
        );
    }

    /// Both callers produce the same opening, which is what having one
    /// writer is for.
    #[test]
    fn both_documents_share_an_opening() {
        let no_table = json_envelope("/x", Container::Raw, 1, "none", ",\"partitions\":[]");
        let with_table = json_envelope("/x", Container::Raw, 1, "mbr", ",\"partitions\":[]");
        let prefix =
            "{\"path\":\"/x\",\"container\":\"raw\",\"container_size_bytes\":1,\"table\":\"";
        assert!(no_table.starts_with(prefix), "{no_table}");
        assert!(with_table.starts_with(prefix), "{with_table}");
    }

    // ---------------------------------------------------------------
    // Container::label / Container::parse
    // ---------------------------------------------------------------

    #[test]
    fn container_label_round_trips_through_parse() {
        for kind in [
            Container::Raw,
            Container::Qcow2,
            Container::Vhd,
            Container::Vhdx,
            Container::Vmdk,
        ] {
            assert_eq!(
                Container::parse(kind.label()),
                Some(kind),
                "{} did not round-trip",
                kind.label()
            );
        }
    }

    #[test]
    fn container_labels_are_the_strings_the_json_contract_publishes() {
        assert_eq!(Container::Raw.label(), "raw");
        assert_eq!(Container::Qcow2.label(), "qcow2");
        assert_eq!(Container::Vhd.label(), "vhd");
        assert_eq!(Container::Vhdx.label(), "vhdx");
        assert_eq!(Container::Vmdk.label(), "vmdk");
    }

    #[test]
    fn container_parse_rejects_names_it_does_not_know() {
        assert_eq!(Container::parse(""), None);
        assert_eq!(Container::parse("iso"), None);
        assert_eq!(
            Container::parse("QCOW2"),
            None,
            "matching is case-sensitive"
        );
        assert_eq!(Container::parse("qcow"), None);
    }

    // ---------------------------------------------------------------
    // fs_kind_label / table_kind_label
    // ---------------------------------------------------------------

    #[test]
    fn fs_kind_label_names_every_discriminant_it_mirrors() {
        let expected = [
            (FsKindCode::Unknown, "unknown"),
            (FsKindCode::Ext2, "ext2"),
            (FsKindCode::Ext3, "ext3"),
            (FsKindCode::Ext4, "ext4"),
            (FsKindCode::Ntfs, "ntfs"),
            (FsKindCode::ExFat, "exfat"),
            (FsKindCode::Fat32, "fat32"),
            (FsKindCode::Fat16, "fat16"),
            (FsKindCode::HfsPlus, "hfs_plus"),
            (FsKindCode::Apfs, "apfs"),
            (FsKindCode::LinuxSwap, "linux_swap"),
            (FsKindCode::Iso9660, "iso9660"),
            (FsKindCode::Squashfs, "squashfs"),
            (FsKindCode::Bcachefs, "bcachefs"),
            (FsKindCode::Xfs, "xfs"),
            (FsKindCode::Btrfs, "btrfs"),
            (FsKindCode::Erofs, "erofs"),
        ];
        for (code, label) in expected {
            assert_eq!(fs_kind_label(code as i32), label, "for {code:?}");
        }
    }

    #[test]
    fn fs_kind_label_cannot_tell_the_error_sentinel_from_an_unmapped_code() {
        // This is the overlap that made `if sniffed >= 0` inert at the
        // sniff call sites: -1 is the documented error sentinel and it
        // lands on the same catch-all arm as a code this file has never
        // heard of. Nothing here is wrong — but it is why the sentinel
        // has to be caught before the label lookup, not after it.
        assert_eq!(fs_kind_label(-1), "unknown");
        assert_eq!(fs_kind_label(FsKindCode::Unknown as i32), "unknown");
        assert_eq!(fs_kind_label(FsKindCode::Erofs as i32 + 1), "unknown");
        assert_eq!(fs_kind_label(i32::MIN), "unknown");
        assert_eq!(fs_kind_label(i32::MAX), "unknown");
    }

    #[test]
    fn table_kind_label_names_the_two_tables_and_calls_everything_else_none() {
        assert_eq!(table_kind_label(TableKindCode::Gpt as i32), "gpt");
        assert_eq!(table_kind_label(TableKindCode::Mbr as i32), "mbr");
        assert_eq!(table_kind_label(TableKindCode::None as i32), "none");
        assert_eq!(table_kind_label(-1), "none");
        assert_eq!(table_kind_label(99), "none");
    }

    // ---------------------------------------------------------------
    // json_escape
    // ---------------------------------------------------------------

    #[test]
    fn json_escape_leaves_ordinary_text_untouched() {
        assert_eq!(json_escape(""), "");
        assert_eq!(json_escape("boot"), "boot");
        assert_eq!(
            json_escape("/Volumes/My Disk/image.img"),
            "/Volumes/My Disk/image.img"
        );
    }

    #[test]
    fn json_escape_escapes_the_characters_json_requires() {
        assert_eq!(json_escape("say \"hi\""), "say \\\"hi\\\"");
        assert_eq!(json_escape("C:\\disks"), "C:\\\\disks");
        assert_eq!(json_escape("a\nb"), "a\\nb");
        assert_eq!(json_escape("a\rb"), "a\\rb");
        assert_eq!(json_escape("a\tb"), "a\\tb");
    }

    #[test]
    fn json_escape_renders_other_control_characters_as_u_sequences() {
        assert_eq!(json_escape("\u{0}"), "\\u0000");
        assert_eq!(json_escape("\u{8}"), "\\u0008");
        assert_eq!(json_escape("\u{b}"), "\\u000b");
        assert_eq!(json_escape("\u{1f}"), "\\u001f");
    }

    #[test]
    fn json_escape_passes_through_del_and_non_ascii() {
        // 0x7f and above need no escaping in JSON, and a partition label
        // is free to contain either.
        assert_eq!(json_escape("\u{7f}"), "\u{7f}");
        assert_eq!(json_escape("Système"), "Système");
        assert_eq!(json_escape("ボリューム"), "ボリューム");
    }

    // ---------------------------------------------------------------
    // fmt_guid
    // ---------------------------------------------------------------

    #[test]
    fn fmt_guid_renders_a_known_gpt_type_guid() {
        // The Linux filesystem-data type GUID, as it is written in the
        // GPT spec and as those bytes actually sit on disk.
        let on_disk = [
            0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47,
            0x7d, 0xe4,
        ];
        assert_eq!(fmt_guid(&on_disk), "0fc63daf-8483-4772-8e79-3d69d8477de4");
    }

    #[test]
    fn fmt_guid_byte_swaps_the_first_three_fields_and_not_the_last_two() {
        // Ascending bytes make the mixed-endian layout legible: the first
        // three groups read backwards, the last two read forwards.
        let ascending: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        assert_eq!(fmt_guid(&ascending), "03020100-0504-0706-0809-0a0b0c0d0e0f");
    }

    #[test]
    fn fmt_guid_zero_pads_every_field() {
        assert_eq!(fmt_guid(&[0u8; 16]), "00000000-0000-0000-0000-000000000000");
        let mut sparse = [0u8; 16];
        sparse[0] = 0x01;
        sparse[15] = 0x02;
        assert_eq!(fmt_guid(&sparse), "00000001-0000-0000-0000-000000000002");
    }

    // ---------------------------------------------------------------
    // auto_detect_container
    // ---------------------------------------------------------------

    #[test]
    fn auto_detect_recognises_each_magic_at_offset_zero() {
        let cases: [(&str, &[u8], Container); 4] = [
            ("qcow2", b"QFI\xfb", Container::Qcow2),
            ("vhdx", b"vhdxfile", Container::Vhdx),
            ("vmdk", b"KDMV", Container::Vmdk),
            ("vhd-header", b"conectix", Container::Vhd),
        ];
        for (name, magic, expected) in cases {
            let image = TempImage::new(name, &image_with(magic, None));
            assert_eq!(
                auto_detect_container(image.path()).unwrap(),
                expected,
                "{name} magic was not recognised"
            );
        }
    }

    #[test]
    fn auto_detect_recognises_a_fixed_vhd_by_its_trailing_footer() {
        // A fixed VHD is a raw image with a 512-byte footer glued on the
        // end; offset 0 is ordinary partition-table data, so the footer
        // is the only thing that distinguishes it.
        let image = TempImage::new("fixed-vhd", &image_with(&[0x00; 4], Some(b"conectix")));
        assert_eq!(auto_detect_container(image.path()).unwrap(), Container::Vhd);
    }

    #[test]
    fn auto_detect_prefers_a_magic_at_offset_zero_over_a_trailing_footer() {
        // The footer check runs last among the signatures on purpose:
        // eight bytes at a position that is payload in every other format
        // is a far weaker signal than an offset-0 magic, so any offset-0
        // magic has to win. This pins that ordering.
        let image = TempImage::new(
            "qcow2-with-footer",
            &image_with(b"QFI\xfb", Some(b"conectix")),
        );
        assert_eq!(
            auto_detect_container(image.path()).unwrap(),
            Container::Qcow2
        );
    }

    #[test]
    fn auto_detect_falls_back_to_raw_when_nothing_matches() {
        let image = TempImage::new("raw", &image_with(&[0x55; 8], None));
        assert_eq!(auto_detect_container(image.path()).unwrap(), Container::Raw);
    }

    #[test]
    fn auto_detect_falls_back_to_raw_for_files_too_short_to_hold_a_signature() {
        let empty = TempImage::new("empty", &[]);
        assert_eq!(auto_detect_container(empty.path()).unwrap(), Container::Raw);

        // Four bytes: shorter than the footer, and shorter than every
        // magic but qcow2's and vmdk's.
        let tiny = TempImage::new("tiny", &[0x00, 0x01, 0x02, 0x03]);
        assert_eq!(auto_detect_container(tiny.path()).unwrap(), Container::Raw);
    }

    // ---------------------------------------------------------------
    // classify_probe — the fork that used to report failure as success
    // ---------------------------------------------------------------

    #[test]
    fn classify_probe_reports_a_table_when_the_probe_succeeded() {
        assert_eq!(
            classify_probe(FsCoreErrorCode::Ok, false, ""),
            ProbeOutcome::Table
        );
    }

    #[test]
    fn classify_probe_ignores_stale_last_error_text_when_the_probe_succeeded() {
        // The last-error thread-local is not cleared on success, so a
        // message left by an earlier call must not be allowed to turn a
        // perfectly good partition table into "no table".
        assert_eq!(
            classify_probe(FsCoreErrorCode::Ok, false, NO_PARTITION_TABLE_MESSAGE),
            ProbeOutcome::Table
        );
    }

    #[test]
    fn classify_probe_calls_a_missing_signature_no_table_rather_than_a_failure() {
        // A whole-device filesystem is a normal disk. This is the one
        // non-Ok return that must still exit 0.
        assert_eq!(
            classify_probe(FsCoreErrorCode::Custom, true, NO_PARTITION_TABLE_MESSAGE),
            ProbeOutcome::NoTable
        );
        assert_eq!(
            classify_probe(
                FsCoreErrorCode::Custom,
                true,
                "  no GPT or MBR signature found\n"
            ),
            ProbeOutcome::NoTable,
            "surrounding whitespace should not change the verdict"
        );
    }

    #[test]
    fn classify_probe_keeps_a_real_failure_distinct_from_an_absent_table() {
        // Every one of these used to end up in the same branch as the
        // case above: "table":"none", empty partitions, exit 0. A caller
        // could not tell a corrupt table from a disk that has none.
        for message in [
            "GPT header CRC32 mismatch",
            "GPT partition-entry array CRC32 mismatch",
            "GPT corrupt: protective MBR present but no GPT signature",
            "MBR corrupt: extended chain loops",
            "read failed at offset 0",
        ] {
            assert_eq!(
                classify_probe(FsCoreErrorCode::Custom, true, message),
                ProbeOutcome::Failed(message.to_string()),
                "{message} should be reported as a failure"
            );
        }
    }

    #[test]
    fn classify_probe_still_reports_a_failure_when_no_detail_was_recorded() {
        let outcome = classify_probe(FsCoreErrorCode::Io, true, "");
        match outcome {
            ProbeOutcome::Failed(detail) => {
                assert!(
                    detail.contains("Io"),
                    "detail should name the code: {detail}"
                );
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn classify_probe_treats_success_with_a_null_list_as_a_broken_promise() {
        // The ABI never does this. If it ever did, the honest answer is
        // "something is wrong", not "this disk has no partitions".
        match classify_probe(FsCoreErrorCode::Ok, true, "") {
            ProbeOutcome::Failed(detail) => assert!(detail.contains("NULL")),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // sniff_outcome — the sentinel that used to read as "unknown"
    // ---------------------------------------------------------------

    #[test]
    fn sniff_outcome_labels_a_recognised_filesystem() {
        assert_eq!(
            sniff_outcome(FsKindCode::Ext4 as i32, || unreachable!()),
            Ok("ext4")
        );
        assert_eq!(
            sniff_outcome(FsKindCode::Squashfs as i32, || unreachable!()),
            Ok("squashfs")
        );
    }

    #[test]
    fn sniff_outcome_reports_a_successful_no_match_as_unknown() {
        // Code 0 means the sniff ran to completion and matched nothing.
        // The detail closure must not be called: reading the last-error
        // thread-local here would pick up an unrelated stale message.
        let asked = std::cell::Cell::new(false);
        let outcome = sniff_outcome(FsKindCode::Unknown as i32, || {
            asked.set(true);
            String::new()
        });
        assert_eq!(outcome, Ok("unknown"));
        assert!(
            !asked.get(),
            "the error detail was fetched on the success path"
        );
    }

    #[test]
    fn sniff_outcome_separates_the_error_sentinel_from_a_successful_no_match() {
        // The whole point. Both of these used to produce "unknown".
        let failed = sniff_outcome(-1, || "read failed at offset 1048576".to_string());
        assert_eq!(failed, Err("read failed at offset 1048576".to_string()));
        assert_ne!(failed, Ok("unknown"));
        assert_eq!(
            sniff_outcome(FsKindCode::Unknown as i32, String::new),
            Ok("unknown")
        );
    }

    #[test]
    fn sniff_outcome_substitutes_a_reason_when_none_was_recorded() {
        match sniff_outcome(-1, String::new) {
            Err(detail) => assert!(
                detail.contains("-1"),
                "detail should name the code: {detail}"
            ),
            Ok(label) => panic!("expected a failure, got {label}"),
        }
    }

    // ---------------------------------------------------------------
    // sniff_error_field
    // ---------------------------------------------------------------

    #[test]
    fn sniff_error_field_adds_nothing_when_the_sniff_succeeded() {
        assert_eq!(sniff_error_field("fs_kind_error", None), "");
    }

    #[test]
    fn sniff_error_field_emits_an_escaped_json_pair() {
        assert_eq!(
            sniff_error_field("fs_kind_error", Some("read \"failed\"")),
            ",\"fs_kind_error\":\"read \\\"failed\\\"\""
        );
        assert_eq!(
            sniff_error_field("device_fs_error", Some("line one\nline two")),
            ",\"device_fs_error\":\"line one\\nline two\""
        );
    }

    #[test]
    fn auto_detect_reports_the_io_error_when_the_file_cannot_be_opened() {
        let mut missing = std::env::temp_dir();
        missing.push("blk.probe-test-no-such-image");
        let _ = std::fs::remove_file(&missing);
        let err = auto_detect_container(missing.to_str().unwrap()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
