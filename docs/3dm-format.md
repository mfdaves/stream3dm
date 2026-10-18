<!-- How the .3dm reader in crates/stream3dm/src/reader/ was written: the object records'
byte layouts, in our own words, with citations of the two references named below. The probe
scripts it mentions were throwaway and are not part of the repository. -->

# Rhino .3dm object records: byte-level decoding guide

Implementation guide for a fast, independent Rust decoder of the object table of Rhino `.3dm`
archives (archive versions 2, 3, 4, 50, 60, 70, 80; notes on 1, 5 and 90 where relevant). The
format is described in our own words from two references:

- **cadmpeg**: `cadmpeg-codec-rhino` 0.6.1 (Apache-2.0, clean-room Rust reader), at
  the `src/` directory of the published crate.
  Citations look like `chunks.rs:563`; paths are relative to that `src/` directory.
- **openNURBS**: McNeel openNURBS, branch 8.x, commit `eb92af3` (v8.35.26251.13001, 2026-09-15).
  Citations name the C++ function and give `file.cpp:line` at that commit.

In sections 3 to 7, citations are prefixed `cad:` or `cad/` (cadmpeg `src/`) and `on:` or `on/`
(openNURBS repository root at that commit), e.g. `cad/mesh.rs:1068`, `on/opennurbs_mesh.cpp:1841`.

Where the two disagree this document says so. The openNURBS writer is the ground truth for what
files contain; cadmpeg is the closest prior Rust implementation and is stricter in several places.

## How to read this document

Notation used in all layout tables:

| Notation | Bytes | Meaning |
|---|---|---|
| `u8`, `i8` | 1 | byte (openNURBS `ReadChar`/`ReadByte`) |
| `bool8` | 1 | openNURBS `ReadBool`: one byte, 0 = false, 1 = true (see 1.2) |
| `i16`/`u16` | 2 | `ReadShort`/`ReadInt16`, little-endian |
| `i32`/`u32` | 4 | `ReadInt`/`ReadInt32`, little-endian |
| `i64`/`u64` | 8 | `ReadInt64`/`ReadBigInt`, little-endian |
| `f32` | 4 | `ReadFloat`, IEEE-754 binary32, little-endian |
| `f64` | 8 | `ReadDouble`, IEEE-754 binary64, little-endian |
| `uuid` | 16 | `ReadUuid`, mixed-endian (see 1.5) |
| `wstr` | 4 + 2n | `ReadString(ON_wString&)`, UTF-16LE with count prefix (see 1.6) |
| `str` | 4 + n | `ReadString(ON_String&)`, 8-bit with count prefix (see 1.6) |
| `arr<T>` | 4 + n·sizeof(T) | `ReadArray(ON_SimpleArray<T>&)`: `i32` count then elements (see 1.7) |
| `cver` | 1 | `Read3dmChunkVersion`: packed `major<<4 | minor` (see 1.8) |
| `vchunk(tc){...}` | 12/16 + body + 4 | chunk with typecode `tc` whose body starts with `i32 major, i32 minor` (`BeginRead3dmChunk(tc,&maj,&min)`, see 1.9) |
| `chunk(tc){...}` | 8/12 + body (+4) | plain chunk (`BeginRead3dmBigChunk`) |
| `object{...}` | variable | an embedded object written by `WriteObject` (class wrapper, see 0.5) |
| `cbuf(n)` | variable | compressed buffer (`ReadCompressedBuffer`, see section 2) |

"Chunk header width" is 8 bytes (u32 typecode + 4-byte value) when archive < 50 and 12 bytes
(u32 typecode + 8-byte value) when archive >= 50. All multi-byte values in the format are
little-endian; openNURBS byte-swaps on big-endian hosts only (`ON_BinaryArchive::ReadInt32`,
`opennurbs_archive.cpp:973`), so files are always little-endian.

## Contents

- **0** File framing recap and corrections
- **1** Primitive encodings inside class data
- **2** Compressed buffers
- **3** Object attributes (`ON_3dmObjectAttributes`)
- **4** Meshes (`ON_Mesh::Read`)
- **5a** Breps (`ON_Brep::Read`): topology and render meshes
- **5b** Curves and surfaces (Read layouts)
- **6** Extrusions (`ON_Extrusion::Read`)
- **7** Block instances (`ON_InstanceRef::Read`)
- **8** Class UUIDs
- **9** Traps and per-version quirks
- **10** Sample coverage summary
- **11** What could not be determined with confidence
- **Appendix A** Verification performed

---

## 0. File framing recap and corrections

The framing summary supplied with the task is correct in outline. The corrections and
precisions below matter for a robust reader.

### 0.1 Header

- 32 bytes: ASCII `"3D Geometry File Format "` (24 bytes) then the archive version as
  right-justified ASCII decimal in 8 bytes (leading spaces, then digits). `5` and `50` are
  different versions. (`Read3dmStartSection`, `opennurbs_archive.cpp:7928`; cadmpeg
  `parse_header`, `chunks.rs:202`, `layout.rs:26`.)
- openNURBS accepts the magic anywhere in the first 32 MiB (files with OLE or other data
  prepended) and remembers the offset (`opennurbs_archive.cpp:7957-7976`); cadmpeg does the same
  (`chunks.rs:203-212`). All 193 public samples start at offset 0.
- Valid archive words: 1, 2, 3, 4, 5 (early V5 betas with 4-byte chunk values), 50, 60, 70, 80
  (and 90 for Rhino 9). Chunk value width is 4 bytes when the word is < 50, else 8
  (`SizeofChunkLength`, `opennurbs_archive.cpp:7092`; cadmpeg `uses_eight_byte_values`,
  `chunks.rs:99`).
- The first chunk after the header must be `TCODE_COMMENTBLOCK` (0x00000001), a long chunk
  holding the start-section comment (bytes, trailing NUL/0x1A trimmed).

### 0.2 Chunk header

```
u32 typecode
value: i64 (archive >= 50) | 4 bytes (archive < 50)
```

- **Short chunk**: `typecode & 0x80000000 != 0`. The value *is* the payload; nothing follows.
- **Long chunk**: the value is the body length in bytes, measured from the first byte after the
  value field to the end of the chunk, **including** the 4-byte trailing CRC when present.
- **Signedness of 4-byte values (archive < 50)**: long-chunk lengths are read as `u32`
  (so lengths up to 4 GiB are legal). Short-chunk values are read as signed `i32`, except for
  the four typecodes treated as unsigned by `ON_IsUnsignedChunkTypecode`
  (`opennurbs_archive.cpp:205`): `TCODE_RGB` 0x80400001, `TCODE_RGBDISPLAY` 0x80400006,
  `TCODE_PROPERTIES_OPENNURBS_VERSION` 0xA0000026, `TCODE_OBJECT_RECORD_TYPE` 0x82000071
  (`ReadChunkValue`, `opennurbs_archive.cpp:7040`).
  *cadmpeg difference*: `chunk_at` (`chunks.rs:583`) uses the list
  `0x80000001 | 0x80000002 | 0xa0000026 | 0x82000071`; the first two entries do not match
  openNURBS's RGB typecodes (0x80400001/0x80400006). Harmless for object records (those codes
  only occur in V1 legacy settings), but do not copy it.
- **Negative long value**: openNURBS treats a long chunk with `value < 0` as not-long
  (`PushBigChunk`, `opennurbs_archive.cpp:7101`, condition `big_value >= 0`); only reachable in
  8-byte archives. cadmpeg does the same (`chunks.rs:591`). Treat it as corruption.
- `TCODE_ENDOFTABLE` = 0xFFFFFFFF is a short chunk with value 0 that terminates every table.
- `TCODE_ENDOFFILE` = 0x00007FFF is a *long* chunk without CRC whose body is the file size
  (u32 if archive < 50, u64 otherwise); its length value is 4 or 8.

### 0.3 CRC rules (precise)

- A long chunk carries a trailing CRC32 (IEEE/zlib polynomial, initial value 0, i.e. what
  `crc32fast::hash` / `zlib.crc32` return) when `typecode & 0x00008000 != 0` and archive >= 2.
  (`PushBigChunk`, `opennurbs_archive.cpp:7120-7166`.) Archive 1 uses a 2-byte CRC16 on some
  legacy typecodes (out of scope here).
- **The CRC covers only the chunk's direct bytes**: bytes written while that chunk was the
  innermost open chunk. Nested chunks (their header, body and their own CRC) are excluded.
  openNURBS disables CRC accumulation while reading/writing chunk headers
  (`BeginRead3dmBigChunk`, `opennurbs_archive.cpp:6319`; `BeginWrite3dmBigChunk`, `:5840`) and
  `UpdateCRC` (`:16452`) only updates the innermost chunk. cadmpeg models this as "container
  checksums exclude complete nested chunks" (`verify_checksum_ranges`, `chunks.rs:700`).
  Verified on samples: every `TCODE_OBJECT_RECORD` (0x20008070) body consists only of nested
  chunks, and its stored CRC is `crc32(b"") = 0x00000000` in all 97 records checked across
  archives 2, 4, 50 and 80.
- Consequence for class data: `TCODE_OPENNURBS_CLASS_DATA` (0x0002FFFC) has the CRC bit, and
  class data may contain nested chunks (anonymous chunks, embedded objects, compressed
  buffers). Its CRC can only be verified by a reader that knows where the nested chunks are,
  i.e. by the class decoder itself. cadmpeg does not verify it in the generic wrapper scan
  (`objects.rs:571-574`). A fast decoder can skip CRC verification entirely; openNURBS itself
  only reports CRC errors and keeps going (`EndRead3dmChunk`, `opennurbs_archive.cpp:6487`).
- openNURBS only checks the CRC when the chunk was read exactly to its end; on a partial read
  it skips to the end without checking (`opennurbs_archive.cpp:6539`, `:6565`).

### 0.4 Skipping unknown or newer data

`EndRead3dmChunk` (`opennurbs_archive.cpp:6487`) always repositions to the declared end of the
chunk. This is the format's forward-compatibility mechanism: newer minor versions append fields
at the end of a chunk (class data, an anonymous chunk, a userdata payload), and older readers
skip them. Rules for our decoder:

1. Bound every parse by the enclosing chunk; never read past it.
2. After decoding the fields known for a (major, minor) version, jump to the chunk end.
3. Unknown chunk typecodes at a level where chunks are enumerated (object record children,
   userdata list, table records) are skipped by length.
4. A major version you do not know means the payload layout is unknown: skip the whole chunk.

### 0.5 The class wrapper (embedded `ON_Object`)

`ON_BinaryArchive::WriteObject`/`Internal_WriteObject` (`opennurbs_archive.cpp:4744`) and
`ReadObjectHelper` (`:5062`) define the wrapper used both at the top of an object record and
for every embedded object inside class data (Brep curve/surface arrays, polycurve segments,
extrusion profiles, cached meshes, ...):

```
chunk 0x00027FFA TCODE_OPENNURBS_CLASS            (long, no CRC)
  chunk 0x0002FFFB TCODE_OPENNURBS_CLASS_UUID     (long, CRC) body = uuid (16) + CRC (4); length value must be 20
  chunk 0x0002FFFC TCODE_OPENNURBS_CLASS_DATA     (long, CRC) body = class-specific Read() payload + CRC
  chunk 0x00027FFD TCODE_OPENNURBS_CLASS_USERDATA (long, no CRC)   0..n times, archive >= 3 only
  chunk 0x80027FFF TCODE_OPENNURBS_CLASS_END      (short, value 0)
```

- **Null object**: when `WriteObject` is given a null pointer the wrapper contains only the
  UUID chunk with the nil UUID; no data, userdata or end chunk follow
  (`opennurbs_archive.cpp:5086-5100`, `:5157-5161`). cadmpeg accepts this form
  (`objects.rs:551-561`).
- **Unknown class UUID**: openNURBS returns code 3 ("newer object") and skips the wrapper
  (`:5190-5198`; `Read3dmObject` `:15803`). Do the same: skip the whole 0x00027FFA chunk.
- The class data body begins directly with the class's own `Read()` payload; the first byte(s)
  are class-specific (most classes start with a `cver` byte, newer ones with an anonymous
  `vchunk`).
- **Class UUID aliases**: `ON_ClassId::ClassId` (`opennurbs_object.cpp:878`) maps legacy UUIDs
  to current classes. Rhino itself writes several of them, so they are common in real files
  (see section 8.2: e.g. 524 of the 625 Breps in the samples use the `TL_Brep` UUID).

### 0.6 Userdata records (class userdata and attribute userdata)

Each `0x00027FFD` chunk (`ReadObjectUserData`, `opennurbs_archive.cpp:5532`;
`WriteObjectUserData`, `:4809`; cadmpeg `parse_userdata`, `objects.rs:609`):

```
cver                                   userdata record version: 1.x or 2.x (writers emit 2.2)
-- version 1.x: header inline --
  uuid  userdata class id
  uuid  item id (m_userdata_uuid; e.g. the user-string list id)
  i32   copy count
  f64[16] xform
-- version 2.x: header in its own chunk --
  chunk 0x0002FFF9 TCODE_OPENNURBS_CLASS_USERDATA_HEADER (long, CRC) {
    uuid  userdata class id
    uuid  item id
    i32   copy count
    f64[16] xform
    uuid  application id                       (minor >= 1)
    bool8 last saved as goo                    (minor >= 2)
    i32   archive version that wrote the payload (minor >= 2)
    u32   openNURBS version that wrote the payload (minor >= 2)
  }
chunk 0x40008000 TCODE_ANONYMOUS_CHUNK (long, CRC) { class-specific userdata Write() payload }
```

- Other major versions are skipped (`:5598-5602`). Unknown chunk typecodes inside the userdata
  list are skipped; the list ends at `TCODE_OPENNURBS_CLASS_END` (`:5560-5580`).
- **Trap (goo)**: userdata saved "as goo" (by an application that did not understand it) keeps
  the chunk-length width of the archive version recorded in the header, not of the file being
  read. A V4-written payload copied into a V5+ file still has 4-byte inner chunk values. openNURBS
  reads such payloads through a memory archive with the recorded version
  (`ReadObjectUserDataAnonymousChunk`, `opennurbs_archive.cpp:5286-5400`). If the header's
  archive version is 0 (pre-2009 writers), openNURBS assumes the current archive version when
  < 50, else 5 (`ReadObjectUserDataHeaderHelper`, `:5498-5525`). For core userdata we need
  (user strings) the writer is openNURBS itself, so this only matters when parsing foreign
  userdata.
- The CRC of the anonymous chunk covers its direct bytes. For unknown (goo) userdata openNURBS
  disables CRC checking because the payload may contain nested chunks (`:5313-5330`), reads the
  chunk body *including* its 4 CRC bytes as an opaque buffer, and on re-save writes that buffer
  back without appending a new CRC (`WriteObjectUserData`, `:4990-5002`). The net effect on disk
  is the same shape (the last 4 bytes are the original CRC), so always skip by the length value
  and never recompute that CRC.

### 0.7 Object record

`Write3dmObject` (`opennurbs_archive.cpp:13198`) and `Read3dmObject` (`:15745`); cadmpeg
`parse_object_record` (`objects.rs:1774`):

```
chunk 0x20008070 TCODE_OBJECT_RECORD (long, CRC; direct body empty so CRC = 0)
  chunk 0x82000071 TCODE_OBJECT_RECORD_TYPE (short) value = ON::object_type (unsigned)
  chunk 0x00027FFA class wrapper (0.5)
  chunk 0x02008072 TCODE_OBJECT_RECORD_ATTRIBUTES (long, CRC)        optional; section 3
  chunk 0x02000073 TCODE_OBJECT_RECORD_ATTRIBUTES_USERDATA (long)    optional, archive >= 4
       { 0x00027FFD userdata records ..., 0x80027FFF class end }
  chunk 0x02008074 TCODE_OBJECT_RECORD_HISTORY (long, CRC)           optional
  chunk 0x8200007F TCODE_OBJECT_RECORD_END (short, value 0)
```

- After the class wrapper, openNURBS loops over chunks until `TCODE_OBJECT_RECORD_END`, decoding
  attributes and attribute userdata and skipping everything else (`:15821-15873`); it does not
  enforce an order. cadmpeg enforces attributes -> attribute userdata -> history -> end and keeps
  unknown trailers (`objects.rs:1844-1915`).
- The object type value can be used to filter records before touching the class wrapper
  (`Read3dmObject` `:15787`: a non-zero type that is not in the filter skips the record).
  Archive-2 "old b-rep" records carry type 0 (seen in 49 sample records), so never trust the
  type alone; dispatch on the class UUID.
- Archive <= 2 writers explode point clouds into individual `ON_Point` records
  (`Write3dmObject`, `:13206-13219`).
- Lights live in the light table (0x10000012) as `TCODE_LIGHT_RECORD` 0x20008060 records:
  class wrapper (`ON_Light`), then optional 0x02008061 attributes, 0x02000062 attribute
  userdata, then short 0x8200006F end; there is no type chunk (`Read3dmLight`, `:13048`).
- Instance definitions live in the instance definition table (0x10000021) as
  `TCODE_INSTANCE_DEFINITION_RECORD` 0x20008076 records holding an `ON_InstanceDefinition`
  class wrapper (section 7).

`ON::object_type` values (`opennurbs_defines.h:2269`): point 0x1, pointset 0x2, curve 0x4,
surface 0x8, brep 0x10, mesh 0x20, light 0x100, annotation 0x200, instance_definition 0x800,
instance_reference 0x1000, text_dot 0x2000, detail 0x8000, hatch 0x10000, morph_control
0x20000, subd 0x40000, cage 0x08000000, phantom 0x10000000, clipplane 0x20000000, extrusion
0x40000000.

### 0.8 Writer version stamp

The openNURBS version that wrote the file is the short chunk
`TCODE_PROPERTIES_OPENNURBS_VERSION` 0xA0000026 inside the properties table
(`ON_3dmProperties::Read`, `opennurbs_3dm_properties.cpp:447`). Two encodings exist:

- Old: decimal `YYYYMMDDn` (< 2^31), e.g. 200107270 (archive 2), 200607130 (archive 4),
  201101285 (archive 50).
- New: `ON_VersionNumberConstruct` (`opennurbs_version_number.cpp:81`) with bit 31 set,
  e.g. 2348834660 (archive 60), 2382393748 (70), 2415950832 (80).

Compare as unsigned 32-bit integers: every old-format stamp sorts below every new one, which is
what openNURBS relies on. Files written before 2000-12-21 have no stamp; openNURBS substitutes
200012210 for archive <= 2 (`opennurbs_archive.cpp:8516-8527`). Several decoders branch on this
stamp (booleans in 1.2, mesh and Brep quirks in sections 4 and 5a).

---

## 1. Primitive encodings inside class data

All primitives are fixed-width little-endian with no alignment or padding anywhere in the
format. No primitive inside class data changes width with the archive version; only chunk
headers (0.2), the EOF file size and goo userdata (0.6) depend on it.

### 1.1 Integers and floats

| openNURBS call | Wire | Notes |
|---|---|---|
| `ReadChar` / `ReadByte` | 1 byte | `opennurbs_archive.cpp:844-893` |
| `ReadShort` / `ReadInt16` | 2 bytes LE | `:895-971` |
| `ReadInt` / `ReadInt32` (`int`, `unsigned int`) | 4 bytes LE | `:973-1053` |
| `ReadLong` (`long`) | **4 bytes** LE even where `long` is 64-bit | `:1088-1160` |
| `ReadInt64` / `ReadBigInt` | 8 bytes LE | `:1055-1083`, `:2985` |
| `ReadSize` (`size_t`) | **4 bytes** (u32), in every archive version | `:3084`; deprecated but used by compressed buffers |
| `ReadBigSize` | 8 bytes (u64) | `:3099` |
| `ReadBigTime` (`time_t`) | 8 bytes | `:3114` |
| `ReadFloat` | 4 bytes IEEE binary32 | `:1163` |
| `ReadDouble` | 8 bytes IEEE binary64 | `:1181` |
| `ReadTime` (`struct tm`) | 8 x i32: sec, min, hour, mday, mon, year, wday, yday | `:1463` |
| `ReadComponentIndex` | i32 type, i32 index | `:3316` |

`size_t` therefore has **no** version-dependent width: `ReadSize` is always 4 bytes and
`ReadBigSize` always 8; which one a field uses is fixed by the class code.

Unset sentinels used throughout: `ON_UNSET_VALUE` = -1.23432101234321e+308,
`ON_UNSET_INT_INDEX` = -2147483647, `ON_UNSET_COLOR` = 0xFFFFFFFF (`opennurbs_defines.h:562`).

### 1.2 Booleans

- `ReadBool` (`opennurbs_archive.cpp:2784`) reads **one byte**. Writers since 2017 emit only 0 or
  1 (`WriteBool`, `:2753`).
- Reading rule: if the byte is neither 0 nor 1, openNURBS treats it as `true` when the file's
  writer stamp is older than `ON_VersionNumberConstruct(6,0,2017,8,24,0)`, and as an error
  otherwise (RH-40908: some 2017 MSVC builds wrote arbitrary non-zero bytes).
  cadmpeg: `BoundedReader::bool` accepts any non-zero (`chunks.rs:314`);
  `bool_with_writer_version` (`chunks.rs:322-339`) is strict when the stamp is >= 2348836140
  (encoded 6.0 2017-08-24) or a date-form stamp in [201708240, 1e9). The second branch is a
  cadmpeg extension; openNURBS compares raw unsigned values, so a date-form stamp is always
  "old" (permissive). **Recommendation**: read `byte != 0` everywhere and never fail on 2..255;
  that matches every file openNURBS accepts.
- `ReadArray(ON_SimpleArray<bool>)` (`:2121`): `i32 count` then `count` bytes.
- **Not every flag is a `bool8`**: many classes store flags with `WriteInt` (4 bytes). The class
  layouts in sections 3-7 state which. cadmpeg has `wire::flag_i32` (`wire.rs:331`) for those.

### 1.3 Points, vectors, intervals, transforms, boxes

All double-precision geometry is a plain sequence of `f64` (`opennurbs_archive.cpp:1248-1445`):

| Type | Layout | Bytes |
|---|---|---|
| `ON_2dPoint`, `ON_2dVector` | x, y | 16 |
| `ON_3dPoint`, `ON_3dVector` | x, y, z | 24 |
| `ON_4dPoint` | x, y, z, w (homogeneous: x,y,z already multiplied by w) | 32 |
| `ON_Interval` (`ReadInterval`) | t0, t1 | 16 |
| `ON_BoundingBox` (`ReadBoundingBox`) | min x,y,z then max x,y,z | 48 |
| `ON_Xform` (`ReadXform`) | 16 f64, `m_xform[row][col]` row-major | 128 |
| `ON_PlaneEquation` | a, b, c, d (a·x + b·y + c·z + d = 0) | 32 |
| `ON_Plane` (`ReadPlane`) | origin(3), xaxis(3), yaxis(3), zaxis(3), plane equation(4) | 128 |
| `ON_Line` (`ReadLine`) | from(3), to(3) | 48 |
| `ON_Circle` (`ReadCircle`) | plane(128), radius f64, then 3 legacy points (3 x 24, ignored on read) | 208 |
| `ON_Arc` (`ReadArc`) | circle(208), angle interval (radians) | 224 |

`ON_Xform` maps column vectors: `x' = m[0][0]x + m[0][1]y + m[0][2]z + m[0][3]`, ...,
`w = m[3][0]x + ... + m[3][3]`, result divided by `w` (`ON_Xform::operator*`,
`opennurbs_xform.cpp:881`). Translation is in `m[0][3], m[1][3], m[2][3]`, i.e. f64 indices 3, 7,
11 of the 16 on disk.

Single-precision types (`ON_2fPoint`, `ON_3fPoint`, `ON_3fVector`, `ON_4fPoint`) are the same
sequences of `f32`. `ON_SurfaceCurvature` is two `f64` (k1, k2) (`:2718`).

### 1.4 Colours

`ReadColor(ON_Color&)` (`opennurbs_archive.cpp:1211`) reads 4 raw bytes in the order
**R, G, B, A** (as a little-endian u32: `0xAABBGGRR`, Windows COLORREF plus alpha). The alpha
byte is *transparency*: 0 = opaque, 255 = fully transparent (`opennurbs_color.h:178-247`).
`0xFFFFFFFF` is `ON_Color::UnsetColor`. Colour arrays (`ReadArray(ON_SimpleArray<ON_Color>)`,
`:2311`) are `i32 count` + `4·count` raw bytes. `ON_4fColor` is 4 x f32 (R, G, B, A).

### 1.5 UUIDs

`ReadUuid` (`opennurbs_archive.cpp:1445`): `Data1` as u32 LE, `Data2` as u16 LE, `Data3` as u16
LE, then `Data4` as 8 bytes verbatim. To get the canonical text form, byte-reverse the first 4,
the next 2 and the next 2 bytes. Example: `ON_NurbsCurve` 4ED7D4DD-E947-11D3-BFE5-0010830122F0 is
stored as `DD D4 D7 4E 47 E9 D3 11 BF E5 00 10 83 01 22 F0` (cadmpeg `Uuid::from_wire`,
`wire.rs:258`; `layout.rs:40`). `ReadArray(ON_SimpleArray<ON_UUID>)` (`:2496`) is `i32 count`
then 16 bytes each. `ON_UuidIndex` arrays add an `i32` after each UUID (`:2519`).

### 1.6 Strings

**`ON_wString`** (`ReadString(ON_wString&)`, `opennurbs_archive.cpp:1689`;
`ReadStringUTF16ElementCount`, `:1605`; writer `WriteString(const ON_wString&)`, `:3789`):

```
u32 n          number of UTF-16 code units INCLUDING the terminating 0; 0 for the empty string
u16[n]         UTF-16LE code units; element n-1 is 0
```

- Empty string: just `n = 0` (no terminator). A non-empty string always has `n >= 2`.
- The string value is the first `n - 1` code units; embedded NULs are possible ("In some cases
  there are embedded nulls", `:3805`), so do not stop at the first 0.
- Always UTF-16LE on disk, also from platforms where `wchar_t` is 32-bit (converted on write and
  read, `:3823-3860`, `:1720-1782`). Unpaired surrogates are possible in old files: decode
  lossily. (cadmpeg `utf16_payload`, `settings.rs:979`, rejects a missing terminator and
  invalid surrogates; that is stricter than openNURBS.)
- Sanity limits in openNURBS: `n & 0x0F000000 != 0` is rejected, and `2n` must fit in the rest
  of the enclosing chunk.

**`ON_String`** (`ReadString(ON_String&)`, `:1673`; `ReadStringUTF8ElementCount`, `:1556`): same
shape with 1-byte elements: `u32 n` (including the terminating 0) then `n` bytes. Content is
UTF-8 in modern files; V1-V3 era files may contain local code-page text.

**UTF-8 string chunk** (`ReadWideString`, `:3627`): a separate, rarely used form
`chunk(0x40008001 TCODE_UTF8_STRING_CHUNK){ u8 format(1) + UTF-8 bytes ..., u8 0 }`. Only used
by specific newer classes; it is never what `ReadString` reads.

String arrays (`ReadArray(ON_ClassArray<ON_wString>)`, `:2061`): `i32 count` then `count`
strings.

### 1.7 Arrays

`ReadArray(ON_SimpleArray<T>&)` (`opennurbs_archive.cpp:2043-2560`): `i32 count` (signed), then
`count` elements of the fixed-size encodings above, contiguous. A `count <= 0` means an empty
array with no element bytes. There is no per-array chunk, alignment or capacity field. Element
sizes: `int` 4, `float` 4, `double` 8, `ON_2dPoint` 16, `ON_3dPoint` 24, `ON_4dPoint` 32,
`ON_2fPoint` 8, `ON_3fPoint` 12, `ON_3fVector` 12, `ON_Color` 4, `ON_UUID` 16,
`ON_SurfaceCurvature` 16, `ON_Xform` 128, `bool` 1, `char`/`u8` 1, `i16` 2.

Some arrays of objects are *not* `SimpleArray`s and carry chunks per element (material
references, mapping channels, Brep component arrays); the class sections spell them out.

### 1.8 Packed chunk version (`cver`)

`Write3dmChunkVersion(major, minor)` writes **one byte** `major*16 + minor`;
`Read3dmChunkVersion` returns `major = byte >> 4`, `minor = byte & 0x0F`
(`opennurbs_archive.cpp:5762-5785`). Range: major 0-15, minor 0-15. Most classes start their
class data with this byte (e.g. `ON_Mesh::Read`, `ON_NurbsCurve::Read`). cadmpeg:
`packed_version` (`chunks.rs:967`), `layout.rs:136`.

### 1.9 Versioned and anonymous chunks inside class data

- `BeginWrite3dmChunk(tcode, major, minor)` (`opennurbs_archive.cpp:5886`) opens a long chunk
  with typecode `tcode` and writes `i32 major` (>= 1) and `i32 minor` (>= 0) as the first 8
  body bytes. `BeginRead3dmChunk(expected_tcode, &major, &minor)` (`:6396`) requires the
  typecode to match and the length to be >= 8, and fails on `major < 1` or `minor < 0`. These
  two `i32`s are *not* interchangeable with the 1-byte `cver` (cadmpeg `anonymous_version`,
  `chunks.rs:973`; `layout.rs:131-145`).
- `TCODE_ANONYMOUS_CHUNK` = 0x40008000 (`TCODE_USER | TCODE_CRC`), so anonymous chunks carry a
  trailing CRC32 over their direct bytes. They are used as `vchunk(0x40008000)` in many V5+
  class layouts, and as plain `chunk(0x40008000)` around userdata payloads (0.6) and around
  compressed zlib streams (2.2).
- `BeginRead3dmAnonymousChunk(&version)` (`:5862`) is `vchunk(0x40008000)` with major fixed at
  1; the minor is the "version".
- Skipping: every such chunk is skipped by its length (0.4). A decoder that only needs a few
  leading fields reads them and jumps to the chunk end.

---

## 2. Compressed buffers

Used by `ON_Mesh::Read` payload major 3 (internally `Read_2`: vertices, normals, texture
coordinates, curvature, colours, surface parameters, double vertices; see section 4) and by bitmap and embedded-file records; not by other geometry classes.
(`ON_BinaryArchive::ReadCompressedBufferSize` / `ReadCompressedBuffer` / `ReadInflate`,
`opennurbs_zlib.cpp:161`, `:170`, `:414`; writer `WriteCompressedBuffer`, `:95`, `WriteDeflate`,
`:226`. cadmpeg `read_buffer`, `mesh.rs:1068-1230`, `inflate`, `mesh.rs:1234`;
`layout.rs:115-129`.)

### 2.1 Layout

```
u32  sizeof_uncompressed     ReadSize: 4 bytes in EVERY archive version (no 8-byte form)
-- if sizeof_uncompressed == 0 the buffer ends here: no CRC, no method, no data --
u32  crc32 of the UNCOMPRESSED bytes (IEEE, init 0)
u8   method                  0 = stored, 1 = zlib/deflate; any other value is an error
method 0: u8[sizeof_uncompressed]  raw bytes
method 1: chunk 0x40008000 TCODE_ANONYMOUS_CHUNK (long, CRC) {
            zlib stream (RFC 1950: 2-byte header, deflate data, Adler-32 trailer)
          }  + u32 CRC32 of the compressed bytes (the chunk's own trailing CRC)
```

- In openNURBS the size is read by a separate call (`ReadCompressedBufferSize`) and the caller
  compares it with the size it expects *before* calling `ReadCompressedBuffer`; for meshes a
  mismatch fails the whole mesh read (`ON_Mesh::Read_2`, `opennurbs_mesh.cpp:1841-1940`).
- The writer uses method 1 only when buffer compression is enabled and the buffer is longer
  than 128 bytes (`opennurbs_zlib.cpp:127`); small buffers are stored.
- The stream is a standard zlib stream (`deflateInit(..., Z_BEST_COMPRESSION)` /
  `inflateInit`, `opennurbs_zlib.cpp:626`, `:642`), so the body begins `78 DA`. Verified on
  samples: 660 compressed buffers in `opennurbs/example_files/V4/v4_Wheel_PG.3dm` and 2865 in
  `opennurbs/example_files/V5/v5_disk_brake.3dm` all start with `78 DA`. Use a zlib decoder
  (`flate2::read::ZlibDecoder` / `miniz_oxide::inflate::decompress_to_vec_zlib`), not raw
  deflate.
- The anonymous chunk's length value includes its 4-byte CRC, so the compressed byte count is
  `length - 4` (`ReadInflate`, `opennurbs_zlib.cpp:443`). It is one contiguous zlib stream;
  the writer emits it in 16 KiB pieces but they are concatenated in one chunk, not split into
  several chunks. Input/output are fed to zlib in pieces of at most 0x7FFFFFF0 bytes, which is
  invisible on disk.
- The decompressed bytes are the in-memory array image, little-endian, with no header: e.g.
  `vcount x 12` bytes of `ON_3fPoint` for mesh vertices.

### 2.2 Limits and failure behaviour

- `sizeof_uncompressed` is a u32, so a single buffer is < 4 GiB; the writer refuses larger
  buffers (`opennurbs_zlib.cpp:107`).
- openNURBS: a CRC mismatch of the uncompressed data is reported (`bFailedCRC`) but the data is
  kept (`opennurbs_zlib.cpp:214-219`); a CRC mismatch of the compressed chunk zeroes the output
  buffer and inflation is still attempted ("we might get something valid at the start",
  `ReadInflate`, `:470-476`).
- cadmpeg is stricter: it caps each buffer at 256 MiB (`MAX_BUFFER_OUTPUT`, `mesh.rs:114`) or the
  session limit, requires the zlib stream to consume the chunk body exactly ("zlib chunk has
  trailing bytes", `mesh.rs:1151-1158`), and drops the channel on a size or CRC mismatch
  (`mesh.rs:1199-1222`) where openNURBS would fail the mesh (size) or keep the data (CRC).
- For our decoder: decompress into a buffer of exactly the expected size, treat a size mismatch
  as a typed error for that mesh, and treat CRC mismatches as warnings.

### 2.3 Not to be confused with

`ON_CompressedBuffer` (`opennurbs_zlib.cpp:794`) is a different, versioned format
(`vchunk(0x40008000)` 1.x with u32 sizes, CRCs, method and element size, then raw compressed
bytes). It is not used by any geometry class's file IO.

---

## 3. Object attributes (ON_3dmObjectAttributes)

Scope: archive versions 2, 3, 4, 50, 60, 70 and 80. Archive 1 files have no attributes chunk: openNURBS builds their attributes from V1 chunks in `Read3dmV1Object` (opennurbs_archive.cpp:15768), which this section does not cover.

**Verification.** A throwaway probe implementing exactly the layouts below was run over every object record (table 0x10000013) and light record (table 0x10000012) in the samples. That is 9,284 records in archives 2 to 80. Every attributes payload was consumed exactly:
- legacy payloads ended at body end minus the 4-byte CRC;
- tagged payloads had their 0x00 terminator as the last byte before the CRC.

Every 0x02000073 payload also parsed, including all user-string lists. The probe is at `probe_attrs.py` and its output at `attrs.out`, one Python dict per record.

Notation: all values are little-endian. "chunk header" means `u32 typecode` followed by a value that is `u32` when archive < 50 and `i64` when archive ≥ 50. So a nested chunk header is 8 bytes in V2–V4 and 12 bytes in V5+.

### 3.1 Where the attributes live

| Chunk (inside object record 0x20008070) | CRC bit | Payload | Reader |
|---|---|---|---|
| 0x82000071 TCODE_OBJECT_RECORD_TYPE | short | object type | — |
| 0x00027FFA class wrapper `{0x0002FFFB uuid, 0x0002FFFC data, 0x00027FFD…, 0x80027FFF}` | — | geometry, plus **object-level (geometry) userdata** | `ReadObject` |
| **0x02008072** TCODE_OBJECT_RECORD_ATTRIBUTES | yes (last 4 body bytes are CRC32) | `ON_3dmObjectAttributes::Write` stream, written directly | `ON_3dmObjectAttributes::Read` |
| **0x02000073** TCODE_OBJECT_RECORD_ATTRIBUTES_USERDATA | **no** | a sequence of 0x00027FFD userdata records, then a short 0x80027FFF (value 0) | `ReadObjectUserData(*attributes)` |
| 0x02008074 history, then 0x8200007F end | | | |

Light records (0x20008060) carry the same two payloads under different typecodes:
- 0x02008061 (attributes, with CRC);
- 0x02000062 (attribute userdata, no CRC).

Sources: openNURBS `Read3dmLight` (opennurbs_archive.cpp:13048, 13101-13112) and opennurbs_3dm.h:212-216; cadmpeg presentation.rs:38-39 and 1959-1975.

- **The 0x02008072 payload has no class wrapper, no nested chunk and no class UUID.** `Read3dmObject` opens the chunk with `BeginRead3dmBigChunk` and calls `pAttributes->Read(*this)` on its body (opennurbs_archive.cpp:15821-15834). The writer calls `BeginWrite3dmChunk(TCODE_OBJECT_RECORD_ATTRIBUTES,0)` then `attributes->Write` (opennurbs_archive.cpp:13248-13256).
- **openNURBS accepts the record's children in any order.** `Read3dmObject` loops over them until 0x8200007F and skips unknown ones (15821-15870).
- **cadmpeg requires a fixed order:** attributes, then attribute userdata, then history, then unknown long chunks (objects.rs:1848-1918). It also checks that 0x8200007F is the last child.
- **0x02000073 is only written for archive ≥ 4** (`Write3dmObject`, opennurbs_archive.cpp:13258-13290). It must end with a short TCODE_OPENNURBS_CLASS_END; the writer adds that end marker deliberately so V3 readers keep working.
- **cadmpeg sources:** typecode constants at objects.rs:19-31; the attributes chunk is picked up at objects.rs:1865-1874 and parsed at 1920-1944.

### 3.2 Start of the payload and choosing the reader

Byte 0 of the payload is always `Read3dmChunkVersion`, one `u8 v` with `major = v >> 4` and `minor = v & 0x0F` (opennurbs_archive.cpp:5771-5785).

- **major 1:** legacy fixed layout, minor 0..8. Written by `ON_3dmObjectAttributes::Write` with `Write3dmChunkVersion(1,8)` when archive < 5 (opennurbs_3dm_attributes.cpp:1513).
- **major 2:** tagged ("V5") layout, minor 0..14. The current openNURBS 8.35 writer always emits **2.13** (`Write3dmChunkVersion(2,13)`, opennurbs_3dm_attributes.cpp:1115), whatever archive version it is saving to.

How openNURBS chooses (`ON_3dmObjectAttributes::Read`, opennurbs_3dm_attributes.cpp:923-930):

```
if Archive3dmVersion() >= 5 && ArchiveOpenNURBSVersion() >= 200712190 -> Internal_ReadV5 (requires major == 2)
else -> legacy reader (requires major == 1)
```

`ArchiveOpenNURBSVersion` is the writer stamp: the raw value of the short chunk 0xA0000026 (TCODE_PROPERTIES_OPENNURBS_VERSION) in the properties table. **Compare it as u32 or i64, never as i32.** Since V6 the stamp is packed by `ON_VersionNumberConstruct` with bit 31 set, for example 2449512653 = openNURBS 9.0.

**Recommended rule: dispatch on the major nibble.** Across all 9,284 sample records, major 1 occurs exactly when archive ≤ 4 and major 2 exactly when archive ≥ 50:

| archive | chunk versions seen (record count) |
|---|---|
| 2 | 1.1 (2341), 1.2 (12) |
| 3 | 1.2 (2488) |
| 4 | 1.7 (175) |
| 50 | 2.1 (205), 2.13 (2) |
| 60 | 2.1 (46) |
| 70 | 2.1 (2098), 2.10 (1), 2.11 (3) |
| 80 | 2.1, 2.3, 2.4, 2.10, 2.13, 2.14 |

cadmpeg also dispatches on the nibble, with extra checks (objects.rs:965-981 and 1143-1147; see 3.9).

**Primitive widths used in this section.** None of these depend on the archive version except nested chunk headers.

| openNURBS primitive | Rust read |
|---|---|
| `ReadChar` | `u8` |
| `ReadShort` | `i16` |
| `ReadInt` | `i32` |
| `ReadDouble` | `f64` |
| `ReadBool` | `u8`: **1 byte**, must be 0 or 1. If a writer older than `ON_VersionNumberConstruct(6,0,2017,8,24,0)` = 2348836140 stored another value it is coerced to true; for newer writers it is an error (opennurbs_archive.cpp:2784-2811). |
| `ReadUuid` | 16 bytes: `Data1 u32`, `Data2 u16`, `Data3 u16`, `Data4 [u8;8]` (1444-1451). Example: CE28DE29-F4C5-4FAA-A50A-C3A6849B6329 is stored as `29 DE 28 CE C5 F4 AA 4F A5 0A C3 A6 84 9B 63 29`. |
| `ReadColor` | 4 raw bytes `[R,G,B,A]` (COLORREF plus alpha); A = 0 is opaque (1210-1217; opennurbs_color.h:178) |
| `ReadXform` | 16 × `f64`, row-major (1307-1311) |
| `ReadString(ON_wString)` | `u32 n` = number of UTF-16 units **including** the terminating NUL; then `n × u16`. `n = 0` is the empty string; otherwise drop the last unit (1604-1641, 1688-). |
| `Read3dmReferencedComponentIndex` | `i32` (opennurbs_model_component.cpp:2981). Negative values pass through as system components. Non-negative values are remapped archive→model by `ONX_Model` when index mapping is on. |
| `Read3dmReferencedComponentIndexArray` | `ReadArray(ON_SimpleArray<int>)`: `i32 n`, then `n × i32`; `n ≤ 0` means empty (model_component.cpp:2939; archive.cpp:2220-2233) |
| `ReadArray(ON_SimpleArray<ON_DisplayMaterialRef>)` | `i32 n`, then `n × (uuid viewport, uuid display_material)`, 32 bytes each (2025-2040) |
| `BeginRead3dmChunk(tcode,&maj,&min)` | chunk header, then `i32 major`, `i32 minor` (6395-6480). TCODE_ANONYMOUS_CHUNK = 0x40008000 has the CRC bit, so the last 4 bytes of the body are CRC32. Skip it by jumping over its length; the length includes the CRC. |

### 3.3 Tagged layout (chunk major 2, archive ≥ 50)

Source: openNURBS `Internal_ReadV5` (opennurbs_3dm_attributes.cpp:441-921), writer `Internal_WriteV5` (1073-1503); cadmpeg objects.rs:1143-1373.

| off | field | primitive | Rust |
|---|---|---|---|
| 0 | chunk version, 0x2m | Read3dmChunkVersion | u8 |
| 1 | object UUID | ReadUuid | [u8;16] |
| 17 | layer index | Read3dmReferencedComponentIndex | i32 |
| 21… | item stream: `u8 id`, then a value of the item's type; repeat; ends with `id = 0` | | |

The UUID and the layer index are always present and carry no item id. The writer emits only non-default values, in **strictly ascending id order**. openNURBS reads the items with a fixed cascade, not a loop: each `if (id == N)` block reads its value and the next id. The cascade also contains `if (minor <= k) break;` gates.

**An item that is unknown, out of order, or below its minimum minor ends the parse.** Item values have no length prefix, so everything after that item is lost and skipped by `EndRead3dmChunk`. openNURBS raises `ON_ERROR` but keeps the fields already read. One exception: when minor ≥ 14 and id > 41, openNURBS sets id to 0 silently (lines 905-910).

Default when an item is absent comes from `ON_3dmObjectAttributes::Default`.

| id | field | value encoding | bytes | min minor | added | written when (8.35 writer) | default |
|---|---|---|---|---|---|---|---|
| 1 | name | ReadString(ON_wString) | 4+2n | 2.0 | 2007-12 (200712190) | name not empty | "" |
| 2 | URL | ReadString(ON_wString) | 4+2n | 2.0 | | URL not empty | "" |
| 3 | linetype index | i32 | 4 | 2.0 | | ≠ −1 | −1 |
| 4 | **material index** | i32 | 4 | 2.0 | | ≠ −1 **and** material source == object (line 1149); older writers omit the source test (see 3.6) | −1 |
| 5 | rendering attributes | `ON_ObjectRenderingAttributes::Read`: one nested 0x40008000 chunk (3.5) | hdr+len | 2.0 | | mappings or material refs present, shadow flags not default, or advanced preview on | — |
| 6 | **color** | ReadColor | 4 | 2.0 | | ≠ 0 | 0 |
| 7 | plot color | ReadColor | 4 | 2.0 | | ≠ 0 | 0 |
| 8 | plot weight (mm) | f64 | 8 | 2.0 | | ≠ 0.0 | 0.0 |
| 9 | decoration | **u8** (legacy: i32) | 1 | 2.0 | | ≠ 0 | 0 |
| 10 | wire density | i32 | 4 | 2.0 | | ≠ 1 | 1 |
| 11 | **visible** | ReadBool, 1 byte | 1 | 2.0 | | == false | true |
| 12 | **mode** | u8 | 1 | 2.0 | | ≠ 0 | 0 |
| 13 | **color source** | u8 | 1 | 2.0 | | ≠ 0 | 0 (layer) |
| 14 | plot color source | u8 | 1 | 2.0 | | ≠ 0 | 0 |
| 15 | plot weight source | u8 | 1 | 2.0 | | ≠ 0 | 0 |
| 16 | **material source** | u8 | 1 | 2.0 | | ≠ 0 | 0 (layer) |
| 17 | linetype source | u8 | 1 | 2.0 | | ≠ 0 | 0 |
| 18 | **groups** | `i32 n`, then `n × i32` (the writer emits n with WriteInt and each index with Write3dmReferencedComponentIndex) | 4+4n | 2.0 | | n > 0 | empty |
| 19 | active space | u8 `ON::active_space` | 1 | 2.0 | | ≠ model (1) | 1 |
| 20 | viewport id | ReadUuid | 16 | 2.0 | | not nil | nil |
| 21 | display material refs | `i32 n`, then `n × 32` | 4+32n | 2.0 | | n > 0 | empty |
| 22 | display order | i32 | 4 | **2.1** | 2009-11-29 | ≠ 0 | 0 |
| 23–26 | obsolete line cap source, cap style, join source, join style | u8 each, discarded | 1 | **2.2** | 2021-08 (Rhino 8 WIP, about 24 h) | never | — |
| 27 | obsolete clip-participation source | u8, discarded | 1 | **2.3** | 2021-09-29 | never (writer code commented out) | — |
| 28 | selective clipping data | ReadBool (1 byte), then `ON_UuidList::Read`: nested 0x40008000 chunk `{i32 1, i32 minor, i32 n, n×16, CRC}` (opennurbs_array.cpp:827-860) | 1+hdr+len | 2.3 | | never (commented out) | — |
| 29 | section attributes source | u8 | 1 | **2.4** | 2021-10-18 | ≠ FromLayer | — |
| 30 | section hatch index | i32 | 4 | 2.4 | | no longer written (since 2023-04) | — |
| 31 | section hatch scale | f64 | 8 | 2.4 | | no longer written | 1.0 |
| 32 | section hatch rotation | f64 | 8 | 2.4 | | no longer written | 0.0 |
| 33 | linetype pattern scale | f64 | 8 | **2.5** | 2021-11-05 | \|1 − s\| > ε | 1.0 |
| 34 | hatch background fill color | ReadColor | 4 | **2.6** | 2022-01-10 | ≠ unset | unset |
| 35 | hatch boundary visible | ReadBool | 1 | 2.6 | | true | false |
| — | (minor 7 was skipped) | | | 2.7 | | | |
| 36 | object frame | ReadXform. The writer stores `ChangeBasis(frame, World_xy)`; the reader sets `frame = World_xy.Transform(xform)`. | 128 | **2.8** | 2022-01-26 | frame set | unset |
| 37 | section fill rule | u8 | 1 | **2.9** | 2022-06-15 | no longer written | — |
| 38 | custom linetype | `ON_Linetype::Read`: nested 0x40008000 chunk `{i32 1, i32 minor, …}` (opennurbs_linetype.cpp:447) | hdr+len | **2.10** | 2022-11-30 | custom linetype present | — |
| 39 | custom section style | `ON_SectionStyle::Read`: nested 0x40008000 chunk (opennurbs_sectionstyle.cpp:392) | hdr+len | **2.11** | 2023-04-18 | not Unset | — |
| 40 | clipping-plane label style | u8 | 1 | **2.12** | 2023-05-10 | ≠ None | — |
| 41 | obsolete selective-clipping list type | ReadBool, discarded | 1 | **2.13** | 2023-05-11 | never | — |
| 42 | detail background visible (**cadmpeg only**) | bool, 1 byte | 1 | cadmpeg: 2.13 | — | — | false |
| 0 | end of items | — | — | — | — | always | — |

The minimum minors in cadmpeg's `AttributeItem::minimum_minor` (objects.rs:914-957) match the openNURBS gates. cadmpeg's item enum and raw ids are at objects.rs:820-912 and the per-item reads at 1236-1366.

**Worked example 1** (v5_extrusions.3dm, object record 1, archive 50):

```
72 80 00 02  23 00 00 00 00 00 00 00   0x02008072, length 35 (i64)
21                                      chunk version 2.1
28 a3 eb 86 82 ea cc 43 ad 83 33 60 5c 6b b5 a1   uuid 86eba328-ea82-43cc-ad83-33605c6bb5a1
00 00 00 00                             layer 0
04 0c 00 00 00                          item 4: material index 12
0c 02                                   item 12: mode 2 (locked)
10 01                                   item 16: material source 1 (object)
00                                      end of items
83 40 40 54                             CRC32
```

**Worked example 2** (groups.3dm, record 1, version 2.13): `2d`, uuid, `00 00 00 00`, then `12 05 00 00 00 00 00 00 00 01 00 00 00 02 00 00 00 03 00 00 00 04 00 00 00` (item 18, groups 0, 1, 2, 3, 4), then `00`, then the CRC.

### 3.4 Legacy layout (chunk major 1, archives 2–4)

Source: openNURBS `ON_3dmObjectAttributes::Read` (opennurbs_3dm_attributes.cpp:931-1069), writer `Write` (1505-1610); cadmpeg objects.rs:973-1141.

Fixed part. Offsets are fixed up to the name.

| off | field | primitive | Rust | min minor |
|---|---|---|---|---|
| 0 | chunk version, 0x1m | Read3dmChunkVersion | u8 | |
| 1 | object UUID | ReadUuid | [u8;16] | 1.0 |
| 17 | layer index | Read3dmReferencedComponentIndex | i32 | 1.0 |
| 21 | material index (always written; meaningful only when material source = 1) | i32 | i32 | 1.0 |
| 25 | color | ReadColor | [u8;4] | 1.0 |
| 29 | obsolete line style, word 1: arrowhead bits. If archive < 4 or writer < 200503170, openNURBS sets decoration = `s & 0x18`. | ReadShort | i16 | 1.0 |
| 31 | obsolete line style, word 2 | ReadShort | i16 | 1.0 |
| 33 | obsolete thickness | ReadDouble | f64 | 1.0 |
| 41 | obsolete scale | ReadDouble | f64 | 1.0 |
| 49 | wire density | ReadInt | i32 | 1.0 |
| 53 | **mode** | ReadChar | u8 | 1.0 |
| 54 | **color source** (normalized by `ON::ObjectColorSource`; unknown → 0) | ReadChar | u8 | 1.0 |
| 55 | linetype source (normalized) | ReadChar | u8 | 1.0 |
| 56 | **material source** (normalized by `ON::ObjectMaterialSource`; unknown → 0) | ReadChar | u8 | 1.0 |
| 57 | **name** | ReadString(ON_wString) | u32 n + 2n | 1.0 |
| … | URL | ReadString(ON_wString) | u32 n + 2n | 1.0 |

Variable part, in this order. Each block is present only if minor ≥ the gate:

| min minor | fields | encoding |
|---|---|---|
| 1.1 | **groups** | `i32 n`, then `n × i32` |
| 1.2 | **visible** | ReadBool, 1 byte. If minor < 2, visible = `(mode & 0x0F) != 1`, set at line 984. |
| 1.3 | display material refs | `i32 n`, then `n × (uuid viewport, uuid display material)` |
| 1.4 (2005-03-23) | decoration **i32**; plot color source u8; plot color 4 bytes; plot weight source u8; plot weight f64 (mm) | 4+1+4+1+8 bytes |
| 1.5 (2005-04-11) | linetype index | i32 |
| 1.6 (2005-09-02) | space byte (**1 = page space**, anything else = model); then a display material ref list again (`i32 n`, then `n × (uuid, uuid)`) | The 1.6 list **replaces** the 1.3 list. An entry whose display-material id is ON_ObsoletePageSpaceObjectId {9BBB37E9-2131-4fb8-B9C6-5524859B98B8} is removed and its viewport uuid becomes the object's page viewport id (lines 1018-1050). |
| 1.7 (2006-06-06) | rendering attributes | one nested 0x40008000 chunk (3.5) |
| 1.8 | no new fields; current openNURBS writes 1.8 for V2–V4 saves | the reader treats it as 1.7 |

openNURBS accepts any minor ≥ 7. The remaining bytes and the CRC are skipped by `EndRead3dmChunk`.

**Worked example 3** (v2_T-Joint1.3dm, object record 7, archive 2, so 4-byte chunk length):

```
72 80 00 02  49 00 00 00     0x02008072, length 73 (u32)
11                           chunk version 1.1
fb de a4 70 03 53 9d 4a b8 dd 6d 53 17 6d cd 02   uuid 70a4defb-5303-4a9d-b8dd-6d53176dcd02
00 00 00 00                  layer 0
ff ff ff ff                  material -1
00 00 00 00                  color
00 00 00 00                  obsolete shorts
00 00 00 00 00 00 00 00      obsolete thickness 0.0
00 00 00 00 00 00 f0 3f      obsolete scale 1.0
01 00 00 00                  wire density 1
01 00 00 00                  mode 1 (hidden); color, linetype, material sources 0
00 00 00 00  00 00 00 00     name "", URL ""
00 00 00 00                  groups: count 0 (minor 1)
8a b3 50 83                  CRC32
```

### 3.5 Nested variable-length values and how to skip them

The only values inside the attributes stream whose length is not known in advance are these:
- **chunked:** items 5, 28 (after its 1-byte bool), 38 and 39, and legacy 1.7. Read the chunk header and jump `value` bytes; the value includes the CRC.
- **counted:** items 1, 2, 18 and 21, legacy name and URL, and the legacy groups and display-material lists.

Layout of the rendering attributes, in case you want material references:

- **`ON_ObjectRenderingAttributes`** (opennurbs_material.cpp:8209-8285): 0x40008000 `{i32 1, i32 minor (≥1; 3 written)`, then:
  - `i32 nMat` and `nMat × MaterialRef`;
  - `i32 nMap` and `nMap × MappingRef`;
  - if minor ≥ 2: bool casts shadows, bool receives shadows;
  - if minor ≥ 3: bool advanced texture preview;
  - `CRC}`.
- **`ON_MaterialRef`** (7629-7688): 0x40008000 `{i32 1, i32 1, uuid plugin, uuid material, i32 n` (an obsolete mapping-channel count, always written as 0), `[minor ≥ 1: uuid backface material, i32 material source], CRC}`.
- **`ON_MappingRef`** (7716-7734): 0x40008000 `{i32 1, i32 0, uuid plugin, i32 n, n × MappingChannel, CRC}`.
- **`ON_MappingChannel`** (7536-7569): 0x40008000 `{i32 1, i32 minor, i32 channel id, uuid mapping id, [minor ≥ 1: xform 128 bytes], CRC}`.

In the samples, the rendering-attribute chunks that were decoded held mapping refs only and had zero material refs. Examples: decal_r8.3dm (V8) and v4_example_file.3dm record 18 (legacy). For per-object material, use item 4 together with item 16.

cadmpeg parses this structure in `settings::parse_rendering_attributes` (settings.rs:1790-1900) and the UUID list in `read_uuid_list` (objects.rs:1375-1413).

### 3.6 Value semantics and traps

- **Mode.** `ON::object_mode` is the **low nibble**: `Mode() = m_mode % 16` (opennurbs_3dm_attributes.cpp:1764-1767). The high nibble held the obsolete `ON::display_mode`, per the comment at opennurbs_3dm_attributes.h:448-451; that enum no longer exists. Values (opennurbs_defines.h:2089-2099): 0 normal, 1 hidden, 2 locked, **3 = idef_object**, meaning the object is geometry of an instance (block) definition. Such objects are listed by UUID in the definition's object list and must not be drawn at their own location. Samples only contain values 0–3. Mask with 0x0F.
- **Visible.** In the tagged layout, visibility is item 11 alone and defaults to **true** even when the mode is 1. The reader does not derive it from the mode, and `SetVisible`/`SetMode` keep the two in step only at run time (1769-1800). Robust rule: hidden if `visible == false || (mode & 0x0F) == 1`. In the legacy layout before minor 2, visibility comes from the mode.
- **Color source** (defines.h:2102-2110): 0 layer, 1 object, 2 material (diffuse colour of the render material), 3 parent (block instance; with no parent, behave as layer).
- **Material source** (2144-2157): 0 layer, 1 object, 3 parent; there is no 2.
- **Plot colour, plot weight and linetype sources:** 0 layer, 1 object, 3 parent; plot colour also has 2 = display colour.
- **Material index.** Use it only when the material source is 1. v5_teacup.3dm (writer 201307115) has item 4 without item 16 on 4 records, so the source there is layer. The legacy layout always writes the index. A value of −1 means none.
- **Layer index.** It is the layer's index as stored in the layer table record. cadmpeg resolves it against the stored `layer.index` and marks the result ambiguous when two layers share an index (objects.rs:2066-2120).
  - **−1 is valid.** It means the system layer `ON_Layer::Default`: index −1, name "Default", id {061DF99E-2EF8-4A3F-8F2D-4B123A166089}, defined at opennurbs_statics.cpp:1976-1985. That layer is not in the table. `Read3dmReferencedComponentIndex` passes negative indices through.
  - Seen in v7_my_surfaces.3dm, subd_creases.3dm, SampleCSJoinCurves/newFile.3dm and Rhino_Logo_Mesh7.3dm, all written by code.
- **Groups.** Indices into the group table, by stored index in the same way as layers.
- **Active space.** The coding differs between layouts.
  - Tagged item 19 uses `ON::active_space` (defines.h:1323-1330): 0 none, 1 model, 2 page, 3 UV editor, 4 block editor.
  - Legacy 1.6 uses 1 = page, any other value = model.
  - Page-space objects belong to a layout. Their page viewport is item 20, or the pseudo display-material entry in legacy files. **Exclude them from model renders.** Sample: v5_example_file.3dm, 5 objects with items 19 and 20.
- **Decoration:** 0, 0x08 start arrowhead, 0x10 end arrowhead, 0x18 both (defines.h:2335-2341).
- **Booleans** are 1 byte everywhere in this section: items 11, 28, 35 and 41, legacy 1.2, rendering-attribute flags, and the userdata header.
- **Archive version and chunk minor.** The item set depends on the chunk minor, not on the archive version. Rhino 8 writes 2.10–2.13 into V5 and V7 archives:
  - MeshSphere_r5.3dm: archive 50, version 2.13;
  - test_in.3dm: archive 70, version 2.11, with item 36.

  The only archive-dependent width inside the payload is the nested chunk header: 8 bytes for V2–V4, 12 bytes for V5+.
- **Nil or duplicate UUIDs.** openNURBS replaces them with new ids after reading (opennurbs_archive.cpp:15876-15893). cadmpeg keys such objects by record offset and emits a warning (objects.rs:1717-1745).

### 3.7 Attribute userdata (0x02000073) and user strings

Readers: openNURBS `ReadObjectUserData` (opennurbs_archive.cpp:5532-5760), `ReadObjectUserDataHeaderHelper` (5440-5530) and `ReadObjectUserDataAnonymousChunk` (5286-5420); writer `WriteObjectUserData` (4809-5032). cadmpeg: `parse_attribute_userdata` (objects.rs:1415-1499), `parse_userdata` (609-721) and `parse_user_string_list` (723-771).

0x02000073 body, which has no CRC:

```
repeat:
  chunk header
    0x80027FFF (short, value 0)  -> end; required as the last child
    0x00027FFD TCODE_OPENNURBS_CLASS_USERDATA (long, NO CRC) -> one userdata record
    any other typecode           -> skip (typecode 0 is a corrupt-file error in openNURBS)
```

The body can contain no records at all, only the end marker. This happens in 136 V4 records and 6 records of v5_rhino_logo.3dm.

One 0x00027FFD record:

| part | content |
|---|---|
| u8 | userdata chunk version. 0x22 in V5+, 0x21 in V4 samples, 0x20 in old V2/V3 point samples. Major must be 1 or 2, otherwise skip the record. |
| major 2: chunk 0x0002FFF9 TCODE_OPENNURBS_CLASS_USERDATA_HEADER (CRC) | `uuid class_id`; `uuid item_id`; `i32 copy_count`; `xform` 128 bytes; if minor ≥ 1: `uuid application_id`; if minor ≥ 2: `bool last_saved_as_goo` (1 byte), `i32 archive_version`, `u32 opennurbs_version` |
| major 1 | the same first four fields inline, with no header chunk |
| chunk 0x40008000 | **outer wrapper**: the userdata class's own `Write` output |

**ON_UserStringList** (opennurbs_userdata.cpp:699-822):
- class uuid **CE28DE29-F4C5-4FAA-A50A-C3A6849B6329**; item uuid is the same value (the constructor sets `m_userdata_uuid = class id`);
- application uuid is ON_opennurbs4_id 17B3ECDA-17BA-4E45-9E67-A2B8D9BE520D (opennurbs_statics.cpp:569).

Its payload inside the outer wrapper is a second anonymous chunk:

```
0x40008000 { i32 major = 1, i32 minor = 0, i32 count,
             count × 0x40008000 { i32 1, i32 0, ON_wString key, ON_wString value, CRC32 },
             CRC32 }
```

The entry write is `ON_UserString::Write` at opennurbs_userdata.cpp:635-683.

**Worked example 4** (object_UserStrings.3dm, record 1, archive 80). The offsets are relative to the 0x02000073 header.

```
000 73 00 00 02 36 03 00 00 00 00 00 00   0x02000073, length 822
00c fd 7f 02 00 1e 03 00 00 00 00 00 00   0x00027FFD, length 798
018 22                                    userdata version 2.2
019 f9 ff 02 00 c1 00 00 00 00 00 00 00   0x0002FFF9, length 193 (189 bytes + CRC)
025 29 de 28 ce … 63 29  ×2               class id = item id = CE28DE29-…
045 13 00 00 00                           copy count 19
049 (128 bytes identity xform)
0c9 da ec b3 17 ba 17 45 4e 9e 67 a2 b8 d9 be 52 0d   application id
0d9 00  50 00 00 00  d2 8f 44 90          goo = 0, archive 80, writer 2420412370
0e2 c6 3c 6f f2                           header CRC
0e6 00 80 00 40 44 02 00 00 00 00 00 00   outer anonymous chunk, length 580
0f2 00 80 00 40 34 02 00 00 00 00 00 00   inner list chunk, length 564
0fe 01 00 00 00 00 00 00 00 05 00 00 00   major 1, minor 0, count 5
10a 00 80 00 40 34 00 00 00 00 00 00 00   entry chunk, length 52
    01 00 00 00 00 00 00 00 09 00 00 00 "firstKey\0" 07 00 00 00 "123456\0" 25 97 49 24
    … 4 more entries …
32e 32 7a ae d7                           inner list CRC
332 00 00 00 00                           outer wrapper CRC (no direct bytes, so CRC32 = 0)
336 ff 7f 02 80 00 00 00 00 00 00 00 00   0x80027FFF end
```

The outer wrapper's CRC is 0 because chunk headers are never included in a parent's CRC: `BeginWrite3dmBigChunk` clears `m_bDoChunkCRC` (5833-5847).

**Trap: userdata saved as "goo".** When the header byte `last_saved_as_goo` is 1, the userdata was re-saved without its class being known. In that case `EndWrite3dmChunk` writes **no** CRC for the outer wrapper even though its typecode has the CRC bit (4990-5002 and 5935-5960). Always skip by the length; never assume a trailing CRC there. ON_UserStringList is core openNURBS, so it is never stored as goo by openNURBS-based writers.

**Object-level and attribute user strings.**
- The same 0x00027FFD record format appears inside the geometry's class wrapper, after 0x0002FFFC. User strings set on the geometry (`ON_Object::SetUserString` on the geometry) live there.
- Strings set on the object attributes live in 0x02000073; Rhino's "Attribute User Text" panel uses this list.
- RectifiedArchimedeanSolids.3dm (archive 70) and RectifiedArchimedeanSolids8.3dm (archive 80) have both kinds: geometry keys `volume`, `face count` and `edge count` on 11 objects; the attribute key `Polyhedra Category` on 1811 objects.
- cadmpeg reads both and keeps them separate (presentation.rs:1298-1341).

**Keys and cleanup.**
- openNURBS deletes the key `$temp_object$` (compared case-insensitively) after reading (opennurbs_archive.cpp:15856-15861). cadmpeg does the same (presentation.rs:1334-1340).
- Other `$…$` keys are Rhino-internal, for example `$block-instance-original-object-id$` in blocks.3dm and file3dm_stuff.3dm. Consider filtering them from the report.
- A list with count 0 occurs: three.js Rhino_Logo.3dm, 224 records.

**Other userdata classes seen on attributes.** All of them can be skipped via the 0x00027FFD length:
- ON_RdkUserData AFA82772-1525-43DD-A63C-C84AC5806911 (decals and RDK data);
- mesh modifiers ON_DisplacementUserData B8C04604…, ON_EdgeSofteningUserData CB5EB395…, ON_ThickeningUserData AA03D9C3…, ON_CurvePipingUserData 2D5AFEA9… and ON_ShutLiningUserData 429DCD06…;
- ON_PerObjectMeshParameters B5628CA9…;
- ON_OBSOLETE_CCustomMeshUserData 69F27695…, which openNURBS converts to custom render-mesh settings at line 15847. cadmpeg does the same in objects.rs:1501-1640.

### 3.8 Minimal reader (pseudo-code)

```rust
const ANON: u32 = 0x4000_8000;
const UD: u32 = 0x0002_7FFD;
const UD_HDR: u32 = 0x0002_FFF9;
const CLASS_END: u32 = 0x8002_7FFF;
const USER_STRING_LIST: [u8; 16] = [0x29,0xDE,0x28,0xCE,0xC5,0xF4,0xAA,0x4F,0xA5,0x0A,0xC3,0xA6,0x84,0x9B,0x63,0x29];
const PAGE_SPACE_DMR: [u8; 16] = [0xE9,0x37,0xBB,0x9B,0x31,0x21,0xB8,0x4F,0xB9,0xC6,0x55,0x24,0x85,0x9B,0x98,0xB8];

struct Attrs {
    uuid: [u8; 16], layer: i32, name: String,
    material_index: i32, material_source: u8,
    color: [u8; 4], color_source: u8, mode: u8, visible: bool,
    groups: Vec<i32>, page_space: bool, viewport_id: [u8; 16],
    version: (u8, u8), truncated: bool,
}

// `body` = 0x02008072 (or 0x02008061) body WITHOUT its trailing 4-byte CRC.
fn read_attributes(body: &[u8], archive: u32) -> Result<Attrs> {
    let mut r = Reader::new(body, archive);           // chunk_header() reads u32 + (u32 | i64)
    let v = r.u8()?;
    let mut a = Attrs::default_with(layer = -1, material_index = -1, visible = true, version = (v >> 4, v & 15));
    match v >> 4 {
        2 => read_tagged(&mut r, v & 15, &mut a)?,
        1 => read_legacy(&mut r, v & 15, &mut a)?,
        _ => return Err(UnsupportedAttributesVersion(v)),
    }
    if a.mode & 0x0F == 1 { a.visible = false; }      // robust hidden rule
    Ok(a)                                              // remaining bytes are ignored
}

fn read_tagged(r: &mut Reader, minor: u8, a: &mut Attrs) -> Result<()> {
    a.uuid = r.uuid()?;
    a.layer = r.i32()?;
    let mut last = 0u8;
    loop {
        let id = r.u8()?;
        if id == 0 { return Ok(()); }
        if id <= last || id > 41 || minor < min_minor(id) { a.truncated = true; return Ok(()); }
        last = id;
        match id {
            1 => a.name = r.wstring()?,
            2 => { r.wstring()?; }
            3 | 10 | 22 | 30 => { r.i32()?; }
            4 => a.material_index = r.i32()?,
            5 | 38 | 39 => r.skip_chunk(ANON)?,        // header, then skip `value` bytes (CRC included)
            6 => a.color = r.bytes::<4>()?,
            7 | 34 => r.skip(4)?,
            8 | 31 | 32 | 33 => r.skip(8)?,
            11 => a.visible = r.bool1()?,              // 1 byte, 0 or 1
            12 => a.mode = r.u8()?,
            13 => a.color_source = r.u8()?,
            16 => a.material_source = r.u8()?,
            9 | 14 | 15 | 17 | 23..=27 | 29 | 37 | 40 => { r.u8()?; }
            35 | 41 => { r.bool1()?; }
            18 => { let n = r.i32()?.max(0); a.groups = (0..n).map(|_| r.i32()).collect::<Result<_>>()?; }
            19 => a.page_space = r.u8()? == 2,
            20 => a.viewport_id = r.uuid()?,
            21 => { let n = r.i32()?.max(0) as usize; r.skip(32 * n)?; }
            28 => { r.bool1()?; r.skip_chunk(ANON)?; }
            36 => r.skip(128)?,
            _ => unreachable!(),
        }
    }
}
// min_minor: 1..=21 -> 0, 22 -> 1, 23..=26 -> 2, 27..=28 -> 3, 29..=32 -> 4, 33 -> 5,
//            34..=35 -> 6, 36 -> 8, 37 -> 9, 38 -> 10, 39 -> 11, 40 -> 12, 41 -> 13

fn read_legacy(r: &mut Reader, minor: u8, a: &mut Attrs) -> Result<()> {
    a.uuid = r.uuid()?;
    a.layer = r.i32()?;
    a.material_index = r.i32()?;
    a.color = r.bytes::<4>()?;
    r.skip(2 + 2 + 8 + 8)?;                            // obsolete line style
    r.i32()?;                                          // wire density
    a.mode = r.u8()?;
    a.color_source = r.u8()?;
    r.u8()?;                                           // linetype source
    a.material_source = r.u8()?;
    a.name = r.wstring()?;
    r.wstring()?;                                      // URL
    a.visible = a.mode & 0x0F != 1;
    if minor >= 1 { let n = r.i32()?.max(0); a.groups = (0..n).map(|_| r.i32()).collect::<Result<_>>()?; }
    if minor >= 2 { a.visible = r.bool1()?; }
    if minor >= 3 { let n = r.i32()?.max(0) as usize; r.skip(32 * n)?; }
    if minor >= 4 { r.skip(4 + 1 + 4 + 1 + 8)?; }
    if minor >= 5 { r.i32()?; }                        // linetype index
    if minor >= 6 {
        a.page_space = r.u8()? == 1;
        for _ in 0..r.i32()?.max(0) {
            let (vp, dm) = (r.uuid()?, r.uuid()?);
            if dm == PAGE_SPACE_DMR { a.viewport_id = vp; }
        }
    }
    if minor >= 7 { r.skip_chunk(ANON)?; }
    Ok(())
}

// `body` = 0x02000073 (or 0x02000062) body; it has no CRC.
fn read_attribute_user_strings(body: &[u8], archive: u32) -> Result<Vec<(String, String)>> {
    let mut r = Reader::new(body, archive);
    let mut out = Vec::new();
    while !r.is_empty() {
        let (tc, len) = r.chunk_header()?;
        if tc == CLASS_END { break; }                  // short chunk, len == 0
        if tc != UD { r.skip(len)?; continue; }
        let mut u = r.sub(len)?;                        // takes `len` bytes; no CRC on 0x00027FFD
        let v = u.u8()?;
        let class_id = match v >> 4 {
            2 => {
                let (t, hl) = u.chunk_header()?;
                ensure(t == UD_HDR)?;
                let mut h = u.sub(hl)?;
                h.uuid()?                               // class id; item id and the rest are ignored
            }
            1 => { let c = u.uuid()?; u.skip(16 + 4 + 128)?; c }
            _ => continue,
        };
        let (t, ol) = u.chunk_header()?;
        ensure(t == ANON)?;
        if class_id != USER_STRING_LIST { continue; }  // other userdata: already bounded by `sub`
        let mut outer = u.sub(ol)?;
        let (t, il) = outer.chunk_header()?;
        ensure(t == ANON)?;
        let mut l = outer.sub(il - 4)?;                // inner list chunk always has a CRC
        ensure(l.i32()? == 1)?;
        l.i32()?;                                      // major, minor
        for _ in 0..l.i32()?.max(0) {
            let (t, el) = l.chunk_header()?;
            ensure(t == ANON)?;
            let mut e = l.sub(el)?;
            ensure(e.i32()? == 1)?;
            e.i32()?;
            let k = e.wstring()?;
            let val = e.wstring()?;                    // e's last 4 bytes are the CRC, ignored
            if !k.eq_ignore_ascii_case("$temp_object$") { out.push((k, val)); }
        }
    }
    Ok(out)
}
```

The class wrapper's 0x00027FFD records (geometry user strings) can be parsed with the same per-record code.

### 3.9 cadmpeg and openNURBS differences

1. **Choosing the reader.**
   - openNURBS decides by `archive >= 5 && writer >= 200712190` without looking at the byte (opennurbs_3dm_attributes.cpp:926-930), then requires major 2 or major 1.
   - cadmpeg decides by the major nibble. It rejects a major-1 payload when archive ≥ 5 and writer ≥ cutoff, or when minor > 8 (objects.rs:973-981). It rejects a major-2 payload when archive < 50 (objects.rs:1143-1147).
   - Effects: archive-5 files (the pre-50 Rhino 5 WIP band) with tagged attributes are rejected by cadmpeg and read by openNURBS. Legacy minor ≥ 9 is read by openNURBS as 1.7 and rejected by cadmpeg. Neither case occurs in the samples.
2. **Item 42** (detail background visible, bool, accepted from minor 13) exists only in cadmpeg (objects.rs:862, 909, 954, 1334-1337). openNURBS 8.35 knows items up to 41 and minor up to 13. It silently tolerates higher ids only when minor ≥ 14 (opennurbs_3dm_attributes.cpp:899-910).
3. **Unknown, out-of-order or too-new items.** Both keep the prefix already read and drop the rest. openNURBS raises `ON_ERROR`; cadmpeg stops silently (objects.rs:1219-1235). A missing 0x00 terminator is an error in cadmpeg (1369-1372); in openNURBS the read fails at the chunk boundary.
4. **Booleans.** cadmpeg's `bool_with_writer_version` (chunks.rs:30-33, 322-339) matches openNURBS `ReadBool`. It additionally treats YYYYMMDDn-style stamps from 201708240 to 999999999 as strict, whereas openNURBS compares the raw value with 2348836140 and is therefore permissive for every date-style stamp. This has no effect on real files.
5. **Strings.** openNURBS drops the last UTF-16 unit without checking it. cadmpeg requires it to be NUL and enforces size limits (settings.rs:979-1000).
6. **Counts.** openNURBS `ReadArray` treats n ≤ 0 as empty. cadmpeg rejects negative counts and caps them, for example at 65536 for groups (objects.rs:811-817; chunks.rs:412-442).
7. **Legacy 1.6 space.**
   - openNURBS maps byte 1 to page_space (2), otherwise model_space (1), and takes the page viewport id from the pseudo display-material entry.
   - cadmpeg stores the raw byte in `active_space` (0/1, a different coding from tagged item 19), leaves `viewport_id` nil, and falls back to the 1.3 list when the 1.6 list is empty (objects.rs:1044-1057, 1106-1110). openNURBS always replaces the list with the 1.6 one.
8. **Enum normalization.** In the legacy layout, openNURBS normalizes colour, linetype, material, plot-colour and plot-weight sources (unknown → layer); in the tagged layout it stores raw bytes. cadmpeg keeps raw values in both and flags invalid colour sources as `ColorSource::Invalid` (objects.rs:146-176).
9. **Legacy decoration.** openNURBS derives arrowheads from the obsolete line-style short when archive < 4 or writer < 200503170. cadmpeg keeps the short raw (`obsolete_line_style`) and sets decoration only from 1.4 onward (objects.rs:987-990, 1031-1042).
10. **Visibility and layers.** In openNURBS, `IsVisible` is `m_bVisible` alone. cadmpeg computes effective visibility as attribute visible AND layer visible (objects.rs:1682-1684). Its layer resolution by stored index treats an index that is missing or duplicated as unresolved; it has no notion of the −1 Default layer (objects.rs:1660-1681).
11. **Index remapping.** openNURBS remaps non-negative layer, material and group indices through the manifest when used by `ONX_Model`. cadmpeg keeps raw archive indices.
12. **Order of children** in the object record: see 3.1.

### 3.10 Samples (paths relative to `samples/`)

| Variant | Files (number of records) |
|---|---|
| Legacy 1.1, archive 2 (Rhino 2 writer 200107270) | `opennurbs/example_files/V2/*` from Rhino, e.g. v2_Stearman.3dm (126), v2_Flow3.3dm, v2_HumanHead2.3dm |
| Legacy 1.2, archives 2 and 3 | V2 `v2_my_*.3dm` (openNURBS 200210150); V3 `v3_*` (2488 records) |
| Legacy 1.7, archive 4 | `V4/*`: v4_example_file.3dm (28), v4_Prarie settle.3dm, v4_Wheel_PG.3dm |
| Legacy 1.0 or 1.8 | none |
| Tagged 2.1 | `V5/*`, `V6/*`, `V7/*`; v8_caps_and_joins.3dm |
| Tagged 2.3 / 2.4 | V8/v8_selective_clipping.3dm / V8/v8_section_hatch.3dm (items 29–32) |
| Tagged 2.10 / 2.11 | rhino3dm/models/RectifiedArchimedeanSolids8.3dm (archive 80); Rhino_Logo_Mesh7.3dm (archive 70) / rhino3dm/tests/DotNetTester/test_in.3dm (archive 70, item 36) |
| Tagged 2.13 | rhino3dm/tests/models/blocks.3dm, groups.3dm, object_UserStrings.3dm, subd.3dm, file3dm_stuff.3dm; decal_r8.3dm; MeshSphere_r5.3dm and nurbsSurface_r5.3dm (archive 50) |
| Tagged 2.14 (openNURBS 9.0 writer) | rhino3dm/tests/models/subd_creases.3dm (no items present; layer −1) |
| Mode 3 (instance-definition geometry) | V4/v4_example_file.3dm (3, legacy); rhino3dm/tests/models/blocks.3dm (4), file3dm_stuff.3dm (3) |
| Mode 1 hidden, visible = false | V2/v2_T-Joint1.3dm (1.1, visibility from mode); V3/v3_T-Joint1.3dm (1); V4/v4_Wheel_PG.3dm (11, which also exercise both display-material lists) |
| Mode 2 locked | V5/v5_extrusions.3dm (10) |
| Material source = object, tagged | v5_disk_brake (66), v5_dril (23), v5_clip (16), v5_extrusions (13), v5_rhino_logo, v5_teapot, V6/V7 `*_my_mesh_with_material`, `*_my_spot_light` (light record) |
| Material source = object, legacy | v4_example_file, v4_DinerMug, v4_Fluted_Cup, v4_PerfumeBottle_Solid, v4_RhinoPhone; many V2/V3 files, e.g. v2_Soccer (indices up to 1046) |
| Item 4 without item 16 | V5/v5_teacup.3dm (4) |
| Colour source = object | v4_Fluted_Cup, v5_dril, v5_teacup, v6_rhino_subd_logo, v7_rhino_logo_annotations, v7_rhino_logo_nurbs, v7_rhino_logo_subd, three.js Rhino_Logo |
| Groups, legacy | v3_my_points, v4_Prarie settle, v4_example_file |
| Groups, tagged | rhino3dm/tests/models/groups.3dm (up to 5 groups per object), file3dm_stuff.3dm (14), v5/v6/v7 rhino_logo, v5_disk_brake, `*_my_points` |
| Names | RectifiedArchimedeanSolids(8).3dm (1811 each); `*_my_curves`, `*_my_points`, etc. |
| Page space (items 19, 20) | V5/v5_example_file.3dm (5) |
| Rendering attributes | legacy 1.7: every V4 record; tagged item 5: decal_r8.3dm, rhino3dm/tests/models/subd.3dm |
| Item 3, 8, 15, 17 | V8/v8_caps_and_joins.3dm |
| Attribute user strings | object_UserStrings.3dm (5 keys, including `%<Volume(...)>%` text fields); RectifiedArchimedeanSolids(8).3dm; blocks.3dm and file3dm_stuff.3dm (`$block-instance-original-object-id$`); three.js Rhino_Logo.3dm (empty lists) |
| Geometry user strings (class wrapper) | RectifiedArchimedeanSolids(8).3dm (11 objects) |
| Other attribute userdata | AllMeshModifiers.3dm and subd_creases.3dm (5 mesh-modifier records each); sphereDecals.3dm and decal_r8.3dm (ON_RdkUserData); v4_DinerMug.3dm (V4, header 2.1) |
| Empty 0x02000073 (end marker only) | 136 V4 records, e.g. v4_Fluted_Cup.3dm and v4_Gear.3dm; v5_rhino_logo.3dm (6) |
| Layer −1 | V7/v7_my_surfaces.3dm, subd_creases.3dm, SampleCSJoinCurves/newFile.3dm, Rhino_Logo_Mesh7.3dm |
| Light-record attributes (0x02008061) | v2/v3 Camera, MatchSrf, SketchOnMesh, Soccer, `*_my_spot_light` (legacy); V5–V8 `*_my_spot_light`, light.3dm (tagged) |

### 3.11 Uncertain

- **Chunk 2.14 and item 42** (Rhino 9). Only one 2.14 sample exists and it carries no items. The openNURBS source on disk (8.35) does not define 2.14 or item 42. cadmpeg's item 42 is accepted from minor 13; that gate cannot be confirmed here.
- **Not observed in any sample.** These encodings come from the openNURBS source only:
  - legacy 1.0 and 1.8;
  - the archive-5 band;
  - items 2, 7, 9, 11, 14, 21, 22, 23–28, 33–35 and 37–41;
  - a hidden object in a tagged (V5+) file, so it is unconfirmed whether Rhino always writes item 11 alongside mode 1.
- **Index remapping.** Matching layer, group and material indices against the index stored in each table record assumes that `ONX_Model`'s manifest remapping is the identity for well-formed files.
- **Per-plug-in material references** (`ON_MaterialRef`) inside the rendering attributes. No sample record decoded here had any; the per-object assignment relied on above is item 4 together with item 16.


---

## 4. Meshes (ON_Mesh::Read)

Sources for this section:

- **cadmpeg**: cadmpeg-codec-rhino 0.6.1, paths relative to its `src/`.
- **openNURBS**: commit eb92af3, repository root (github.com/mcneel/opennurbs, branch 8.x).
- **Probes**: every claim marked "probed" was checked byte by byte on the samples with `probe_mesh.py`. Its output is `out2.tsv`, one row per ON_Mesh or ON_PointCloud payload found anywhere in a file, nested ones included. The public samples plus `synthetic/` give 7,967 mesh payloads in 70 files. All of them parsed to exactly the end of their class-data chunk (0 bytes left over), with 0 CRC or size anomalies.

### 4.0 Where the payload sits

- **Class UUID.** `ON_Mesh` is `4ED7D4E4-E947-11d3-BFE5-0010830122F0` (confirmed: openNURBS `ON_OBJECT_IMPLEMENT(ON_Mesh…)` at `opennurbs_mesh.cpp:127`; cadmpeg `mesh.rs:70-72`).
  - It has no alias UUID. The alias table in `ON_ClassId::ClassId`, `opennurbs_object.cpp:892-935`, maps only old NURBS curves and surfaces, polycurves, Breps, rev and sum surfaces.
  - On disk the UUID is u32 LE, u16 LE, u16 LE, then 8 raw bytes (`ON_BinaryArchive::ReadUuid`, `opennurbs_archive.cpp:1445-1452`). The 16 bytes are `e4 d4 d7 4e 47 e9 d3 11 bf e5 00 10 83 01 22 f0`.
- **The payload.** `ON_Mesh::Read` (`opennurbs_mesh.cpp:2521-2786`) parses the body of the `0x0002FFFC` class-data chunk, with the CRC trailer excluded. Everything below is offset from the start of that body.
- **The same routine serves nested meshes.**
  - Brep render and analysis meshes are stored per face through `ReadObject`, which uses a full `0x00027FFA` class wrapper (`ON_Brep::Read`, `opennurbs_brep_io.cpp:1000-1075`).
  - Extrusion mesh caches go through `ON_MeshCache::Read` (`opennurbs_mesh.cpp:16588`).
  - cadmpeg calls `mesh::decode` for top-level meshes (`decode.rs:935`), Brep render meshes (`decode.rs:5217`, slots built in `brep.rs:997`) and extrusion caches (`extrusion.rs:848`, `extrusion.rs:934`).
  - In the V4 samples, 3,094 of the 3,096 mesh payloads are nested Brep render meshes.
- **Every nested chunk follows the archive's chunk-length width.** That means the mesh-parameter, curvature-stat, zlib, mapping-tag, n-gon and double-vertex chunks all have a u32 length for archive < 50 and an i64 length for archive >= 50. All of them are long chunks with typecode `TCODE_ANONYMOUS_CHUNK = 0x40008000`, which has the CRC bit set (`opennurbs_3dm.h:82-87`). Each ends in a u32 CRC32 counted inside its length.

### 4.1 Primitive notation used below

| openNURBS call | Rust | Bytes |
|---|---|---|
| `Read3dmChunkVersion` (`opennurbs_archive.cpp:5771-5785`) | `u8`; major = `v >> 4`, minor = `v & 0x0F` | 1 |
| `ReadChar` / `ReadBool` | `u8` (bool: 0/1; values above 1 count as true for writers older than 2017-08-24, otherwise an error: `ReadBool`, `opennurbs_archive.cpp:2784-2810`) | 1 |
| `ReadInt` | `i32` LE (`u32` where the target is unsigned) | 4 |
| `ReadSize` (`opennurbs_archive.cpp:3084-3091`) | `u32` LE, **always 4 bytes, even in archive >= 50** | 4 |
| `ReadFloat(n)` / `ReadDouble(n)` | `n × f32` / `n × f64` LE | 4n / 8n |
| `ReadInterval` (`opennurbs_archive.cpp:1439-1442`) | `[f64; 2]` | 16 |
| `ReadBoundingBox` (`:1294-1299`) | min `[f64;3]`, max `[f64;3]` | 48 |
| `ReadXform` (`:1308`) | `[f64; 16]`, row-major | 128 |
| `ReadUuid` | see 4.0 | 16 |
| `ReadArray(ON_SimpleArray<T>)` (`:2311-2481`, `:2718`) | `i32 count`, then `count × sizeof(T)` raw LE; count <= 0 means empty | 4 + n·s |
| `ReadArray<ON_Color>` (`:2311-2323`) | `i32 count` + `count × 4` raw bytes (`ReadByte`, no endian swap) | |
| `BeginRead3dmChunk(TCODE_ANONYMOUS_CHUNK,&maj,&min)` (`:6396-6470`) | chunk header, then `i32 major` (>= 1) and `i32 minor` (>= 0) as the first 8 body bytes | |
| `BeginRead3dmBigChunk` | chunk header only, no version ints | |
| **compressed buffer** (`ReadCompressedBufferSize` + `ReadCompressedBuffer`, `opennurbs_zlib.cpp:161-224`) | `u32 size`; if size is 0, nothing follows. Otherwise `u32 crc32(uncompressed)`, `u8 method`. Method 0: `size` raw bytes. Method 1: one anonymous chunk `0x40008000` whose body (length − 4) is a single zlib (RFC 1950) stream inflating to exactly `size` bytes, then the chunk CRC32 of the compressed bytes (`ReadInflate`, `opennurbs_zlib.cpp:412-470`). | |

Rules for the compressed buffer:

- **Which method the writer picks.** It uses method 1 only when the archive's buffer-compression flag is on *and* size > 128 bytes (`WriteCompressedBuffer`, `opennurbs_zlib.cpp:126-129`). A reader must accept both methods at any size and in any archive version.
  - Rhino-written files compress large arrays.
  - Several rhino3dm-written samples store megabyte arrays with method 0 (for example `Rhino_Logo_Mesh8_NamedViews.3dm`, which stores a 1 MB vertex buffer raw).
- **Byte order of the uncompressed bytes.** They are always little-endian, laid out as an array of structs. Big-endian writers swap before compressing (`Write_2`, `opennurbs_mesh.cpp:1800-1825`).

### 4.2 Version byte, and which writers emit what

`ON_Mesh::Read` accepts major 1 (uncompressed vertex arrays, "Read_1") or major 3 (compressed, "Read_2"). The writer comment at `opennurbs_mesh.cpp:2167-2169` says major 2 was a beta format that was never used. For any other major, openNURBS returns `true` with an empty mesh (`opennurbs_mesh.cpp:2528-2530`).

The current writer (`ON_Mesh::Write`, `opennurbs_mesh.cpp:2164-2176`) always emits major 3: minor 8 when the archive is >= 60, minor 5 otherwise.

Observed in the samples (probed; "writer stamp" is the properties `0xA0000026` value = `ArchiveOpenNURBSVersion()`):

| Archive | Mesh version seen | Writer stamps seen | Meshes / files |
|---|---|---|---|
| 2 | 3.2 | 200107270, 200210150 | 1207 / 5 |
| 3 | 3.2 | 200210150, 200211120, 200308080 | 1217 / 8 |
| 4 | 3.4 | 200607130, 200710174, 200710309 | 3096 / 15 |
| 50 | 3.5 | 201101285 … 202308218 (a modern writer saving V5) | 2176 / 11 |
| 60 | 3.7 | 2348834660, 2348834708 (early V6 builds) | 9 / 3 |
| 60 | 3.8 | 2348836381 … | 34 / 3 |
| 70 | 3.8 | 2382393748 … 2425133316 | 126 / 9 |
| 80 | 3.8 | 2415951020 … 2425133316 | 102 / 16 |

- **Not seen in the samples:** major 1, and minors 3.0, 3.1, 3.3 and 3.6. Their layouts below come from the source only.
- **The minor follows the writer's build, not the archive number.** A recent openNURBS saving archive 2-4 (permitted by `ONX_Model::Write`, `opennurbs_extensions.cpp:3111-3115`) would emit 3.5.
- **Private files.** The project's `samples/private/` files hold 2,197 meshes, all 3.8 and all clean (not named here).

### 4.3 Common header (every version)

| Offset | Field (openNURBS) | Read call | Rust | Notes |
|---|---|---|---|---|
| +0 | version | `Read3dmChunkVersion` | `u8` | `0x32`, `0x34`, `0x35`, `0x37`, `0x38` observed |
| +1 | `vcount` | `ReadInt` | `i32` | written from `VertexUnsignedCount()`, read as signed |
| +5 | `fcount` | `ReadInt` | `i32` | |
| +9 | `m_packed_tex_domain[0]` | `ReadInterval` | `[f64;2]` | often ON_UNSET_VALUE (−1.23432101234321e308) |
| +25 | `m_packed_tex_domain[1]` | `ReadInterval` | `[f64;2]` | |
| +41 | `m_srf_domain[0]` | `ReadInterval` | `[f64;2]` | |
| +57 | `m_srf_domain[1]` | `ReadInterval` | `[f64;2]` | |
| +73 | `m_srf_scale[2]` | `ReadDouble(2)` | `[f64;2]` | |
| +89 | legacy float vertex bbox | `ReadFloat(6)` | `[f32;6]` | min xyz, max xyz, rounded outward to f32; `(1,1,1,−1,−1,−1)` means empty (`opennurbs_mesh.cpp:2186-2198`, `2543-2550`) |
| +113 | `m_nbox` | `ReadFloat(6)` | `[f32;6]` | normal bbox |
| +137 | `m_tbox` | `ReadFloat(4)` | `[f32;4]` | texture-coordinate bbox |
| +153 | closed | `ReadInt` | `i32` | −1 unknown, 0 open, 1 closed, 2 obsolete (means closed) (`:2555-2573`) |
| +157 | has mesh parameters | `ReadChar` | `u8` | nonzero: an anonymous chunk follows (`BeginRead3dmBigChunk`, typecode must be `0x40008000`; `:2574-2593`) |
| … | `ON_MeshParameters` chunk | | chunk | layout in 4.4; skip it by its length |
| … | 4 × { has kstat `u8`; [anonymous chunk with `ON_MeshCurvatureStats`] } | `ReadChar` + `BeginRead3dmBigChunk` | | `:2595-2618`. No sample sets any of the four. |
| … | face index width `w` | `ReadInt` | `i32` | 1, 2 or 4 (`ReadFaceArray`, `:1715-1757`) |
| … | faces | `ReadChar(4)` / `ReadShort(4)` / `ReadInt(4)` per face | `fcount × 4 × w` | 4.5 |
| … | vertex attribute block | | | 4.6 |
| … | version-dependent tail | | | 4.7 |

The fixed part is always 157 bytes, so the mesh-parameter flag sits at +157. This was probed on `V2/v2_my_mesh.3dm` and `V7/v7_my_mesh.3dm`; see the annotated dump in 4.10.

### 4.4 Optional sub-records (skippable by chunk length)

**ON_MeshParameters** (`ON_MeshParameters::Read`, `opennurbs_mesh.cpp:7895-7985`). The chunk is written by `BeginWrite3dmChunk(TCODE_ANONYMOUS_CHUNK, 0)`, so it carries *no* i32 version pair. The body starts with a `Read3dmChunkVersion` byte; major must be 1.

| Since | Fields | Cumulative body size (incl. version byte) |
|---|---|---|
| 1.0 | `i32` bComputeCurvature, `i32` bSimplePlanes, `i32` bRefine, `i32` bJaggedSeams, `i32` obsolete bWeld; `f64` tolerance, `f64` min_edge_length, `f64` max_edge_length, `f64` grid_aspect_ratio; `i32` grid_min_count, `i32` grid_max_count; `f64` grid_angle_radians, `f64` grid_amplification, `f64` refine_angle_radians, `f64` obsolete combine_angle; `i32` face_type | 97 |
| 1.1 | `u32` texture_range | 101 (probed, `v3_Smooth.3dm`) |
| 1.2 | `u8` bool bCustomSettings, `f64` relative_tolerance | 110 |
| 1.3 | `u8` mesher | 111 |
| 1.4 | `u8` bool bCustomSettingsEnabled | 112 |
| 1.5 | `ON_SubDDisplayParameters` in its own anonymous chunk: `i32 1`, `i32 ver`, `u32` display_density, `u32` mesh_location, [ver >= 2] `u8` bool absolute, [ver >= 3] `u8` bool compute_curvature (`:8010-8060`) | 144 in `three.js/…/Rhino_Logo.3dm` (inner chunk version 1.1: two u32 only) |

Versions seen: 1.1 in V3 files; 1.2 in V4; 1.3 in `v5_example_file.3dm`; 1.4 in V5 files and also in some V6/V7 files; 1.5 in V8 files and `three.js/…/Rhino_Logo.3dm`.

**ON_MeshCurvatureStats** (`opennurbs_mesh.cpp:8123-8140`). Body: `u8` version (1.1 written), `i32` style, `f64` infinity, `i32` count_infinite, `i32` count, `f64` mode, `f64` average, `f64` adev, `[f64;2]` range, 61 bytes in total. Not exercised by any sample.

### 4.5 Face array

- **Stored width.** `w` is chosen by the writer from `vcount`: below 256 it is 1 (u8), below 65,536 it is 2 (u16), otherwise 4 (`WriteFaceArray`, `opennurbs_mesh.cpp:1666-1712`). Read the stored `w`; do not re-derive it.
  - 1- and 2-byte indices are unsigned.
  - 4-byte indices are written from `int` and should be read as `u32`, so a negative index becomes huge and therefore out of range.
- **Each face is 4 indices `vi[0..4]`.** A triangle is stored with `vi[2] == vi[3]` (`ON_MeshFace::IsTriangle`, `opennurbs_mesh.cpp:5126-5135`; header comment `opennurbs_mesh.h:1540`). Anything else is a quad. Probed example `v7_my_mesh.3dm`: `01 02 00 00` is triangle (1,2,0); `05 08 07 06` is quad (5,8,7,6).
- **openNURBS does not validate indices when reading.** Validity (`ON_MeshFace::IsValid`, `opennurbs_mesh.cpp:167-187`) requires all four indices < vcount, `vi[0..3]` pairwise distinct, and for a quad `vi[3]` distinct from `vi[0]` and `vi[1]`. `ON_MeshFace::Repair` (`:238-270`) collapses duplicate indices into a triangle.
- **If `w` is not 1, 2 or 4,** openNURBS reads zero faces, returns success, and leaves the stream misaligned. Treat that as an error.
- **Splitting quads.** openNURBS `ConvertQuadsToTriangles` splits along the shorter diagonal (`opennurbs_mesh.cpp:8582-8595`, documented at `opennurbs_mesh.h:4110-4116`). cadmpeg does the same (`mesh.rs:788-846`): if |P0P2| <= |P1P3| the triangles are (0,1,2) and (0,2,3); otherwise (0,1,3) and (1,2,3).
- **The face array comes before the vertices.** You must walk past it, or record its offset, before you can reach the positions.

### 4.6 Vertex attribute block

**Major 3 ("Read_2", `opennurbs_mesh.cpp:1842-1957`).** Present only when `vcount > 0`. When `vcount == 0`, *zero bytes* are written (`Write_2`, `:1792`) and none are read. The block is five compressed buffers, always in this order:

| # | Array | Uncompressed size | Element layout (LE) |
|---|---|---|---|
| 1 | `m_V` positions | `vcount×12` | `[f32;3]` x, y, z |
| 2 | `m_N` vertex normals | `vcount×12` or 0 | `[f32;3]` |
| 3 | `m_T` texture coordinates | `vcount×8` or 0 | `[f32;2]` u, v |
| 4 | `m_K` principal curvatures | `vcount×16` or 0 | `[f64;2]` k1, k2 (`ON_SurfaceCurvature`) |
| 5 | `m_C` vertex colours | `vcount×4` or 0 | bytes R, G, B, A; A is 0 = opaque, 255 = transparent; as a LE u32 this is `0xAABBGGRR` (`opennurbs_color.h:56-63`, `:208`). Probed on `rhino3dm/tests/models/mesh.3dm`: byte 3 is 0 everywhere. |

- **Absent arrays.** The writer writes size 0 (a single u32, 4 bytes) for any array whose count differs from the vertex count (`Write_2`, `:1795-1799`).
- **Wrong sizes are fatal in openNURBS.** A non-zero size other than `vcount × elt` fails the whole `ON_Mesh::Read` (`:1862-1868` and the matching checks).
- **CRC mismatches are not.** openNURBS keeps the data and only raises `bFailedCRC` (`opennurbs_zlib.cpp:208-219`).

**Major 1 ("Read_1", `opennurbs_mesh.cpp:1772-1783`).** Five `ReadArray` calls, uncompressed and in the same order:

`i32 nV` + `nV×12`; `i32 nN` + `nN×12`; `i32 nT` + `nT×8`; `i32 nK` + `nK×16`; `i32 nC` + `nC×4`.

openNURBS takes the arrays as they are; their counts are not compared with `vcount`. No sample has major 1.

### 4.7 Tail, by minor version (read in this order)

| Gate | Field | Read call | Rust | Notes |
|---|---|---|---|---|
| minor >= 2 (major 1 or 3) | `m_packed_tex_rotate` | `ReadInt` | `i32` | 0/1 (`:2633-2638`) |
| major 3, minor >= 3 | `m_Ttag.m_mapping_id` | `ReadUuid` | 16 bytes | (`:2645`) |
| major 3, minor >= 3, **vcount > 0** | `m_S` surface parameters | compressed buffer | `vcount×16`: `[f64;2]` s, t | (`:2647-2690`). A wrong-size buffer is read and discarded if writer stamp <= 201011049 and size % 16 == 0 (known old bug); otherwise the read fails. |
| major 3, minor >= 4 **and** writer stamp >= 200606010 | `ON_MappingTag` | anonymous chunk with version | body: `i32 1`, `i32 minor`, uuid mapping_id, `u32` mapping_crc, `[f64;16]` mesh_xform, [minor >= 1] `u32` mapping_type | `ON_MappingTag::Read`, `:13604-13645`. 160-byte body plus 4-byte CRC: length 164 probed. If the stamp gate fails, nothing past S is read and `EndRead3dmChunk` skips the rest (`:2692`). |
| … minor >= 5 | manifold, oriented, solid | `ReadChar ×3` | `[u8;3]` | 0 unset; manifold and oriented: 1 yes, 2 no; solid: 1 outward, 2 inward, **3 not solid** (`opennurbs_mesh.h:6187-6190`) |
| … minor >= 6 | has n-gons | `ReadBool` | `u8` | if 1: n-gon chunk (below) |
| … minor >= 7 | has double-precision vertices | `ReadBool` | `u8` | if 1: double-vertex chunk (below) |
| … minor >= 8 | `m_vertex_bbox` | `ReadBoundingBox` | `[f64;6]` | may be ON_UNSET_VALUE ×6 (probed: `v7_my_mesh.3dm`, `Rhino_Logo_Mesh7.3dm`). Do not trust it. |

After the tail there is nothing else; the class-data chunk ends. Old meshes with T but no S get S reconstructed from T in memory (`:2755-2782`); that has no wire effect.

**N-gon chunk** (`ReadMeshNgons`, `opennurbs_mesh.cpp:2005-2050`). Anonymous chunk with version (1.0 written):

- `u32 ngon_count`, then per n-gon:
  - `u32 Vcount`. If 0, move to the next n-gon; **no Fcount follows**.
  - `u32 Fcount`.
  - `Vcount × u32` boundary vertex indices, then `Fcount × u32` face indices.
- N-gons only *group* faces that are already present in the face array. They can be ignored for geometry. Probed: `RectifiedArchimedeanSolids*.3dm` has n-gons of 3 to 10 sides covering every face.

**Double-vertex chunk** (`ReadMeshDoublePrecisionVertices`, `opennurbs_mesh.cpp:2101-2161`). Anonymous chunk with version; major must be 1 or the whole mesh read fails.

- `u32 dVcount`; if 0, done.
- Then one compressed buffer whose size must equal exactly `dVcount×24`, holding `[f64;3]` per vertex.
- openNURBS keeps `m_dV` only if `dVcount == vcount` and, for every vertex, `|f32(dV) − V|max <= 1e-6 × |V|max`. Otherwise it drops `m_dV` (`:2713-2740`).
- Probed: in all 173 public payloads with this chunk, `f32(dV)` equals `V` bit-for-bit. The maximum relative difference is 5.8e-8 (3.5e-8 in the private files).

**Compressed buffers in one major-3 record, in stream order:** V, N, T, K, C (only when `vcount > 0`), then S (minor >= 3, `vcount > 0`), then dV (minor >= 7, inside the double-vertex chunk). Nothing else in the mesh record is compressed.

### 4.8 Out-of-band carriers in the class wrapper (V4/V5 era)

These live in `0x00027FFD` user-data chunks that follow `0x0002FFFC` inside the same `0x00027FFA` wrapper. Framing (`opennurbs_archive.cpp:4947-5000`): `0x00027FFD { u8 0x22 ; 0x0002FFF9 header {uuid class, uuid item, i32 copycount, [f64;16] xform, uuid app, u8 unknown-flag, i32 3dm version, u32 writer stamp} ; 0x40008000 { class payload } }`.

- **`ON_V5_MeshDoubleVertices`** (`17F24E75-21BE-4a7b-9F3D-7F85225247E3`; `opennurbs_mesh.cpp:13655`, `Read` at `:13771-13805`).
  - Written only to archive-50 files, and only when the doubles are exactly synchronised with the floats (`ON_Mesh::Write`, `:2318-2335`).
  - Payload: an inner anonymous chunk with version (1.0) holding `i32 fcount`, `i32 dcount`, `u32 fCRC`, `u32 dCRC`, then `ReadArray<ON_3dPoint>` = `i32 n` + `n×24` **uncompressed** f64.
  - Adopted only if `n == vcount` and `f32(d) == V` exactly (`DeleteAfterRead`, `:13717-13740`; `HasSynchronizedDoubleAndSinglePrecisionVertices`, `:13972-13998`).
  - Probed on `rhino-developer-samples/rhino3dm/models/MeshSphere_r5.3dm` (92 vertices).
- **`ON_V4V5_MeshNgonUserData`** (`31F55AA3-71FB-49f5-A975-757584D937FF`; `opennurbs_mesh_ngon.cpp:2298-2332`, `Read` at `:2456-2520`).
  - Payload: anonymous chunk with version 1.x holding `i32 count`, then per n-gon `i32 N`, [N > 0] `N×i32` vi and `N×i32` fi (fi may be `-1`-padded), then [minor >= 1] `i32` mesh F count, `i32` mesh V count.
  - No sample has it.

### 4.9 What is *not* in the record

None of these are serialised by `ON_Mesh::Write` (`:2164-2340`): face normals (`m_FN`), hidden-vertex flags (`m_H`), extra texture-coordinate sets (`m_TC`; they are regenerated from mappings and `m_Ttag`), topology (`m_top`), and the n-gon map. Compute face normals and topology from positions and faces.

### 4.10 Annotated example (probed, `opennurbs/example_files/V7/v7_my_mesh.3dm`)

Archive 70; object class-data body at file offset 3401, length 778 including CRC.

```
+0000 38                       version 3.8
+0001 09000000 05000000        vcount 9, fcount 5
+0009 4 x ON_Interval          all ON_UNSET_VALUE (d21d339ebdf8e5ff)
+0073 srf_scale                0.0, 0.0
+0089 float vbox / nbox / tbox (1,1,1,-1,-1,-1) = empty
+0153 ffffffff                 closed = -1
+0157 00 | 00 00 00 00         no mesh params, no kstats
+0162 01000000                 index width 1
+0166 01020000 02030000 03040000 04010000 05080706   4 triangles, 1 quad
+0186 V : size 108, crc, method 0, 108 raw bytes
+0303 N : size 108, crc, method 0, 108 raw bytes
+0420 T,K,C : 00000000 x3      absent
+0432 00000000                 packed_tex_rotate
+0436 16 x 00                  mapping id (nil)
+0452 00000000                 S absent
+0456 chunk 40008000 len 164   ON_MappingTag 1.1
+0632 00 00 00                 manifold/oriented/solid
+0635 00                       no n-gons
+0636 01                       has doubles -> chunk 40008000 len 77: 1,0, count 9, buffer 216 bytes method 1
+0726 6 x f64                  vertex bbox (unset)
end (0 bytes left)
```

`V2/v2_my_mesh.3dm` (3.2) is byte-identical up to +0436, where its payload ends after `packed_tex_rotate`.

### 4.11 Minimal "positions + triangles" reader

Everything after the face array can be skipped without inflating anything:

- Chunks carry their length.
- Method-0 buffers carry their size.
- Method-1 buffers are one chunk.
- The class-data chunk bounds the record.

So a reader can stop right after V, or walk cheaply to the double-vertex chunk.

```rust
// p = body of the 0x0002FFFC chunk (CRC excluded); av = archive version; stamp = writer stamp (0xA0000026)
fn read_mesh(p: &[u8], av: u32, stamp: u32) -> Result<(Vec<[f64;3]>, Vec<[u32;3]>)> {
    let mut r = Cur::new(p);
    let v = r.u8()?; let (major, minor) = (v >> 4, v & 0xF);
    if major != 1 && major != 3 { return Err(Unsupported) }
    let vcount = usize::try_from(r.i32()?)?;          // reject negative
    let fcount = usize::try_from(r.i32()?)?;
    r.skip(4*16 + 16 + 24 + 24 + 16 + 4)?;            // intervals, srf_scale, 3 legacy boxes, closed  (= 148 bytes: 157 - 9 already read)
    if r.u8()? != 0 { skip_chunk(&mut r, av)? }       // ON_MeshParameters
    for _ in 0..4 { if r.u8()? != 0 { skip_chunk(&mut r, av)? } } // ON_MeshCurvatureStats
    let w = r.i32()?; if !matches!(w, 1 | 2 | 4) { return Err(Bad) }
    let faces = r.take(fcount.checked_mul(4 * w as usize)?)?;   // decode after positions

    let vf: Vec<[f32;3]> = if major == 1 {
        let n = r.i32()?.max(0) as usize; let b = r.take(n * 12)?;  // ReadArray<ON_3fPoint>
        f32x3(b)                                                    // N,T,K,C arrays follow; not needed
    } else if vcount > 0 {
        f32x3(&read_buffer(&mut r, av, vcount * 12)?)               // inflate V only
    } else { Vec::new() };
    if vf.len() != vcount { return Err(Bad) }

    // Optional: precise positions (3.7+). Walk the tail without inflating anything.
    let mut vd: Option<Vec<[f64;3]>> = None;
    if major == 3 && minor >= 7 && stamp >= 200_606_010 {
        if vcount > 0 { for _ in 0..4 { skip_buffer(&mut r, av)? } }  // N,T,K,C
        r.skip(4)?;                                        // packed_tex_rotate (minor >= 2)
        r.skip(16)?;                                       // mapping id
        if vcount > 0 { skip_buffer(&mut r, av)? }         // S
        skip_chunk(&mut r, av)?;                           // ON_MappingTag
        r.skip(3)?;                                        // manifold/oriented/solid
        if r.u8()? != 0 { skip_chunk(&mut r, av)? }        // n-gons
        if r.u8()? != 0 {
            let mut c = chunk_body(&mut r, av)?;           // anonymous chunk
            let (maj, _min, n) = (c.i32()?, c.i32()?, c.u32()? as usize);
            if maj == 1 && n == vcount && n > 0 {
                let d = f64x3(&read_buffer(&mut c, av, n * 24)?);
                if synced(&d, &vf) { vd = Some(d) }        // f32(d) == f (or the 1e-6 relative test)
            }
        }
    }
    // Archive 50: also try ON_V5_MeshDoubleVertices user data in the class wrapper (4.8).
    let pos: Vec<[f64;3]> = vd.unwrap_or_else(|| vf.iter().map(|p| p.map(f64::from)).collect());

    let mut tris = Vec::with_capacity(fcount * 2);
    for f in faces.chunks_exact(4 * w as usize) {
        let [a, b, c, d] = idx4(f, w);                     // u8/u16/u32 LE
        if [a, b, c, d].iter().any(|&i| i as usize >= vcount) { continue }  // drop, count it
        // ON_MeshFace::Repair semantics (opennurbs_mesh.cpp:238): keep first occurrences in order
        let mut u: Vec<u32> = Vec::with_capacity(4);
        for i in [a, b, c, d] { if !u.contains(&i) { u.push(i) } }
        if u.len() < 3 { continue }                        // degenerate: drop, count it
        if u.len() == 3 { tris.push([u[0], u[1], u[2]]); continue }   // includes the c == d triangles
        let (a, b, c, d) = (u[0], u[1], u[2], u[3]);
        let (p0, p1, p2, p3) = (pos[a as usize], pos[b as usize], pos[c as usize], pos[d as usize]);
        if dist(p0, p2) <= dist(p1, p3) { tris.push([a, b, c]); tris.push([a, c, d]) }
        else                            { tris.push([a, b, d]); tris.push([b, c, d]) }
    }
    Ok((pos, tris))
}
fn skip_buffer(r: &mut Cur, av: u32) -> Result<()> {
    let size = r.u32()?; if size == 0 { return Ok(()) }
    r.skip(4)?;                                            // crc32 of uncompressed bytes
    match r.u8()? { 0 => r.skip(size as usize), 1 => skip_chunk(r, av), _ => Err(Bad) }
}
fn skip_chunk(r: &mut Cur, av: u32) -> Result<()> {
    let _tc = r.u32()?;                                    // expect 0x40008000 (long, CRC inside length)
    let len = if av >= 50 { r.i64()? as usize } else { r.u32()? as usize };
    r.skip(len)
}
// read_buffer: size, crc, method; method 0 = take(size); method 1 = zlib-inflate the chunk body
// (length-4 bytes) to exactly `size` bytes; check size == expected and crc32(out) == crc.
```

Notes on the minimal reader:

- **Stop early.** A reader that needs only float positions can stop after V and jump to the end of the class-data chunk.
- **Prefer double precision** (the 3.7+ chunk, or the V5 user data) when it is present and synchronised. Float positions lose millimetre precision at georeferenced coordinates (about 1e7 in model units).
- **Per-face error policy.** Drop invalid faces rather than the mesh: `Rhino_Logo_Mesh7.3dm` (4.14) depends on it.

### 4.12 ON_PointCloud::Read (closely related)

Class `2488F347-F8FA-11d3-BFEC-0010830122F0` (`opennurbs_pointcloud.cpp:24`); `Read` at `opennurbs_pointcloud.cpp:195-217`; cadmpeg `read_cloud`, `curves.rs:1476-1554`. Nothing is compressed.

| Since | Field | Read call | Rust |
|---|---|---|---|
| 1.0 | version | `Read3dmChunkVersion` | `u8` (major must be 1) |
| 1.0 | `m_P` | `ReadArray<ON_3dPoint>` | `i32 n` + `n × [f64;3]` |
| 1.0 | `m_plane` | `ReadPlane` (`opennurbs_archive.cpp:1338`) | origin, x, y, z axes (`[f64;3]` each), equation `[f64;4]` = 128 bytes |
| 1.0 | `m_bbox` | `ReadBoundingBox` | `[f64;6]` |
| 1.0 | `m_flags` | `ReadInt` | `u32` (bit 1 = ordered) |
| 1.1 | `m_N` | `ReadArray<ON_3dVector>` | `i32` + `n × [f64;3]` |
| 1.1 | `m_C` | `ReadArray<ON_Color>` | `i32` + `n × 4` bytes RGBA |
| 1.2 | `m_V` (point values) | `ReadArray<double>` | `i32` + `n × f64` |

- **Not stored:** hidden flags `m_H`.
- **No point clouds before archive 3.** For archive <= 2 the writer emits one `ON_Point` per point instead (`Write3dmObject`, `opennurbs_archive.cpp:13210-13226`).
- **Probed:** 1.0 in `V3/v3_my_points.3dm`; 1.1 in `V4/v4_example_file.3dm`, `V5/v5_example_file.3dm`, `V5/v5_my_points.3dm`; 1.2 in `V6/my_points.3dm`, `V7/v7_my_points.3dm`, `V8/v8_selective_clipping.3dm` (3 clouds of 92), `rhino3dm/tests/models/file3dm_stuff.3dm`, `three.js/examples/models/3dm/Rhino_Logo.3dm` (848 points). None has normals, colours or values.

### 4.13 cadmpeg vs openNURBS differences

1. **Version acceptance** (`mesh.rs:276-290`). cadmpeg rejects major 0, 2 and > 3 with an error, and rejects major 3 with minor > 5 in an archive-50 file. openNURBS returns an empty mesh for unknown majors and applies no archive/minor check.
2. **Count caps** (`mesh.rs:104-108`, `count` at `1750-1764`). cadmpeg refuses negative counts and counts above 2^24 (16,777,216) vertices or faces. openNURBS has no cap.
3. **Intervals** (`interval`, `mesh.rs:1733-1748`). cadmpeg rejects a non-finite or decreasing (lo > hi) interval and fails the mesh. openNURBS `ReadInterval` accepts anything. No sample triggers it (probed).
4. **Presence bytes** (`mesh.rs:302-333`). cadmpeg rejects values above 1 and skips the chunk without checking its typecode (`consume_optional_chunk`, `1618-1628`). openNURBS treats any nonzero value as present and requires `0x40008000`.
5. **Faces** (`read_faces`, `mesh.rs:749-786`). cadmpeg fails the *whole mesh* on any index >= vcount. openNURBS does not check at read time. That is why `Rhino_Logo_Mesh7.3dm` is lost in cadmpeg: decoded with cadmpeg (`inspect --cadmpeg`), its only object is reported as undecoded.
6. **Triangulation** (`mesh.rs:788-865`). cadmpeg counts *distinct* indices: 3 gives one triangle (even for `(0,1,2,0)`), 4 gives a shorter-diagonal split, fewer than 3 drops the face silently. openNURBS's definition is `vi[2]==vi[3]` plus the `IsValid` and `Repair` rules. The diagonal choice is the same.
7. **`vcount == 0`** (`read_compressed_channels`, `975-1021`; S read at `359-375`). cadmpeg always reads five buffer-size words and an S size word. openNURBS reads none when `vcount == 0`. cadmpeg therefore misparses empty meshes and most likely fails them at the mapping-tag chunk (judged from the code; no sample has `vcount == 0`).
8. **Buffer checks** (`read_buffer`, `mesh.rs:1068-1203`).
   - Wrong size: cadmpeg drops that channel with a warning (`1175-1182`), where openNURBS fails the mesh.
   - CRC mismatch: cadmpeg *drops* the channel (`1188-1197`), where openNURBS keeps the data. A dropped V channel fails the mesh ("vertex channel is required", `1017-1019`).
   - Method 1: cadmpeg requires a non-short `0x40008000` chunk (`1102-1107`) and a zlib stream that fills the chunk body exactly (`1149-1154`).
9. **Major 1** (`read_counted_raw`, `1023-1061`; `925-927`). cadmpeg drops arrays whose count differs from vcount and requires `nV == vcount`. openNURBS keeps the arrays as they are.
10. **Writer-stamp gate** (`mesh.rs:378-379`, `498-511`). Both use 200606010. cadmpeg skips the post-S tail and records a loss when the archive has no stamp. openNURBS defaults the stamp to 200012210 for archive <= 2 (`opennurbs_archive.cpp:8514-8526`).
11. **Tri-state flags** (`mesh.rs:382-389`). cadmpeg warns for values above 2. openNURBS `m_mesh_is_solid` legitimately uses 3 (not solid), so this produces spurious warnings on 14 sample meshes (`Rhino_Logo_Mesh8_NamedViews.3dm`, `v5_dril.3dm`, `v5_example_file.3dm`).
12. **Mapping tag** (`read_mapping_tag`, `1298-1352`). cadmpeg fails the mesh if the transform is non-finite. openNURBS does not check.
13. **Double-vertex sync** (`synchronization_ok`, `1701-1712`). cadmpeg compares the f64 directly against the f32 with tolerance 1e-6·max|f|. openNURBS first casts the double to float. The difference is practically nil. For V5 user data both require an exact f32 match (`v5_synchronization_ok`, `1714-1720`).
14. **N-gons** (`read_ngons`, `1248-1296`; V4/V5 user data at `1516-1616`). cadmpeg validates the n-gons and keeps only their count.
15. **Resource budget** (`mesh.rs:112-177`; `decode.rs:274-279`). cadmpeg caps each decompressed buffer at min(policy, 256 MiB). It also caps *all retained mesh-buffer bytes in a document* (V, N, T, K, C, S and dV, stored or inflated) at min(policy, 256 MiB). Exceeding either raises `CodecError::ResourceLimit`, and the mesh branch propagates that as a **fatal error for the whole document** (`decode.rs:1022`). At roughly 72 bytes per vertex (V+N+T+S+dV), that is about 3.7 M vertices per file, which is a real risk for large architectural models.

### 4.14 Traps

- **Faces before vertices.** You cannot index into V until you are past the face array.
- **Chunk-length width applies everywhere.** In archive >= 50 the zlib wrapper and every nested anonymous chunk have 8-byte lengths. `ReadSize` (the buffer size) stays 4 bytes.
- **Empty meshes.** Zero vertices means no attribute block and no S buffer at all. Zero faces still writes the `i32` width.
- **The minor-4+ tail depends on the writer stamp, not only on minor.** The archive's writer stamp must already be known when meshes are decoded.
- **3.8 bounding box can be unset.** It may be ON_UNSET_VALUE ×6, and the legacy float box may be the "empty" marker. Recompute boxes from positions.
- **Invalid faces exist in real files.** `Rhino_Logo_Mesh7.3dm` declares 89,700 faces over 89,700 vertices, but only faces 0-29,899 are in range; the rest continue a triangle-soup pattern up to index 269,099. Dropping out-of-range faces recovers exactly the mesh in `Rhino_Logo_Mesh8_NamedViews.3dm` (89,700 V / 29,900 F).
- **Method is not tied to archive or size.** Method 0 appears for megabyte arrays when a writer disables compression.
- **Signed vs unsigned counts.**
  - Signed: vertex and face counts, and the `ReadArray` counts.
  - Unsigned: the n-gon count, n-gon `Vcount`/`Fcount`, and `dVcount`.
  - An n-gon record with `Vcount == 0` has no `Fcount` field.
- **Booleans.** `ReadBool` values above 1 are legal (true) only for writer stamps older than 2017-08-24.
- **The S buffer can have the wrong size** in writers stamped <= 201011049. Skipping by declared size handles it.

### 4.15 Samples (probed; paths relative to `samples/`)

| Variant | Files |
|---|---|
| 3.2, archive 2 | `opennurbs/example_files/V2/v2_my_mesh.3dm` (smallest: 9 V, 5 F), `v2_Soccer.3dm`, `v2_Flow3.3dm`, `v2_SketchOnMesh.3dm`, `v2_my_mesh_with_material.3dm` |
| 3.2, archive 3 | `V3/v3_my_mesh.3dm`, `v3_Smooth.3dm`, `v3_Speeder.3dm`, `v3_SphericalSpiral.3dm`, `v3_Flow3.3dm`, `v3_Soccer.3dm`, `v3_SketchOnMesh.3dm` |
| 3.4, archive 4 (mostly Brep render meshes) | all 15 `V4/*.3dm`, e.g. `v4_DVDCase_Solid.3dm`, `v4_RhinoPhone.3dm`, `v4_Prarie settle.3dm` |
| 3.5, archive 50 | `V5/v5_my_mesh.3dm`, `v5_disk_brake.3dm`, `v5_clip.3dm`, `v5_dril.3dm`, `v5_example_file.3dm`; `rhino-developer-samples/rhino3dm/models/MeshSphere_r5.3dm` |
| 3.7, archive 60 | `opennurbs/example_files/V6/my_mesh.3dm`, `V6/my_mesh_with_material.3dm`, `rhino-developer-samples/rhinocommon/cs/SampleCsRendererIntegration/MockingBird/TestFiles/MockingBirdTest.3dm` |
| 3.8 | `V6/v6_rhino_logo.3dm`, `V6/v6_rhino_subd_logo.3dm`, `V7/v7_my_mesh.3dm`, `V7/v7_rhino_logo_nurbs.3dm`, `V8/v8_section_hatch.3dm`, `V8/v8_selective_clipping.3dm`, `three.js/examples/models/3dm/Rhino_Logo.3dm`, `rhino-developer-samples/rhino3dm/models/Rhino_Logo_Mesh7.3dm`, `Rhino_Logo_Mesh8_NamedViews.3dm`, `rhino3dm/tests/models/mesh.3dm`, `meshWithTexture.3dm` |
| Method 1 (zlib) V buffer | 53 files, e.g. `V2/v2_Flow3.3dm`, `V5/v5_clip.3dm`, `rhino3dm/tests/models/mesh.3dm`, `Rhino_Logo_Mesh7.3dm` |
| Method 0 (stored) | `V*/…my_mesh.3dm` (buffers <= 128 B), `Rhino_Logo_Mesh8_NamedViews.3dm` (1 MB stored), `meshWithTexture.3dm`, `three.js/…/Rhino_Logo.3dm` |
| Index width 2 / 4 | width 2: 46 files (e.g. `v2_Flow3.3dm`, `three.js/…/Rhino_Logo.3dm`); width 4: `V4/v4_Wheel_PG.3dm`, `Rhino_Logo_Mesh7.3dm`, `Rhino_Logo_Mesh8_NamedViews.3dm` |
| Quads | 61 files, e.g. `v2_my_mesh.3dm`, `meshWithTexture.3dm` (80 quads), `three.js/…/Rhino_Logo.3dm` |
| Out-of-range face indices | `rhino-developer-samples/rhino3dm/models/Rhino_Logo_Mesh7.3dm` (only one) |
| Texture coordinates T | compressed: `V2/v2_Soccer.3dm`, `V5/v5_clip.3dm`, `rhino3dm/tests/models/mesh.3dm`; stored: `V4/v4_Prarie settle.3dm`, `meshWithTexture.3dm` |
| Surface parameters S | 51 files, e.g. `V4/v4_DVDCase_Solid.3dm`, `V5/v5_disk_brake.3dm`, `meshWithTexture.3dm` |
| Vertex colours C | `rhino3dm/tests/models/mesh.3dm` only (1 mesh, compressed) |
| Curvature K, curvature-stat chunks | none |
| Mesh-parameter chunk | 1.1 `V3/v3_Smooth.3dm`; 1.2 V4 files; 1.3 `V5/v5_example_file.3dm`; 1.4 `V5/v5_clip.3dm`, `V6/v6_rhino_logo.3dm`; 1.5 `V8/v8_selective_clipping.3dm`, `three.js/…/Rhino_Logo.3dm`, `rhino-developer-samples/rhino3dm/models/RenderContent.3dm` |
| N-gon chunk (3.6+) | `V6/v6_rhino_subd_logo.3dm` (pentagons), `rhino-developer-samples/rhino3dm/js/SampleUserStrings/RectifiedArchimedeanSolids.3dm`, `rhino-developer-samples/rhino3dm/models/RectifiedArchimedeanSolids8.3dm` |
| Double-vertex chunk (3.7+) | compressed: `V6/my_mesh.3dm`, `V7/v7_my_mesh.3dm`, `V6/v6_rhino_subd_logo.3dm`, `V8/v8_selective_clipping.3dm`, `rhino3dm/tests/models/mesh.3dm`, `rhino-developer-samples/rhino3dm/cs/SampleCSTestSetMesh/MeshBox_R8.3dm`, `…/models/MeshBox_r5.3dm`, `synthetic/house.3dm`; stored: `meshWithTexture.3dm`, `…/SampleCSTestSetMesh/MeshSphere_R8.3dm`, `RectifiedArchimedeanSolids*.3dm`, `…/models/decal_r8.3dm` |
| `ON_V5_MeshDoubleVertices` user data | `rhino-developer-samples/rhino3dm/models/MeshSphere_r5.3dm` |
| `ON_V4V5_MeshNgonUserData` | none |
| Solid flag = 3 | `Rhino_Logo_Mesh8_NamedViews.3dm`, `V5/v5_dril.3dm`, `V5/v5_example_file.3dm` |
| Unset 3.8 vertex bbox | `V7/v7_my_mesh.3dm`, `Rhino_Logo_Mesh7.3dm` |

### 4.16 Uncertain

- The layouts for major 1.x and minors 3.0, 3.1, 3.3 and 3.6 come from current source only; no sample exercises them. Whether historical writers ever emitted them is unverified.
- The meaning of the 200606010 gate for minor >= 4 is inferred from the code. No sample has minor >= 4 with an older stamp.
- Curvature buffers, curvature-stat chunks, V4/V5 n-gon user data and `vcount == 0` meshes are not exercised by any sample. The cadmpeg behaviour for `vcount == 0` (difference 7) comes from reading the code; it was not run.
- Behaviour on big-endian writers was not tested; all samples are LE. openNURBS's own big-endian swap of `m_C` in `Read_2` uses `Count()*3` (`opennurbs_mesh.cpp:1952`), which looks like a bug but does not affect LE reading.
- The 256 MiB cadmpeg document budget (difference 15) was not run against a file large enough to hit it.

Probe tooling (throwaway, outside the repo): `probe_mesh.py` (per-payload parse following `ON_Mesh::Read`), `annot.py` (the annotated dump in 4.10), `dbl.py` (double/float sync check), `dumpfaces.py`; output in `out2.tsv` and `pub.tsv`. No file in the repository or in the crate sources was modified.


---

## 5a. Breps (ON_Brep::Read): topology and render meshes

**Scope.** This section covers the class-data payload of an object whose class UUID resolves to `ON_Brep`. The payload is the body of chunk `0x0002FFFC`, without its trailing CRC. It covers archives 2, 3, 4, 50, 60, 70 and 80. The class layouts of curves and surfaces are documented in the curves/surfaces section, and the `ON_Mesh` class data in the mesh section. This section covers how those classes are embedded and everything that is specific to Breps.

**Sources.**
- cadmpeg = `cadmpeg-codec-rhino-0.6.1/src`
- ON = openNURBS repository root (branch 8.x, commit eb92af3)

**Probe scripts.** They were throwaway, run with `python3 -I`:
- `probe_brep.py`: walks every major-3 Brep.
- `probe_legacy.py`: walks major-2 Breps.
- `orient.py`: checks orientation rules.
- `meshorient.py`: checks render-mesh winding.
- `trimcheck.py`, `edgerev.py`, `skipshare.py`: smaller checks.

The probes parsed all 910 Brep records in `samples/` and consumed exactly the declared bytes, with no errors. The 910 records are 285 of payload 2.1 and 625 of payload 3.x. These counts match the per-sample class inventory used for section 8.

**Notation** (all values little-endian):

| Name | openNURBS primitive | On disk |
|---|---|---|
| `u8` | `ReadChar` | 1 byte |
| `i32` | `ReadInt` | 4 bytes |
| `f64` | `ReadDouble` | 8 bytes |
| `interval` | `ReadInterval` | 2×f64 `[t0,t1]` (ON `opennurbs_archive.cpp:1439`) |
| `point3` | `ReadPoint(ON_3dPoint)` | 3×f64 (ON :1256) |
| `bbox` | `ReadBoundingBox` | 2×point3 (min, max) = 48 bytes |
| `int_array` | `ReadArray(ON_SimpleArray<int>)` | i32 `count`, then `count`×i32. Nothing follows if `count` ≤ 0 (ON :2221-2232) |
| `uuid` | `ReadUuid` | u32 Data1, u16 Data2, u16 Data3, 8 bytes Data4 = 16 bytes (ON :1445) |
| `color` | `ReadColor` | 4 raw bytes R,G,B,A. `0xFFFFFFFF` = unset (ON :1211; `opennurbs_color.h:51-62`) |
| version byte | `Read3dmChunkVersion` | u8: major = `v>>4`, minor = `v&15` (ON :5771-5786) |
| `len` | chunk length field | u32 when archive < 50 (archive 5 included); i64 when archive ≥ 50 (`ON_BinaryArchive::SizeofChunkLength`, ON :7093-7099) |

---

### 5a.1 Locating the payload: class UUIDs (aliases)

| Class UUID (canonical) | Bytes on disk | Name | Archives seen | Payload |
|---|---|---|---|---|
| `60B5DBC5-E660-11d3-BFE4-0010830122F0` | `C5 DB B5 60 60 E6 D3 11 BF E4 00 10 83 01 22 F0` | ON_Brep | 2 (toolkit writer), 3, 50, 60, 70 | 3.x |
| `F06FC243-A32A-4608-9DD8-A7D2C4CE2A36` | `43 C2 6F F0 2A A3 08 46 9D D8 A7 D2 C4 CE 2A 36` | TL_Brep (Rhino's alias) | 3, 4, 50, 60, 70, 80 (the majority) | 3.x |
| `0705FDEF-3E2A-11d4-800E-0010830122F0` | | "old trimmed surface" | 2 (Rhino 2) | 2.1 |
| `2D4CFEDB-3E2A-11d4-800E-0010830122F0` | | "old b-rep" | 2 (Rhino 2) | 2.1 |

- openNURBS maps all three aliases to `ON_Brep` in `ON_ClassId::ClassId` (ON `opennurbs_object.cpp:889-935`).
- cadmpeg declares them at `brep.rs:27-45` and accepts them in `supported_class` (`brep.rs:1969-1974`).

**Trap: embedded children use alias UUIDs too.** Breps from the Rhino 3 to 5 era embed their curves and surfaces under these UUIDs:
- TL_NurbsCurve `5EAF1119-0B51-11d4-BFFE-0010830122F0`
- TL_NurbsSurface `4760C817-0BE3-11d4-BFFE-0010830122F0`
- TL_RevSurface `0A8401B6-4D34-4b99-8615-1B4E723DC4E5`
- TL_SumSurface `665F6331-2A66-4cce-81D0-B5EEBD9B5417`
- "old nurbs curve" `76A709D5-1550-11d4-8000-0010830122F0`, inside archive-2 legacy polycurves.

All of these are mapped by the same table (`opennurbs_object.cpp:893-930`). A class lookup without them loses most curve and surface children in V3, V4 and V5 files.

### 5a.2 Framing used inside the payload

1. **Anonymous chunk `0x40008000`** (`TCODE_ANONYMOUS_CHUNK` = `TCODE_USER|TCODE_CRC`, `opennurbs_3dm.h:87`).
   - Layout: u32 typecode, `len`, body, u32 CRC32. `len` counts the body plus the 4 CRC bytes.
   - The CRC covers only the chunk's *direct* bytes. The header, body and CRC of every nested chunk are excluded:
     - `BeginWrite3dmBigChunk` disables CRC accumulation while it writes a header (ON :5833-5847).
     - Only the innermost open chunk accumulates CRC (`PushBigChunk`, ON :7101-7178).
   - Verified on all 6,250 component chunks of the 625 major-3 Breps.
   - cadmpeg checks it with `finish_anonymous_children` and `direct_checksum_ranges` (`brep.rs:3115-3126`, `chunks.rs:865-920`).
   - A reader that only skips may ignore the CRC.
2. **Embedded object** = `ON_BinaryArchive::ReadObject` → `ReadObjectHelper` (ON :5062-5283). It is exactly the class wrapper used for top-level objects:
   - Layout: `0x00027FFA` `len` { `0x0002FFFB` len=20: uuid + CRC32; `0x0002FFFC` len: class data + CRC32; zero or more `0x00027FFD` userdata chunks; `0x80027FFF` short chunk, value 0 }.
   - `0x00027FFA` has no CRC bit, so its `len` spans everything through the class-end chunk. **One `len` skips the whole child.**
   - A nil UUID means a null object. openNURBS returns success with no object and skips the rest of the wrapper.
3. **Versioned anonymous chunk**: `BeginRead3dmAnonymousChunk` → `BeginRead3dmChunk(tcode,&major,&minor)` (ON :5862-5883, writer :5888-5925).
   - The body starts with **two i32** (major, minor) instead of a version byte.
   - Within the Brep it is used only by the 3.3 region topology.

### 5a.3 Payload versions and dispatch

`ON_Brep::Read` (ON `opennurbs_brep_io.cpp:883-1180`) reads one version byte and dispatches on its major:
- **major 2** → `ReadOld200` (:1182-1472): the legacy Rhino 2 trimmed-surface collection, unchunked (§5a.12).
- **major 3** → the current format. The minor selects the optional trailing parts:
  - 3.0: topology and bbox.
  - 3.1: adds the render and analysis mesh chunks.
  - 3.2: adds the i32 `m_is_solid`.
  - 3.3: adds the region topology chunk (added August 2016 for V6).

  These are taken from the comments in `ON_Brep::Write` (:681-822).
- **Any other major**: openNURBS reads nothing more and returns *success* with an empty Brep. cadmpeg returns `UnsupportedVersion` (`brep.rs:937-947`).

Which minor gets written:
- `ON_Brep::Write` writes minor 2 when archive ≤ 50 and minor 3 otherwise (:690-695).
- Rhino 2 wrote 2.1.
- `ReadOld100` (old RhinoIO toolkit) and `ReadOld101` (Rhino 1.1) exist (:1474-1690) but **nothing calls them**. Archive-1 Breps arrive through the V1 typecode readers instead (`ReadV1_TCODE_LEGACY_SHL/FAC`, `ReadV1_TCODE_RHINOIO_OBJECT_BREP`, ON `opennurbs_archive.cpp:14891-15340`), which are outside this section.

**The edge and trim record layout depends on the writer stamp, not on the payload version.** The edge domain and the trim "proxy block" are present iff `Archive3dmVersion() >= 3 && ArchiveOpenNURBSVersion() >= 200206180`:
- ON readers: `ON_BrepEdge::Read` :81-90 and `ON_BrepTrim::Read` :233-260.
- ON writers: `ON_BrepEdge::Write` :60-65 and `ON_BrepTrim::Write` :125-145, gated on archive ≥ 3.
- cadmpeg: `brep.rs:2122` and `brep.rs:2174`.

Where the stamp comes from:
- It is the properties-table short chunk `0xA0000026` (`TCODE_PROPERTIES_OPENNURBS_VERSION`); its value is a u32 in 4-byte archives.
- If the chunk is absent, openNURBS substitutes 200012210 for archive ≤ 2. For archive ≥ 3 it substitutes its *own* version, which selects the current layout (ON :8516-8527).
- Modern stamps are encoded 10-digit numbers (for example 2382393748), so a plain integer `>=` comparison works.

Observed in the samples:

| Archive (writer stamp) | Class | Payload | Brep records | Edge/trim layout | Face array |
|---|---|---|---|---|---|
| 2 (Rhino 2, 200107270) | old trimmed surface 236 / old b-rep 49 | 2.1 | 285 | n/a | n/a |
| 2 (openNURBS toolkit 200210150) | ON_Brep | 3.2 | 2 | **legacy** (archive < 3) | 1.1 |
| 3 | TL_Brep 327, ON_Brep 2 | 3.2 | 329 | current | 1.1 |
| 4 | TL_Brep | 3.2 | 143 | current | 1.1 |
| 50 | TL_Brep 17, ON_Brep 94 | 3.2 | 111 | current | 1.1 |
| 60 | TL_Brep 8, ON_Brep 1 | 3.3 | 9 | current | 1.1 |
| 70 | TL_Brep 14, ON_Brep 2 | 3.3 | 16 | current | **1.1 or 1.2** |
| 80 | TL_Brep | 3.3 | 15 | current | 1.2 |

### 5a.4 Major 3 payload: top level

| # | Field | Encoding | Since | Skippable by length |
|---|---|---|---|---|
| 0 | version byte `0x3m` | u8 | | no (1 byte) |
| 1 | C2 array (2D trim curves) | anon chunk §5a.5 | 3.0 | yes |
| 2 | C3 array (3D edge curves) | anon chunk §5a.5 | 3.0 | yes |
| 3 | S array (untrimmed surfaces) | anon chunk §5a.5 | 3.0 | yes |
| 4 | vertices | anon chunk §5a.6 | 3.0 | yes |
| 5 | edges | anon chunk §5a.7 | 3.0 | yes |
| 6 | trims | anon chunk §5a.8 | 3.0 | yes |
| 7 | loops | anon chunk §5a.9 | 3.0 | yes |
| 8 | faces | anon chunk §5a.10 | 3.0 | yes |
| 9 | Brep bbox | bbox, bare (48 bytes) | 3.0 | fixed size |
| 10 | render meshes | anon chunk §5a.11 | 3.1 | yes |
| 11 | analysis meshes | anon chunk §5a.11 | 3.1 | yes |
| 12 | `m_is_solid` | i32, bare | 3.2 | fixed size |
| 13 | region topology | versioned anon chunk | 3.3 | yes |

- openNURBS readers: `ON_Brep::Read` :897-1130. cadmpeg: `parse`, `brep.rs:928-1100`.
- Both readers tolerate trailing bytes: openNURBS via `EndRead3dmChunk`, cadmpeg via `skip_remaining` (`brep.rs:1056`).
- After reading, openNURBS attaches each edge to `C3[c3i]`, each trim to `C2[c2i]` and each face to `S[si]` only when the index is in range (:917-985). Out-of-range indexes silently leave null proxies.

### 5a.5 C2 / C3 / S arrays

ON: `ON_CurveArray::Read` (`opennurbs_curve.cpp:2492-2540`), `ON_SurfaceArray::Read` (`opennurbs_surface.cpp:1449-1490`). cadmpeg: `read_children` (`brep.rs:1976-2044`).

| Field | Encoding |
|---|---|
| version | u8 `0x10` (major must be 1) |
| count | i32 |
| per slot: flag | i32. `1` = an embedded object (§5a.2 item 2) follows. Otherwise a null slot |
| CRC | u32, over version, count and flags only |

- Indexes (`trim.m_c2i`, `edge.m_c3i`, `face.m_si`) are **slot positions, null slots included**.
- openNURBS treats any flag value other than 1 as null and consumes no object. cadmpeg rejects values other than 0 and 1 (`brep.rs:2005-2028`).
- Classes seen in samples:
  - C2: (TL_)NurbsCurve, LineCurve, PolyCurve.
  - C3: (TL_)NurbsCurve, LineCurve, ArcCurve, PolyCurve, PolylineCurve.
  - S: (TL_)NurbsSurface, PlaneSurface, (TL_)RevSurface, (TL_)SumSurface.
- No null slots occur in the samples.
- C2 entries that no trim references exist: 3,141, mostly in `v4_Prarie settle.3dm`.
- C3 curves shared by two edges occur 18 times.

### 5a.6 Vertex array

ON: `ON_BrepVertexArray::Read` :355-403, `ON_BrepVertex::Read` :38-48. cadmpeg: `read_vertices` (`brep.rs:2046-2079`).

The body is u8 `0x10`, i32 count, then the records, then the CRC. Each record is 40 + 4n bytes:

| Field | Encoding |
|---|---|
| `m_vertex_index` | i32 |
| point | point3 |
| `m_ei` (edges using this vertex) | int_array |
| `m_tolerance` | f64. `ON_UNSET_VALUE` = -1.23432101234321e308 means "not computed" |

### 5a.7 Edge array

ON: `ON_BrepEdgeArray::Read` :405-436, `ON_BrepEdge::Read` :69-97. cadmpeg: `read_edges` (`brep.rs:2110-2160`).

The body is u8 `0x10`, i32 count, then the records, then the CRC. A record is 48 + 4n bytes in the legacy layout and 64 + 4n bytes in the current layout:

| Field | Encoding | Meaning |
|---|---|---|
| `m_edge_index` | i32 | |
| `m_c3i` | i32 | slot in C3 |
| reversed | i32 | nonzero = the edge runs opposite to its 3D curve (`ON_CurveProxy` reversal) |
| proxy domain | interval | increasing sub-interval of the C3 curve's domain used by the edge |
| `m_vi[2]` | 2×i32 | start and end vertex of the **edge**, after reversal |
| `m_ti` | int_array | trims using this edge |
| `m_tolerance` | f64 | |
| edge domain | interval | **only if archive ≥ 3 and stamp ≥ 200206180**; otherwise it equals the proxy domain |

Evaluation follows `ON_CurveProxy::RealCurveParameter` (ON `opennurbs_curveproxy.cpp:119-131`):
1. Normalise the edge parameter `t` over the edge domain: `s = (t-d0)/(d1-d0)`.
2. If the edge is reversed, set `s = 1-s`.
3. The curve parameter is `p0 + s·(p1-p0)` over the proxy domain.

Reversal also negates the domain. Example: `v7_rhino_logo_nurbs.3dm` edge 0 stores rev=1, proxy domain [0, 2.8068] and domain [-2.8068, -0]. Its `m_vi[0]` lies at the curve's *end*, which `edgerev.py` confirmed against the curve's control points.

### 5a.8 Trim array

ON: `ON_BrepTrimArray::Read` :455-503, `ON_BrepTrim::Read` :152-273, writer :99-150. cadmpeg: `read_trims` (`brep.rs:2162-2239`).

The body is u8 `0x10`, i32 count, then the records, then the CRC. **Every record is 132 bytes in both layouts.**

| Offset | Field | Encoding |
|---|---|---|
| 0 | `m_trim_index` | i32. openNURBS overwrites it with the array position if it differs (:475-484, RH-18299) |
| 4 | `m_c2i` | i32, slot in C2. May be -1 only for ptonsrf trims |
| 8 | proxy domain | interval |
| 24 | `m_ei` | i32. -1 for singular and ptonsrf trims |
| 28 | `m_vi[2]` | 2×i32: start and end vertex of the **trim** |
| 36 | `m_bRev3d` | i32. Nonzero = the trim runs opposite to its edge |
| 40 | `m_type` | i32 enum |
| 44 | `m_iso` | i32 enum |
| 48 | `m_li` | i32, loop index |
| 52 | `m_tolerance[2]` | 2×f64: accuracy of the 2D curve in u and in v |
| 68 | current layout | trim domain (interval, 16 bytes); then 8 bytes, where byte 0 == 1 means the proxy is reversed and the rest are 0; then 24 reserved zero bytes |
| 68 | legacy layout (archive < 3 or stamp < 200206180) | 2×point3. Written as zeros, ignored |
| 116 | `m__legacy_2d_tol` | f64 |
| 124 | `m__legacy_3d_tol` | f64 |

Enums:
- **Trim type** (ON `opennurbs_brep.h:432-465`): 0 unknown, 1 boundary, 2 mated, 3 seam, 4 singular, 5 crvonsrf, 6 ptonsrf, 7 slit (reserved, invalid).
- **Iso** (ON `opennurbs_surface.h:68-84`): 0 not_iso, 1 x_iso, 2 y_iso, 3 W_iso, 4 S_iso, 5 E_iso, 6 N_iso.

**Trap: the openNURBS reader only maps trim types 0-4** (`switch` at :179-195). Values 5, 6 and 7 leave `m_type = unknown`. The writer writes whatever value is in memory.

Trim curve: the trim is C2 proxied over its proxy domain, with the same mapping as edges.
- In all sample trims the trim domain equals the proxy domain.
- The proxy-reversed byte is never set in the samples.

### 5a.9 Loop array

ON: `ON_BrepLoopArray::Read` :516-547, `ON_BrepLoop::Read` :287-318. cadmpeg: `read_loops` (`brep.rs:2241-2272`).

Each record is 16 + 4n bytes:

| Field | Encoding |
|---|---|
| `m_loop_index` | i32 |
| `m_ti` | int_array: trims in traversal order |
| `m_type` | i32 |
| `m_fi` | i32, face index |

- **Loop type** (ON `opennurbs_brep.h:870-880`): 0 unknown, 1 outer, 2 inner, 3 slit, 4 crvonsrf, 5 ptonsrf. The openNURBS reader maps only 0-3 (:300-315).
- `m_pbox` is **not stored**. It is recomputed from the 2D curves by `ReadFillInMissingBoxes` (:828-858). Trim `m_pbox` is recomputed the same way.

### 5a.10 Face array

ON: `ON_BrepFaceArray::Read` :566-626, writer :628-679, `ON_BrepFace::Read` :331-353. cadmpeg: `read_faces` (`brep.rs:2274-2344`).

| Field | Encoding |
|---|---|
| version | u8: `0x10`, `0x11` or `0x12` |
| count | i32 |
| count × record (20 + 4n bytes) | `m_face_index` i32; `m_li` int_array (`li[0]` is the outer loop); `m_si` i32 (slot in S); `m_bRev` i32 (nonzero = face normal opposite to the surface's natural normal); `m_face_material_channel` i32 (negative values are set to 0 on read) |
| 1.1 and later: face UUIDs | count × uuid, in face order, **after all records** (`m_face_uuid`) |
| 1.2 and later: per-face colours | u8 `ReadBool` flag; if 1, count × color |
| CRC | u32 |

**Trap: key on the version byte, not on the archive.** The current writer writes minor 2 when archive ≥ 70, else minor 1 (:636). But V7 builds stamped 2382393748 still wrote 1.1 into archive 70 (the `V7/v7_*` files), while build 2382395042 wrote 1.2 (`three.js/.../Rhino_Logo.3dm`).

Material channel semantics:
- 0 = use the object's material.
- A value > 0 indexes the base material's channel list (`opennurbs_brep.h:1266-1270`).
- -1 appears in files written by Rhino 3-era builds. Both readers clamp it to 0 (cadmpeg `brep.rs:805-809`).

### 5a.11 Trailing parts: bbox, meshes, solid flag, regions

**bbox** (:988-993). openNURBS recomputes face and Brep boxes for archive < 90 because stored boxes can be oversized (RH-64277, :1162-1177). Do not rely on the stored box for tight bounds.

**Render meshes (3.1 and later):** anonymous chunk. The body is, for each face `fi` in 0..face_count, a u8 flag followed by an embedded object when the flag is nonzero. The CRC covers the flags only.
- Openers: ON :1011-1046; writer :733-750. cadmpeg: `read_mesh_sides`, `brep.rs:2346-2429`.
- **Slot `i` belongs to face `i`.**
- The embedded object is a full `ON_Mesh` class wrapper, UUID `4ED7D4E4-E947-11d3-BFE5-0010830122F0`. Its class data is identical to a top-level mesh (see the mesh section). There is no compression at the Brep level; the mesh compresses its own arrays.
- Mesh class-data versions in the samples: 3.4, 3.5, 3.7, 3.8. Mesh userdata: none in the samples.
- **Analysis meshes:** a second chunk with the identical layout (ON :1048-1084).
- The writer always emits both chunks. Flags are 0 when `Save3dmRenderMesh` / `Save3dmAnalysisMesh` are false, as with "Save Small".
- openNURBS discards non-mesh objects. A `ReadObject` failure fails the whole Brep (3.x only).
- **Coordinate system:** world coordinates, or block-definition coordinates for Breps inside a block definition, the same as the Brep itself.
- **Orientation:** mesh winding and vertex normals already follow the *face* orientation, with `m_bRev` applied. On 351 planar faces with cached meshes, the 181 faces with bRev=0 are wound along +plane Z and the 170 faces with bRev=1 along −Z; the vertex normals agree (`meshorient.py`).
- Meshes are per face. Neighbouring faces share boundary positions but not vertex indices, so weld them to get one mesh.

**`m_is_solid` (3.2 and later):** i32. Values (ON `opennurbs_brep.h:4102-4109`, `opennurbs_brep.cpp:6958-6990`):
- 0 = unset
- 1 = solid, normals point **outward**
- 2 = solid, normals point **inward**
- 3 = not solid

openNURBS maps values outside 0..2 to 0 (:1087-1091). In the samples, 274 of 625 Breps store 3, 332 store 1, 19 store 0, and none stores 2.

**Region topology (3.3 and later):** versioned anonymous chunk.
- Layout: i32 major = 1, i32 minor (1 is written), u8 present flag; if present, an `ON_BrepRegionTopology`; then the CRC. Sources: ON :1092-1129, writer :794-817; `ON_BrepRegionTopology::Read`, `opennurbs_brep_region.cpp:882-908`.
- It is not needed for rendering; skip it by `len`.
- All 3.3 samples store (1,1) with the present flag 0.
- V5 files carry the same data instead as userdata `7FE23D63-E536-43f1-98E2-C807A2625AFF` on the Brep wrapper (:787-793; cadmpeg :1026-1055). No sample has it.

### 5a.12 Legacy major 2 (`ReadOld200`, archive 2 / Rhino 2)

There are **no chunks**: every field is bare (ON :1182-1472; cadmpeg `parse_legacy_major2`, `brep.rs:1180-1716`).

| Field | Encoding |
|---|---|
| face_count, edge_count, loop_count, trim_count | 4×i32, each must be ≥ 1 |
| outer_flag | i32. 1 = closed and outward; it sets `m_is_solid=1` if `IsSolid()` (:1468-1469) |
| bbox | 48 bytes |
| 2D curves | trim_count × `ON_PolyCurve::Read`, **raw, without a class wrapper** (`opennurbs_polycurve.cpp:441-487`): u8 version; i32 seg_count; i32 reserved1; i32 reserved2; bbox; `m_t` (i32 n, then n×f64); seg_count × embedded object (in the samples always "old nurbs curve") |
| 3D curves | edge_count × raw `ON_PolyCurve` |
| surfaces | face_count × `ON_NurbsSurface::Read`, raw (`opennurbs_nurbssurface.cpp:773-895`): u8 version; i32 dim, is_rat, order0, order1, cv_count0, cv_count1, reserved1, reserved2; bbox; i32 kc0, then kc0×f64; i32 kc1, then kc1×f64; i32 cv_count, then cv_count × (dim + (is_rat?1:0)) × f64 |
| per face | i32 legacy index; i32 obsolete material; i32 bRev; i32 face type; bbox (3D); i32 loop_count; then the loops |
| per loop | i32 legacy index; i32 btype (0 outer, 1 inner, -1 slit, other values unknown); 4×f64 2D box (umin, vmin, umax, vmax); i32 trim_count; then the trims |
| per trim (41 bytes) | i32 legacy index; i32 twin index; u8 managed-edge flag; i32 edge index (out of range → -1; an error if the managed flag is set); i32 bRev3d; i32 gcon; i32 mono; f64 3D tol; f64 2D tol |
| render meshes | face_count × (u8 flag [+ embedded ON_Mesh]) |
| analysis meshes (minor ≥ 1) | face_count × (u8 flag [+ embedded ON_Mesh]) |

Implied indexes:
- Trim k (counted across all faces in file order) uses `C2[k]`. Edge e uses `C3[e]`. Face f uses `S[f]`.

Not stored: vertices, trim types, iso flags and edge–vertex links. Each reader derives them differently:
- openNURBS uses `SetTrimIsoFlags`, `SetTrimTypeFlags`, `SetVertices` and `SetTolsFromLegacyValues` (:1375-1382).
- cadmpeg merges trim endpoints with union-find along loops and shared edges (`brep.rs:1400-1560`). It infers trim types from edge use counts (:1590-1610) and sets iso to none.

Failures while reading meshes are swallowed (:1446-1462). **To reach the meshes you must parse every polycurve and NURBS surface**; only the polycurve segments are length-prefixed. The probe consumed all 285 archive-2 records exactly. None has a render or analysis mesh.

### 5a.13 Pseudo-code (a): skip to the render meshes (major 3)

```rust
// payload = class-data body of 0x0002FFFC (CRC excluded); wide = archive >= 50
fn read_len(r: &mut Rd, wide: bool) -> u64 { if wide { r.i64() as u64 } else { r.u32() as u64 } }
fn anon(r: &mut Rd, wide: bool) -> Range<usize> {          // returns body WITHOUT the CRC
    ensure!(r.u32() == 0x4000_8000);
    let len = read_len(r, wide) as usize;                   // len includes the 4-byte CRC
    let body = r.pos..r.pos + len - 4; r.pos += len; body
}
fn brep_meshes(p: &[u8], wide: bool) -> Result<(Vec<Option<Range<usize>>>, Vec<Option<Range<usize>>>)> {
    let mut r = Rd::new(p);
    let v = r.u8();
    match v >> 4 {
        3 => {}
        2 => return legacy_major2_full_parse(p, wide),      // §5a.12: no length prefixes
        _ => bail!("unsupported ON_Brep major"),
    }
    let minor = v & 15;
    let mut face_count = 0usize;
    for k in 0..8 {                                         // C2,C3,S,V,E,T,L,F
        let body = anon(&mut r, wide);
        if k == 7 {                                         // face array: version byte + count
            ensure!(p[body.start] >> 4 == 1);
            face_count = i32_at(p, body.start + 1).max(0) as usize;
        }
    }
    r.skip(48);                                             // Brep bbox
    if minor == 0 { return Ok((vec![None; face_count], vec![None; face_count])); }
    let mut sides = [vec![], vec![]];                       // render, analysis
    for side in &mut sides {
        let body = anon(&mut r, wide);
        let mut q = Rd::sub(p, body.clone());
        for _ in 0..face_count {
            if q.u8() == 0 { side.push(None); continue; }
            let start = q.pos;
            ensure!(q.u32() == 0x0002_7FFA);                // class wrapper, no CRC bit
            let len = read_len(&mut q, wide) as usize;
            q.pos += len;
            side.push(Some(start..q.pos));                  // hand to generic ReadObject -> ON_Mesh
        }
        // q.pos should == body.end; if not, trust face_count (openNURBS does) and warn
    }
    // minor>=2: r.i32() is_solid; minor>=3: anon(&mut r, wide) region topology (skip)
    Ok((sides[0].clone(), sides[1].clone()))
}
```

Implementation notes:
- Everything before the meshes is skipped by eight `len` jumps plus 48 bytes, so no topology parse is needed.
- The face count comes from the first 5 bytes of the face-array body.
- Check each slot's class UUID against `ON_Mesh`.
- Brep bytes are dominated by curves and meshes:
  - `v4_Wheel_PG.3dm`: render meshes are 85% of the Brep bytes.
  - `v7_rhino_logo_nurbs.3dm`: render meshes are 76%.
  - `v5_disk_brake.3dm`: render meshes are 23% and C2 curves 55%.
- Validated by `meshorient.py`, which walked every meshed 3.x Brep this way.
- Because this path does not depend on topology, a bad topology record never costs you the meshes. cadmpeg, by contrast, emits mesh caches only after the full parse (`decode.rs:5208-5260`), so a structural error earlier in the payload loses them.

### 5a.14 Pseudo-code (b): planar-face tessellation data walk

**Orientation rules.** Each rule is backed by openNURBS and checked on the samples:
1. **Loop direction is stored implicitly, relative to the surface.**
   - In the surface's (u,v) parameter space, outer-loop trims run counter-clockwise and inner loops clockwise (`opennurbs_brep.h:870-874`, `FlipLoop` note :3105-3111; `ComputeLoopType` derives type from `LoopDirection`, `opennurbs_brep.cpp:2711-2722`).
   - This holds regardless of `face.m_bRev`. On PlaneSurface faces, outer loops are CCW about +Z in 224 of 225 cases, for both bRev values. Inner loops are CW in 51 of 53. The 3 exceptions are loops with curved edges, where my vertex-polygon test is unreliable.
   - `li[0]` is always the outer loop (6,126 of 6,126 faces).
2. **Trim vs edge direction is `m_bRev3d`.**
   - When false, the trim and its edge run the same way. When true, they run opposite.
   - Invariant: `trim.m_vi[0] == edge.m_vi[bRev3d?1:0]` and `trim.m_vi[1] == edge.m_vi[bRev3d?0:1]` (`ON_Brep::IsValidTrim`, `opennurbs_brep.cpp:2972-2989`; `ON_Brep::IsValid` :5189-5202). This held for 28,259 of 28,259 non-singular trims.
   - In a loop, `trim[k].m_vi[1] == trim[k+1].m_vi[0]`, cyclically (28,723 of 28,723).
3. **Edge vs its 3D curve is the stored "reversed" int plus the proxy domain** (§5a.7). The edge runs from `C3(pdom[1])` to `C3(pdom[0])` when reversed. `edge.m_vi[0]` is the edge's start.
4. **Face normal:** `N_face = (m_bRev ? -1 : +1) · (∂S/∂u × ∂S/∂v)`.
   - For a PlaneSurface, `∂S/∂u × ∂S/∂v` points along the plane's zaxis.
   - Seen from `N_face`, the outer loop therefore runs CCW when bRev=0 and CW when bRev=1.
   - If `m_is_solid == 1`, every `N_face` points out of the solid; if it is 2, every `N_face` points in.
   - Cached render meshes already follow `N_face`.
5. **Stored vs derived:**
   - Stored: loop type, trim type, iso, `m_bRev3d`, `m_bRev`, both reversal flags, all indexes, vertex points, tolerances.
   - Derived on read (not stored): trim and loop `m_pbox`, face bbox, and in 2.x Breps also vertices, trim types and iso.
   - Never stored: loop direction. Loop type is the only stored hint.

**Planar detection.**
- `PlaneSurface` class data (layout in the surfaces section; ON `opennurbs_planesurface.cpp:122-147`):
  - Fields: u8 version; plane (origin, X, Y, Z, each point3; then 4×f64 equation); domain0 and domain1 (intervals); if minor ≥ 1, extents0 and extents1 (else extents = domain).
  - Evaluation (ON :337-362): `x = ext0.ParameterAt(dom0.NormalizedParameterAt(u))`, `y` likewise; `P = O + x·X + y·Y`.
- Many planar faces are **not** PlaneSurface. For example, every `v*_my_brep.3dm` box face is a NurbsSurface. Treat a NurbsSurface as planar if its control points are coplanar within tolerance, and take the normal's sign from `∂S/∂u × ∂S/∂v` at an interior point.

```rust
fn tessellate_planar_face(b: &Brep, fi: usize, tol: f64) -> Result<Vec<[P3; 3]>> {
    let f = &b.faces[fi];
    let srf = b.s.get(f.si).flatten().ok_or("face surface missing")?;   // report, do not skip silently
    let fr = planar_frame(srf, tol)?;           // (O, X, Y, Z) with Z = X×Y = natural normal
    let mut rings: Vec<Vec<[f64; 2]>> = vec![];
    for (k, &li) in f.li.iter().enumerate() {
        let lp = &b.loops[li];
        match (k, lp.kind) { (0, Outer) | (_, Inner) => {}, _ => continue } // slit/crvonsrf/ptonsrf/unknown
        let mut ring = vec![];
        for &ti in &lp.ti {
            let t = &b.trims[ti];
            if t.ei < 0 { continue; }            // singular trim: zero length in 3D (type 4, vi[0]==vi[1])
            let e = &b.edges[t.ei as usize];
            let c3 = b.c3[e.c3i].as_ref().ok_or("edge curve missing")?;
            let mut pts = sample(c3, e.proxy_domain, tol); // increasing curve parameter
            if e.reversed { pts.reverse(); }     // now V[e.vi[0]] -> V[e.vi[1]]
            if t.rev3d   { pts.reverse(); }      // now V[t.vi[0]] -> V[t.vi[1]]
            pts[0] = b.v[t.vi[0]].p; *pts.last_mut().unwrap() = b.v[t.vi[1]].p; // snap: crack-free
            pts.pop();                           // next trim starts here
            ring.extend(pts);
        }
        rings.push(ring.iter().map(|p| [(*p - fr.o).dot(fr.x), (*p - fr.o).dot(fr.y)]).collect());
    }
    // Expected: rings[0] CCW, holes CW in (X,Y). If a stored type disagrees with the
    // signed area, trust the area (types can be wrong in third-party files).
    let tris = triangulate_with_holes(&rings);   // ear clipping or CDT; output CCW in (X,Y)
    Ok(tris.into_iter().map(|t| if f.rev { [t[0], t[2], t[1]] } else { t }).collect()) // flip if bRev
}
```

Alternatives and notes:
- **2D route:** sample `C2[t.c2i]` over the trim's proxy domain, reversed if proxy byte 0 == 1. Points are already in (u,v), and loops are then oriented exactly as stored. Map through the surface evaluation.
- **3D route (shown above):** sampling the same edge curve for both adjacent faces gives crack-free seams.
- For straight-edged loops the stored vertex points `V[t.vi[0]]` alone define the polygon.
- Seam trims (two trims of one loop sharing an edge) and singular trims occur on closed and revolved surfaces, not on planes. Skip singular trims as shown.

### 5a.15 cadmpeg vs openNURBS differences

| # | Topic | openNURBS | cadmpeg |
|---|---|---|---|
| 1 | Brep major other than 2 or 3 | returns success, empty Brep (`ON_Brep::Read` :891-897) | `UnsupportedVersion` error (`brep.rs:937-947`) |
| 2 | Array slot flag | only `==1` reads an object; anything else is null (`ON_CurveArray::Read` / `ON_SurfaceArray::Read`) | 0 or 1, otherwise error (`brep.rs:2005-2028`) |
| 3 | Trim type / loop type / iso | unmapped values become unknown / unknown / not_iso; trim types only 0-4, loop types only 0-3 (:179-226, :300-315) | trim 0-7, loop 0-5, iso 0-6; anything else is a fatal error (`brep.rs:197-264`, :2197-2200, :2253) |
| 4 | Edge reversed, trim `bRev3d`, face `bRev` | nonzero = true | must be exactly 0 or 1 (`brep.rs:2131-2135`, :2193-2197, :2308-2312) |
| 5 | Trim proxy-reversed byte | `b[0]==1` → reversed; other values → not reversed (:248-252) | 0 or 1, otherwise error (`brep.rs:2205-2210`) |
| 6 | Face-array minor > 2 | accepted, tail skipped | rejected (`brep.rs:2284`) |
| 7 | Per-face colour flag | `ReadBool`, lenient for old stamps (ON :2784-2800) | u8 > 1 is an error (`brep.rs:2331-2334`) |
| 8 | Missing writer stamp, archive ≥ 3 | uses its own version → **current** layout (ON :8520-8526) | **legacy** layout plus a loss note (`brep.rs:2090-2108`, :2122, :2174) |
| 9 | `m_is_solid` meaning | 0 unset, 1 out, 2 in, 3 not solid; values ≥ 3 read as 0 (:1087-1091) | names 0/1/2 Open/Closed/ClosedManifold; 3 is `OutOfRange` with a warning (`brep.rs:375-411`). Wrong naming; it affects 274 sample Breps |
| 10 | "Ignore solid flag for old writers" | `ArchiveOpenNURBSVersion() < 20021002`, an 8-digit constant that never matches a real stamp (:1134-1137) | `200_210_020` (`brep.rs:870-909`) |
| 11 | Validation at read | none; bad indexes leave null proxies | full reciprocity, ring and outer-first checks (`brep.rs:567-810`); failure gives `SemanticInvalid` with the raw data kept |
| 12 | Finite values | not checked | vertex points, intervals and bbox must be finite (`brep.rs:3047-3052`; `settings.rs:907-940`) |
| 13 | Failed mesh slot (3.x) | whole Brep read fails | mesh chunk degraded to empty with a warning (`brep.rs:2412-2427`) |
| 14 | Stored bbox | recomputed for archive < 90 (:1162-1177) | kept as stored |
| 15 | Trim index ≠ position | index overwritten (:475-484) | warning; array order used (`brep.rs:572-596`). Equivalent outcome |
| 16 | Legacy 2.x reconstruction | geometric (`SetVertices` etc.); `outer_flag` sets solid; the twin-index fix-up loop is dead code because its conditions are inverted (:1348-1365) | topological union-find; `outer_flag` ignored; iso always none |

### 5a.16 Samples (paths relative to `samples/`)

**By payload version and layout:**
- **2.1 legacy:** every `opennurbs/example_files/V2/*.3dm` except `v2_HumanHead1`, `v2_my_brep` and `v2_my_trimmed_surface`. Examples:
  - `V2/v2_RhinoLogo.3dm`: inner loops.
  - `V2/v2_UntrimmedSurfaces.3dm`: 141 faces, edgeless trims.
  - `V2/v2_Camera.3dm`.
  - None has meshes.
- **3.2 with legacy edge/trim layout:** `opennurbs/example_files/V2/v2_my_brep.3dm` and `v2_my_trimmed_surface.3dm` (archive 2, ON_Brep, stamp 200210150).
- **3.2, current layout:**
  - All `V3/*.3dm`, plus `V2/v2_HumanHead1.3dm`, which is archive 3 despite its folder.
  - All `V4/*.3dm`.
  - All `V5/*.3dm`.
  - `rhino-developer-samples/rhino3dm/models/nurbsSurface_r5.3dm`.
  - `rhino-developer-samples/rhinomobile/HelloRhinoMobile/HelloRhino.Droid/Assets/Models/Rhino Logo.3dm`.
- **3.3:**
  - Archive 60: `V6/v6_rhino_logo.3dm`, `V6/my_trimmed_surface.3dm`, `rhino-developer-samples/rhinocommon/cs/SampleCsRendererIntegration/MockingBird/TestFiles/MockingBirdTest.3dm`.
  - Archive 70: `V7/v7_rhino_logo_nurbs.3dm`, `V7/v7_my_brep.3dm`, `V7/v7_my_trimmed_surface.3dm`, `three.js/examples/models/3dm/Rhino_Logo.3dm`, `rhino3dm/tests/DotNetTester/test_in.3dm`, `rhino-developer-samples/rhino3dm/js/SampleJSNurbsSurfaceProperties/nurbssurface.3dm`.
  - Archive 80: `V8/v8_selective_clipping.3dm`, `rhino3dm/tests/models/{blocks,groups,sphereDecals,file3dm_stuff}.3dm`, `rhino-developer-samples/rhino3dm/models/{RenderContent,nurbssurface}.3dm`, `rhino-developer-samples/rhino.inside/dotnet-netcore/BatchOperation/files/0{2,3}.3dm`.

**Face-array version:**
- 1.1 in archive 70: the `V7/v7_*` files.
- 1.2: `three.js/.../Rhino_Logo.3dm`, `DotNetTester/test_in.3dm`, and all archive-80 files.
- The per-face colour flag is always 0.

**Render meshes:**
- Present on every face:
  - `V4/*` except `v4_Wheel_PG`, which mixes meshed Breps with four unmeshed RevSurface Breps.
  - `V5/v5_disk_brake.3dm`: 888 faces.
  - `V5/v5_clip.3dm`, `v5_teacup.3dm`, `v5_ring.3dm`, `v5_rhino_logo.3dm`.
  - `V3/v3_Smooth.3dm`, `v3_Speeder.3dm`, `v3_SphericalSpiral.3dm`.
  - All the Rhino_Logo variants.
  - `V8/v8_selective_clipping.3dm`, `MockingBirdTest.3dm`, `sphereDecals.3dm`, `RenderContent.3dm`.
- Absent: `V*/…my_brep.3dm` and `…my_trimmed_surface.3dm`, `V5/v5_teapot.3dm`, `V5/v5_example_file.3dm`, most of `V3/*`, `blocks.3dm`, `groups.3dm`.
- Mixed within one file: `rhino3dm/tests/models/file3dm_stuff.3dm` (one of five Breps is meshed).
- Within a single Brep, the samples are always all faces meshed or none.
- 5,329 of 6,155 major-3 faces carry a render mesh.
- **Analysis meshes: none anywhere.**

**Planar faces:**
- **PlaneSurface without a mesh** (fallback tessellation targets):
  - `V3`, `V5`, `V6`, `V7` `…my_trimmed_surface.3dm` (3 LineCurve boundary trims).
  - `V5/v5_example_file.3dm`, `V5/v5_teapot.3dm`.
  - `file3dm_stuff.3dm`: two single-face PlaneSurface Breps without meshes.
- **PlaneSurface with a mesh and inner loops:**
  - `V5/v5_disk_brake.3dm`: 189 PlaneSurfaces, 740 inner loops.
  - `V4/v4_DVDCase_Solid.3dm`: 78 PlaneSurfaces.
  - `V4/v4_Prarie settle.3dm`: 41 PlaneSurfaces.
- **Planar NurbsSurface box:** `V*/…my_brep.3dm`, 6 faces of which 4 are reversed.
- **Per-face material channels 1 and 2:** `MockingBirdTest.3dm`.

**Edge cases:**
- **Edge proxy reversal:** one edge in each Rhino_Logo variant (`V5`/`V6`/`V7` logos, the three.js logo, HelloRhino `Rhino Logo.3dm`).
- **Singular trims:** 464 trims with `ei=-1`, for example `V3/v3_HumanHead2.3dm`, `V4/v4_TreeFrog.3dm`, `V8/v8_selective_clipping.3dm`.
- **Seam trims:** common, for example `V5/v5_disk_brake.3dm`.
- Trim types seen: 1-4 only. Loop types seen: 1 and 2 only. No `c2i=-1`. No trim proxy reversal. No region topology present.

### 5a.17 Uncertain

- The V7 writer stamp at which face array 1.2 began. It lies between 2382393748 (writes 1.1) and 2382395042 (writes 1.2).
- Payload 3.0 and 3.1 (toolkit writers before about 2002) are not in the samples. Their layouts are inferred from the reader only.
- Not exercised by any sample, so taken from source only: crvonsrf, ptonsrf and slit trims and loops; `c2i = -1`; trim proxy reversal; per-face colours; analysis meshes; region topology present (inline in 3.3 or as V5 userdata).
- Whether Rhino ever writes a partial set of render meshes within one Brep.
- The orientation check used vertex polygons. All 3 of its disagreements involve curved edges (PolyCurve or NURBS), so they look like test artefacts, not rule violations, but this was not proven by sampling the curves.
- That cached render meshes follow face orientation was verified on planar faces only (351 faces). It is assumed to hold for curved faces too.


---

## 5b. Curves and surfaces (Read layouts)

Scope: the class-data payloads of the openNURBS curve and surface classes, both as top-level object records and as objects embedded through `ON_BinaryArchive::ReadObject` (Brep C2/C3/S arrays, polycurve segments, revolution/sum-surface profiles, extrusion profiles, hatch loops). Archive versions 2, 3, 4, 50, 60, 70 and 80. Archive 1 (V1 legacy records) is out of scope here; see the last trap.

Citations: `cad/` = `cadmpeg-codec-rhino-0.6.1/src/`, `on/` = openNURBS repository root (v8.35, commit eb92af3).

**How the layouts were checked.** A throwaway probe (`probe.py`) found every class wrapper in every sample, including nested ones, and decoded the class data with the tables below. It required the bytes consumed to equal the class-data length minus the 4-byte CRC exactly. Results: 86,131 wrappers in the 195 manifest and synthetic files and 55,625 in the local `samples/private/` files (aggregate only, paths not listed). Every curve and surface payload decoded with **zero** failures or leftover bytes.

---

### 5b.0 Primitives used below

All values are little-endian. "Chunk version" is one byte.

| openNURBS call | Rust | Bytes | Notes |
|---|---|---|---|
| `Read3dmChunkVersion(&maj,&min)` | `u8`; `maj = b >> 4`, `min = b & 0x0f` | 1 | `on/opennurbs_archive.cpp:5771` (writer at 5762 writes `maj*16+min`). Same in every archive version. |
| `ReadInt` | `i32` | 4 | |
| `ReadDouble` | `f64` | 8 | |
| `ReadChar` | `u8` | 1 | |
| `ReadBool` | `u8` (0/1) | 1 | `on/…archive.cpp:2784`. Writers before 6.0 (2017-08-24) may store other non-zero values; treat any non-zero as true. cadmpeg: `BoundedReader::bool` (`cad/chunks.rs:314`). |
| `ReadPoint` / `ReadVector` | `[f64;3]` | 24 | |
| `ReadInterval` | `[f64;2]` | 16 | `on/…archive.cpp:1439`; cadmpeg `interval` (`cad/settings.rs:907`) |
| `ReadBoundingBox` | min `[f64;3]`, max `[f64;3]` | 48 | `on/…archive.cpp:1294`; cadmpeg `bbox` (`cad/settings.rs:935`). The default "empty" box is min (1,0,0), max (−1,0,0) (`on/opennurbs_bounding_box.cpp:24`). |
| `ReadLine` | from `[f64;3]`, to `[f64;3]` | 48 | `on/…archive.cpp:1357` |
| `ReadPlane` | origin `[f64;3]`, xaxis `[f64;3]`, yaxis `[f64;3]`, zaxis `[f64;3]`, plane equation `[f64;4]` (a,b,c,d with a·x+b·y+c·z+d=0) | **128** | `on/…archive.cpp:1338`; cadmpeg `plane` (`cad/settings.rs:914-932`). In every sample the equation equals (zaxis, −origin·zaxis). Readers can ignore it. |
| `ReadCircle` | plane (128), radius `f64`, then 3 points `[f64;3]` that are discarded | **208** | `on/…archive.cpp:1407`. Since Nov 2001 the writer stores `PointAt(0)`, `PointAt(π/2)` and `PointAt(π)` (`WriteCircle`, ~1385). openNURBS reads them into a scratch variable. |
| `ReadArc` | circle (208) + angle interval (16, radians) | **224** | `on/…archive.cpp:1374` |
| `ReadArray(ON_SimpleArray<double>)` | `count: i32`, then `count × f64` | 4+8n | `on/…archive.cpp:2296`. A count ≤ 0 means empty. |
| `ReadArray(ON_SimpleArray<ON_3dPoint>)` | `count: i32`, then `count × [f64;3]` | 4+24n | `on/…archive.cpp:2343`. Always 3 doubles per point, even for 2-D curves. |
| `ReadUuid` | `u32 Data1, u16 Data2, u16 Data3, [u8;8] Data4` | 16 | `on/…archive.cpp:1445`; cadmpeg `Uuid::from_wire` (`cad/wire.rs:258`) |
| `BeginRead3dmChunk(TCODE_ANONYMOUS_CHUNK,&maj,&min)` | chunk `0x40008000` header, then `maj: i32`, `min: i32` (requires `maj ≥ 1`); the body ends with a CRC32 | hdr+8 | `on/…archive.cpp:6396`. The header is 8 bytes for archive < 50 and 12 bytes for ≥ 50. |

`ON_KnotCount(order, cv_count) = order + cv_count − 2`: openNURBS omits the two "superfluous" end knots.

---

### 5b.1 Framing of an embedded object (`ReadObject`)

`ReadObject(&p)` (`on/…archive.cpp:5044`, body in `ReadObjectHelper` at 5062) reads the same class wrapper used at top level. It can appear anywhere inside a parent's class data:

| Seq | Chunk | Contents |
|---|---|---|
| 1 | `0x00027FFA` TCODE_OPENNURBS_CLASS (long, no CRC) | container for 2 to 5 |
| 2 | `0x0002FFFB` CLASS_UUID (long, CRC) | length must be **20**: 16-byte UUID + CRC32 |
| 3 | `0x0002FFFC` CLASS_DATA (long, CRC) | `Class::Read` payload, followed by a CRC32 over the chunk's direct bytes (nested chunks excluded, see 0.3) |
| 4 | `0x00027FFD` CLASS_USERDATA (long) × 0..n | optional; may appear on embedded objects too |
| 5 | `0x80027FFF` CLASS_END (short, value 0) | |

- **Nil UUID.** `WriteObject(nullptr)` writes only chunks 1 and 2 with the nil UUID. `ReadObject` returns 1 with a null object (`on/…:5150-5155`). cadmpeg returns an empty data range for this case (`cad/objects.rs:546-557`).
- **Unknown UUID.** `ClassId(uuid)` returns null, `ReadObject` returns **3**, and `EndRead3dmChunk` skips the whole `0x00027FFA` chunk (`on/…:5188-5197`, 5300). What the parent does next depends on the parent:
  - `ON_CurveArray::Read` / `ON_SurfaceArray::Read` (Brep C2/C3/S) treat 3 as success and leave a **null slot** (`on/opennurbs_curve.cpp:2492-2540`, `on/opennurbs_surface.cpp:1449-1490`).
  - `ON_PolyCurve::Read` fails: the null object fails `ON_Curve::Cast` (`on/opennurbs_polycurve.cpp:475-484`).
  - `ON_RevSurface` and `ON_SumSurface` keep a null curve.
- **Recommended Rust behaviour.** Parse the wrapper generically (cadmpeg `parse_class_wrapper`, `cad/objects.rs:502-604`) and advance past the whole wrapper whatever the UUID. Dispatch on the UUID, and report unknown classes as unsupported objects without failing the parent's framing.
- **Class-data length.** For archive ≥ 2, the payload is `length − 4`. cadmpeg's `chunk_at` strips the CRC32 for typecodes with bit `0x8000` (`cad/chunks.rs:465-478`, `563-647`). Archive 1 uses CRC16 instead.
- **Trailing bytes.** cadmpeg calls `reader.skip_remaining()` after every payload (`cad/curves.rs:545`, `cad/surfaces.rs:253`). Future minor versions with extra fields are tolerated this way. Keep this.
- **Brep array framing (reference only; the Brep layout is covered elsewhere).**
  - Each of C2, C3 and S is `TCODE_ANONYMOUS_CHUNK` (`BeginRead3dmBigChunk`, **no** major/minor ints). Its body is: chunk version byte (1.0), `count: i32`, then per slot `flag: i32` (1 = present, 0 = null) followed by a class wrapper when flag = 1. A CRC32 (direct bytes only) closes the body.
  - Sources: `on/opennurbs_curve.cpp:2467-2530`; cadmpeg `read_children`, `cad/brep.rs:1976-2060`. cadmpeg rejects flags other than 0/1.
- **Legacy Brep 2.x (archive 2).** `ON_Brep::ReadOld200` (`on/opennurbs_brep_io.cpp:1182`) stores each C2/C3 curve as a **bare `ON_PolyCurve::Read` payload with no wrapper**; its segments are wrapped. Each surface is a **bare `ON_NurbsSurface::Read` payload**. cadmpeg: `parse_legacy_major2`, `cad/brep.rs:1180-1270`, using `consume_legacy_polycurve(_2d)` (`cad/curves.rs:1870`, `1453`) and `read_nurbs_surface_prefix` (`cad/surfaces.rs:1079`). In the samples, old-style Breps carry chunk version 2.1 and every newer Brep carries 3.2 or 3.3.

### 5b.2 Class UUIDs (wire → class)

The canonical text form is shown. On disk Data1/2/3 are little-endian; for example ON_LineCurve is stored as `db d4 d7 4e 47 e9 d3 11 bf e5 00 10 83 01 22 f0`.

| UUID | Class | Status |
|---|---|---|
| 4ED7D4DD-E947-11d3-BFE5-0010830122F0 | ON_NurbsCurve | |
| 5EAF1119-0B51-11d4-BFFE-0010830122F0 | TL_NurbsCurve → ON_NurbsCurve | alias (`on/opennurbs_object.cpp:894-930`). The most common NURBS UUID in archives 3 to 80. |
| 76A709D5-1550-11d4-8000-0010830122F0 | old NURBS curve → ON_NurbsCurve | alias; archive 2 only |
| 4ED7D4DE-E947-11d3-BFE5-0010830122F0 | ON_NurbsSurface | |
| 4760C817-0BE3-11d4-BFFE-0010830122F0 | TL_NurbsSurface → ON_NurbsSurface | alias |
| FA4FD4B5-1613-11d4-8000-0010830122F0 | old NURBS surface → ON_NurbsSurface | alias (none seen) |
| 4ED7D4E6-E947-11d3-BFE5-0010830122F0 | ON_PolylineCurve | |
| 4ED7D4DB-E947-11d3-BFE5-0010830122F0 | ON_LineCurve | |
| CF33BE2A-09B4-11d4-BFFB-0010830122F0 | ON_ArcCurve | |
| CF33BE29-09B4-11d4-BFFB-0010830122F0 | ON__OBSOLETE__CircleCurve → read as an arc | V2-era (`on/opennurbs_arccurve.cpp:1210-1270`); none in samples; **cadmpeg lacks it** |
| 4ED7D4E0-E947-11d3-BFE5-0010830122F0 | ON_PolyCurve | |
| EF638317-154B-11d4-8000-0010830122F0 | old poly curve → ON_PolyCurve | alias; all 722 archive-2 polycurves use it |
| 4ED7D4DF-E947-11d3-BFE5-0010830122F0 | ON_PlaneSurface | |
| DBC5A584-CE3F-4170-98A8-497069CA5C36 | ON_ClippingPlaneSurface | |
| A16220D3-163B-11d4-8000-0010830122F0 | ON_RevSurface | |
| 0A8401B6-4D34-4b99-8615-1B4E723DC4E5 | TL_RevSurface → ON_RevSurface | alias |
| C4CD5359-446D-4690-9FF5-29059732472B | ON_SumSurface | |
| 665F6331-2A66-4cce-81D0-B5EEBD9B5417 | TL_SumSurface → ON_SumSurface | alias; **cadmpeg lacks it**, yet 129 occurrences in archive 4/50 samples |
| 4ED7D4D8-E947-11d3-BFE5-0010830122F0 | ON_CurveOnSurface | none in samples |
| 4ED7D4D9-E947-11d3-BFE5-0010830122F0 | ON_CurveProxy | `Read`/`Write` return false, so never serialized |
| 4ED7D4E2-E947-11d3-BFE5-0010830122F0 | ON_SurfaceProxy | same |
| 00C61749-D430-4ecc-83A8-29130A20CF9C | ON_OffsetSurface | inherits ON_SurfaceProxy::Read, so never serialized |

cadmpeg constants: `cad/curves.rs:29-70`, `cad/surfaces.rs:30-56`. Dispatch: `supported_class` / `curve_class` / `surface_class` at `cad/curves.rs:262-317`.

---

### 5b.3 ON_NurbsCurve::Read

Source: `on/opennurbs_nurbscurve.cpp:816-886` (Write at 761). cadmpeg: `read_nurbs_curve_inner`, `cad/surfaces.rs:993-1066`.

| # | Field | Read call | Rust | Bytes |
|---|---|---|---|---|
| 1 | chunk version | Read3dmChunkVersion | u8: **major must be 1**; minor 0 or 1 | 1 |
| 2 | dim | ReadInt | i32 (2 for Brep C2/trim curves, 3 otherwise) | 4 |
| 3 | is_rat | ReadInt | i32 (0/1) | 4 |
| 4 | order | ReadInt | i32 (degree+1, ≥ 2) | 4 |
| 5 | cv_count | ReadInt | i32 (≥ order) | 4 |
| 6 | reserved1 | ReadInt | i32: "legacy flag values"; **ignore** | 4 |
| 7 | reserved2 | ReadInt | i32: ignore | 4 |
| 8 | bbox | ReadBoundingBox | 6 × f64: ignore | 48 |
| 9 | knot_count | ReadInt | i32 = order+cv_count−2 (or 0) | 4 |
| 10 | knots | ReadDouble(knot_count) | knot_count × f64 | 8k |
| 11 | stored_cv_count | ReadInt | i32 (= cv_count, or 0) | 4 |
| 12 | CVs | cv_count × ReadDouble(cv_size) | `cv_size = dim + is_rat` f64 per CV, CV-major | 8·cv_count·cv_size |
| 13 | SubD-friendly tag | ReadBool, **only if minor ≥ 1** | u8 | 1 |

Fixed prefix before the knots: 1 + 6×4 + 48 = **73 bytes**.

- **Rational CVs are homogeneous**: stored as (w·x, w·y, [w·z], w), with the weight last.
  - Documented in `on/opennurbs_nurbscurve.h:1327-1385` and `on/opennurbs_nurbssurface.h:1181`.
  - Verified on bytes: a `v5_disk_brake.3dm` arc stores (1.83377, −1.47159, 0.970994) in the middle. Dividing by w gives equal control-polygon legs and cos(half-angle) = w (exact circle); taking the stored values as Euclidean does not.
  - cadmpeg divides by w while reading (`read_poles`, `cad/surfaces.rs:1281-1328`).
- **The minor version follows the writer's date, not the archive version.** The writer emits `minor = (Archive3dmVersion() >= 60)` since April 2019 (`on/…nurbscurve.cpp:765`). Archive-60 files from Rhino 6 (including all 40k curves in the private V6 files and every V6 sample) are 1.0; archive 70/80 files are 1.1. **Branch on the minor nibble.**
- In the samples the tag byte is always 0. Archives 2/3/4/50/60 store 1.0; 70/80 store 1.1.
- **Old writers.** In archive 2 (Rhino 2), reserved1 holds legacy flags (0x80000000, 0x80000001, 0x80000067, …) and the bbox holds a real box. Archives 3 and later write 0, 0 and the empty box.
- **2-D curves.** dim = 2 means 2 (or 3 with w) doubles per CV; there is no z.
- **Domain.** It is `[knot[order−2], knot[cv_count−1]]` on the stored, 0-based knot array (`ON_GetKnotVectorDomain`). To build a standard clamped knot vector, prepend `knot[0]` and append `knot[last]`. For periodic curves, extrapolate as in cadmpeg `reconstructed_endpoints` (`cad/surfaces.rs:1377-1414`).

### 5b.4 ON_NurbsSurface::Read

Source: `on/opennurbs_nurbssurface.cpp:773-902` (Write at 724). cadmpeg: `read_nurbs_surface_prefix`, `cad/surfaces.rs:1079-1183`.

| # | Field | Rust | Bytes |
|---|---|---|---|
| 1 | chunk version | u8, major 1; writer always 1.0 | 1 |
| 2 | dim | i32 | 4 |
| 3 | is_rat | i32 | 4 |
| 4-5 | order[0] (u), order[1] (v) | i32, i32 (each ≥ 2) | 8 |
| 6-7 | cv_count[0] (u), cv_count[1] (v) | i32, i32 (each ≥ its order) | 8 |
| 8-9 | reserved1, reserved2 | i32, i32: ignore | 8 |
| 10 | bbox | 6 × f64: ignore | 48 |
| 11 | knot_count0 + knots | i32 = order0+cv0−2, then f64s | 4+8k₀ |
| 12 | knot_count1 + knots | i32 = order1+cv1−2, then f64s | 4+8k₁ |
| 13 | cv_total | i32 = cv0·cv1 | 4 |
| 14 | CVs | for i in 0..cv0 { for j in 0..cv1 { cv_size × f64 } } | 8·cv0·cv1·cv_size |

Fixed prefix: 1 + 8×4 + 48 = **81 bytes**.

- **CV order is u-major**: the outer loop runs over the u index i and v varies fastest (`on/…:880-893`). Flat index = `i·cv_count[1] + j`. cadmpeg builds rows of length `v_count` (`cad/surfaces.rs:1157-1167`).
- Rational CVs are homogeneous, as for curves.
- openNURBS 8 enforces exact counts: `knot_count == ON_KnotCount` and `cv_total == cv0·cv1` (`on/…:820-878`).
- Samples contain dim 3 only; 7,810 non-rational and 2,172 rational.

### 5b.5 ON_PolylineCurve::Read

Source: `on/opennurbs_polylinecurve.cpp:248-259`. cadmpeg: `read_polyline`, `cad/curves.rs:1587-1662`.

| # | Field | Rust | Bytes |
|---|---|---|---|
| 1 | chunk version | u8, major 1 (1.0) | 1 |
| 2 | points | ReadArray(ON_3dPoint): `n: i32`, then n × [f64;3] | 4+24n |
| 3 | parameters t | ReadArray(double): `m: i32`, then m × f64 (m = n in all samples; strictly increasing) | 4+8m |
| 4 | dim | i32 (2 or 3) | 4 |

2-D polylines (Brep trims, extrusion profiles) still store 3 doubles per point, with z = 0. As a NURBS curve the polyline has degree 1 and knots `t[0], t[0..n], t[n−1]` (cadmpeg 1645-1656).

### 5b.6 ON_LineCurve::Read

Source: `on/opennurbs_linecurve.cpp:229-243`. cadmpeg: `read_line`, `cad/curves.rs:1552-1585`.

| # | Field | Rust | Bytes |
|---|---|---|---|
| 1 | chunk version | u8 1.0 | 1 |
| 2 | line | from [f64;3], to [f64;3] | 48 |
| 3 | domain t | [f64;2] | 16 |
| 4 | dim | i32 (2 inside Brep C2 arrays, else 3) | 4 |

The total is always **69 bytes**, plus the CRC.

Worked example, `V5/v5_my_curves.3dm`, archive 50, so lengths are i64:

```
fa 7f 02 00 | 81 00 00 00 00 00 00 00          TCODE_OPENNURBS_CLASS, len 0x81
fb ff 02 00 | 14 00 00 00 00 00 00 00          CLASS_UUID, len 20
db d4 d7 4e 47 e9 d3 11 bf e5 00 10 83 01 22 f0   ON_LineCurve UUID (wire order)
f4 9b e9 23                                    CRC32
fc ff 02 00 | 49 00 00 00 00 00 00 00          CLASS_DATA, len 73 = 69 + CRC
10 | from(1,2,-1.5)… to … | t[0, 5.41..] | 03 00 00 00 | CRC
ff 7f 02 80 | 00 00 00 00 00 00 00 00          CLASS_END (short)
```

Archive 4 has the same layout with 4-byte lengths.

### 5b.7 ON_ArcCurve::Read

Source: `on/opennurbs_arccurve.cpp:262-286`. cadmpeg: `read_arc` + `read_circle`, `cad/curves.rs:1664-1794`.

| # | Field | Rust | Bytes |
|---|---|---|---|
| 1 | chunk version | u8; major must be 1 | 1 |
| 2 | plane | ReadPlane (origin, x, y, z, equation) | 128 |
| 3 | radius | f64 | 8 |
| 4 | 3 redundant points | 3 × [f64;3] (PointAt 0, π/2, π): skip | 72 |
| 5 | angle interval | [f64;2], radians, increasing, span ≤ 2π | 16 |
| 6 | domain t | [f64;2] | 16 |
| 7 | dim | i32; openNURBS forces 3 if not 2 or 3 | 4 |

The total is **245 bytes**.

- Evaluation: C(t) = origin + r·(cos a·x + sin a·y), where a is t mapped linearly from the domain onto the angle interval.
- In the samples, every arc's redundant points match the canonical values within 1e‑8, including archive 3.
- **Obsolete circle (CF33BE29…)**: version byte, ReadCircle (208), domain (16), dim (4) = 229 bytes. It is a full circle with angle [0, 2π].

### 5b.8 ON_PolyCurve::Read

Source: `on/opennurbs_polycurve.cpp:441-520`. cadmpeg: `read_polycurve`, `cad/curves.rs:1796-1868` (2-D variant at 1387).

| # | Field | Rust | Bytes |
|---|---|---|---|
| 1 | chunk version | u8 (1.0). openNURBS does **not** check the major. | 1 |
| 2 | segment_count | i32 | 4 |
| 3-4 | reserved1, reserved2 | i32, i32 | 8 |
| 5 | bbox | 6 × f64 (empty box since archive 3) | 48 |
| 6 | t | ReadArray(double): count = segment_count+1, strictly increasing | 4+8(n+1) |
| 7 | segments | segment_count × embedded class wrapper (5b.1), each an ON_Curve | var |

- Segment i covers polycurve parameters `[t[i], t[i+1]]`, reparameterized linearly from the segment's own domain.
- After a successful read, openNURBS calls `SanitizeDomain()` (`:492`) to snap tiny "fuzz" in `t` to the segment ends.
- **Archive 2.** All curves (top-level and inside old Breps) are old-UUID polycurves, even with a single segment. reserved1 = 0x80000000, 0x80000001 or 0x80000002, and the bbox is filled. Their segments are old-UUID NURBS curves.
- Segment classes seen: NURBS (all UUID aliases), arc, line, polyline. No nested polycurves appear, but the format allows them, so recurse with a depth limit (cadmpeg: 32, `cad/curves.rs:25`).

### 5b.9 ON_PlaneSurface::Read

Source: `on/opennurbs_planesurface.cpp:122-149`. cadmpeg: `read_plane_surface_with_parameterization`, `cad/surfaces.rs:1185-1224`.

| # | Field | Rust | Bytes |
|---|---|---|---|
| 1 | chunk version | u8; major 1; writer 1.1 (all samples) | 1 |
| 2 | plane | ReadPlane | 128 |
| 3-4 | domain[0] (u), domain[1] (v) | 2 × [f64;2]: parameter space | 32 |
| 5-6 | extents[0], extents[1] | 2 × [f64;2]: **only if minor ≥ 1**, else extents = domain | 32 |

The total is 193 bytes for 1.1 and 161 bytes for 1.0.

- **Geometry**: P(s,t) = origin + x·X(s) + y·Y(t), where X maps s from domain[0] onto extents[0] and Y maps t from domain[1] onto extents[1] (`ON_PlaneSurface::Evaluate`, `on/…:337-370`).
- The extents are the physical rectangle in plane coordinates. They are **not** always equal to the domain: the example writer sets the domain to ±10 and leaves the extents at [0,1] (`V*/…my_spot_light`, `…my_trimmed_surface`). Trims live in domain space.

### 5b.10 ON_ClippingPlaneSurface::Read (and how to skip it)

Source: `on/opennurbs_planesurface.cpp:1735-1775` (Write 1706), ON_ClippingPlane::Read at 1345-1499. cadmpeg: `read_clipping_plane_surface` / `read_clipping_plane` / `read_uuid_list` / `read_clipping_participation`, `cad/surfaces.rs:257-404`.

```
ANON chunk 0x40008000 { maj:i32 = 1, min:i32 = 0
  ANON chunk 0x40008000 (no version ints) { ON_PlaneSurface payload (5b.9) } CRC
  ON_ClippingPlane: ANON chunk { maj:i32 = 1, min:i32 (writer 5; samples 1.1)
     viewport_id uuid(16) [used only if min==0], plane_id uuid(16), ReadPlane(128), enabled u8,
     if min>0: ON_UuidList = ANON chunk { maj,min i32; count:i32; count×uuid } CRC,
     if min>1: depth f64,  if min>=4: depth_enabled u8,
     if min>=5: tagged items u8 {10: n:i32 + n×uuid; 11: n:i32 + n×i32 layer index;
                 12: bool; 13: bool; 0 = end; ≥14 = newer, stop} } CRC
} CRC
```

- **To skip.** Take the outer anonymous chunk's length; geometry readers only need the inner plane surface. Clipping planes are not render geometry: report them, don't draw them.
- Samples: archive 80 (`V8/v8_section_hatch.3dm`, `V8/v8_selective_clipping.3dm` ×2) and one private archive-60 file. All have outer 1.0, plane 1.1 and clip chunk minor 1, so the clip body is 217 bytes.

### 5b.11 ON_RevSurface::Read

Source: `on/opennurbs_revsurface.cpp:265-325` (Write 241). cadmpeg: `read_revolution`, `cad/surfaces.rs:406-500`.

| # | Field | Rust | Bytes |
|---|---|---|---|
| 1 | chunk version | u8: **major 1 or 2** (writer 2.0; all samples 2.0) | 1 |
| 2 | axis | ReadLine (from, to) | 48 |
| 3 | angle | [f64;2] radians | 16 |
| 4 | t | [f64;2], **major 2 only**; for major 1, t = [angle.min, angle.max] | 16 |
| 5 | bbox | 6 × f64 | 48 |
| 6 | transposed | i32 (non-zero = true; samples hold 0 and 1) | 4 |
| 7 | has_curve | ReadChar u8 | 1 |
| 8 | profile curve | embedded wrapper, only if has_curve ≠ 0 | var |

- Not transposed: u = angle (t domain) and v = the profile curve parameter. Transposed swaps them.
- Profiles seen: line, arc, NURBS, polycurve, polyline.
- A revolution surface is planar only in special cases (for example a line profile perpendicular to the axis, which gives a disk or annulus). Tessellate it, or convert it exactly to NURBS (rational in the angular direction; see cadmpeg `revolution_nurbs`, `cad/surfaces.rs:551`).

### 5b.12 ON_SumSurface::Read

Source: `on/opennurbs_sumsurface.cpp:262-288`. cadmpeg: `read_sum`, `cad/surfaces.rs:502-544`.

| # | Field | Rust | Bytes |
|---|---|---|---|
| 1 | chunk version | u8 1.0 | 1 |
| 2 | basepoint | [f64;3] vector | 24 |
| 3 | bbox | 6 × f64 | 48 |
| 4 | curve[0] | embedded wrapper (u direction) | var |
| 5 | curve[1] | embedded wrapper (v direction) | var |

- S(u,v) = curve0(u) + curve1(v) + basepoint.
- It is planar when both curves are lines (a parallelogram), or more generally when both curves are planar in parallel planes and at least one is a line. Rhino writes extrusions of planar curves this way, mostly in V4/V5 Breps.

### 5b.13 ON_CurveOnSurface, ON_CurveProxy, ON_SurfaceProxy / ON_OffsetSurface, Bezier

- **ON_CurveOnSurface** (`on/opennurbs_curveonsurface.cpp:184-230`) has **no version byte**. Layout: wrapper c2, `has_c3: i32`, [wrapper c3], wrapper surface.
  - openNURBS's reader is buggy: it sets `rc = false` after c2 and writes c3 into `m_c2`, so it only succeeds when there is no c3.
  - cadmpeg reads all three (`cad/curve_on_surface.rs:42-131`) and requires has_c3 to be 0 or 1.
  - It does not occur in any sample. Treat it as unsupported and skip the wrapper.
- **ON_CurveProxy, ON_SurfaceProxy, ON_OffsetSurface.** `Read`/`Write` return false (`on/opennurbs_curveproxy.cpp:323-337`, `on/opennurbs_surfaceproxy.cpp:120-134`). They can never appear as wrappers.
  - ON_BrepEdge, ON_BrepTrim and ON_BrepFace derive from these proxies but are serialized by Brep code. For example `ON_BrepEdge::Read` (`on/opennurbs_brep_io.cpp:69`) stores `m_c3i`, `bReversed`, a proxy-domain interval and, in newer minors, a separate edge domain. That belongs to the Brep section.
  - ON_BumpFunction (from `opennurbs_offsetsurface.h`) is not serialized.
- **ON_BezierCurve / ON_BezierSurface** are not ON_Objects and have no Read/Write, so they never appear in .3dm files.

### 5b.14 Planarity tips (keep cheap)

1. **ON_PlaneSurface** is planar by class. Its rectangle is the extents; trims are in domain space.
2. **ON_NurbsSurface** with order 2×2 and 2×2 CVs is planar if the 4 Euclidean CVs (divide by w) are coplanar within tolerance; a bilinear patch may be twisted. More generally, for any NURBS surface, fit a plane to all Euclidean CVs. By the convex-hull property, max distance ≤ tol means planar. This is the cheapest robust test.
3. **ON_SumSurface**: see 5b.12. **ON_RevSurface**: line profile perpendicular to the axis.
4. For Breps, use per-face tests and remember that `face.m_bRev` flips the normal. For style signals, weight faces by area.

---

### 5b.15 cadmpeg vs openNURBS differences

1. **NURBS counts.**
   - openNURBS accepts `knot_count == 0` (`on/…nurbscurve.cpp:850-853`). If `stored_cv_count > 0` it reads `m_cv_count` CVs whatever the stored value.
   - cadmpeg requires `knot_count == order+cv_count−2` and `stored_cv_count == cv_count` (`cad/surfaces.rs:1017-1040`), and also dim ∈ {2,3}, is_rat ∈ {0,1}, order ≥ 2, non-decreasing finite knots and an increasing domain (1261-1279, 1483-1503).
   - Every sample satisfies the stricter checks (94,972 curves).
2. **Homogeneous storage.** openNURBS keeps (w·x, w·y, w·z, w). cadmpeg converts to Euclidean points plus a separate weight and rejects w = 0 (`cad/surfaces.rs:1281-1328`).
3. **Knot vector.** openNURBS stores order+cv_count−2 knots. cadmpeg rebuilds a full order+cv_count vector with an extrapolation rule for periodic-looking knots (`cad/surfaces.rs:1377-1414`).
4. **Arcs.** openNURBS discards the 3 redundant circle points. cadmpeg **requires** them to match PointAt(0), PointAt(π/2) and PointAt(π) within 1e‑8, and the axes to be orthonormal within 1e‑10 (`cad/curves.rs:1757-1789`). This holds for every sample, but files written before Nov 2001 (when `ON_Circle::m_point[]` held arbitrary points) would be rejected by cadmpeg. Recommendation: skip the 72 bytes, as openNURBS does.
5. **Plane-surface extents.** cadmpeg reads them only when `minor == 1` (`cad/surfaces.rs:1199`); openNURBS reads them when `minor >= 1`. Use `>= 1`.
6. **Polycurve major.** openNURBS ignores the major; cadmpeg requires major 1 (`cad/curves.rs:1804-1809`). cadmpeg also requires count > 0, t count = count+1 and strictly increasing t. It does not apply `SanitizeDomain`.
7. **Lines and polylines.** cadmpeg rejects zero-length lines, decreasing domains, polylines with fewer than 2 points or mismatched t counts, and dims other than 2/3 (`cad/curves.rs:1567-1572`, `1593-1640`). openNURBS validates none of these at read time.
8. **Revolution surfaces.** cadmpeg requires has_curve == 1 and transposed ∈ {0,1}, and rejects a degenerate axis (`cad/surfaces.rs:447-468`). openNURBS accepts a missing curve and any non-zero flag.
9. **Missing aliases and nil children.** cadmpeg lacks TL_SumSurface (665F6331…) and the obsolete circle (CF33BE29…). openNURBS maps both. cadmpeg also requires both sum-surface curves to be present; openNURBS tolerates nil-UUID children.
10. **Allowed embedded children.** cadmpeg `decode_embedded_curve` (revolution/sum profiles) accepts only line, arc, polyline, polycurve and NURBS UUIDs (`cad/curves.rs:579-591`). Polycurve segments go through `supported_class` (`cad/curves.rs:1838`). openNURBS accepts any ON_Curve.
11. **Clipping-plane depth.** cadmpeg reads the depth f64 for minor ≥ 2 and ignores the openNURBS rule for minors 1–3 that turns negative or unset values into "depth disabled" (`on/…planesurface.cpp:1383-1397`). This only matters for clipping semantics.

### 5b.16 Traps

- **Archive version vs chunk version.** Branch on the payload's version nibble, never on the archive version (see the NURBS 1.0/1.1 split in V6 vs V7 files).
- **2-D curves still use 3-D containers.** Line and polyline points are always 3 doubles and arcs carry a full 3-D plane. Only NURBS CVs shrink to `dim` doubles.
- **Archive 2.** Expect old UUIDs; non-zero NURBS `reserved1` and real bboxes; and unwrapped polycurve and NURBS-surface payloads inside 2.x Breps. Also, `v2_HumanHead1.3dm` is actually archive 3. Read the header, not the filename.
- **Archive 1.** These files contain no class wrappers. Curves and surfaces arrive as V1 records (TCODE_LEGACY_CRV/SPL/SRF/FAC/SHL, TCODE_RHINOIO_OBJECT_NURBS_CURVE/SURFACE/BREP; readers at `on/opennurbs_archive.cpp:14149-15300`) and need a separate reader (cadmpeg `legacy.rs`). `ON_Brep::ReadOld100`'s curve readers are stubs that return null (`on/opennurbs_brep_io.cpp:1690-1700`).
- **Chunk lengths and CRCs.** CLASS_DATA and anonymous-chunk lengths include the trailing CRC32. Archive ≥ 50 uses i64 lengths.

### 5b.17 Samples (paths relative to `samples/`)

Counts are wrapper occurrences from the probe; "in Brep" includes nested occurrences.

| Class / variant | Top-level examples | Embedded examples |
|---|---|---|
| NURBS curve 1.0, ON_NurbsCurve UUID | `opennurbs/example_files/V2/v2_my_curves.3dm` (arch 2, ×4), `V3/v3_my_curves.3dm`, `V5/v5_my_curves.3dm`, `V6/my_curves.3dm` | `V5/v5_disk_brake.3dm`, `V5/v5_clip.3dm` (15.5k in arch-50 Breps, many rational dim-2 trims) |
| NURBS curve via TL alias | `V4/v4_example_file.3dm`, `V4/v4_Wheel_PG.3dm`, `V5/v5_example_file.3dm`, `rhino-developer-samples/rhino3dm/models/nurbscurve.3dm`, `three.js/examples/models/3dm/Rhino_Logo.3dm` (218) | `V3/*`, `V4/*` Breps (23k in arch 4), `V4/v4_DVDCase_Solid.3dm` (rational) |
| NURBS curve 1.1 (+ tag byte) | `V7/v7_my_curves.3dm`, `V7/v7_rhino_logo_annotations.3dm`, `rhino3dm/tests/models/file3dm_stuff.3dm`, `rhino3dm/tests/models/blocks.3dm` | `V7/v7_rhino_logo_nurbs.3dm` (199), `V8/v8_selective_clipping.3dm`, `rhino3dm/tests/models/sphereDecals.3dm` |
| Old NURBS curve alias (arch 2) | none (top-level arch-2 curves are polycurves) | all `V2/v2_*` files: segments of polycurves and old Breps (6,880) |
| NURBS surface (ON / TL) | `V2/v2_my_surfaces.3dm`, `V3/v3_my_surfaces.3dm`, `V5/v5_my_surfaces.3dm`, `V7/v7_my_surfaces.3dm` | `V4/*` Breps (2.5k TL), `V5/v5_disk_brake.3dm`; bare 2.x payloads in `V2/v2_Patch.3dm`, `V2/v2_Camera.3dm` |
| Polyline | `V3/v3_Soccer.3dm`, `V3/v3_Stearman.3dm`, `V5/v5_extrusions.3dm`, `V8/v8_caps_and_joins.3dm`, `rhino3dm/tests/models/file3dm_stuff.3dm` | Breps and polycurves in `V4/*`, `V5/*`; dim-2 extrusion profiles in `V5/v5_extrusions.3dm` and arch-80 files |
| Line | `rhino-developer-samples/rhino3dm/js/SampleUserStrings/RectifiedArchimedeanSolids.3dm` (1,800, arch 70), `…/models/RectifiedArchimedeanSolids8.3dm` (arch 80), `V3/v3_Soccer.3dm`, `V*/…my_curves` | dim-2 trims and dim-3 edges in all Brep files; revolution/sum profiles in `V5/v5_disk_brake.3dm` |
| Arc | `V*/…my_curves` (×2 each), `V3/v3_UntrimmedSurfaces.3dm`, `V8/v8_selective_clipping.3dm` | `V4/*` Breps and polycurves; revolution profiles (`V5/*`); dim-2 extrusion profiles |
| Polycurve (ON) | `V3/v3_Camera.3dm`, `V3/v3_SketchOnMesh.3dm`, `V6/V6_annotation.3dm`, `rhino-developer-samples/rhino3dm/cs/SampleCSJoinCurves/newFile.3dm` | Brep C2/C3 in `V3`/`V4`/`V5` files, `V5/v5_clip.3dm` |
| Old polycurve alias (arch 2) | `V2/v2_5_curves.3dm`, `V2/v2_Soccer.3dm`, `V2/v2_Stearman.3dm`, … (722 total) | bare payloads inside old 2.x Breps |
| Plane surface 1.1 | `V3/v3_my_spot_light.3dm`, `V5/v5_my_spot_light.3dm`, `V6/my_spot_light.3dm`, `V7/v7_my_spot_light.3dm` (domain ≠ extents) | `V4/*` (142), `V5/*` Breps (213), `V*/…my_trimmed_surface` (domain [0,10], extents [0,1]) |
| Clipping-plane surface | `V8/v8_section_hatch.3dm`, `V8/v8_selective_clipping.3dm` | none |
| Rev surface 2.0 (ON / TL) | `V6/my_viewports.3dm`, `V7/v7_my_viewports.3dm`, `rhino-developer-samples/rhino3dm/cs/SampleCSPBRMaterial/fileWithMaterial.3dm` | `V5/*` Breps (609), `V4/*` (273 TL; 73 transposed) |
| Sum surface (ON / TL) | none | `V5/*` Breps (150 ON), `V4/*` Breps (127 TL alias, which cadmpeg cannot read) |
| Curve-on-surface, curve proxy, obsolete circle, offset surface | none | none |

The per-file inventory with class UUIDs is in `inventory.tsv` (generated during this research; not kept, see Appendix A). The probe output is `out.tsv`, with columns file, archive, parent class, class, status, decoded fields.

### 5b.18 Uncertain

- **Curve-on-surface, obsolete circle and archive-2 old NURBS-surface UUIDs** have no sample bytes. Their layouts come from openNURBS source only (FA4FD4B5 appears only in openNURBS's alias table).
- **Pre-Nov-2001 circle points.** It is assumed, not observed, that some old files hold non-canonical `m_point[]` values in arcs. Every archive-3 sample arc is canonical.
- **ReadOld200 surfaces.** The bare NURBS-surface payloads inside 2.x Breps were not byte-verified by the probe (it scans wrappers only). cadmpeg's `parse_legacy_major2` reads them with the 5b.4 layout and openNURBS source matches.
- **Rev surface 1.x** (no t interval) and **NURBS surface minor > 0** do not occur in the samples; their layouts follow openNURBS `Read`.
- **ON_ClippingPlane minors 2–5.** These were checked against source only; the samples contain only minor 1.


---

## 6. Extrusions (ON_Extrusion::Read)

### 6.1 Where it occurs
- Class `ON_Extrusion`, UUID `36F53175-72B8-4d47-BF1F-B4E6FC24F4B9`. On disk: `75 31 f5 36 b8 72 47 4d bf 1f b4 e6 fc 24 f4 b9` (on: opennurbs_beam.cpp:1161; cad: extrusion.rs:28-30). Seen in `groups.3dm` at offset 3097087.
- The object record type chunk `0x82000071` carries `ON::extrusion_object = 0x40000000` (on: opennurbs_defines.h:2316). Use it to filter records before parsing the class UUID.
- **Archive 50 and later only in practice.** When openNURBS writes an archive of version 4 or lower, it converts every extrusion to a Brep or surface (on: opennurbs_archive.cpp:4568-4590). So archives 1-4 never contain `ON_Extrusion`.
  - Chunk lengths are i64 when the archive version is 50 or more (on: opennurbs_archive.cpp:7092-7098).
  - Early Rhino 5 WIP files with archive version `5` use 4-byte lengths, but have the same field layout (not in the samples).
- The class-data chunk `0x0002FFFC` body is exactly one `TCODE_ANONYMOUS_CHUNK`, `0x40008000` = `TCODE_USER|TCODE_CRC` (on: opennurbs_3dm.h:87). Because that body is entirely nested, the class-data chunk's own trailing CRC is `00 00 00 00` (observed in `groups.3dm`).

### 6.2 Chunk versions
| Version | Written by | Added fields |
|---|---|---|
| 1.0 | (old) | profile, path, t, up, miter flags and vectors, path domain, transposed |
| 1.1 | | `m_profile_count` |
| 1.2 | openNURBS when archive < 60, i.e. all archive-50 files | `m_bCap[2]` |
| 1.3 (2015-06-03) | openNURBS when archive >= 60 | `m_mesh_cache` (ON_MeshCache) |

Sources: on: opennurbs_beam.cpp:1465-1528 (Write: `minor = Archive3dmVersion() >= 60 ? 3 : 2`), 1531-1617 (Read). cad: extrusion.rs:172 (1.1), 180-187 (1.2), 190-226 (1.3). Observed: every archive-50 sample is 1.2; every archive-70/80 sample is 1.3.

`BeginRead3dmChunk(TCODE_ANONYMOUS_CHUNK,&major,&minor)` reads two i32 values after the chunk header. It requires the chunk length to be at least 8, major >= 1 and minor >= 0 (on: opennurbs_archive.cpp:6396-6470). This differs from `Read3dmChunkVersion`, which is a single byte (used by ON_InstanceRef, §7).

### 6.3 Byte table (class-data body, archive >= 50)
`P` = offset just after the profile class wrapper.

| Offset | Size | Field | openNURBS read, Rust type |
|---|---|---|---|
| 0 | 4 | typecode `0x40008000` | u32 |
| 4 | 8 | length L (body plus 4-byte CRC) | i64 |
| 12 | 4 | major = 1 (anything else: openNURBS fails the read) | ReadInt, i32 |
| 16 | 4 | minor | ReadInt, i32 |
| 20 | var | `m_profile`: an embedded object via `ReadObject` (class wrapper `0x00027FFA`; must cast to ON_Curve) | skip using the wrapper's length |
| P+0 | 24 | `m_path.from` | ReadLine, ReadPoint: 3×f64 |
| P+24 | 24 | `m_path.to` | 3×f64 |
| P+48 | 16 | `m_t` = [t0, t1], fractions of the line parameter (0..1), not lengths | ReadInterval: 2×f64 |
| P+64 | 24 | `m_up`: unit vector, perpendicular to the path | ReadVector: 3×f64 |
| P+88 | 1 | `m_bHaveN[0]` (start miter present) | ReadBool, u8 |
| P+89 | 1 | `m_bHaveN[1]` (end miter present) | u8 |
| P+90 | 24 | `m_N[0]`: miter-plane normal in the profile-plane frame | 3×f64 |
| P+114 | 24 | `m_N[1]` | 3×f64 |
| P+138 | 16 | `m_path_domain`: surface parameter interval along the path (writers set [0, length]) | 2×f64 |
| P+154 | 1 | `m_bTransposed` | u8 |
| P+155 | 4 | `m_profile_count` (minor >= 1) | ReadInt, i32 |
| P+159 | 1 | `m_bCap[0]` (bottom cap) (minor >= 2) | u8 |
| P+160 | 1 | `m_bCap[1]` (top cap) | u8 |
| P+161 | var | `m_mesh_cache`: ON_MeshCache chunk (minor >= 3), see 6.5 | chunk |
| … | | unknown later fields: `EndRead3dmChunk` skips them | skip |
| 12+L-4 | 4 | CRC32 over the direct bytes only: the version ints and the 161 fixed bytes, excluding the profile wrapper and the mesh-cache chunk | u32 |

Sources: on: opennurbs_beam.cpp:1544-1600; primitives at opennurbs_archive.cpp:1250-1290 (points and vectors), 1357 (ReadLine), 1439 (ReadInterval), 2784 (ReadBool). cad: extrusion.rs:129-184.

Checks I made:
- Fixed tail = 161 bytes in `groups.3dm`.
- Total sizes: 12+8+245+161+25+4 = 455 (1.3, polyline profile, empty cache); 12+8+245+161+4 = 430 (1.2).
- CRC: stored `853119ef` equals the CRC32 of the direct bytes, while the whole body gives `738e48ba`.

Defaults:
- Minor < 1: `m_profile_count = (profile != null) ? 1 : 0` (on: opennurbs_beam.cpp:1578).
- Minor < 2: both caps = `Profile(0)->IsClosed()` (on: opennurbs_beam.cpp:1602-1609).

### 6.4 Profile
- **Encoding.** A full class wrapper, possibly with its own userdata. The curve lies in the profile plane with z = 0. Setters force it to 2-D with `ChangeDimension(2)` (on: opennurbs_beam.cpp:700-709), so NURBS profiles have `m_dim = 2`.
- **Classes seen in the samples:**
  - `ON_PolylineCurve` `4ED7D4E6-…`
  - `ON_ArcCurve` `CF33BE2A-…` (circles)
  - `ON_NurbsCurve`, stored under the alias `TL_NurbsCurve` `5EAF1119-0B51-11d4-BFFE-0010830122F0`. **Trap: accept alias UUIDs** (on: opennurbs_object.cpp:894).
  - `ON_PolyCurve` `4ED7D4E0-…`
  - `ON_LineCurve` is allowed but not seen.
- **`m_profile_count == 1`.** `m_profile` is the single profile, which may itself be an ON_PolyCurve made of segments (on: `Profile()`, opennurbs_beam.cpp:917-925).
- **`m_profile_count > 1`.** `m_profile` must be an ON_PolyCurve whose segment count equals `m_profile_count`. Segment 0 is the closed outer boundary and segments 1..n-1 are closed holes. Each segment can itself be a nested polycurve (on: `AddInnerProfile`, opennurbs_beam.cpp:841-910; IsValid 1217-1265).
  - ON_PolyCurve data, for splitting: `Read3dmChunkVersion` u8 (0x10), i32 count, i32 reserved1, i32 reserved2, bbox 6×f64, `ReadArray(m_t)` (i32 n plus n×f64), then `count` × `ReadObject` (on: opennurbs_polycurve.cpp:410-470).
- **Orientation (enforced only when the profile is set, not when read).** The outer profile is counter-clockwise (+1); holes are clockwise (−1) (on: ProfileHelper, opennurbs_beam.cpp:711-750).
- **Null profile.** It is written as a null wrapper: `0x00027FFA` with length 32, containing only `0x0002FFFB` (16 zero bytes plus CRC). That is 44 bytes, with no class-data chunk and no end chunk (on: opennurbs_archive.cpp:4239-4258, 5085-5100, 5143-5148). openNURBS reads it as `m_profile = null`.

### 6.5 Mesh cache, minor >= 3 (`ON_MeshCache::Read`)
Layout from on: opennurbs_mesh.cpp:16588-16635 (`ON_MeshCache::Read`) and 16155-16200 (`ON_MeshCacheItem::Read`); cad: extrusion.rs:795-895.

```
ON_MeshCache:   0x40008000, i64 len, i32 major=1, i32 minor=0,
                { u8 marker=1, ON_MeshCacheItem }*,  u8 marker=0,  u32 CRC
ON_MeshCacheItem: 0x40008000, i64 len, i32 major=1, i32 minor=0,
                ON_UUID mesh_id (16), ReadObject -> ON_Mesh class wrapper, u32 CRC
```

- **Marker byte.** 0 ends the list. 1 means an item chunk follows. Any other value makes openNURBS stop reading and fail the cache, which fails the whole extrusion read (on: opennurbs_mesh.cpp:16604-16620).
- **Item rules.** Item major must be 1. The item's object must be non-null and cast to ON_Mesh (`4ED7D4E4-E947-11d3-BFE5-0010830122F0`). Decode the mesh class data with the ON_Mesh reader from the mesh section.
- **Empty cache.** Length field 13 (8 version bytes, the 0 marker, 4 CRC), so 25 bytes on disk. openNURBS writes `ON_MeshCache::Empty` when render meshes are not being saved, i.e. "Save Small" (on: opennurbs_beam.cpp:1515-1521).
- **Mesh IDs.** Values from on: opennurbs_statics.cpp:1292-1314, with the on-disk wire bytes:

| Mesh | UUID | On-disk bytes |
|---|---|---|
| Render | `F5E3BAA9-A7A2-49FD-B8A1-66EB274A5F91` | `a9 ba e3 f5 a2 a7 fd 49 b8 a1 66 eb 27 4a 5f 91` |
| Analysis | `AC12F955-A29F-437B-A4C9-06C85B7FF57C` | `55 f9 12 ac 9f a2 7b 43 a4 c9 06 c8 5b 7f f5 7c` |
| Preview | `3AACA7F2-0444-4587-8083-A13C5BA0AE50` | `f2 a7 ac 3a 44 04 87 45 80 83 a1 3c 5b a0 ae 50` |
| Coarse (`ON::default_mesh`) | `85D9E08D-386C-45C3-83DC-0C354685D504` | |
| Fine | `A2DAF594-F44B-44A6-A44B-E856CD7C05E8` | |
| Any (lookup wildcard, never stored) | `A82C0F1B-13E8-4DEC-AD35-D3DDD277203C` | |

- **Item order.** `SetMesh` adds new items at the front (on: opennurbs_mesh.cpp:16416-16452), so the file order is not fixed. Select the mesh by UUID, not by position.
- **Coordinates.** Meshes are in the object's own coordinates, the same space as the path (`ON_MeshCache::Transform` moves them with the object; on: opennurbs_mesh.cpp:16659). For block members, apply the instance transform.

### 6.6 Mesh cache, minor 2 (archive 50): obsolete userdata
- **Where it lives.** In archive-50 files the cached meshes are not in the class data. They sit in a userdata chunk `0x00027FFD` that is a sibling of `0x0002FFFC` inside the extrusion's own class wrapper.
- **Identity.** Class `ON_V5ExtrusionDisplayMeshCache`, `A8130A3E-E4F3-4CB0-BB8A-F10A473912D0` (on-disk `3e 0a 13 a8 f3 e4 b0 4c bb 8a f1 0a 47 39 12 d0`). Both class UUID and item UUID hold this value. Application UUID is `ON_opennurbs5_id` `C8CDA597-D957-4625-A4B3-A0B510FC30D4` (on: opennurbs_beam.cpp:4775-4785; opennurbs_statics.cpp:572).
- **Userdata chunk framing** (on: opennurbs_archive.cpp:4946-4995; cad: objects.rs:608-720):

```
0x00027FFD (no CRC bit) {
  u8 version (0x22 from Write3dmChunkVersion(2,2); 0x1m layout also exists)
  0x0002FFF9 header (CRC) { class uuid 16, item uuid 16, i32 copycount, xform 16×f64,
                            [v>=2.1] app uuid 16, [v>=2.2] u8 is_goo, i32 3dm_version, u32 opennurbs_version }
  0x40008000 payload (CRC) — written with BeginWrite3dmChunk(tcode,0): NO major/minor ints
}
```

- **Payload** (on: opennurbs_beam.cpp:4826-4841 Write, 4844-4875 Read): three back-to-back `ReadObject` wrappers.
  - Slot 0 = render mesh or null wrapper.
  - Slot 1 = analysis mesh or null.
  - Slot 2 = always null (deleted if not null).
- **After reading.** `DeleteAfterRead` moves slot 0 to `RenderMeshId` and slot 1 to `AnalysisMeshId` (on: opennurbs_beam.cpp:4882-4897).
- **When it is written.** Only when render meshes are saved and a render or analysis mesh exists (on: opennurbs_beam.cpp:1509-1513, 4906-4945).
- **Same typecode, two layouts.** `0x40008000` appears both with and without the two version ints. Choose by context.

### 6.7 Minimal reader (render mesh only, skip the rest)

```rust
// chunk(b, at, end, wide) -> Chunk{tc, body /*excl. CRC if tc&0x8000 and long*/, next}
fn extrusion_render_mesh(b:&[u8], class_data:Range<usize>, userdata:&[Range<usize>]) -> Result<Option<Range<usize>>> {
    let a = chunk(b, class_data.start, class_data.end, true)?;           // 0x40008000
    ensure!(a.tc == 0x4000_8000);
    let mut r = Reader::at(b, a.body.clone());
    ensure!(r.i32()? == 1); let minor = r.i32()?;
    let prof = chunk(b, r.pos, r.end, true)?; ensure!(prof.tc == 0x0002_7FFA);
    r.pos = prof.next;                                                     // skip profile
    if minor >= 3 {
        r.pos += 161;                                                      // fixed tail (minor>=2)
        let c = chunk(b, r.pos, r.end, true)?; ensure!(c.tc == 0x4000_8000);
        let mut cr = Reader::at(b, c.body.clone());
        ensure!(cr.i32()? == 1); let _ = cr.i32()?;
        let mut any = None;
        loop {
            match cr.u8()? { 0 => break, 1 => {}, _ => return Ok(None) }   // corrupt → treat as no cache
            let it = chunk(b, cr.pos, cr.end, true)?; ensure!(it.tc == 0x4000_8000);
            let mut ir = Reader::at(b, it.body.clone());
            ensure!(ir.i32()? == 1); let _ = ir.i32()?;
            let id = ir.uuid_wire()?;                                       // 16 raw bytes
            let w = chunk(b, ir.pos, ir.end, true)?;                        // ON_Mesh wrapper
            if id == RENDER_MESH_WIRE { return Ok(Some(ir.pos..w.next)); }
            any.get_or_insert(ir.pos..w.next);
            cr.pos = it.next;
        }
        return Ok(any);                                                    // fallback: any cached mesh
    }
    // minor <= 2: V5 userdata on the same class wrapper
    for ud in userdata {
        let (cls, payload) = parse_userdata_header(b, ud.clone())?;        // §userdata
        if cls != V5_EXTRUSION_CACHE { continue; }
        let w0 = chunk(b, payload.start, payload.end, true)?;              // slot 0 = render
        if class_uuid_of(b, &w0)? == ON_MESH { return Ok(Some(payload.start..w0.next)); }
        let w1 = chunk(b, w0.next, payload.end, true)?;                    // slot 1 = analysis
        if class_uuid_of(b, &w1)? == ON_MESH { return Ok(Some(w0.next..w1.next)); }
    }
    Ok(None)
}
```

- For full analytic data, read the fields in §6.3 order instead of `+161`.
- Ignore trailing bytes in every chunk (forward compatibility).
- Verifying CRCs is optional.

### 6.8 Mapping to geometry and tessellating without a cached mesh
Sources: on: `ON_GetEndCapTransformation` opennurbs_beam.cpp:208-282, `GetProfileTransformation` 632-670, `PathStart`/`PathEnd` 401-418, `IsCapped` 496-524, `Domain` 3439-3447; cad: extrusion.rs:265-410, 643-793.

**Frame.**
- `T = unit(to − from)`, `Y = m_up`, `X = Y × T` (on: beam.cpp:231-236). This frame is right-handed.
- Cap origins: `O0 = from + t0·(to−from)` and `O1 = from + t1·(to−from)`.
- A profile point (x, y) maps to `O + x·X + y·Y`.

**Miters.**
- A miter is active only if `m_bHaveN[i]` is set and `N.z > 1/64` (`m_Nz_min`, on: beam.cpp:1157; cad: extrusion.rs:37, 1083-1089).
- Point mapping is `R·S` applied in the profile frame before the frame transform:
  - Axis `A = unit(−N.y, N.x, 0)` and `c = 1 − 1/N.z`.
  - `S` is a 2-D scale with entries `[1−c·Ay², c·Ax·Ay; c·Ax·Ay, 1−c·Ax²]`.
  - `R` rotates about A with `sin = |(N.x, N.y)|` and `cos = N.z` (cad: `mitered_local` extrusion.rs:764-793).
- The map is affine per cap, so each cap stays planar and a 2-D triangulation of the profile remains valid.

**Sides.**
- Sample each profile into a polyline (exact vertices for polylines and lines, angle steps for arcs, steps per knot span for NURBS).
- Map each polyline to the bottom and top caps and join matching points with quads. The sides are ruled surfaces.
- Outward normal = `tangent2d × Z`, mapped to world as `X·n.x + Y·n.y`. Because the outer boundary is counter-clockwise and holes are clockwise, this one rule points out of the material for both.

**Caps.**
- Present only if `m_bCap[i]` is set and the outer profile is closed (`IsCapped`).
- Triangulate the outer boundary plus holes in profile 2-D, ear clipping with holes (for example the `earcut` crate). Map the result to O0 and O1.
- Top cap normal = +T (mitered: R·ẑ in the frame). Bottom cap normal = −T, so reverse the bottom winding.

**Open profiles.**
- No caps. Render double-sided.

**Parameters.**
- `m_bTransposed` and `m_path_domain` change only the surface parameters: not transposed means u = profile and v = path (`PathParameter`, on: beam.cpp:391-399). They do not change the shape.
- Transposing swaps Du and Dv, which flips Du×Dv. Use the geometric rule above for normals.

**Faces.**
- Face count: `profile_count + 2` when both caps are present; 1 when uncapped (on: `FaceCount`, beam.cpp:526-562). A Brep built with `bSmoothFaces` splits sides at profile kinks (beam.cpp:2680-2711).

### 6.9 cadmpeg vs openNURBS differences
1. **Profile.** cadmpeg rejects a null profile and `profile_count <= 0` (extrusion.rs:172-177; test at 1551). openNURBS reads both and only `IsValid` fails them.
2. **`m_t`.** cadmpeg requires t0 < t1 inside [0,1] and an increasing path domain (extrusion.rs:153-159, 169-170, 1062-1072). openNURBS reads any value; IsValid checks `0 <= t0 < t1 <= 1`.
3. **Up vector.** cadmpeg requires it to be a unit vector and perpendicular to the path within 1e-10 (extrusion.rs:273-279). openNURBS IsValid uses ON_SQRT_EPSILON ≈ 1.5e-8 (beam.cpp:1300-1312). A strict reader may reject files Rhino accepts.
4. **Profile z.** cadmpeg requires every profile control point to have z exactly 0 (extrusion.rs:629-641, 676-681), and requires orientation outer +1 and holes −1 (376-386). openNURBS does not check either at read time.
5. **Default caps when minor < 2.** cadmpeg: all profiles closed (388-394). openNURBS: outer profile closed.
6. **Mesh cache errors.** cadmpeg drops a malformed cache with a warning and keeps the analytic geometry (extrusion.rs:206-224). openNURBS fails the whole extrusion read.
7. **Mesh ID.** cadmpeg skips the 16-byte mesh ID (extrusion.rs:834) and decodes every cached mesh. In V5 userdata it treats slots 0 and 1 the same (932-956). openNURBS keys meshes by ID: render, analysis and so on.
8. **V5 userdata lookup.** cadmpeg needs class UUID == item UUID == A8130A3E (911-919). openNURBS resolves by class UUID.
9. **Booleans.** cadmpeg rejects values above 1 only for writers dated after 2017-08-24 (chunks.rs:322-339). openNURBS does the same: values other than 0/1 become true for older writers and an error after that date (opennurbs_archive.cpp:2784-2810).

### 6.10 Samples (paths relative to `samples/`)
- **1.2, archive 50, no cache, no V5 userdata in any file:**
  - `opennurbs/example_files/V5/v5_example_file.3dm`: 3 extrusions — capped circle (ArcCurve), open uncapped TL_NurbsCurve, capped polyline.
  - `opennurbs/example_files/V5/v5_extrusions.3dm`: 7 extrusions.
    - `profile_count=2` polycurves: a tube from 2 ArcCurves, and 2 NURBS profiles.
    - The **only miter in the corpus**: the 2012-byte record has `bHaveN=(0,1)` and `N1=(-0.497,0,0.868)`.
    - Open uncapped NURBS and polyline profiles.
- **1.3 with an empty cache (25 bytes):**
  - `rhino3dm/tests/DotNetTester/test_in.3dm` (archive 70)
  - `rhino3dm/tests/models/blocks.3dm` (2, both block members)
  - `rhino3dm/tests/models/file3dm_stuff.3dm` (3, one uncapped)
  - `rhino3dm/tests/models/groups.3dm` (5)
  - `rhino3dm/tests/models/object_UserStrings.3dm` (1)
- **1.3 with one Render mesh item:**
  - `opennurbs/example_files/V8/v8_section_hatch.3dm` (3, circle profiles, ~5.6 KB meshes)
  - `opennurbs/example_files/V8/v8_selective_clipping.3dm` (3, polyline)
  - `rhino-developer-samples/rhino3dm/models/RenderContent.3dm` (1)
  - `rhino-developer-samples/rhino.inside/dotnet-netcore/BatchOperation/files/01.3dm` (1)
- **Not covered:** archive 60; V5 userdata cache; analysis or preview items; `m_bTransposed=1`; `m_t ≠ [0,1]`; a start miter; more than 2 profiles.

### 6.11 Uncertain
- Whether real Rhino 5 files store cached meshes in exactly the three-slot `ON_V5ExtrusionDisplayMeshCache` layout. This is inferred from the openNURBS 8 reader; no sample has it.
- Whether Rhino ever saves analysis, preview or coarse items in the 1.3 cache. Every sample cache holds Render only.
- Archive-5 extrusions with 4-byte chunk lengths: assumed to have the same layout, unverified.


---

## 7. Block instances (ON_InstanceRef::Read)

### 7.1 Where it occurs
- Class `ON_InstanceRef`, UUID `F9CFB638-B9D4-4340-87E3-C56E7865D96A`. On disk: `38 b6 cf f9 d4 b9 40 43 87 e3 c5 6e 78 65 d9 6a` (on: opennurbs_instance.cpp:1741; cad: instances.rs:25-27). Seen in `blocks.3dm` at offset 3103248.
- Stored in the object table (`0x10000013`) as object records `0x20008070`. The record type chunk value is `ON::instance_reference = 0x1000` (on: opennurbs_defines.h:2289).
- Present in archive 4 and in 50+. The class data is identical; only the surrounding chunk-length width differs.

### 7.2 Byte table (class-data body)
| Offset | Size | Field | openNURBS read, Rust type |
|---|---|---|---|
| 0 | 1 | version byte: major = b>>4 (must be 1), minor = b&0x0F (0 in every writer) | `Read3dmChunkVersion`, u8 (not a chunk) |
| 1 | 16 | `m_instance_definition_uuid` | `Read3dmReferencedComponentId` = ReadUuid, then an optional id remap when merging models |
| 17 | 128 | `m_xform`: `ReadXform` = `ReadDouble(16, &m_xform[0][0])`, **row-major** `m[row][col]`, 4 rows × 4 f64 | 16×f64 |
| 145 | 24 | `m_bbox.m_min` | ReadBoundingBox: 3×f64 |
| 169 | 24 | `m_bbox.m_max` | 3×f64 |
| 193 | – | end. The class-data chunk length is 197, including its 4-byte CRC | |

Sources: on: opennurbs_instance.cpp:1779-1811; opennurbs_archive.cpp:1294-1311, 5771-5785; opennurbs_model_component.cpp:3254-3260; cad: instances.rs:1464-1544.

- **No later fields.** Write always emits version 1.0. A minimal reader should still ignore any bytes past 193.
- **Point transform.** `p' = M·[x y z 1]ᵀ` with a homogeneous divide by row 3 (on: opennurbs_xform.cpp:881-897). Translation is `m[0][3], m[1][3], m[2][3]` = flat indices 3, 7, 11. Confirmed: `blocks.3dm` has `-78.8301` at row0[3].
- **Bounding box.** `m_bbox` is the world-space box of the placed instance. In `blocks.3dm` the two placements' boxes differ by exactly the translation difference. It is a cache: use it for culling, not as truth.

### 7.3 Instance definitions (needed to resolve placements)
**Location and identity.**
- Table `TCODE_INSTANCE_DEFINITION_TABLE = 0x10000021`. Record `TCODE_INSTANCE_DEFINITION_RECORD = 0x20008076` (on: opennurbs_3dm.h:133, 245; cad: container.rs:49, 65).
- Each record body is directly one class wrapper, with no type or attributes chunks (on: opennurbs_archive.cpp:11763-11836).
- Class `ON_InstanceDefinition`, `26F8BFF6-2618-417f-A158-153D64A94989` (on: opennurbs_instance.cpp:24; cad: instances.rs:22-24).
- **The definition table comes before the object table** in files written by openNURBS (on: opennurbs_extensions.cpp:3456, after the other tables; confirmed in the samples).

**Choosing the layout** (on: `ON_InstanceDefinition::Read`, opennurbs_instance.cpp:2304-2328):
- Archive ≤ 50: V5 layout.
- Archive 60: V5 layout if the openNURBS version is ≤ 2348834153, or if the next chunk is not `0x40008000`.
- Otherwise: V6 layout.
- Shortcut used by cadmpeg (instances.rs:1290-1295): if the first byte of class data is `0x00`, it is the anonymous chunk (V6 layout); otherwise it is a V5 version byte `0x1m`.

**V5 layout** (Internal_ReadV5, opennurbs_instance.cpp:2491-2742; cad: instances.rs:825-910): fields in order:
- u8 version.
- **uuid id**.
- **`ReadArray(ON_UUID)`: i32 count, then count × 16-byte member object UUIDs** (archive.cpp:2496-2512).
- wstring **name**, description, url, url_tag. A wstring is u32 count of UTF-16 units including the NUL, then count×u16.
- bbox 6×f64.
- **u32 type** (0/1 Static, 2 LinkedAndEmbedded, 3 Linked).
- wstring linked-file path. An empty path forces the type to Static.
- Minor-gated fields:
  - 1.1: checksum (u64 size, u64 time, 8×u32) = 48 bytes.
  - 1.2: u32 unit system.
  - 1.3: f64 metres per unit, u8 relative-path flag.
  - 1.4: ON_UnitSystem chunk.
  - 1.5: i32 update depth.
  - 1.6: u32 linked appearance.
  - 1.7: u8 flag, then an optional ON_FileReference chunk, then abandoned bytes (skip).
- Archive-50 writers emit 1.6; early V6 WIP emitted 1.7 (instance.cpp:2339-2343).

**V6 layout** (Internal_ReadV6, opennurbs_instance.cpp:2848-2958; cad: instances.rs:912-1047): chunk `0x40008000` containing, in order:
- i32 major = 1, i32 minor.
- The **`TCODE_MODEL_ATTRIBUTES_CHUNK 0x40008002`** chunk: i32 1, i32 minor, then five status-prefixed groups. Status byte: 0 = absent, 1 = value follows, 2 = cleared.
  - serial numbers: 3×u32
  - **id: uuid**
  - type: u32
  - index: i32
  - **name: wstring**
  - (opennurbs_model_component.cpp:2676-2805; cad: instances.rs:504-582)
- **u32 type**: 0 Unset, 1 Static, 2 LinkedAndEmbedded, 3 Linked.
- ON_UnitSystem anonymous chunk: i32 1, i32 minor, u32 unit, f64 metres per unit, wstring custom name (opennurbs_3dm_settings.cpp:509-545).
- wstring description, url, url_tag.
- bbox 6×f64.
- **u8 has_member_list, then (if 1) i32 count + count × uuid**. Writers omit the list for type Linked (instance.cpp:2779).
- u8 is_linked, then (if 1) an anonymous chunk containing an ON_FileReference chunk, i32 update depth, u32 appearance, u8 flag and optional settings.
- CRC.

**Members.**
- Members are ordinary object-table records (geometry, meshes, or nested `ON_InstanceRef`).
- Their attributes `Mode()` is `ON::idef_object = 3` (on: opennurbs_defines.h:2089-2097; `IsInstanceDefinitionObject`, opennurbs_3dm_attributes.cpp:1780). In the V5+ attributes encoding, mode is item id 12.
- ONX_Model leaves such objects out of the model bounding box (opennurbs_extensions.cpp:537-539).
- Do not draw members at top level. Draw them only through placements.

### 7.4 Resolving placements and nesting

```rust
// pass A: idefs  -> HashMap<Uuid, Def{name, kind, members: Vec<Uuid>}>
// pass B: objects -> HashMap<Uuid /*attributes uuid*/, Obj{kind, class_data, mode, ...}>
//         (members may appear anywhere in the object table: resolve after the full scan)
fn read_iref(b:&[u8], d:Range<usize>) -> Result<IRef> {
    ensure!(d.len() >= 193);
    let v = b[d.start]; ensure!(v >> 4 == 1);
    let def = Uuid::from_wire(&b[d.start+1..d.start+17]);
    let mut m = [[0f64;4];4];
    for i in 0..16 { m[i/4][i%4] = f64::from_le_bytes(b[d.start+17+8*i..][..8].try_into()?); }
    let bbox_min = read3(b, d.start+145); let bbox_max = read3(b, d.start+169);
    Ok(IRef{def, m, bbox_min, bbox_max})   // ignore bytes past 193
}
fn expand(def_id, world: M4, stack: &mut Vec<Uuid>, out) {
    if stack.len() >= 64 || stack.contains(&def_id) { report("cycle/depth"); return; }
    let Some(def) = defs.get(&def_id) else { report("missing definition"); return };
    if def.kind == Linked && def.members.is_empty() { report("linked, geometry not in file"); return; }
    stack.push(def_id);
    for id in &def.members {
        match objects.get(id) {
            Some(Obj::IRef(r)) => expand(r.def, world * r.m, stack, out),  // outer · inner
            Some(o)            => out.push((o, world)),
            None               => report("missing member"),
        }
    }
    stack.pop();
}
// top level: for every object with mode != 3 (or not in any member list) that is an IRef:
//   expand(r.def, r.m, &mut vec![], out)
```

- **Composition.** `world = M_outer · M_inner` with column vectors. openNURBS composes the same way (`ON_InstanceRef::Transform`: `m_xform = xform*m_xform`, opennurbs_instance.cpp:1874-1880). cadmpeg uses `parent.compose(local)` (decode.rs:2429).
- **Depth and cycles.** cadmpeg limits depth to 64 and detects cycles with a stack of definition IDs (decode.rs:2351-2370, 2416). openNURBS `ONX_Model::UsesIDef` treats more than 10000 nested refs as circular (opennurbs_extensions.cpp:3591-3680).
- **Mirrored placements.** If det(upper 3×3) < 0, flip triangle winding.
- **Non-uniform scale or shear.** Transform normals by the inverse transpose.

### 7.5 cadmpeg vs openNURBS differences
1. **Validation at read.** cadmpeg rejects a nil definition UUID, a bottom row not exactly `[0,0,0,1]`, non-finite values, and transforms whose inverse·M differs from identity beyond `1e-6·max|column|` (instances.rs:1477-1537). openNURBS reads any 4×4; only `IsValid` checks nil and singular, with tolerance 1e-6 (instance.cpp:1743-1762).
2. **Bounding box.** cadmpeg reads the bbox but discards it, and tolerates trailing bytes (1490-1491).
3. **Member detection.** cadmpeg decides membership by presence in any definition's member list, not by attribute mode (decode.rs:878-879, 2134-2137).
4. **Rejected definitions.** cadmpeg rejects a definition that is duplicated, Unset, Linked without members, or has duplicate members (decode.rs:2384-2414; instances.rs:1354-1411).
5. **V5-layout definitions.** cadmpeg requires minor ≥ 6 (instances.rs:836-841, 1081-1085), so 1.5 records fail, including those in `v4_example_file.3dm`. For archives 3 and 4 it never picks the V5 layout at all (`v5_layout` is true only for archive 50 or 60, instances.rs:1293-1295), so their definitions are kept as opaque records. openNURBS reads every minor from 1.0 up.
6. **Kind mapping.** V6 type 0 maps to Unset in cadmpeg (instances.rs:398-405). In the V5 layout, 0 and 1 both mean Static (389-396). This matches openNURBS.

### 7.6 Samples (paths relative to `samples/`)
- `opennurbs/example_files/V4/v4_example_file.3dm` (archive 4): **the only nested-block sample**.
  - Definitions 'Block 01' (one Brep) and 'Block 02' (one Brep plus a nested `ON_InstanceRef` → Block 01). Both are V5 layout, version 1.5.
  - One nested reference (member of Block 02) and two top-level placements of Block 02.
  - All transforms are translations.
- `rhino3dm/tests/models/blocks.3dm` (archive 80):
  - One V6 definition 'Block 01' (type 1) with 4 members: 2 extrusions (empty mesh caches), 1 Brep, 1 NURBS curve.
  - 2 top-level placements, translation in x only.
- `rhino3dm/tests/models/file3dm_stuff.3dm` (archive 80):
  - One V6 definition 'Block 01' with 3 members (2 Breps, 1 extrusion).
  - 2 placements: identity, and y −190.716.
- **Not covered:** rotated, scaled or mirrored placements; nested blocks in archive 50+; linked or linked-and-embedded definitions; archives 50, 60 and 70.

### 7.7 Uncertain
- Nested expansion and the composition order in archive 50+ files are taken from the openNURBS and cadmpeg code. The only nested sample is archive 4.
- How the definition's own unit system (`m_us`) should scale members of linked or linked-and-embedded definitions. Not examined; assume static definitions use model units.
- Whether Rhino ever writes `ON_InstanceRef` with a minor version above 0 or a non-affine bottom row. Neither is seen; openNURBS 8.35 writes 1.0.

---

## 8. Class UUIDs

Source: the `ON_OBJECT_IMPLEMENT` / `ON_VIRTUAL_OBJECT_IMPLEMENT` macros in openNURBS 8.x
(file per class, e.g. `opennurbs_mesh.cpp:127` for `ON_Mesh`, `opennurbs_brep.cpp:1313` for
`ON_Brep`, `opennurbs_beam.cpp:1161` for `ON_Extrusion`, `opennurbs_instance.cpp:1741` for
`ON_InstanceRef`), the hand-written class id of `ON__OBSOLETE__CircleCurve`
(`opennurbs_arccurve.cpp:1233`) and the alias table in `ON_ClassId::ClassId`
(`opennurbs_object.cpp:878-950`). cadmpeg constants: `mesh.rs:70`, `brep.rs:27-45`,
`extrusion.rs:28`, `instances.rs:22-25`, `curves.rs:29-62`, `surfaces.rs:30-56`, `subd.rs:25`,
`hatch.rs:23`, `annotations.rs:21-42`, `detail.rs:15`, `morph.rs:23`, `cage.rs:21`.

UUIDs are given in canonical text form; on disk the first three groups are byte-reversed (1.5).
Counts come from a scan of every sample (`samples/manifest.tsv`, 167 archives with version
>= 2): "top level" counts object-table records (lights: light-table records; instance
definitions: idef-table records); "embedded" counts class wrappers found inside class data
(Brep component arrays, polycurve segments, cached meshes, profiles). The embedded scan is
heuristic (it finds `0x00027FFA` + `0x0002FFFB` length-20 headers in class data and cannot see
inside compressed buffers), so treat those numbers as lower bounds.

### 8.1 Current class UUIDs

**Geometry objects requested in the task**

| Class | UUID (canonical) | Top level, own UUID (archive:count) | Top level via alias | Embedded, own UUID | Embedded via alias |
|---|---|---|---|---|---|
| `ON_Mesh` | `4ED7D4E4-E947-11D3-BFE5-0010830122F0` | 2:1207, 3:1207, 4:2, 50:33, 60:14, 70:14, 80:21 | - | 3:10, 4:3094, 50:2143, 60:29, 70:46, 80:15 | - |
| `ON_Brep` | `60B5DBC5-E660-11D3-BFE4-0010830122F0` | 2:2, 3:2, 50:94, 60:1, 70:2 | 2:285, 3:327, 4:143, 50:17, 60:8, 70:14, 80:15 | - | - |
| `ON_Extrusion` | `36F53175-72B8-4D47-BF1F-B4E6FC24F4B9` | 50:10, 70:1, 80:19 | - | - | - |
| `ON_InstanceRef` | `F9CFB638-B9D4-4340-87E3-C56E7865D96A` | 4:3, 80:4 | - | - | - |
| `ON_Point` | `C3101A1D-F157-11D3-BFE7-0010830122F0` | 2:54, 3:33, 4:4, 50:4, 60:2, 70:2, 80:12 | - | - | - |
| `ON_PointCloud` | `2488F347-F8FA-11D3-BFEC-0010830122F0` | 3:1, 4:1, 50:2, 60:1, 70:2, 80:4 | - | - | - |
| `ON_NurbsCurve` | `4ED7D4DD-E947-11D3-BFE5-0010830122F0` | 2:4, 3:1, 50:1, 60:1, 70:1 | 4:9, 50:10, 70:227, 80:3 | 2:42, 50:15529, 80:2 | 2:6880, 3:6889, 4:23058, 50:508, 60:239, 70:440, 80:133 |
| `ON_PolylineCurve` | `4ED7D4E6-E947-11D3-BFE5-0010830122F0` | 3:22, 4:1, 50:4, 80:4 | - | 4:40, 50:56, 70:1, 80:15 | - |
| `ON_LineCurve` | `4ED7D4DB-E947-11D3-BFE5-0010830122F0` | 3:290, 4:2, 50:13, 60:1, 70:1801, 80:1801 | - | 3:42, 4:4311, 50:808, 60:10, 70:42, 80:12 | - |
| `ON_ArcCurve` | `CF33BE2A-09B4-11D4-BFFB-0010830122F0` | 3:36, 4:1, 50:8, 60:2, 70:2, 80:4 | - | 4:394, 50:240, 60:7, 70:3, 80:21 | - |
| `ON_PolyCurve` | `4ED7D4E0-E947-11D3-BFE5-0010830122F0` | 3:500, 60:1, 70:3, 80:1 | 2:722 | 3:243, 4:867, 50:232, 60:16, 70:32 | - |
| `ON_CurveProxy` | `4ED7D4D9-E947-11D3-BFE5-0010830122F0` | - | - | - | - |
| `ON_PlaneSurface` | `4ED7D4DF-E947-11D3-BFE5-0010830122F0` | 3:1, 50:1, 60:1, 70:1 | - | 3:1, 4:142, 50:214, 60:3, 70:1, 80:3 | - |
| `ON_NurbsSurface` | `4ED7D4DE-E947-11D3-BFE5-0010830122F0` | 2:2, 3:1, 50:1, 70:1 | - | 2:7, 3:6, 50:1199, 70:6 | 3:675, 4:2542, 50:49, 60:26, 70:45, 80:12 |
| `ON_SubD` | `F09BA4D9-455B-42C3-BA3B-E6CCACEF853B` | 60:6, 70:13, 80:6 | - | - | - |
| `ON_TextDot` | `74198302-CDF4-4F95-9609-6D684F22AB37` | 3:56, 70:4, 80:1 | - | - | - |
| `ON_Light` | `85A08513-F383-11D3-BFE7-0010830122F0` | 2:11, 3:11, 4:2, 50:1, 60:1, 70:3, 80:1 | - | - | - |
| `ON_Hatch` | `0559733B-5332-49D1-A936-0532AC76ADE5` | 4:1, 70:7, 80:3 | - | - | - |
| `ON_ClippingPlaneSurface` | `DBC5A584-CE3F-4170-98A8-497069CA5C36` | 80:3 | - | - | - |
| `ON_DetailView` | `C8C66EFA-B3CB-4E00-9440-2AD66203379E` | 50:1 | - | - | - |
| `ON_MorphControl` | `D379E6D8-7C31-4407-A913-E3B7040D034A` | - | - | - | - |

**V6+ annotation classes (archive >= 60)**

| Class | UUID (canonical) | Top level, own UUID (archive:count) | Top level via alias | Embedded, own UUID | Embedded via alias |
|---|---|---|---|---|---|
| `ON_Text` | `57376349-62A9-4A16-B411-A46BCD544790` | 60:2, 80:10 | - | - | - |
| `ON_Leader` | `945BF594-6FF9-4F5C-BFC0-B3AF528F29D2` | - | - | - | - |
| `ON_DimLinear` | `E550882B-F44D-4154-A1EF-6E50CBBBF543` | 60:2, 70:2 | - | - | - |
| `ON_DimAngular` | `D417786B-F6CD-4F12-9E1F-063F414DBEB6` | 60:1 | - | - | - |
| `ON_DimRadial` | `FC749C2F-4C00-41FD-9840-26D94F047AD3` | 60:1, 70:1 | - | - | - |
| `ON_DimOrdinate` | `03124828-4C9B-4D28-9A82-664DDDE7A14F` | - | - | - | - |
| `ON_Centermark` | `D46767BA-7E8F-4D9D-9A92-66050219A5B9` | - | - | - | - |

**Pre-V6 annotation classes (still read; converted to the V6 classes by openNURBS)**

| Class | UUID (canonical) | Top level, own UUID (archive:count) | Top level via alias | Embedded, own UUID | Embedded via alias |
|---|---|---|---|---|---|
| `ON_OBSOLETE_V2_TextDot` | `8BD94E19-59E1-11D4-8018-0010830122F0` | 2:56 | - | - | - |
| `ON_OBSOLETE_V2_AnnotationArrow` | `8BD94E1A-59E1-11D4-8018-0010830122F0` | 2:10 | - | - | - |
| `ON_OBSOLETE_V2_TextObject` | `5DE6B210-486B-11D4-8014-0010830122F0` | - | - | - | - |
| `ON_OBSOLETE_V2_Leader` | `5DE6B211-486B-11D4-8014-0010830122F0` | - | - | - | - |
| `ON_OBSOLETE_V2_DimLinear` | `5DE6B20D-486B-11D4-8014-0010830122F0` | - | - | - | - |
| `ON_OBSOLETE_V2_DimRadial` | `5DE6B20E-486B-11D4-8014-0010830122F0` | - | - | - | - |
| `ON_OBSOLETE_V2_DimAngular` | `5DE6B20F-486B-11D4-8014-0010830122F0` | - | - | - | - |
| `ON_OBSOLETE_V5_TextObject` | `46F75541-F46B-48BE-AA7E-B353BBE068A7` | 4:2, 50:1 | - | - | - |
| `ON_OBSOLETE_V5_Leader` | `14922B7A-5B65-4F11-8345-D415A9637129` | 50:4 | - | - | - |
| `ON_OBSOLETE_V5_DimLinear` | `BD57F33B-A1B2-46E9-9C6E-AF09D30FFDDE` | 4:1, 50:1 | - | - | - |
| `ON_OBSOLETE_V5_DimRadial` | `B2B683FC-7964-4E96-B1F9-9B356A76B08B` | 4:1 | - | - | - |
| `ON_OBSOLETE_V5_DimAngular` | `841BC40B-A971-4A8E-94E5-BBA26D67348E` | 4:1, 50:1 | - | - | - |
| `ON_OBSOLETE_V5_DimOrdinate` | `C8288D69-5BD8-4F50-9BAF-525A0086B0C3` | 4:1 | - | - | - |

**Other classes that can appear embedded or in other tables**

| Class | UUID (canonical) | Top level, own UUID (archive:count) | Top level via alias | Embedded, own UUID | Embedded via alias |
|---|---|---|---|---|---|
| `ON_InstanceDefinition` | `26F8BFF6-2618-417F-A158-153D64A94989` | 4:2, 80:2 | - | - | - |
| `ON_CurveOnSurface` | `4ED7D4D8-E947-11D3-BFE5-0010830122F0` | - | - | - | - |
| `ON_PolyEdgeCurve` | `39FF3DD3-FE0F-4807-9D59-185F0D73C0E4` | - | - | - | - |
| `ON_PolyEdgeSegment` | `42F47A87-5B1B-4E31-AB87-4639D78325D6` | - | - | - | - |
| `ON_RevSurface` | `A16220D3-163B-11D4-8000-0010830122F0` | 60:1, 70:1, 80:1 | - | 50:609 | 4:273, 50:2, 60:1, 70:1, 80:8 |
| `ON_SumSurface` | `C4CD5359-446D-4690-9FF5-29059732472B` | - | - | 50:150 | 4:127, 50:2 |
| `ON_SurfaceProxy` | `4ED7D4E2-E947-11D3-BFE5-0010830122F0` | - | - | - | - |
| `ON_OffsetSurface` | `00C61749-D430-4ECC-83A8-29130A20CF9C` | - | - | - | - |
| `ON_PointGrid` | `4ED7D4E5-E947-11D3-BFE5-0010830122F0` | - | - | - | - |
| `ON_NurbsCage` | `06936AFB-3D3C-41AC-BF70-C9319FA480A1` | - | - | - | - |
| `ON_BrepVertex` | `60B5DBC0-E660-11D3-BFE4-0010830122F0` | - | - | - | - |
| `ON_BrepEdge` | `60B5DBC1-E660-11D3-BFE4-0010830122F0` | - | - | - | - |
| `ON_BrepTrim` | `60B5DBC2-E660-11D3-BFE4-0010830122F0` | - | - | - | - |
| `ON_BrepLoop` | `60B5DBC3-E660-11D3-BFE4-0010830122F0` | - | - | - | - |
| `ON_BrepFace` | `60B5DBC4-E660-11D3-BFE4-0010830122F0` | - | - | - | - |
| `ON_BrepRegion` | `CA7A0092-7EE6-4F99-B9D2-E1D6AA798AA1` | - | - | - | - |
| `ON_BrepFaceSide` | `30930370-0D5B-4EE4-8083-BD635C7398A4` | - | - | - | - |

Notes:

- `ON_CurveProxy`, `ON_SurfaceProxy` and `ON_OffsetSurface` have UUIDs but their `Read`/`Write`
  return false, so they never appear in files (`opennurbs_curveproxy.cpp:323-337`,
  `opennurbs_surfaceproxy.cpp:120-134`). `ON_BrepEdge`/`ON_BrepTrim`/`ON_BrepFace` derive from
  them but are serialized inline by `ON_Brep::Read`, never through a class wrapper.
- `ON_Annotation` (`B5802E0C-5B16-43C9-BD43-A3E0AC18203B`), `ON_Dimension`
  (`EE6571FE-1596-4D5B-BD6D-7072B0643986`), `ON_Curve`, `ON_Surface`, `ON_Geometry` and
  `ON_Object` are abstract (`ON_VIRTUAL_OBJECT_IMPLEMENT`) and never appear as a stored class.
- `ON_Light` records are in the light table (0.7), not the object table.
- `ON_InstanceDefinition` records are in the instance definition table (section 7).
- `ON_DetailView` and `ON_ClippingPlaneSurface` describe layout/clipping state, not renderable
  geometry; `ON_MorphControl` and `ON_NurbsCage` are deformation controls. Report them, do not
  draw them.

### 8.2 Alias UUIDs (must be accepted)

`ON_ClassId::ClassId` (`opennurbs_object.cpp:878-950`) maps these legacy UUIDs when no class is
registered under them. Rhino writes several of them itself because its runtime classes derive
from the openNURBS ones (`TL_*` = Rhino's own subclasses), so they are the **common** case, not
an edge case:

| Alias UUID | Name in openNURBS comment | Read as | Seen in samples |
|---|---|---|---|
| `F06FC243-A32A-4608-9DD8-A7D2C4CE2A36` | TL_Brep | `ON_Brep` | top level 3:327, 4:143, 50:17, 60:8, 70:14, 80:15 (524 of 625 Breps) |
| `0705FDEF-3E2A-11D4-800E-0010830122F0` | old trimmed surface | `ON_Brep` | top level 2:236 (object type 0x8) |
| `2D4CFEDB-3E2A-11D4-800E-0010830122F0` | old b-rep | `ON_Brep` | top level 2:49 (object type 0) |
| `5EAF1119-0B51-11D4-BFFE-0010830122F0` | TL_NurbsCurve | `ON_NurbsCurve` | top level 4:9, 50:10, 70:227, 80:3; embedded 3:6889, 4:23058, 50:508, 60:239, 70:440, 80:133 |
| `76A709D5-1550-11D4-8000-0010830122F0` | old nurbs curve | `ON_NurbsCurve` | embedded 2:6880 |
| `4760C817-0BE3-11D4-BFFE-0010830122F0` | TL_NurbsSurface | `ON_NurbsSurface` | embedded 3:675, 4:2542, 50:49, 60:26, 70:45, 80:12 |
| `FA4FD4B5-1613-11D4-8000-0010830122F0` | old nurbs surface | `ON_NurbsSurface` | none |
| `EF638317-154B-11D4-8000-0010830122F0` | old poly curve | `ON_PolyCurve` | top level 2:722 (every archive-2 curve object) |
| `0A8401B6-4D34-4B99-8615-1B4E723DC4E5` | TL_RevSurface | `ON_RevSurface` | embedded 4:273, 50:2, 60:1, 70:1, 80:8 |
| `665F6331-2A66-4CCE-81D0-B5EEBD9B5417` | TL_SumSurface | `ON_SumSurface` | embedded 4:127, 50:2 (**missing in cadmpeg**) |
| `CF33BE29-09B4-11D4-BFFB-0010830122F0` | ON__OBSOLETE__CircleCurve | `ON_ArcCurve` layout variant (full circle; see 5b.7) | none (**missing in cadmpeg**) |

cadmpeg knows all other aliases (`curves.rs:53-62`, `surfaces.rs:36-53`, `brep.rs:37-45`).

### 8.3 Userdata class UUIDs that matter for rendering

| UUID | Class | Where | Meaning |
|---|---|---|---|
| `CE28DE29-F4C5-4FAA-A50A-C3A6849B6329` | `ON_UserStringList` | attribute userdata (0x02000073), also class userdata | object user strings (section 3) |
| `17F24E75-21BE-4A7B-9F3D-7F85225247E3` | `ON_V5_MeshDoubleVertices` | mesh class userdata | double-precision vertices in V5-era meshes (section 4) |
| `31F55AA3-71FB-49F5-A975-757584D937FF` | `ON_V4V5_MeshNgonUserData` | mesh class userdata | V4/V5 n-gons (section 4) |
| `A8130A3E-E4F3-4CB0-BB8A-F10A473912D0` | `ON_V5ExtrusionDisplayMeshCache` | extrusion class userdata | V5 extrusion cached meshes (section 6) |
| `7FE23D63-E536-43F1-98E2-C807A2625AFF` | `ON_V5_BrepRegionTopologyUserData` | Brep class userdata | region topology (not needed for rendering) |
| `2868B9CD-28AE-4EA7-8073-BD390B3E97C8` | `ON_SubDMeshProxyUserData` | mesh class userdata | mesh that is a SubD proxy |
| `B5628CA9-82C4-4CAE-9883-487B3E4AB28B` | `ON_PerObjectMeshParameters` | attribute userdata | per-object render mesh settings |
| `AFA82772-1525-43DD-A63C-C84AC5806911` | `ON_RdkUserData` | material/attribute userdata | RDK (render content) data |
| `B8C04604-B4EF-43B7-8C26-1AFB8F1C54EB`, `CB5EB395-BF1B-4112-8F2F-F728FCE8169C`, `AA03D9C3-4CCF-4431-A06E-25F38CF3913F`, `2D5AFEA9-F458-4079-992F-C2D405D9383B`, `429DCD06-5643-4254-BDE8-C0557F8FD083` | displacement, edge softening, thickening, curve piping, shut lining | attribute userdata | mesh modifiers: the render mesh Rhino displays differs from the stored geometry |

---

## 9. Traps and per-version quirks (consolidated)

Class-specific traps are repeated in their sections; this list collects everything a decoder
author must keep in mind, roughly in order of how likely it is to bite.

### 9.1 Framing and dispatch

1. **Alias class UUIDs are the norm** (8.2). 524 of the 625 sample Breps and most NURBS curves
   and surfaces use `TL_*` or "old" UUIDs. A decoder keyed only on the `ON_OBJECT_IMPLEMENT`
   UUIDs silently loses most geometry. cadmpeg lacks `TL_SumSurface` (127 embedded occurrences
   in archive-4 Breps) and the obsolete circle.
2. **Branch on the payload version, not the archive version.** The same archive version holds
   several payload minors (e.g. archive-60 files from Rhino 6 hold `ON_NurbsCurve` 1.0 while
   archive 70/80 hold 1.1; see 5b.3). Conversely, writers emit old payload versions into new
   archives.
3. **Folder names lie**: `samples/opennurbs/example_files/V2/v2_HumanHead1.3dm` is archive 3.
   Read the header.
4. **Chunk value width** changes at archive 50 (archive 5 = early V5 beta still uses 4 bytes,
   with V5 class layouts; none in the samples). Short-chunk values are 8 bytes too in V5+.
5. **4-byte short values are signed** except four typecodes (0.2); long lengths are unsigned.
6. **Long-chunk lengths include the trailing CRC.** Body = length - 4 when the CRC bit is set.
7. **CRC coverage excludes nested chunks** (0.3). Do not compute a parent CRC over its whole
   body. A fast decoder can skip CRC checks entirely; openNURBS never aborts on a CRC mismatch.
8. **Null class wrapper**: nil UUID chunk only, no data/userdata/end chunks (0.5).
9. **Unknown class / unknown chunk / newer minor**: skip by length; never fail the file
   (0.4). openNURBS reports partially read chunks only as warnings.
10. **Object type 0** in archive-2 "old b-rep" records; dispatch on the class UUID.
11. **Duplicate or nil object UUIDs** occur in older files; openNURBS assigns fresh ids in both
    cases (`Read3dmObject`, `opennurbs_archive.cpp:15886-15897`). Do not key maps on the
    attribute UUID without a fallback.
12. **No alignment or padding anywhere**; everything is packed little-endian.

### 9.2 Primitives

13. **`bool8` vs `i32` flags**: `ReadBool` is one byte, but many flags are written with
    `WriteInt`. Follow each class layout exactly.
14. **Non-canonical booleans** (2..255) exist in files written by 2017-era Rhino 6 builds;
    accept any non-zero as true (1.2).
15. **`ReadSize` is 4 bytes in every archive**, including the compressed-buffer size in V5+
    files (2.1). Only `ReadBigSize` is 8 bytes.
16. **Strings**: the count includes the terminator; the empty string is a bare `0`; embedded
    NULs are legal; content is always UTF-16LE for `ON_wString` (1.6).
17. **Homogeneous CVs**: rational NURBS control points are stored as (w·x, w·y, w·z, w)
    (5b.3, 5b.4).
18. **`ON_Xform` is row-major with translation in elements 3, 7, 11** (1.3).
19. **Colours are R, G, B, A bytes with A = transparency** (0 = opaque) (1.4).

### 9.3 Writer and archive history

20. **Writer stamp formats**: `YYYYMMDDn` (up to archive 50 in the samples) vs encoded with bit
    31 set (archive 60+). Compare as `u32` (0.8). Files without a stamp are pre-2000-12-21.
21. **No userdata in archives 1 and 2**: `WriteObjectUserData` returns immediately when
    archive < 3 (`opennurbs_archive.cpp:4811-4815`), and attribute userdata chunks are only
    written for archive >= 4 (`:13260`). User strings therefore only exist from archive 4 on.
22. **Goo userdata** keeps the chunk width of the archive that first wrote it (0.6).
23. **Archive <= 2 point clouds** are written as individual `ON_Point` records.
24. **Archive 2 geometry**: every curve object is an "old poly curve" (alias UUID) even with a
    single segment; Breps use "old trimmed surface"/"old b-rep" UUIDs and the Brep 2.x layout
    with unwrapped curves and surfaces (5a, 5b.1); NURBS `reserved` fields hold legacy flags
    and bounding boxes are filled in.
25. **Archive 1 (V1)** has no object table and no class wrappers: geometry is stored as flat
    legacy records (`TCODE_LEGACY_*`, `TCODE_RH_POINT`, `TCODE_MESH_OBJECT`,
    `TCODE_RHINOIO_OBJECT_*`, annotation records) at the top level, with CRC16 on legacy
    typecodes. openNURBS reads it with `Read3dmV1Object` and the `ReadV1_*` functions
    (`opennurbs_archive.cpp:13333-15700`); cadmpeg with `legacy.rs`. 26 samples are V1
    (`samples/opennurbs/example_files/V1/*`). Out of scope for this document.
26. **Archive 90 (Rhino 9)**: the framing is the same, but cadmpeg does not decode it and no
    sample exists; expect new payload minors and new classes. Treat unknown majors as
    unsupported, not corrupt.
27. **Early Rhino 6 WIP instance definitions** (archive 60, writer stamp <= 2348834153) were not
    wrapped in an internal chunk; openNURBS suppresses the partial-read warning for them
    (`ReadObjectHelper`, `opennurbs_archive.cpp:5243-5255`).

### 9.4 Class-specific traps (details in the class sections)

**Attributes (section 3)**

28. The tagged (V5+) item stream has no per-item lengths: an unknown, out-of-order or
    too-new item id ends the parse (keep what was read, skip to the chunk end) (3.3).
29. `visible` (item 11) defaults to true and is independent of `mode`; treat an object as hidden
    when `visible == false` or `(mode & 0x0F) == 1`. `mode & 0x0F == 3` marks block-definition
    geometry, which must only be drawn through instance references (3.6).
30. Use the material index only when the material source is 1 (object); older V5 writers emit
    item 4 without item 16 (3.6).
31. Layer index -1 is legal (the system "Default" layer, not stored in the layer table) (3.6).
32. Page-space objects (item 19 = 2, legacy 1.6 space byte = 1) belong to layouts; exclude them
    from model renders (3.6).
33. Attribute user strings (0x02000073) and geometry user strings (class-wrapper userdata) are
    different lists; both use `ON_UserStringList` (3.7). Drop `$temp_object$`.

**Meshes (section 4)**

34. Faces come before vertices in the record (4.5).
35. `vcount == 0` means the five vertex buffers and the S buffer are absent entirely (4.6, 4.7).
36. The tail after S (mapping tag, flags, n-gons, double vertices, vertex box) is read only when
    minor >= 4 **and** the writer stamp >= 200606010 (4.7).
37. Real files contain out-of-range face indices (`Rhino_Logo_Mesh7.3dm`): drop bad faces, not
    the mesh (4.14).
38. Compression method is not tied to archive version or size: rhino3dm writers store megabyte
    arrays uncompressed (4.1).
39. Prefer synchronised double-precision vertices (3.7+ chunk or V5 userdata) for georeferenced
    coordinates (4.7, 4.8).

**Breps (section 5a)**

40. The edge and trim record layout depends on the writer stamp (>= 200206180) and archive >= 3,
    not on the Brep payload version (5a.3).
41. The face-array minor (1.1 vs 1.2) depends on the writer build, not the archive (5a.10).
42. openNURBS only maps trim types 0-4 and loop types 0-3 on read (5a.8, 5a.9).
43. Archive-2 Breps (payload 2.1) have no length prefixes: reaching their (absent in the
    samples) meshes requires parsing every curve and surface (5a.12).
44. Render mesh slot *i* belongs to face *i*; meshes already follow the face orientation
    (`m_bRev` applied) (5a.11).
45. `m_is_solid` = 3 means "not solid" (cadmpeg names it out of range) (5a.11).
46. The stored Brep bounding box can be oversized; openNURBS recomputes it (5a.11).

**Curves and surfaces (section 5b)**

47. `ON_NurbsCurve` 1.1 has one extra trailing byte; archive-60 files from Rhino 6 hold 1.0 (5b.3).
48. Rational control points are homogeneous (5b.3, 5b.4).
49. 2-D curves still store 3-D points (lines, polylines) and full planes (arcs) (5b.16).
50. `ON_PlaneSurface` extents are not the domain; trims live in domain space (5b.9).

**Extrusions and instances (sections 6, 7)**

51. Two cache layouts: archive 50 keeps cached meshes in `ON_V5ExtrusionDisplayMeshCache`
    userdata on the class wrapper; archive 60+ has an `ON_MeshCache` chunk inside the class data
    (6.5, 6.6). Pick cached meshes by mesh UUID, not by position.
52. Typecode 0x40008000 appears both with and without the two version `i32`s; the context
    decides (6.6).
53. Instance definitions have a V5 and a V6 layout, chosen by archive and writer stamp (7.3).
    Nested placements compose as `M_outer · M_inner`; mirrored placements flip winding (7.4).

---

## 10. Sample coverage summary

All paths are relative to `samples/` (see `samples/manifest.tsv`). Each class section has its
own detailed sample table (3.10, 4.15, 5a.16, 5b.17, 6.10, 7.6); this is the cross-section view
for choosing regression files.

### 10.1 Archive versions and writer stamps

| Archive | Files | Writer stamps (examples) | Notes |
|---|---|---|---|
| 1 | 26 (`opennurbs/example_files/V1/*`) | none | V1 flat grammar, out of scope (9.3 #25) |
| 2 | 33 | 200107270 (Rhino 2), 200210150 (openNURBS toolkit) | old UUIDs, Brep 2.1, attributes 1.1/1.2 |
| 3 | 34 (includes `V2/v2_HumanHead1.3dm`) | 200209100 ... 200308080 | TL_Brep, attributes 1.2 |
| 4 | 15 | 200607130, 200710174, 200710309 | attributes 1.7, compressed Brep render meshes |
| 5 | 0 | | early V5 beta band, not covered |
| 50 | 22 | 201101285 ... 202308218 | tagged attributes, extrusions 1.2 |
| 60 | 13 | 2348834660 ... | first encoded stamps; mesh 3.7 and 3.8 |
| 70 | 19 | 2382393748 ... | face array 1.1 and 1.2 |
| 80 | 31 | 2415950832 ... 2425133316 | extrusion mesh cache 1.3 |
| 90 | 0 | | Rhino 9, not covered |

`rhino3dm/tests/models/subd_creases.3dm` (archive 80) was written by an openNURBS 9.0 build
(attributes 2.14).

### 10.2 Recommended regression set

| Purpose | File(s) |
|---|---|
| Smallest mesh, every archive | `opennurbs/example_files/V2/v2_my_mesh.3dm`, `V3/v3_my_mesh.3dm`, `V5/v5_my_mesh.3dm`, `V6/my_mesh.3dm`, `V7/v7_my_mesh.3dm` |
| Large compressed meshes / Brep render meshes | `V4/v4_Wheel_PG.3dm` (index width 4), `V5/v5_disk_brake.3dm` (888 meshed faces, 740 inner loops) |
| Stored (method 0) megabyte mesh | `rhino-developer-samples/rhino3dm/models/Rhino_Logo_Mesh8_NamedViews.3dm` |
| Out-of-range faces | `rhino-developer-samples/rhino3dm/models/Rhino_Logo_Mesh7.3dm` |
| Double-precision vertices | `V7/v7_my_mesh.3dm` (3.7+ chunk), `rhino-developer-samples/rhino3dm/models/MeshSphere_r5.3dm` (V5 userdata) |
| N-gons, vertex colours, texture coordinates | `rhino-developer-samples/rhino3dm/models/RectifiedArchimedeanSolids8.3dm`, `rhino3dm/tests/models/mesh.3dm`, `rhino3dm/tests/models/meshWithTexture.3dm` |
| Brep 2.1 (archive 2, no length prefixes) | `V2/v2_RhinoLogo.3dm`, `V2/v2_UntrimmedSurfaces.3dm` |
| Brep 3.2 with legacy edge/trim layout | `V2/v2_my_brep.3dm`, `V2/v2_my_trimmed_surface.3dm` |
| Brep 3.3, face array 1.1 vs 1.2 | `V7/v7_rhino_logo_nurbs.3dm` vs `three.js/examples/models/3dm/Rhino_Logo.3dm` |
| Breps without render meshes (planar fallback) | `V*/…my_trimmed_surface.3dm` (PlaneSurface), `V*/…my_brep.3dm` (planar NURBS box), `V5/v5_teapot.3dm`, `rhino3dm/tests/models/file3dm_stuff.3dm` (mixed) |
| Every curve class and alias | `V*/…my_curves.3dm`, `V2/v2_Soccer.3dm` (old poly curve), `V4/v4_example_file.3dm` (TL_NurbsCurve), `V4/*` Breps (TL_SumSurface, TL_RevSurface) |
| Extrusions | `V5/v5_extrusions.3dm` (1.2, multi-profile, the only miter), `rhino3dm/tests/models/blocks.3dm` (1.3, empty cache), `V8/v8_section_hatch.3dm` (1.3, render mesh) |
| Blocks | `V4/v4_example_file.3dm` (nested, V5-layout definitions 1.5), `rhino3dm/tests/models/blocks.3dm`, `rhino3dm/tests/models/file3dm_stuff.3dm` (V6 layout) |
| Attributes: legacy / tagged / user strings / groups / page space | `V2/v2_T-Joint1.3dm`, `V4/v4_Wheel_PG.3dm`, `rhino3dm/tests/models/object_UserStrings.3dm`, `rhino3dm/tests/models/groups.3dm`, `V5/v5_example_file.3dm` |
| Annotations and other non-render classes | `V6/V6_annotation.3dm`, `V7/v7_rhino_logo_annotations.3dm`, `rhino3dm/tests/models/textEntities_r8.3dm`, `V8/v8_selective_clipping.3dm` (clipping planes, SubD, hatch), `V5/v5_example_file.3dm` (detail view, V5 annotations) |
| Lights (light table) | `V*/…my_spot_light.3dm`, `rhino3dm/tests/models/light.3dm` |

### 10.3 Not covered by any sample (candidates for `samples/synthetic/`)

- Architectural scale and units; files over 100 MB; millions of mesh vertices (cadmpeg's 256 MiB
  per-document mesh budget would refuse them, 4.13 #15).
- Archive 5 and archive 90.
- V5-style extrusion mesh cache (`ON_V5ExtrusionDisplayMeshCache` userdata); analysis or preview
  items in a 1.3 mesh cache; extrusions with a start miter, `m_t != [0,1]`, transposed, or more
  than 2 profiles.
- Nested blocks in archive 50+; rotated, scaled or mirrored placements; linked or
  linked-and-embedded definitions.
- Brep analysis meshes; Breps with only some faces meshed; crvonsrf/ptonsrf/slit trims and loops;
  `c2i = -1`; trim proxy reversal; per-face colours; region topology present.
- Mesh major 1, minors 3.0/3.1/3.3/3.6; `vcount == 0`; curvature arrays and curvature stats;
  `ON_V4V5_MeshNgonUserData`.
- Tagged attributes with a hidden object (item 11), material references inside rendering
  attributes, items 2, 7, 9, 14, 21-28, 33-35, 37-41.
- `ON_CurveOnSurface`, obsolete circle (CF33BE29), `ON_RevSurface` 1.x, `ON_ClippingPlane`
  minors 2-5, `ON_MorphControl`, `ON_Leader` (V6), `ON_DimOrdinate`, `ON_Centermark`.

---

## 11. What could not be determined with confidence

Consolidated from the per-section "Uncertain" lists. Everything else in this document was
checked against both references and, where marked "probed" or "verified", against the sample
bytes.

1. **Source-only layouts (no sample bytes)**: mesh major 1 and minors 3.0/3.1/3.3/3.6; Brep
   payloads 3.0/3.1; NURBS surface minor > 0; `ON_RevSurface` 1.x; `ON_CurveOnSurface`; the
   obsolete circle; legacy attributes 1.0 and 1.8; many tagged attribute items (3.11); clipping
   plane minors 2-5; V4/V5 n-gon userdata; region topology present; the three-slot
   `ON_V5ExtrusionDisplayMeshCache` payload as written by real Rhino 5.
2. **Archive 5** (early V5 beta, 4-byte chunk values with V5 layouts): assumed identical to 50
   apart from chunk width; openNURBS and cadmpeg disagree on attribute dispatch there (3.9 #1).
3. **Archive 90 / openNURBS 9**: attributes 2.14 and cadmpeg's item 42 (detail background
   visible) cannot be confirmed from the 8.35 source; one 2.14 sample exists and carries no
   items.
4. **Writer-stamp thresholds**: the exact V7 build at which the Brep face array moved to 1.2
   (between 2382393748 and 2382395042); the practical effect of the mesh tail gate 200606010 and
   the Brep layout gate 200206180 on files with missing stamps (openNURBS assumes its own
   version for archive >= 3, cadmpeg assumes the legacy layout).
5. **Pre-November-2001 arcs** may store non-canonical redundant circle points; cadmpeg would
   reject them, openNURBS ignores them. No sample shows this.
6. **Orientation rules** for Breps were verified on planar faces (vertex polygons, 351 meshed
   planar faces); loops with curved edges and curved faces were not sampled exhaustively.
7. **Partial render meshes within one Brep** and **analysis meshes** never occur in the samples;
   whether Rhino writes them is unknown.
8. **Block semantics** beyond the samples: unit scaling of linked definitions, composition order
   in archive 50+ nested blocks (taken from code), placements with non-affine bottom rows.
9. **Goo userdata with mismatched chunk widths** (V4 payload copied into a V5+ file) is handled
   by openNURBS through a memory archive; we did not find a sample to confirm the byte shape.
10. **Big-endian writers**: all samples are little-endian; openNURBS has a suspicious
    `Count()*3` swap for mesh colours on big-endian hosts (4.16) that does not affect LE files.

---

## Appendix A. Verification performed

- A per-sample inventory of every class-wrapped table record (archive, writer stamp, table,
  object type, class UUID, class-data version byte, userdata classes, nested class UUIDs) was
  generated for all 193 manifest files; it backs the counts in sections 8 and 10.
- Each class section was validated by a byte-exact probe that parsed every payload of that class
  in the public samples (some probes also covered the two `samples/synthetic/` files) and required the parse to end exactly at
  the class-data chunk end: 9,284 attribute records, 7,967 mesh payloads, 910 Breps, 86,131
  curve/surface wrappers, all extrusions and instance references. No failures or leftover bytes.
  The curve and mesh probes were also run over the local `samples/private/` files (aggregate
  counts only; no names recorded here).
- CRC coverage (direct bytes only) was confirmed on object records (stored CRC = 0), Brep
  component chunks (6,250), extrusion class data and userdata wrappers.
- The probe scripts and the inventory lived in a temporary directory during this research
  (`scripts-attrs/`, `scripts-mesh/`, `scripts-brep/`, `scripts-curves/`, `scripts-extr/`,
  `inventory.tsv`); they are throwaway and not part of the repository.
